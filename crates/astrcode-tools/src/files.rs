//! File tools: readFile, writeFile, editFile, applyPatch, findFiles, grep.
//! Patterns: structured error metadata, UTF-8 safe truncation, smart-quote normalization.

use std::collections::BTreeMap;
use std::path::PathBuf;

use astrcode_core::tool::*;

// ─── readFile ────────────────────────────────────────────────────────────

pub struct ReadFileTool { pub working_dir: PathBuf }

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "readFile".into(),
            description: "Read a file. Use offset/limit for large files. Returns line-numbered output.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}),
        }
    }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let raw = args["path"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
        let path = resolve_path(&self.working_dir, raw);
        if !path.exists() { return Ok(not_found(&path)); }
        if path.is_dir() { return Ok(directory(&path)); }
        if is_binary(&path) { return Ok(binary(&path)); }

        let content = std::fs::read_to_string(&path).map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().map(|n| n as usize).unwrap_or(usize::MAX);

        let lines: Vec<String> = content.lines().skip(offset).take(limit).enumerate()
            .map(|(i, l)| format!("{:>6}\t{}", i + offset + 1, l)).collect();

        let mut meta = BTreeMap::new();
        meta.insert("path".into(), serde_json::json!(path.display().to_string()));
        meta.insert("totalLines".into(), serde_json::json!(content.lines().count()));
        meta.insert("shownLines".into(), serde_json::json!(lines.len()));

        Ok(ToolResult { call_id: String::new(), content: lines.join("\n"), is_error: false, metadata: meta })
    }
}

// ─── writeFile ───────────────────────────────────────────────────────────

pub struct WriteFileTool { pub working_dir: PathBuf }

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "writeFile".into(),
            description: "Create or overwrite a file. Creates parent directories.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let raw = args["path"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
        let content = args["content"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'content'".into()))?;
        let path = resolve_path(&self.working_dir, raw);
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).map_err(|e| ToolError::Execution(format!("mkdir: {e}")))?; }

        let old = std::fs::read_to_string(&path).ok();
        std::fs::write(&path, content).map_err(|e| ToolError::Execution(format!("write: {e}")))?;

        let msg = if let Some(o) = old {
            format!("Updated {} ({}→{} bytes)", path.display(), o.len(), content.len())
        } else {
            format!("Created {} ({} bytes)", path.display(), content.len())
        };
        Ok(ToolResult { call_id: String::new(), content: msg, is_error: false, metadata: BTreeMap::new() })
    }
}

// ─── editFile ────────────────────────────────────────────────────────────

pub struct EditFileTool { pub working_dir: PathBuf }

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "editFile".into(),
            description: "Replace exactly one occurrence of old_string with new_string. Add context to disambiguate.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"}},"required":["path","old_string","new_string"]}),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let raw = args["path"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
        let old_str = clean_quotes(args["old_string"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'old_string'".into()))?);
        let new_str = clean_quotes(args["new_string"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'new_string'".into()))?);
        let path = resolve_path(&self.working_dir, raw);
        if old_str.is_empty() { return Err(ToolError::InvalidArguments("old_string empty".into())); }

        let original = clean_quotes(&std::fs::read_to_string(&path).map_err(|e| ToolError::Execution(format!("read: {e}")))?);
        let count = original.matches(&old_str).count();

        if count == 0 { return Err(ToolError::Execution(format!("old_string not found in {}", path.display()))); }
        if count > 1 { return Err(ToolError::Execution(format!("old_string found {count} times — add context"))); }

        let updated = original.replacen(&old_str, &new_str, 1);
        std::fs::write(&path, &updated).map_err(|e| ToolError::Execution(format!("write: {e}")))?;
        Ok(ToolResult { call_id: String::new(), content: format!("Edited {}", path.display()), is_error: false, metadata: BTreeMap::new() })
    }
}

// ─── applyPatch ──────────────────────────────────────────────────────────

pub struct ApplyPatchTool { pub working_dir: PathBuf }

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "applyPatch".into(),
            description: "Apply a unified diff patch. Supports multi-file patches.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"]}),
        }
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        Err(ToolError::Execution("applyPatch: full parser pending. Use writeFile + editFile for now.".into()))
    }
}

// ─── findFiles ───────────────────────────────────────────────────────────

pub struct FindFilesTool { pub working_dir: PathBuf }

#[async_trait::async_trait]
impl Tool for FindFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "findFiles".into(),
            description: "Find files by glob pattern (e.g. '**/*.rs'). Results sorted newest first, max 500.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        }
    }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let pattern = args["pattern"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'pattern'".into()))?;
        let p = self.working_dir.join(pattern);
        let mut results: Vec<(String, std::time::SystemTime)> = Vec::new();
        for entry in glob::glob(&p.display().to_string()).map_err(|e| ToolError::Execution(format!("glob: {e}")))?.flatten() {
            if entry.is_file() {
                let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
                let rel = entry.strip_prefix(&self.working_dir).map(|p| p.display().to_string()).unwrap_or_else(|_| entry.display().to_string());
                results.push((rel, mtime));
            }
        }
        results.sort_by(|a, b| b.1.cmp(&a.1));
        let out: Vec<_> = results.into_iter().take(500).map(|(s, _)| s).collect();
        let mut meta = BTreeMap::new();
        meta.insert("count".into(), serde_json::json!(out.len()));
        Ok(ToolResult { call_id: String::new(), content: out.join("\n"), is_error: false, metadata: meta })
    }
}

// ─── grep ────────────────────────────────────────────────────────────────

pub struct GrepTool { pub working_dir: PathBuf }

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description: "Search files with a regex. Returns file:line:content, max 250 matches.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]}),
        }
    }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let pat = args["pattern"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'pattern'".into()))?;
        let re = regex::Regex::new(pat).map_err(|e| ToolError::Execution(format!("regex: {e}")))?;
        let root = if let Some(p) = args["path"].as_str() { resolve_path(&self.working_dir, p) } else { self.working_dir.clone() };
        let mut matches = Vec::new();
        walk_grep(&root, &re, &mut matches, 250).map_err(|e| ToolError::Execution(format!("grep: {e}")))?;
        let mut meta = BTreeMap::new();
        meta.insert("matches".into(), serde_json::json!(matches.len()));
        Ok(ToolResult { call_id: String::new(), content: matches.join("\n"), is_error: false, metadata: meta })
    }
}

fn walk_grep(root: &PathBuf, re: &regex::Regex, out: &mut Vec<String>, max: usize) -> std::io::Result<()> {
    if out.len() >= max { return Ok(()); }
    if root.is_file() {
        if let Ok(c) = std::fs::read_to_string(root) {
            for (i, line) in c.lines().enumerate() {
                if re.is_match(line) {
                    out.push(format!("{}:{}:{}", root.display(), i + 1, trunc(&line, 500)));
                    if out.len() >= max { break; }
                }
            }
        }
    } else if root.is_dir() {
        for e in std::fs::read_dir(root)? {
            let p = e?.path();
            if !(p.is_dir() && p.file_name().map_or(false, |n| n == ".git")) { walk_grep(&p, re, out, max)?; }
        }
    }
    Ok(())
}

// ─── Shared ──────────────────────────────────────────────────────────────

fn resolve_path(cwd: &PathBuf, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn is_binary(p: &PathBuf) -> bool {
    std::fs::read(p).map(|d| d.iter().take(8192).any(|&b| b == 0)).unwrap_or(false)
}

fn not_found(p: &PathBuf) -> ToolResult {
    ToolResult { call_id: String::new(), content: format!("Not found: {}", p.display()), is_error: false, metadata: BTreeMap::from([("notFound".into(), serde_json::json!(true))]) }
}

fn directory(p: &PathBuf) -> ToolResult {
    ToolResult { call_id: String::new(), content: format!("Is a directory: {} — use findFiles or shell ls", p.display()), is_error: false, metadata: BTreeMap::from([("directory".into(), serde_json::json!(true))]) }
}

fn binary(p: &PathBuf) -> ToolResult {
    ToolResult { call_id: String::new(), content: format!("Binary file: {}", p.display()), is_error: false, metadata: BTreeMap::from([("binary".into(), serde_json::json!(true))]) }
}

fn trunc(s: &str, max: usize) -> String {
    if s.len() <= max { return s.into(); }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}

fn clean_quotes(s: &str) -> String {
    s.replace('\u{201C}', "\"").replace('\u{201D}', "\"").replace('\u{2018}', "'").replace('\u{2019}', "'")
}

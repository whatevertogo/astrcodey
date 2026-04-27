//! File tools: readFile, writeFile, editFile, applyPatch, findFiles, grep.
//! Patterns: structured error metadata, UTF-8 safe truncation, smart-quote normalization.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use astrcode_core::tool::*;
use serde::Deserialize;

// ─── readFile ────────────────────────────────────────────────────────────

pub struct ReadFileTool {
    pub working_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadFileArgs {
    path: PathBuf,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    char_offset: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "readFile".into(),
            description: "Read a file. Use offset/limit for large files. Returns line-numbered \
                          output."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "maxChars": { "type": "integer", "minimum": 1 },
                    "charOffset": { "type": "integer", "minimum": 0 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: ReadFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid readFile args: {e}")))?;
        let path = resolve_path(&self.working_dir, &args.path);
        if !path.exists() {
            return Ok(not_found(&path));
        }
        if path.is_dir() {
            return Ok(directory(&path));
        }
        if is_binary(&path) {
            return Ok(binary(&path));
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(usize::MAX);
        let char_offset = args.char_offset.unwrap_or(0);
        let max_chars = args.max_chars.unwrap_or(20_000);

        let lines: Vec<String> = content
            .lines()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, l)| format!("{:>6}\t{}", i + offset + 1, l))
            .collect();
        let rendered = lines.join("\n");
        let rendered = slice_chars(&rendered, char_offset, max_chars);

        let mut meta = BTreeMap::new();
        meta.insert("path".into(), serde_json::json!(path.display().to_string()));
        meta.insert(
            "totalLines".into(),
            serde_json::json!(content.lines().count()),
        );
        meta.insert("shownLines".into(), serde_json::json!(lines.len()));
        meta.insert("charOffset".into(), serde_json::json!(char_offset));
        meta.insert("maxChars".into(), serde_json::json!(max_chars));

        Ok(ToolResult {
            call_id: String::new(),
            content: rendered,
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: None,
        })
    }
}

// ─── writeFile ───────────────────────────────────────────────────────────

pub struct WriteFileTool {
    pub working_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteFileArgs {
    path: PathBuf,
    content: String,
    #[serde(default)]
    create_dirs: bool,
}

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "writeFile".into(),
            description: "Create or overwrite a UTF-8 text file.".into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "createDirs": { "type": "boolean" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: WriteFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid writeFile args: {e}")))?;
        let path = resolve_path(&self.working_dir, &args.path);
        if args.create_dirs {
            let Some(parent) = path.parent() else {
                return Err(ToolError::Execution("path has no parent directory".into()));
            };
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::Execution(format!("mkdir: {e}")))?;
        }

        let old = std::fs::read_to_string(&path).ok();
        std::fs::write(&path, &args.content)
            .map_err(|e| ToolError::Execution(format!("write: {e}")))?;

        let msg = if let Some(o) = old {
            format!(
                "Updated {} ({}→{} bytes)",
                path.display(),
                o.len(),
                args.content.len()
            )
        } else {
            format!("Created {} ({} bytes)", path.display(), args.content.len())
        };
        Ok(ToolResult {
            call_id: String::new(),
            content: msg,
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        })
    }
}

// ─── editFile ────────────────────────────────────────────────────────────

pub struct EditFileTool {
    pub working_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditFileArgs {
    path: PathBuf,
    #[serde(rename = "oldStr", alias = "old_string")]
    old_str: String,
    #[serde(rename = "newStr", alias = "new_string")]
    new_str: String,
    #[serde(default, alias = "replace_all")]
    replace_all: bool,
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "editFile".into(),
            description: "Edit an existing file by replacing exact text. oldStr must appear \
                          exactly once unless replaceAll is true."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "oldStr": { "type": "string" },
                    "newStr": { "type": "string" },
                    "replaceAll": { "type": "boolean" }
                },
                "required": ["path", "oldStr", "newStr"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: EditFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid editFile args: {e}")))?;
        let old_str = clean_quotes(&args.old_str);
        let new_str = clean_quotes(&args.new_str);
        let path = resolve_path(&self.working_dir, &args.path);
        if old_str.is_empty() {
            return Err(ToolError::InvalidArguments("oldStr cannot be empty".into()));
        }

        let original = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let updated = if args.replace_all {
            if !original.contains(&old_str) {
                return Err(ToolError::Execution(format!(
                    "oldStr not found in {}",
                    path.display()
                )));
            }
            original.replace(&old_str, &new_str)
        } else {
            let Some(pos) = find_unique_occurrence(&original, &old_str)? else {
                return Err(ToolError::Execution(format!(
                    "oldStr not found in {}",
                    path.display()
                )));
            };
            let mut updated = String::with_capacity(original.len() - old_str.len() + new_str.len());
            updated.push_str(&original[..pos]);
            updated.push_str(&new_str);
            updated.push_str(&original[pos + old_str.len()..]);
            updated
        };
        std::fs::write(&path, &updated).map_err(|e| ToolError::Execution(format!("write: {e}")))?;
        Ok(ToolResult {
            call_id: String::new(),
            content: format!("Edited {}", path.display()),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        })
    }
}

// ─── applyPatch ──────────────────────────────────────────────────────────

pub struct ApplyPatchTool {
    pub working_dir: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: "Apply a unified diff patch. Supports multi-file patches.".into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string" }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        Err(ToolError::Execution(
            "applyPatch: full parser pending. Use writeFile + editFile for now.".into(),
        ))
    }
}

// ─── findFiles ───────────────────────────────────────────────────────────

pub struct FindFilesTool {
    pub working_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindFilesArgs {
    pattern: String,
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default = "default_true")]
    respect_gitignore: bool,
    #[serde(default = "default_true")]
    include_hidden: bool,
}

fn default_true() -> bool {
    true
}

#[async_trait::async_trait]
impl Tool for FindFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "findFiles".into(),
            description: "Find files by glob pattern (e.g. '**/*.rs'). Results sorted newest \
                          first, max 500."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "root": { "type": "string" },
                    "maxResults": { "type": "integer", "minimum": 1 },
                    "respectGitignore": { "type": "boolean" },
                    "includeHidden": { "type": "boolean" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: FindFilesArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid findFiles args: {e}")))?;
        let root = args
            .root
            .as_deref()
            .map(|root| resolve_path(&self.working_dir, root))
            .unwrap_or_else(|| self.working_dir.clone());
        let p = root.join(&args.pattern);
        let max_results = args.max_results.unwrap_or(500);
        let gitignore = if args.respect_gitignore {
            load_simple_gitignore(&root)
        } else {
            BTreeSet::new()
        };
        let mut results: Vec<(String, std::time::SystemTime)> = Vec::new();
        for entry in glob::glob(&p.display().to_string())
            .map_err(|e| ToolError::Execution(format!("glob: {e}")))?
            .flatten()
        {
            if entry.is_file()
                && (args.include_hidden || !has_hidden_component(&entry))
                && !is_gitignored(&root, &entry, &gitignore)
            {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::UNIX_EPOCH);
                let rel = entry
                    .strip_prefix(&self.working_dir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| entry.display().to_string());
                results.push((rel, mtime));
            }
        }
        results.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        let out: Vec<_> = results
            .into_iter()
            .take(max_results)
            .map(|(s, _)| s)
            .collect();
        let mut meta = BTreeMap::new();
        meta.insert("count".into(), serde_json::json!(out.len()));
        meta.insert("maxResults".into(), serde_json::json!(max_results));
        Ok(ToolResult {
            call_id: String::new(),
            content: out.join("\n"),
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: None,
        })
    }
}

// ─── grep ────────────────────────────────────────────────────────────────

pub struct GrepTool {
    pub working_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    recursive: Option<bool>,
    #[serde(default, alias = "case_insensitive")]
    case_insensitive: bool,
    #[serde(default, alias = "max_matches")]
    max_matches: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default, alias = "file_type")]
    file_type: Option<String>,
    #[serde(default, alias = "before_context")]
    before_context: Option<usize>,
    #[serde(default, alias = "after_context")]
    after_context: Option<usize>,
    #[serde(default, alias = "output_mode")]
    output_mode: GrepOutputMode,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrepOutputMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

#[derive(Debug)]
struct GrepMatch {
    file: String,
    line_no: usize,
    line: String,
    before: Vec<String>,
    after: Vec<String>,
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description: "Search files with a regex. Returns file:line:content, max 250 matches."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "literal": { "type": "boolean" },
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean" },
                    "caseInsensitive": { "type": "boolean" },
                    "maxMatches": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "glob": { "type": "string" },
                    "fileType": { "type": "string" },
                    "beforeContext": { "type": "integer", "minimum": 0 },
                    "afterContext": { "type": "integer", "minimum": 0 },
                    "outputMode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"]
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: GrepArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid grep args: {e}")))?;
        let pattern = if args.literal {
            regex::escape(&args.pattern)
        } else {
            args.pattern.clone()
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(args.case_insensitive)
            .build()
            .map_err(|e| ToolError::Execution(format!("regex: {e}")))?;
        let root = args
            .path
            .as_deref()
            .map(|p| resolve_path(&self.working_dir, p))
            .unwrap_or_else(|| self.working_dir.clone());
        let max_matches = args.max_matches.unwrap_or(250);
        let offset = args.offset.unwrap_or(0);
        let mut state = GrepState {
            seen: 0,
            max_matches,
            offset,
            matches: Vec::new(),
            counts: BTreeMap::new(),
            files: BTreeSet::new(),
        };
        let options = GrepWalkOptions {
            recursive: args.recursive.unwrap_or_else(|| root.is_dir()),
            glob: args.glob.as_deref(),
            file_type: args.file_type.as_deref(),
            before_context: args.before_context.unwrap_or(0),
            after_context: args.after_context.unwrap_or(0),
        };
        walk_grep(&root, &re, &options, &mut state)
            .map_err(|e| ToolError::Execution(format!("grep: {e}")))?;
        let matches = render_grep_output(args.output_mode, &state);
        let mut meta = BTreeMap::new();
        meta.insert("matches".into(), serde_json::json!(state.seen));
        meta.insert("shown".into(), serde_json::json!(matches.len()));
        meta.insert(
            "outputMode".into(),
            serde_json::json!(format!("{:?}", args.output_mode)),
        );
        Ok(ToolResult {
            call_id: String::new(),
            content: matches.join("\n"),
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: None,
        })
    }
}

struct GrepWalkOptions<'a> {
    recursive: bool,
    glob: Option<&'a str>,
    file_type: Option<&'a str>,
    before_context: usize,
    after_context: usize,
}

struct GrepState {
    seen: usize,
    max_matches: usize,
    offset: usize,
    matches: Vec<GrepMatch>,
    counts: BTreeMap<String, usize>,
    files: BTreeSet<String>,
}

fn walk_grep(
    root: &Path,
    re: &regex::Regex,
    options: &GrepWalkOptions<'_>,
    state: &mut GrepState,
) -> std::io::Result<()> {
    if state.matches.len() >= state.max_matches {
        return Ok(());
    }
    if root.is_file() {
        grep_file(root, re, options, state);
    } else if root.is_dir() {
        for e in std::fs::read_dir(root)? {
            let p = e?.path();
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if p.is_dir() {
                if options.recursive {
                    walk_grep(&p, re, options, state)?;
                }
            } else {
                walk_grep(&p, re, options, state)?;
            }
        }
    }
    Ok(())
}

fn grep_file(path: &Path, re: &regex::Regex, options: &GrepWalkOptions<'_>, state: &mut GrepState) {
    if !matches_grep_filters(path, options) {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    let file = path.display().to_string();
    for (i, line) in lines.iter().enumerate() {
        if !re.is_match(line) {
            continue;
        }

        state.seen += 1;
        *state.counts.entry(file.clone()).or_insert(0) += 1;
        state.files.insert(file.clone());
        if state.seen <= state.offset {
            continue;
        }
        if state.matches.len() >= state.max_matches {
            break;
        }

        let before_start = i.saturating_sub(options.before_context);
        let before = lines[before_start..i]
            .iter()
            .map(|line| trunc(line, 500))
            .collect();
        let after_end = (i + 1 + options.after_context).min(lines.len());
        let after = lines[i + 1..after_end]
            .iter()
            .map(|line| trunc(line, 500))
            .collect();
        state.matches.push(GrepMatch {
            file: file.clone(),
            line_no: i + 1,
            line: trunc(line, 500),
            before,
            after,
        });
    }
}

fn render_grep_output(mode: GrepOutputMode, state: &GrepState) -> Vec<String> {
    match mode {
        GrepOutputMode::FilesWithMatches => state.files.iter().cloned().collect(),
        GrepOutputMode::Count => state
            .counts
            .iter()
            .map(|(file, count)| format!("{file}:{count}"))
            .collect(),
        GrepOutputMode::Content => state
            .matches
            .iter()
            .map(|m| {
                let mut parts = Vec::new();
                for line in &m.before {
                    parts.push(format!("{}-{}", m.file, line));
                }
                parts.push(format!("{}:{}:{}", m.file, m.line_no, m.line));
                for line in &m.after {
                    parts.push(format!("{}+{}", m.file, line));
                }
                parts.join("\n")
            })
            .collect(),
    }
}

fn matches_grep_filters(path: &Path, options: &GrepWalkOptions<'_>) -> bool {
    if let Some(file_type) = options.file_type {
        if !matches_file_type(path, file_type) {
            return false;
        }
    }
    if let Some(pattern) = options.glob {
        let Ok(pattern) = glob::Pattern::new(pattern) else {
            return false;
        };
        if !pattern.matches_path(path) {
            return false;
        }
    }
    true
}

fn matches_file_type(path: &Path, file_type: &str) -> bool {
    let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    match file_type {
        "rust" | "rs" => ext == "rs",
        "typescript" | "ts" => matches!(ext, "ts" | "tsx"),
        "javascript" | "js" => matches!(ext, "js" | "jsx" | "mjs" | "cjs"),
        "json" => ext == "json",
        "toml" => ext == "toml",
        "markdown" | "md" => ext == "md",
        other => ext == other.trim_start_matches('.'),
    }
}

// ─── Shared ──────────────────────────────────────────────────────────────

fn resolve_path(cwd: &Path, raw: &Path) -> PathBuf {
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    }
}

fn is_binary(p: &PathBuf) -> bool {
    std::fs::read(p)
        .map(|d| d.iter().take(8192).any(|&b| b == 0))
        .unwrap_or(false)
}

fn slice_chars(s: &str, char_offset: usize, max_chars: usize) -> String {
    let mut iter = s.chars().skip(char_offset);
    let mut out: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        out.push_str("\n... [truncated]");
    }
    out
}

fn find_unique_occurrence(haystack: &str, needle: &str) -> Result<Option<usize>, ToolError> {
    let mut first_match = None;
    let mut offset = 0usize;

    while let Some(relative_pos) = haystack[offset..].find(needle) {
        let absolute_pos = offset + relative_pos;
        if first_match.replace(absolute_pos).is_some() {
            return Err(ToolError::Execution(
                "oldStr appears multiple times, must be unique to edit safely".into(),
            ));
        }

        // 逐 UTF-8 标量前进，避免漏掉重叠匹配，例如 "ababa" 中的 "aba"。
        let step = haystack[absolute_pos..]
            .chars()
            .next()
            .map_or(1, |c| c.len_utf8());
        offset = absolute_pos + step;
    }

    Ok(first_match)
}

fn has_hidden_component(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|part| part.starts_with('.') && part != "." && part != "..")
    })
}

fn load_simple_gitignore(root: &Path) -> BTreeSet<String> {
    let Ok(content) = std::fs::read_to_string(root.join(".gitignore")) else {
        return BTreeSet::new();
    };
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
        .map(|line| line.trim_end_matches('/').to_string())
        .collect()
}

fn is_gitignored(root: &Path, path: &Path, patterns: &BTreeSet<String>) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_text = rel.to_string_lossy().replace('\\', "/");
    patterns.iter().any(|pattern| {
        rel_text == *pattern
            || rel_text.starts_with(&format!("{pattern}/"))
            || rel.file_name().and_then(|name| name.to_str()) == Some(pattern.as_str())
    })
}

fn not_found(p: &Path) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: format!("Not found: {}", p.display()),
        is_error: false,
        error: None,
        metadata: BTreeMap::from([("notFound".into(), serde_json::json!(true))]),
        duration_ms: None,
    }
}

fn directory(p: &Path) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: format!(
            "Is a directory: {} — use findFiles or shell ls",
            p.display()
        ),
        is_error: false,
        error: None,
        metadata: BTreeMap::from([("directory".into(), serde_json::json!(true))]),
        duration_ms: None,
    }
}

fn binary(p: &Path) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: format!("Binary file: {}", p.display()),
        is_error: false,
        error: None,
        metadata: BTreeMap::from([("binary".into(), serde_json::json!(true))]),
        duration_ms: None,
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.into();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn clean_quotes(s: &str) -> String {
    s.replace(['\u{201C}', '\u{201D}'], "\"")
        .replace(['\u{2018}', '\u{2019}'], "'")
}

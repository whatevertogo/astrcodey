//! 文件类内置工具。
//!
//! 职责边界借鉴 adapter-tools/builtin_tools：
//! - findFiles 只按路径 glob 找候选文件；
//! - grep 只按内容搜索；
//! - readFile 只读取已知文件；
//! - editFile 做窄范围精确替换；
//! - writeFile 创建或整文件覆盖；
//! - apply_patch 应用统一 diff 多文件变更。
//!
//! 这些边界会写进工具描述，减少模型在相近工具之间犹豫。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use astrcode_core::tool::*;
use serde::Deserialize;
use serde_json::{Map, Value};

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
            description: "Read a known file's contents. Use after a path is identified by the \
                          user, findFiles, or grep. This is not a directory listing or content \
                          search tool. Supports line offset/limit and returns line-numbered text."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to a file that is already known."
                    },
                    "maxChars": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum characters to return (default 20000)."
                    },
                    "charOffset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Character offset for continuing a truncated read."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Starting line offset, 0-based."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of lines to return from offset."
                    }
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
            description: "Create a UTF-8 text file or fully replace an existing file when the \
                          complete final content is known. Prefer editFile for narrow edits to \
                          existing files."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative target path."
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete UTF-8 content to write. This replaces the whole file."
                    },
                    "createDirs": {
                        "type": "boolean",
                        "description": "Create missing parent directories when true."
                    }
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
            description: "Apply a narrow exact string replacement inside an existing file. oldStr \
                          must appear exactly once unless replaceAll is true. Prefer this over \
                          writeFile for small edits."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Existing UTF-8 file to edit."
                    },
                    "oldStr": {
                        "type": "string",
                        "description": "Exact text to replace. Include enough surrounding context to match once."
                    },
                    "newStr": {
                        "type": "string",
                        "description": "Replacement text."
                    },
                    "replaceAll": {
                        "type": "boolean",
                        "description": "Replace every occurrence. Use only when every match should change."
                    }
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyPatchArgs {
    patch: String,
}

#[derive(Debug)]
struct FilePatch {
    old_path: Option<String>,
    new_path: Option<String>,
    hunks: Vec<Hunk>,
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    _old_count: usize,
    _new_start: usize,
    _new_count: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
enum HunkLine {
    Context(String),
    Add(String),
    Delete(String),
}

#[derive(Debug)]
struct FileChange {
    change_type: String,
    path: String,
    applied: bool,
    summary: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

#[derive(Debug)]
struct TextDocument {
    lines: Vec<String>,
    line_ending: LineEnding,
    has_trailing_newline: bool,
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: "Apply a unified diff patch for coordinated multi-file changes, distant \
                          hunks, file creation, or deletion. Use editFile for a single exact \
                          replacement."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Unified diff patch text containing one or more file changes."
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: ApplyPatchArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid apply_patch args: {e}")))?;
        if args.patch.trim().is_empty() {
            return Ok(apply_patch_error("patch cannot be empty"));
        }

        let file_patches = match parse_patch(&args.patch) {
            Ok(file_patches) => file_patches,
            Err(error) => return Ok(apply_patch_error(&error)),
        };

        let total_files = file_patches.len();
        let mut results = Vec::with_capacity(total_files);
        for file_patch in &file_patches {
            results.push(apply_file_patch(&self.working_dir, file_patch));
        }

        let applied = results.iter().filter(|result| result.applied).count();
        let failed = total_files - applied;
        let first_error = results.iter().find_map(|result| result.error.clone());
        let ok = failed == 0;
        let content = if ok {
            format!("apply_patch: {applied}/{total_files} files changed successfully")
        } else if applied == 0 {
            format!("apply_patch: all {total_files} file(s) failed to apply")
        } else {
            format!(
                "apply_patch: {applied}/{total_files} files changed, {failed} failed (partial \
                 changes committed)"
            )
        };

        Ok(ToolResult {
            call_id: String::new(),
            content,
            is_error: !ok,
            error: if ok {
                None
            } else {
                first_error.or(Some(format!("{failed} file(s) failed to apply")))
            },
            metadata: build_apply_patch_metadata(&results, applied, failed),
            duration_ms: None,
        })
    }
}

fn parse_patch(patch: &str) -> std::result::Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut file_patches = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("rename ")
            || line.starts_with("similarity ")
            || line.starts_with("copy ")
        {
            i += 1;
            continue;
        }

        let Some(old_path_line) = line.strip_prefix("--- ") else {
            return Err(format!(
                "patch format error: unexpected line '{}'",
                line.chars().take(30).collect::<String>()
            ));
        };
        let old_path = strip_diff_prefix(old_path_line);
        i += 1;

        let Some(new_path_line) = lines.get(i).and_then(|line| line.strip_prefix("+++ ")) else {
            return Err("patch format error: expected '+++ new_path' after '--- old_path'".into());
        };
        let new_path = strip_diff_prefix(new_path_line);
        i += 1;
        let hunks = parse_hunks(&lines, &mut i)?;

        file_patches.push(FilePatch {
            old_path: (old_path != "/dev/null").then(|| old_path.to_string()),
            new_path: (new_path != "/dev/null").then(|| new_path.to_string()),
            hunks,
        });
    }

    if file_patches.is_empty() {
        return Err("patch does not contain any file changes (no '---' lines found)".into());
    }

    Ok(file_patches)
}

fn strip_diff_prefix(s: &str) -> &str {
    if s.starts_with("/dev/null") {
        return "/dev/null";
    }
    let trimmed = s.split('\t').next().unwrap_or(s);
    trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed)
}

fn parse_hunks(lines: &[&str], i: &mut usize) -> std::result::Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    while *i < lines.len() {
        let line = lines[*i];
        if line.starts_with("--- ") || line.starts_with("diff ") {
            break;
        }
        if !line.starts_with("@@") {
            *i += 1;
            continue;
        }

        let (old_start, old_count, new_start, new_count) = parse_hunk_header(line)?;
        *i += 1;

        let mut hunk_lines = Vec::new();
        while *i < lines.len()
            && !lines[*i].starts_with("@@")
            && !lines[*i].starts_with("--- ")
            && !lines[*i].starts_with("diff ")
        {
            let line = lines[*i];
            if line.is_empty() {
                hunk_lines.push(HunkLine::Context(String::new()));
                *i += 1;
                continue;
            }

            match line.chars().next().unwrap_or_default() {
                ' ' => hunk_lines.push(HunkLine::Context(line.chars().skip(1).collect())),
                '+' => hunk_lines.push(HunkLine::Add(line.chars().skip(1).collect())),
                '-' => hunk_lines.push(HunkLine::Delete(line.chars().skip(1).collect())),
                '\\' => {
                    *i += 1;
                    continue;
                },
                _ => hunk_lines.push(HunkLine::Context(line.to_string())),
            }
            *i += 1;
        }

        hunks.push(Hunk {
            old_start,
            _old_count: old_count,
            _new_start: new_start,
            _new_count: new_count,
            lines: hunk_lines,
        });
    }

    Ok(hunks)
}

fn parse_hunk_header(header: &str) -> std::result::Result<(usize, usize, usize, usize), String> {
    let content = header
        .strip_prefix("@@")
        .and_then(|value| value.rsplit_once("@@"))
        .map(|(inner, _)| inner.trim())
        .ok_or_else(|| format!("invalid hunk header: {header}"))?;
    let parts: Vec<&str> = content.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(format!("invalid hunk header (too few parts): {header}"));
    }

    let (old_start, old_count) = parse_range(parts[0], "old")?;
    let (new_start, new_count) = parse_range(parts[1], "new")?;
    Ok((old_start, old_count, new_start, new_count))
}

fn parse_range(value: &str, kind: &str) -> std::result::Result<(usize, usize), String> {
    let inner = value
        .strip_prefix('-')
        .or_else(|| value.strip_prefix('+'))
        .ok_or_else(|| {
            format!("invalid {kind} range in hunk header: expected -/+ prefix, got '{value}'")
        })?;
    if let Some((start, count)) = inner.split_once(',') {
        let start = start
            .parse()
            .map_err(|_| format!("invalid {kind} range start: {start}"))?;
        let count = count
            .parse()
            .map_err(|_| format!("invalid {kind} range count: {count}"))?;
        Ok((start, count))
    } else {
        let start = inner
            .parse()
            .map_err(|_| format!("invalid {kind} range: {inner}"))?;
        Ok((start, if start == 0 { 0 } else { 1 }))
    }
}

fn apply_file_patch(working_dir: &Path, file_patch: &FilePatch) -> FileChange {
    let Some(target_path_str) = file_patch
        .new_path
        .clone()
        .or_else(|| file_patch.old_path.clone())
    else {
        return FileChange {
            change_type: "error".into(),
            path: "unknown".into(),
            applied: false,
            summary: "patch specifies neither old nor new path".into(),
            error: Some("patch specifies neither old nor new path".into()),
        };
    };

    let is_new_file = file_patch.old_path.is_none();
    let is_delete = file_patch.new_path.is_none();
    let change_type = if is_new_file {
        "created"
    } else if is_delete {
        "deleted"
    } else {
        "updated"
    };
    let target_path = resolve_path(working_dir, Path::new(&target_path_str));

    if is_unc_path(&target_path) {
        return failed_file_change(
            change_type,
            &target_path_str,
            format!("UNC paths are not supported: {}", target_path.display()),
        );
    }
    if std::fs::symlink_metadata(&target_path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return failed_file_change(
            change_type,
            &target_path_str,
            format!("refusing to patch symlink {}", target_path.display()),
        );
    }

    let original_content = if target_path.exists() {
        match std::fs::read_to_string(&target_path) {
            Ok(content) => Some(content),
            Err(error) => {
                return failed_file_change(
                    change_type,
                    &target_path_str,
                    format!("failed to read existing file: {error}"),
                );
            },
        }
    } else if is_new_file {
        None
    } else {
        return failed_file_change(
            change_type,
            &target_path_str,
            format!("file does not exist: {target_path_str}"),
        );
    };

    let original_doc = original_content
        .as_deref()
        .map(parse_text_document)
        .unwrap_or(TextDocument {
            lines: Vec::new(),
            line_ending: LineEnding::Lf,
            has_trailing_newline: false,
        });

    let result_lines = match apply_hunks(&original_doc.lines, &file_patch.hunks) {
        Ok(lines) => lines,
        Err(error) => {
            return failed_file_change(
                change_type,
                &target_path_str,
                format!(
                    "failed to apply patch to {}: {error}",
                    target_path.display()
                ),
            );
        },
    };

    if is_delete {
        if !result_lines.is_empty() {
            return failed_file_change(
                "deleted",
                &target_path_str,
                format!(
                    "delete patch for {} does not remove the full file",
                    target_path.display()
                ),
            );
        }
        if let Err(error) = std::fs::remove_file(&target_path) {
            return failed_file_change(
                "deleted",
                &target_path_str,
                format!("failed to delete {}: {error}", target_path.display()),
            );
        }
        return FileChange {
            change_type: "deleted".into(),
            path: target_path_str,
            applied: true,
            summary: format!("deleted {}", target_path.display()),
            error: None,
        };
    }

    let new_content = render_text_document(
        &result_lines,
        original_doc.line_ending,
        original_doc.has_trailing_newline,
    );
    if is_new_file {
        if let Some(parent) = target_path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return failed_file_change(
                    change_type,
                    &target_path_str,
                    format!("failed to create parent directory: {error}"),
                );
            }
        }
    }
    if let Err(error) = std::fs::write(&target_path, &new_content) {
        return failed_file_change(
            change_type,
            &target_path_str,
            format!("failed to write {}: {error}", target_path.display()),
        );
    }

    let (added_lines, removed_lines) = patch_line_counts(file_patch);
    FileChange {
        change_type: change_type.into(),
        path: target_path_str,
        applied: true,
        summary: format!(
            "{change_type} {} (+{added_lines} -{removed_lines})",
            target_path.display()
        ),
        error: None,
    }
}

fn failed_file_change(change_type: &str, path: &str, error: String) -> FileChange {
    FileChange {
        change_type: change_type.into(),
        path: path.into(),
        applied: false,
        summary: error.clone(),
        error: Some(error),
    }
}

fn parse_text_document(text: &str) -> TextDocument {
    TextDocument {
        lines: if text.is_empty() {
            Vec::new()
        } else {
            text.lines().map(String::from).collect()
        },
        line_ending: if text.contains("\r\n") {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        },
        has_trailing_newline: text.ends_with('\n'),
    }
}

fn render_text_document(
    lines: &[String],
    line_ending: LineEnding,
    has_trailing_newline: bool,
) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let mut content = lines.join(line_ending.as_str());
    if has_trailing_newline {
        content.push_str(line_ending.as_str());
    }
    content
}

fn apply_hunks(
    content_lines: &[String],
    hunks: &[Hunk],
) -> std::result::Result<Vec<String>, String> {
    let mut result = content_lines.to_vec();
    let mut line_delta = 0isize;

    for (index, hunk) in hunks.iter().enumerate() {
        let anchor = expected_anchor(hunk, line_delta, result.len());
        let pos = find_context_match(&result, hunk, anchor).ok_or_else(|| {
            format!(
                "hunk #{} (line ~{}) failed to apply: context mismatch",
                index + 1,
                hunk.old_start
            )
        })?;
        apply_hunk_in_place(&mut result, hunk, pos).map_err(|error| {
            format!(
                "hunk #{} (line ~{}) failed to apply: {error}",
                index + 1,
                hunk.old_start
            )
        })?;
        line_delta += hunk_line_delta(hunk);
    }

    Ok(result)
}

fn apply_hunk_in_place(
    content_lines: &mut Vec<String>,
    hunk: &Hunk,
    content_start: usize,
) -> std::result::Result<(), String> {
    let mut source_idx = content_start;
    let mut replacement = Vec::new();

    for hunk_line in &hunk.lines {
        match hunk_line {
            HunkLine::Context(expected) => {
                let actual = content_lines.get(source_idx).ok_or_else(|| {
                    format!("expected context line '{expected}' but reached end of file")
                })?;
                if actual != expected {
                    return Err(format!(
                        "expected context line '{expected}', got '{actual}'"
                    ));
                }
                replacement.push(actual.clone());
                source_idx += 1;
            },
            HunkLine::Delete(expected) => {
                let actual = content_lines.get(source_idx).ok_or_else(|| {
                    format!("expected delete line '{expected}' but reached end of file")
                })?;
                if actual != expected {
                    return Err(format!("expected delete line '{expected}', got '{actual}'"));
                }
                source_idx += 1;
            },
            HunkLine::Add(line) => replacement.push(line.clone()),
        }
    }

    content_lines.splice(content_start..source_idx, replacement);
    Ok(())
}

fn expected_anchor(hunk: &Hunk, line_delta: isize, content_len: usize) -> usize {
    let base = if hunk.old_start == 0 {
        0
    } else {
        hunk.old_start.saturating_sub(1)
    };
    (base as isize + line_delta).clamp(0, content_len as isize) as usize
}

fn hunk_line_delta(hunk: &Hunk) -> isize {
    let (added, removed) = hunk_line_counts(hunk);
    added as isize - removed as isize
}

fn find_context_match(content_lines: &[String], hunk: &Hunk, anchor: usize) -> Option<usize> {
    let pattern: Vec<&str> = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(value) | HunkLine::Delete(value) => Some(value.as_str()),
            HunkLine::Add(_) => None,
        })
        .collect();

    if pattern.is_empty() {
        return Some(anchor.min(content_lines.len()));
    }
    if try_match_at(content_lines, &pattern, anchor) {
        return Some(anchor);
    }

    let search_range = pattern.len().max(10);
    let lower = anchor.saturating_sub(search_range);
    for offset in (lower..anchor).rev() {
        if try_match_at(content_lines, &pattern, offset) {
            return Some(offset);
        }
    }

    let upper_limit = content_lines.len().saturating_sub(pattern.len());
    ((anchor + 1)..=upper_limit).find(|&offset| try_match_at(content_lines, &pattern, offset))
}

fn try_match_at(content_lines: &[String], pattern: &[&str], start: usize) -> bool {
    if start + pattern.len() > content_lines.len() {
        return false;
    }
    pattern.iter().enumerate().all(|(index, expected)| {
        content_lines
            .get(start + index)
            .is_some_and(|line| line == expected)
    })
}

fn patch_line_counts(file_patch: &FilePatch) -> (usize, usize) {
    file_patch
        .hunks
        .iter()
        .map(hunk_line_counts)
        .fold((0, 0), |(added, removed), (hunk_added, hunk_removed)| {
            (added + hunk_added, removed + hunk_removed)
        })
}

fn hunk_line_counts(hunk: &Hunk) -> (usize, usize) {
    hunk.lines
        .iter()
        .fold((0, 0), |(added, removed), line| match line {
            HunkLine::Add(_) => (added + 1, removed),
            HunkLine::Delete(_) => (added, removed + 1),
            HunkLine::Context(_) => (added, removed),
        })
}

fn build_apply_patch_metadata(
    results: &[FileChange],
    applied: usize,
    failed: usize,
) -> BTreeMap<String, Value> {
    let files: Vec<Value> = results
        .iter()
        .map(|result| {
            let mut item = Map::new();
            item.insert("path".into(), serde_json::json!(result.path));
            item.insert("changeType".into(), serde_json::json!(result.change_type));
            item.insert("applied".into(), serde_json::json!(result.applied));
            item.insert("summary".into(), serde_json::json!(result.summary));
            if let Some(error) = &result.error {
                item.insert("error".into(), serde_json::json!(error));
            }
            Value::Object(item)
        })
        .collect();

    BTreeMap::from([
        ("filesChanged".into(), serde_json::json!(applied)),
        ("filesApplied".into(), serde_json::json!(applied)),
        ("filesFailed".into(), serde_json::json!(failed)),
        ("files".into(), serde_json::json!(files)),
    ])
}

fn apply_patch_error(error: &str) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: String::new(),
        is_error: true,
        error: Some(error.into()),
        metadata: BTreeMap::from([
            ("filesChanged".into(), serde_json::json!(0)),
            ("filesApplied".into(), serde_json::json!(0)),
            ("filesFailed".into(), serde_json::json!(0)),
        ]),
        duration_ms: None,
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
            description: "Find candidate file paths by glob pattern. This searches file paths \
                          only, not file contents. Use grep for content search and readFile to \
                          inspect a known result."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern for paths, e.g. '*.rs', '**/*.ts', '*.{json,toml}'. Does not search file contents."
                    },
                    "root": {
                        "type": "string",
                        "description": "Directory to search from. Defaults to the working directory."
                    },
                    "maxResults": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of paths to return (default 500)."
                    },
                    "respectGitignore": {
                        "type": "boolean",
                        "description": "Respect .gitignore-style exclusions (default true)."
                    },
                    "includeHidden": {
                        "type": "boolean",
                        "description": "Include hidden files and directories (default true)."
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
            description: "Search file contents with regex or literal text. Defaults to \
                          outputMode=files_with_matches; use outputMode=content when matching \
                          lines are needed. Use findFiles for path glob search."
                .into(),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Pattern to search inside file contents. Regex unless literal is true."
                    },
                    "literal": {
                        "type": "boolean",
                        "description": "Treat pattern as exact text. Use for punctuation-heavy code."
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search. Defaults to the working directory."
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "Search subdirectories. Defaults to true for directories."
                    },
                    "caseInsensitive": { "type": "boolean" },
                    "maxMatches": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum matches or matched files to return (default 250)."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of matches to skip for pagination."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Optional path filter inside path, e.g. '*.rs'. Does not replace path."
                    },
                    "fileType": {
                        "type": "string",
                        "description": "Optional file type filter, e.g. rust, typescript, markdown."
                    },
                    "beforeContext": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines of context before each content match."
                    },
                    "afterContext": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines of context after each content match."
                    },
                    "outputMode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                        "description": "Output mode (default files_with_matches). Use content for matching lines."
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
        let args: GrepArgs = serde_json::from_value(normalize_grep_args(args))
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

fn normalize_grep_args(mut args: Value) -> Value {
    let Some(object) = args.as_object_mut() else {
        return args;
    };

    move_alias(object, "output_mode", "outputMode");
    move_alias(object, "type", "fileType");
    move_alias(object, "file_type", "fileType");
    move_alias(object, "head_limit", "maxMatches");
    move_alias(object, "max_matches", "maxMatches");
    move_alias(object, "-A", "afterContext");
    move_alias(object, "-B", "beforeContext");
    move_alias(object, "-i", "caseInsensitive");
    move_alias(object, "case_insensitive", "caseInsensitive");
    move_alias(object, "before_context", "beforeContext");
    move_alias(object, "after_context", "afterContext");

    if let Some(context) = object.remove("-C").or_else(|| object.remove("context")) {
        object
            .entry("beforeContext".to_string())
            .or_insert_with(|| context.clone());
        object.entry("afterContext".to_string()).or_insert(context);
    }

    normalize_bool_field(object, "literal");
    normalize_bool_field(object, "recursive");
    normalize_bool_field(object, "caseInsensitive");
    normalize_usize_field(object, "maxMatches");
    normalize_usize_field(object, "offset");
    normalize_usize_field(object, "beforeContext");
    normalize_usize_field(object, "afterContext");

    args
}

fn move_alias(object: &mut Map<String, Value>, from: &str, to: &str) {
    if object.contains_key(to) {
        object.remove(from);
        return;
    }
    if let Some(value) = object.remove(from) {
        object.insert(to.to_string(), value);
    }
}

fn normalize_bool_field(object: &mut Map<String, Value>, key: &str) {
    let Some(value) = object.get_mut(key) else {
        return;
    };
    let Some(text) = value.as_str() else {
        return;
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => *value = Value::Bool(true),
        "false" | "0" | "no" | "off" => *value = Value::Bool(false),
        _ => {},
    }
}

fn normalize_usize_field(object: &mut Map<String, Value>, key: &str) {
    let Some(value) = object.get_mut(key) else {
        return;
    };
    if value.as_u64() == Some(0) && key == "maxMatches" {
        *value = serde_json::json!(250);
        return;
    }
    let Some(text) = value.as_str() else {
        return;
    };
    let Ok(parsed) = text.trim().parse::<usize>() else {
        return;
    };
    if key == "maxMatches" && parsed == 0 {
        *value = serde_json::json!(250);
    } else {
        *value = serde_json::json!(parsed);
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

fn is_unc_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    path.starts_with("\\\\") || path.starts_with("//")
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("astrcode-tools-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("temp dir should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn unique_temp_dir(name: &str) -> TestDir {
        TestDir::new(name)
    }

    fn tool_descriptions() -> Vec<ToolDefinition> {
        let working_dir = PathBuf::from(".");
        vec![
            ReadFileTool {
                working_dir: working_dir.clone(),
            }
            .definition(),
            WriteFileTool {
                working_dir: working_dir.clone(),
            }
            .definition(),
            EditFileTool {
                working_dir: working_dir.clone(),
            }
            .definition(),
            ApplyPatchTool {
                working_dir: working_dir.clone(),
            }
            .definition(),
            FindFilesTool {
                working_dir: working_dir.clone(),
            }
            .definition(),
            GrepTool { working_dir }.definition(),
        ]
    }

    #[test]
    fn file_tool_descriptions_separate_search_read_and_write_roles() {
        let definitions = tool_descriptions();
        let find_files = definitions
            .iter()
            .find(|definition| definition.name == "findFiles")
            .expect("findFiles definition should exist");
        let grep = definitions
            .iter()
            .find(|definition| definition.name == "grep")
            .expect("grep definition should exist");
        let read_file = definitions
            .iter()
            .find(|definition| definition.name == "readFile")
            .expect("readFile definition should exist");
        let write_file = definitions
            .iter()
            .find(|definition| definition.name == "writeFile")
            .expect("writeFile definition should exist");
        let edit_file = definitions
            .iter()
            .find(|definition| definition.name == "editFile")
            .expect("editFile definition should exist");

        assert!(find_files.description.contains("file paths only"));
        assert!(grep.description.contains("Search file contents"));
        assert!(grep.description.contains("files_with_matches"));
        assert!(read_file.description.contains("known file"));
        assert!(write_file.description.contains("complete final content"));
        assert!(
            edit_file
                .description
                .contains("narrow exact string replacement")
        );
    }

    #[tokio::test]
    async fn apply_patch_creates_new_file() {
        let temp = unique_temp_dir("patch-create");
        let tool = ApplyPatchTool {
            working_dir: temp.path().to_path_buf(),
        };
        let patch = "--- /dev/null\n+++ b/hello.rs\n@@ -0,0 +1,3 @@\n+fn main() {\n+    \
                     println!(\"hello\");\n+}\n";

        let result = tool
            .execute(serde_json::json!({ "patch": patch }))
            .await
            .expect("apply_patch should execute");

        assert!(!result.is_error, "{result:?}");
        assert!(temp.path().join("hello.rs").exists());
    }

    #[tokio::test]
    async fn apply_patch_updates_existing_file() {
        let temp = unique_temp_dir("patch-update");
        let file = temp.path().join("test.rs");
        std::fs::write(&file, "fn foo() {\n    old();\n}\n").expect("seed write");
        let tool = ApplyPatchTool {
            working_dir: temp.path().to_path_buf(),
        };
        let patch = "--- a/test.rs\n+++ b/test.rs\n@@ -1,3 +1,3 @@\nfn foo() {\n-    old();\n+    \
                     new();\n}\n";

        let result = tool
            .execute(serde_json::json!({ "patch": patch }))
            .await
            .expect("apply_patch should execute");

        assert!(!result.is_error, "{result:?}");
        let content = std::fs::read_to_string(file).expect("updated file should be readable");
        assert!(content.contains("new()"));
        assert!(!content.contains("old()"));
    }

    #[tokio::test]
    async fn apply_patch_preserves_crlf_line_endings() {
        let temp = unique_temp_dir("patch-crlf");
        let file = temp.path().join("windows.rs");
        std::fs::write(&file, "fn foo() {\r\n    old();\r\n}\r\n").expect("seed write");
        let tool = ApplyPatchTool {
            working_dir: temp.path().to_path_buf(),
        };
        let patch = "--- a/windows.rs\n+++ b/windows.rs\n@@ -1,3 +1,3 @@\nfn foo() {\n-    \
                     old();\n+    new();\n}\n";

        let result = tool
            .execute(serde_json::json!({ "patch": patch }))
            .await
            .expect("apply_patch should execute");

        assert!(!result.is_error, "{result:?}");
        let content = std::fs::read_to_string(file).expect("updated file should be readable");
        assert_eq!(content, "fn foo() {\r\n    new();\r\n}\r\n");
    }

    #[tokio::test]
    async fn apply_patch_rejects_delete_when_content_differs() {
        let temp = unique_temp_dir("patch-delete-mismatch");
        let file = temp.path().join("old.txt");
        std::fs::write(&file, "line one\nline changed\n").expect("seed write");
        let tool = ApplyPatchTool {
            working_dir: temp.path().to_path_buf(),
        };
        let patch = "--- a/old.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-line one\n-line two\n";

        let result = tool
            .execute(serde_json::json!({ "patch": patch }))
            .await
            .expect("apply_patch should execute");

        assert!(result.is_error);
        assert!(file.exists());
    }

    #[tokio::test]
    async fn grep_accepts_adapter_style_aliases() {
        let temp = unique_temp_dir("grep-aliases");
        std::fs::write(temp.path().join("lib.rs"), "before\nTARGET\nmatch target\n")
            .expect("seed write");
        let tool = GrepTool {
            working_dir: temp.path().to_path_buf(),
        };

        let result = tool
            .execute(serde_json::json!({
                "pattern": "target",
                "output_mode": "content",
                "-i": "true",
                "-B": "1",
                "head_limit": "1"
            }))
            .await
            .expect("grep should execute");

        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("before"));
        assert!(result.content.contains("TARGET"));
    }
}

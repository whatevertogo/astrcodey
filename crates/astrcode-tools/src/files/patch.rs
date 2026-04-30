use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::{is_path_within, resolve_path};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::shared::{is_unc_path, tool_call_id};
// ─── applyPatch ──────────────────────────────────────────────────────────

/// 统一差异补丁应用工具，支持多文件协调变更、文件创建和删除。
///
/// 适用于需要同时修改多个文件或进行远距离 hunk 编辑的场景；
/// 单文件的精确替换优先使用 `EditFileTool`。
pub struct ApplyPatchTool {
    /// 工具的工作目录
    pub working_dir: PathBuf,
}

/// applyPatch 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyPatchArgs {
    /// 统一差异格式的补丁文本
    patch: String,
}

/// 解析后的单个文件补丁，包含旧/新路径和所有 hunk。
#[derive(Debug)]
struct FilePatch {
    /// 原始文件路径（`/dev/null` 表示新建文件时为 None）
    old_path: Option<String>,
    /// 目标文件路径（`/dev/null` 表示删除文件时为 None）
    new_path: Option<String>,
    /// 该文件的所有变更块
    hunks: Vec<Hunk>,
}

/// 单个 hunk（差异块），描述一段连续的行级变更。
#[derive(Debug)]
struct Hunk {
    /// 旧文件的起始行号（1-based）
    old_start: usize,
    /// 旧文件的行数
    _old_count: usize,
    /// 新文件的起始行号（1-based）
    _new_start: usize,
    /// 新文件的行数
    _new_count: usize,
    /// hunk 中的每一行（上下文/新增/删除）
    lines: Vec<HunkLine>,
}

/// hunk 中的行类型。
#[derive(Debug, Clone)]
enum HunkLine {
    /// 上下文行（未变更，用于定位）
    Context(String),
    /// 新增行
    Add(String),
    /// 删除行
    Delete(String),
}

/// 单个文件的补丁应用结果。
#[derive(Debug)]
struct FileChange {
    /// 变更类型：created / updated / deleted / error
    change_type: String,
    /// 文件路径
    path: String,
    /// 是否成功应用
    applied: bool,
    /// 结果摘要
    summary: String,
    /// 错误信息（如果有）
    error: Option<String>,
}

/// 行尾符类型，用于在应用补丁时保留原始文件的行尾风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    /// Unix 风格 `\n`
    Lf,
    /// Windows 风格 `\r\n`
    Crlf,
}

impl LineEnding {
    /// 返回行尾符的字符串表示。
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

/// 解析后的文本文档，按行分割并保留行尾符和末尾换行信息。
#[derive(Debug)]
struct TextDocument {
    /// 文档的所有行
    lines: Vec<String>,
    /// 检测到的行尾符类型
    line_ending: LineEnding,
    /// 原文是否以换行符结尾
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
            origin: ToolOrigin::Builtin,
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
    /// 执行补丁应用：解析补丁文本 → 逐文件应用 → 汇总结果。
    ///
    /// 即使部分文件应用失败，已成功的变更也会保留（partial commit）。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
        let args: ApplyPatchArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid apply_patch args: {e}")))?;
        if args.patch.trim().is_empty() {
            return Ok(apply_patch_error(ctx, started_at, "patch cannot be empty"));
        }

        let file_patches = match parse_patch(&args.patch) {
            Ok(file_patches) => file_patches,
            Err(error) => return Ok(apply_patch_error(ctx, started_at, &error)),
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
            call_id: tool_call_id(ctx),
            content,
            is_error: !ok,
            error: if ok {
                None
            } else {
                first_error.or(Some(format!("{failed} file(s) failed to apply")))
            },
            metadata: build_apply_patch_metadata(&results, applied, failed),
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

/// 解析统一差异格式的补丁文本，返回每个文件的补丁列表。
///
/// 跳过 diff 头部元信息（index、mode 等），提取 `---`/`+++` 路径对和后续的 hunk 块。
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

/// 去除 diff 路径前缀（`a/`、`b/`）和尾部时间戳。
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

/// 从当前位置开始解析所有连续的 hunk 块，直到遇到下一个文件头或补丁结束。
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

/// 解析 hunk 头部 `@@ -old_start,old_count +new_start,new_count @@`。
///
/// 返回 (old_start, old_count, new_start, new_count)。
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

/// 解析 hunk 头部中的范围值（如 `-3,5` 或 `+1`），返回 (start, count)。
///
/// 当省略 count 时，默认为 1（start 为 0 时默认为 0）。
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

/// 将单个文件的补丁应用到工作目录。
///
/// 处理新建文件、删除文件和更新文件三种情况，包含路径遍历防护和符号链接拒绝。
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

    // applyPatch 同样需要路径遍历防护：diff 中的路径可能包含 ../ 逃逸
    if !is_path_within(&target_path, working_dir) {
        return failed_file_change(
            change_type,
            &target_path_str,
            format!("path escapes working directory: {}", target_path.display()),
        );
    }

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

/// 构造一个失败的文件变更结果。
fn failed_file_change(change_type: &str, path: &str, error: String) -> FileChange {
    FileChange {
        change_type: change_type.into(),
        path: path.into(),
        applied: false,
        summary: error.clone(),
        error: Some(error),
    }
}

/// 将文本内容解析为 `TextDocument`，自动检测行尾符类型和末尾换行。
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

/// 将行列表重新渲染为字符串，使用指定的行尾符并可选追加末尾换行。
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

/// 依次应用所有 hunk 到内容行列表。
///
/// 使用模糊匹配（`find_context_match`）定位每个 hunk 的插入位置，
/// 并通过 `line_delta` 跟踪前面 hunk 造成的行偏移。
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

/// 在指定位置将单个 hunk 的行变更应用到内容行列表中（原地修改）。
///
/// 逐行验证上下文行和删除行是否匹配，收集新增行，最后用 splice 替换。
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

/// 根据 hunk 的 old_start 和之前 hunk 累积的行偏移，计算期望的锚点行位置。
fn expected_anchor(hunk: &Hunk, line_delta: isize, content_len: usize) -> usize {
    let base = if hunk.old_start == 0 {
        0
    } else {
        hunk.old_start.saturating_sub(1)
    };
    (base as isize + line_delta).clamp(0, content_len as isize) as usize
}

/// 计算单个 hunk 造成的行数变化（新增行数 - 删除行数）。
fn hunk_line_delta(hunk: &Hunk) -> isize {
    let (added, removed) = hunk_line_counts(hunk);
    added as isize - removed as isize
}

/// 在内容行中查找 hunk 上下文行的匹配位置。
///
/// 先尝试锚点位置，再向前搜索，最后向后搜索，实现模糊匹配以容忍行偏移。
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

/// 检查从 start 位置开始，内容行是否与模式完全匹配。
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

/// 统计整个文件补丁的新增行数和删除行数。
fn patch_line_counts(file_patch: &FilePatch) -> (usize, usize) {
    file_patch
        .hunks
        .iter()
        .map(hunk_line_counts)
        .fold((0, 0), |(added, removed), (hunk_added, hunk_removed)| {
            (added + hunk_added, removed + hunk_removed)
        })
}

/// 统计单个 hunk 的新增行数和删除行数。
fn hunk_line_counts(hunk: &Hunk) -> (usize, usize) {
    hunk.lines
        .iter()
        .fold((0, 0), |(added, removed), line| match line {
            HunkLine::Add(_) => (added + 1, removed),
            HunkLine::Delete(_) => (added, removed + 1),
            HunkLine::Context(_) => (added, removed),
        })
}

/// 构建 applyPatch 工具返回的 metadata，包含每个文件的变更详情和汇总统计。
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

/// 构造一个 applyPatch 错误结果（无文件变更）。
fn apply_patch_error(ctx: &ToolExecutionContext, started_at: Instant, error: &str) -> ToolResult {
    ToolResult {
        call_id: tool_call_id(ctx),
        content: String::new(),
        is_error: true,
        error: Some(error.into()),
        metadata: BTreeMap::from([
            ("filesChanged".into(), serde_json::json!(0)),
            ("filesApplied".into(), serde_json::json!(0)),
            ("filesFailed".into(), serde_json::json!(0)),
        ]),
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

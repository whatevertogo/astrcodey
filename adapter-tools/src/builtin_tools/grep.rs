//! # Grep 工具
//!
//! 实现 `grep` 工具，用于在文件或目录中搜索正则表达式匹配行。
//!
//! ## 设计要点
//!
//! - 使用 `ignore` crate（ripgrep 同源）进行 .gitignore 感知的文件遍历
//! - 支持递归搜索（`recursive: true`）和单层搜索
//! - 可配置大小写敏感、glob 过滤、文件类型过滤、上下文行
//! - 三种输出模式：content / files_with_matches / count
//! - 支持 `offset` 分页，LLM 可迭代获取超出 `maxMatches` 的后续结果
//! - `GrepMatch` 增加 `match_text` 字段，精确提取匹配到的子串
//! - 超过 500 字符的行自动截断，避免 minified 文件污染 context window
//! - 空结果返回友好提示文本，避免空输出触发 stop sequence

use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Instant,
};

use astrcode_core::{AstrError, CancelToken, Result, SideEffect};
use astrcode_runtime_contract::tool::{
    Tool, ToolCapabilityMetadata, ToolContext, ToolDefinition, ToolExecutionResult,
    ToolPromptMetadata,
};
use async_trait::async_trait;
use log::warn;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::builtin_tools::fs_common::{
    check_cancel, maybe_persist_large_tool_result, merge_persisted_tool_output_metadata,
    read_utf8_file, resolve_path, session_dir_for_tool_results,
};

/// 匹配行最大显示字符数。
/// 超过此长度的行被截断并追加 `...`，避免 minified JS/CSS 污染 context window。
const MAX_LINE_DISPLAY: usize = 500;

/// 默认最大匹配数量。
/// 未指定 max_matches 时使用此默认值，防止返回数千条结果污染 context window。
/// 用户可以通过 offset 参数分页获取更多结果。
const DEFAULT_MAX_MATCHES: usize = 250;

/// Grep 工具实现。
///
/// 在指定路径下搜索包含正则表达式匹配的文件内容。
#[derive(Default)]
pub struct GrepTool;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    /// Rust 正则表达式模式，必填。
    pattern: String,
    /// 按字面量搜索 pattern，等价于 ripgrep 的 -F。
    #[serde(default)]
    literal: bool,
    /// 搜索路径，可选。未提供时使用当前工作目录。
    #[serde(default)]
    path: Option<PathBuf>,
    /// 递归搜索子目录。默认为 true 当 path 是目录时。
    #[serde(default)]
    recursive: Option<bool>,
    /// 是否大小写敏感。默认为 false（不区分大小写）。
    #[serde(default)]
    case_insensitive: bool,
    /// 最大匹配数量，超过则截断结果。默认为 250。
    #[serde(default)]
    max_matches: Option<usize>,
    /// 跳过的匹配数量，用于分页获取后续结果。
    #[serde(default)]
    offset: Option<usize>,
    /// Glob 过滤器，如 "*.rs", "*.{ts,tsx}"。
    #[serde(default)]
    glob: Option<String>,
    /// 文件类型过滤，如 "rust", "typescript"。
    #[serde(default)]
    file_type: Option<String>,
    /// 匹配行前 N 行上下文。
    #[serde(default)]
    before_context: Option<usize>,
    /// 匹配行后 N 行上下文。
    #[serde(default)]
    after_context: Option<usize>,
    /// 输出模式: content / files_with_matches / count。
    #[serde(default)]
    output_mode: Option<GrepOutputMode>,
}

/// Grep 输出模式。
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrepOutputMode {
    /// 返回匹配行内容。
    Content,
    /// 仅返回包含匹配的文件路径列表（默认）。
    #[default]
    FilesWithMatches,
    /// 返回每个文件的匹配计数。
    Count,
}

/// 单次正则匹配的结果。
///
/// 包含文件路径、行号、完整行内容和精确匹配子串。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GrepMatch {
    /// 文件路径（绝对路径字符串）。
    file: String,
    /// 匹配行号（1-based）。
    line_no: usize,
    /// 完整行内容（超长行已截断）。
    line: String,
    /// 精确匹配到的子串。
    /// 优先返回第一个捕获组内容；无捕获组时返回整个匹配到的子串（即正则第一个匹配片段）。
    /// 帮助 LLM 快速定位长行中的关键片段。
    #[serde(skip_serializing_if = "Option::is_none")]
    match_text: Option<String>,
    /// 匹配行前的上下文行。
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<Vec<String>>,
    /// 匹配行后的上下文行。
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<Vec<String>>,
}

/// count 模式下每个文件的匹配计数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GrepFileCount {
    file: String,
    count: usize,
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

fn move_alias(object: &mut serde_json::Map<String, Value>, from: &str, to: &str) {
    if object.contains_key(to) {
        object.remove(from);
        return;
    }
    if let Some(value) = object.remove(from) {
        object.insert(to.to_string(), value);
    }
}

fn normalize_bool_field(object: &mut serde_json::Map<String, Value>, key: &str) {
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

fn normalize_usize_field(object: &mut serde_json::Map<String, Value>, key: &str) {
    let Some(value) = object.get_mut(key) else {
        return;
    };
    if value.as_u64() == Some(0) && key == "maxMatches" {
        *value = json!(DEFAULT_MAX_MATCHES);
        return;
    }
    let Some(text) = value.as_str() else {
        return;
    };
    let Ok(parsed) = text.trim().parse::<usize>() else {
        return;
    };
    if key == "maxMatches" && parsed == 0 {
        *value = json!(DEFAULT_MAX_MATCHES);
    } else {
        *value = json!(parsed);
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search file contents with regex or literal text. Defaults to returning \
                          matching file paths; request content mode for matching lines."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Pattern to search for inside file contents. Interpreted as regex unless `literal` is true."
                    },
                    "literal": {
                        "type": "boolean",
                        "description": "Treat `pattern` as exact text instead of regex, equivalent to ripgrep -F. Use this for punctuation-heavy code such as brackets, parentheses, quotes, or operators."
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional. File or directory to search in (defaults to the current working directory)"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "Search subdirectories recursively. Defaults to true when `path` resolves to a directory."
                    },
                    "caseInsensitive": { "type": "boolean" },
                    "maxMatches": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of matches to return (default 250)"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of matches to skip for pagination"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Optional file path filter inside `path`, e.g. '*.rs', '*.{ts,tsx}'. This does not replace `path`"
                    },
                    "fileType": {
                        "type": "string",
                        "description": "Optional file type filter inside `path`, e.g. 'rust', 'typescript', 'python'"
                    },
                    "beforeContext": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines of context before each match"
                    },
                    "afterContext": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines of context after each match"
                    },
                    "outputMode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count"],
                        "description": "Output mode (default: files_with_matches). Use content when you need matching lines."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn capability_metadata(&self) -> ToolCapabilityMetadata {
        ToolCapabilityMetadata::builtin()
            .tags(["filesystem", "read", "search"])
            .permission("filesystem.read")
            .side_effect(SideEffect::None)
            .concurrency_safe(true)
            .compact_clearable(true)
            .prompt(
                ToolPromptMetadata::new(
                    "Search file contents. Defaults to `files_with_matches`; use `outputMode: \
                     \"content\"` for matching lines.",
                    "Use `grep` for content search inside a known file or directory. Use \
                     `outputMode: \"content\"` when matching lines are needed. `glob` and \
                     `fileType` only narrow files inside `path`; they do not replace `path`. Use \
                     `literal: true` for exact punctuation-heavy text. Use regex when you need \
                     regex behavior. If you only know a file path glob, use `findFiles` first.",
                )
                .caveat(
                    "Pattern uses Rust regex syntax unless `literal: true`. If regex parsing \
                     fails and you meant exact text, retry with `literal: true`.",
                )
                .prompt_tag("search")
                .always_include(true),
            )
            .max_result_inline_size(20_000)
    }

    async fn execute(
        &self,
        tool_call_id: String,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolExecutionResult> {
        check_cancel(ctx.cancel())?;

        let args: GrepArgs = serde_json::from_value(normalize_grep_args(args))
            .map_err(|e| AstrError::parse(explain_grep_args_error(&e), e))?;
        let path = match &args.path {
            Some(p) => resolve_path(ctx, p)?,
            None => ctx.working_dir().to_path_buf(),
        };
        let started_at = Instant::now();
        let pattern = if args.literal {
            regex::escape(&args.pattern)
        } else {
            args.pattern.clone()
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(args.case_insensitive)
            .build()
            .map_err(|error| AstrError::ToolError {
                name: "grep".to_string(),
                reason: explain_regex_error(&error),
            })?;

        let recursive = args.recursive.unwrap_or(path.is_dir());
        let output_mode = args.output_mode.unwrap_or_default();
        let max_matches = args.max_matches.unwrap_or(DEFAULT_MAX_MATCHES);
        let offset = args.offset.unwrap_or(0);
        let glob_matcher = build_glob_matcher(args.glob.as_deref())?;
        let type_extensions = args.file_type.as_deref().and_then(extensions_for_file_type);
        let context_before = args.before_context.unwrap_or(0);
        let context_after = args.after_context.unwrap_or(0);

        let files = collect_candidate_files(
            &path,
            recursive,
            ctx.cancel(),
            glob_matcher.as_ref(),
            type_extensions,
        )?;

        match output_mode {
            GrepOutputMode::Content => {
                let result = search_content_mode(
                    &files,
                    &regex,
                    max_matches,
                    offset,
                    context_before,
                    context_after,
                    ctx.cancel(),
                )
                .await?;
                build_content_result(
                    tool_call_id,
                    result.matches,
                    result.has_more,
                    result.skipped_files,
                    &args,
                    started_at,
                    ctx,
                )
            },
            GrepOutputMode::FilesWithMatches => {
                let result =
                    search_files_mode(&files, &regex, max_matches, offset, ctx.cancel()).await?;
                let output = serde_json::to_string(&result.matched_files)
                    .map_err(|e| AstrError::parse("failed to serialize grep results", e))?;
                let session_dir = session_dir_for_tool_results(ctx)?;
                let final_output =
                    maybe_persist_large_tool_result(&session_dir, &tool_call_id, &output, false);
                let is_persisted = final_output.persisted.is_some();
                let mut metadata = serde_json::Map::new();
                metadata.insert("pattern".to_string(), json!(args.pattern));
                metadata.insert("literal".to_string(), json!(args.literal));
                metadata.insert("returned".to_string(), json!(result.matched_files.len()));
                metadata.insert("has_more".to_string(), json!(result.has_more));
                metadata.insert(
                    "truncated".to_string(),
                    json!(result.has_more || is_persisted),
                );
                metadata.insert("skipped_files".to_string(), json!(result.skipped_files));
                metadata.insert(
                    "message".to_string(),
                    json!(grep_empty_message(offset, result.matched_files.is_empty())),
                );
                metadata.insert("output_mode".to_string(), json!("files_with_matches"));
                merge_persisted_tool_output_metadata(
                    &mut metadata,
                    final_output.persisted.as_ref(),
                );
                Ok(ToolExecutionResult {
                    tool_call_id,
                    tool_name: "grep".to_string(),
                    ok: true,
                    output: final_output.output,
                    error: None,
                    metadata: Some(serde_json::Value::Object(metadata)),
                    continuation: None,
                    duration_ms: started_at.elapsed().as_millis() as u64,
                    truncated: result.has_more || is_persisted,
                })
            },
            GrepOutputMode::Count => {
                let result = search_count_mode(&files, &regex, ctx.cancel()).await?;
                let output = serde_json::to_string(&result.counts)
                    .map_err(|e| AstrError::parse("failed to serialize count results", e))?;
                let session_dir = session_dir_for_tool_results(ctx)?;
                let final_output =
                    maybe_persist_large_tool_result(&session_dir, &tool_call_id, &output, false);
                let is_persisted = final_output.persisted.is_some();
                let mut metadata = serde_json::Map::new();
                metadata.insert("pattern".to_string(), json!(args.pattern));
                metadata.insert("literal".to_string(), json!(args.literal));
                metadata.insert("total_files".to_string(), json!(result.counts.len()));
                metadata.insert("truncated".to_string(), json!(is_persisted));
                metadata.insert("skipped_files".to_string(), json!(result.skipped_files));
                metadata.insert(
                    "message".to_string(),
                    json!(grep_empty_message(0, result.counts.is_empty())),
                );
                metadata.insert("output_mode".to_string(), json!("count"));
                merge_persisted_tool_output_metadata(
                    &mut metadata,
                    final_output.persisted.as_ref(),
                );
                Ok(ToolExecutionResult {
                    tool_call_id,
                    tool_name: "grep".to_string(),
                    ok: true,
                    output: final_output.output,
                    error: None,
                    metadata: Some(serde_json::Value::Object(metadata)),
                    continuation: None,
                    duration_ms: started_at.elapsed().as_millis() as u64,
                    truncated: is_persisted,
                })
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 搜索模式实现
// ---------------------------------------------------------------------------

struct ContentSearchResult {
    matches: Vec<GrepMatch>,
    has_more: bool,
    skipped_files: usize,
}

/// content 模式：逐文件逐行匹配，收集 GrepMatch。
async fn search_content_mode(
    files: &[PathBuf],
    regex: &regex::Regex,
    max_matches: usize,
    offset: usize,
    context_before: usize,
    context_after: usize,
    cancel: &CancelToken,
) -> Result<ContentSearchResult> {
    let mut matches = Vec::new();
    let mut total_in_page = 0usize;
    let mut skipped_files = 0usize;
    let mut hit_limit = false;

    for file in files {
        check_cancel(cancel)?;

        let content = match read_utf8_file(file).await {
            Ok(content) => content,
            Err(error) => {
                warn!("grep: skipping '{}': {error}", file.display());
                skipped_files += 1;
                continue;
            },
        };

        // 需要上下文时，先将所有行收集为 Vec 以支持向后查看
        let need_context = context_before > 0 || context_after > 0;
        if need_context {
            let all_lines: Vec<&str> = content.lines().collect();
            let mut recent_lines: VecDeque<(usize, &str)> = VecDeque::with_capacity(context_before);

            for (index, &line) in all_lines.iter().enumerate() {
                check_cancel(cancel)?;
                if regex.is_match(line) {
                    total_in_page += 1;
                    if total_in_page <= offset {
                        // 维护 ring buffer 即使跳过匹配
                        if context_before > 0 {
                            recent_lines.push_back((index, line));
                            while recent_lines.len() > context_before {
                                recent_lines.pop_front();
                            }
                        }
                        continue;
                    }

                    let before = if context_before > 0 {
                        let ctx_lines: Vec<String> =
                            recent_lines.iter().map(|(_, l)| truncate_line(l)).collect();
                        if ctx_lines.is_empty() {
                            None
                        } else {
                            Some(ctx_lines)
                        }
                    } else {
                        None
                    };

                    let after = if context_after > 0 {
                        let ctx_lines: Vec<String> = all_lines
                            .iter()
                            .skip(index + 1)
                            .take(context_after)
                            .map(|l| truncate_line(l))
                            .collect();
                        if ctx_lines.is_empty() {
                            None
                        } else {
                            Some(ctx_lines)
                        }
                    } else {
                        None
                    };

                    let match_text = extract_match_text(regex, line);
                    matches.push(GrepMatch {
                        file: file.to_string_lossy().to_string(),
                        line_no: index + 1,
                        line: truncate_line(line),
                        match_text,
                        before,
                        after,
                    });

                    if matches.len() >= max_matches {
                        hit_limit = true;
                        break;
                    }
                }

                // 维护 ring buffer
                if context_before > 0 {
                    recent_lines.push_back((index, line));
                    while recent_lines.len() > context_before {
                        recent_lines.pop_front();
                    }
                }
            }
        } else {
            // 无上下文：保持原有逐行流式处理，内存更友好
            for (index, line) in content.lines().enumerate() {
                check_cancel(cancel)?;
                if regex.is_match(line) {
                    total_in_page += 1;
                    if total_in_page <= offset {
                        continue;
                    }
                    let match_text = extract_match_text(regex, line);
                    matches.push(GrepMatch {
                        file: file.to_string_lossy().to_string(),
                        line_no: index + 1,
                        line: truncate_line(line),
                        match_text,
                        before: None,
                        after: None,
                    });
                    if matches.len() >= max_matches {
                        hit_limit = true;
                        break;
                    }
                }
            }
        }

        if hit_limit {
            break;
        }
    }

    Ok(ContentSearchResult {
        matches,
        has_more: hit_limit,
        skipped_files,
    })
}

struct FilesSearchResult {
    matched_files: Vec<String>,
    has_more: bool,
    skipped_files: usize,
}

/// files_with_matches 模式：每个文件首个匹配即收录，跳过该文件后续行。
async fn search_files_mode(
    files: &[PathBuf],
    regex: &regex::Regex,
    max_matches: usize,
    offset: usize,
    cancel: &CancelToken,
) -> Result<FilesSearchResult> {
    let mut matched_files = Vec::new();
    let mut total_in_page = 0usize;
    let mut skipped_files = 0usize;

    for file in files {
        check_cancel(cancel)?;

        let content = match read_utf8_file(file).await {
            Ok(content) => content,
            Err(error) => {
                warn!("grep: skipping '{}': {error}", file.display());
                skipped_files += 1;
                continue;
            },
        };

        let mut file_has_match = false;
        for line in content.lines() {
            check_cancel(cancel)?;
            if regex.is_match(line) {
                file_has_match = true;
                break;
            }
        }

        if file_has_match {
            total_in_page += 1;
            if total_in_page <= offset {
                continue;
            }
            matched_files.push(file.to_string_lossy().to_string());
            if matched_files.len() >= max_matches {
                return Ok(FilesSearchResult {
                    matched_files,
                    has_more: true,
                    skipped_files,
                });
            }
        }
    }

    Ok(FilesSearchResult {
        matched_files,
        has_more: false,
        skipped_files,
    })
}

struct CountSearchResult {
    counts: Vec<GrepFileCount>,
    skipped_files: usize,
}

/// count 模式：统计每个文件的匹配数。
async fn search_count_mode(
    files: &[PathBuf],
    regex: &regex::Regex,
    cancel: &CancelToken,
) -> Result<CountSearchResult> {
    let mut counts = Vec::new();
    let mut skipped_files = 0usize;

    for file in files {
        check_cancel(cancel)?;

        let content = match read_utf8_file(file).await {
            Ok(content) => content,
            Err(error) => {
                warn!("grep: skipping '{}': {error}", file.display());
                skipped_files += 1;
                continue;
            },
        };

        let mut file_count = 0usize;
        for line in content.lines() {
            check_cancel(cancel)?;
            if regex.is_match(line) {
                file_count += 1;
            }
        }

        if file_count > 0 {
            counts.push(GrepFileCount {
                file: file.to_string_lossy().to_string(),
                count: file_count,
            });
        }
    }

    Ok(CountSearchResult {
        counts,
        skipped_files,
    })
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 构建 content 模式的 ToolExecutionResult。
fn build_content_result(
    tool_call_id: String,
    matches: Vec<GrepMatch>,
    has_more: bool,
    skipped_files: usize,
    args: &GrepArgs,
    started_at: Instant,
    ctx: &ToolContext,
) -> Result<ToolExecutionResult> {
    let offset = args.offset.unwrap_or(0);

    let output = serde_json::to_string(&matches)
        .map_err(|e| AstrError::parse("failed to serialize grep matches", e))?;

    // 溢出存盘检查
    let session_dir = session_dir_for_tool_results(ctx)?;
    let final_output = maybe_persist_large_tool_result(&session_dir, &tool_call_id, &output, false);
    let is_persisted = final_output.persisted.is_some();
    let mut metadata = serde_json::Map::new();
    metadata.insert("pattern".to_string(), json!(args.pattern));
    metadata.insert("literal".to_string(), json!(args.literal));
    metadata.insert("returned".to_string(), json!(matches.len()));
    metadata.insert("has_more".to_string(), json!(has_more));
    metadata.insert("truncated".to_string(), json!(has_more || is_persisted));
    metadata.insert("skipped_files".to_string(), json!(skipped_files));
    metadata.insert("offset_applied".to_string(), json!(offset));
    metadata.insert(
        "message".to_string(),
        json!(grep_empty_message(offset, matches.is_empty())),
    );
    merge_persisted_tool_output_metadata(&mut metadata, final_output.persisted.as_ref());

    Ok(ToolExecutionResult {
        tool_call_id,
        tool_name: "grep".to_string(),
        ok: true,
        output: final_output.output,
        error: None,
        metadata: Some(serde_json::Value::Object(metadata)),
        continuation: None,
        duration_ms: started_at.elapsed().as_millis() as u64,
        truncated: has_more || is_persisted,
    })
}

fn grep_empty_message(offset: usize, is_empty: bool) -> Option<&'static str> {
    if !is_empty {
        return None;
    }

    if offset > 0 {
        Some("No more matches found (all remaining results after offset have been exhausted).")
    } else {
        Some("No matches found for the given pattern.")
    }
}

fn explain_regex_error(error: &regex::Error) -> String {
    format!(
        "invalid regex: {error}. If you meant to search exact text, retry with `literal: true`. \
         Use regex only for alternation, wildcards, anchors, or captures."
    )
}

/// 收集候选文件列表。
///
/// 使用 `ignore` crate（ripgrep 同源）进行 .gitignore 感知的文件遍历，
/// 自动排除 `.git`、`node_modules`、`target` 等噪音目录。
/// 非递归模式保持原有 `read_dir` 逻辑。
fn collect_candidate_files(
    path: &Path,
    recursive: bool,
    cancel: &CancelToken,
    glob_matcher: Option<&globset::GlobSet>,
    type_extensions: Option<&'static [&'static str]>,
) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }

    if !path.is_dir() {
        return Err(AstrError::Validation(format!(
            "path is neither a file nor directory: {}",
            path.display()
        )));
    }

    if !recursive {
        // 非递归：保持原有 read_dir 逻辑
        let mut files = Vec::new();
        let read_dir = std::fs::read_dir(path).map_err(|e| {
            AstrError::io(format!("failed reading directory '{}'", path.display()), e)
        })?;
        for entry in read_dir {
            check_cancel(cancel)?;
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let p = entry.path();
                if passes_filters(&p, glob_matcher, type_extensions) {
                    files.push(p);
                }
            }
        }
        return Ok(files);
    }

    // 递归：使用 ignore crate 遍历，自动尊重 .gitignore / .ignore
    let mut files = Vec::new();
    let mut builder = ignore::WalkBuilder::new(path);
    let search_root = path.to_path_buf();
    builder
        .hidden(false)      // agent 需要看到 .env.example 等隐藏文件
        .git_ignore(true)   // 尊重 .gitignore
        .git_global(true)   // 尊重全局 gitignore
        .git_exclude(true)  // 尊重 .git/info/exclude
        .ignore(true); // 尊重 .ignore
    builder.filter_entry(move |entry| should_descend_into_search_entry(&search_root, entry.path()));

    for result in builder.build() {
        check_cancel(cancel)?;
        let entry = result.map_err(|e| {
            AstrError::io(
                format!("failed walking '{}'", path.display()),
                std::io::Error::other(e.to_string()),
            )
        })?;
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let p = entry.path().to_path_buf();
            if passes_filters(&p, glob_matcher, type_extensions) {
                files.push(p);
            }
        }
    }

    Ok(files)
}

/// 递归搜索时显式跳过 `.git` 内部目录，避免在放开隐藏文件后误扫 git object store。
///
/// 这里不屏蔽普通隐藏文件，只裁掉仓库内部实现细节；如果用户显式把搜索根设为 `.git`
/// 目录本身，则允许继续遍历该根下面的内容。
fn should_descend_into_search_entry(search_root: &Path, candidate: &Path) -> bool {
    let Ok(relative) = candidate.strip_prefix(search_root) else {
        return true;
    };

    !relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name) if name == OsStr::new(".git")
        )
    })
}

/// 检查文件路径是否通过 glob 和文件类型过滤器。
fn passes_filters(
    path: &Path,
    glob_matcher: Option<&globset::GlobSet>,
    type_extensions: Option<&'static [&'static str]>,
) -> bool {
    if let Some(matcher) = glob_matcher {
        // globset 匹配需要文件名或相对路径，这里匹配完整路径
        if !matcher.is_match(path) && !matcher.is_match(path.file_name().unwrap_or_default()) {
            return false;
        }
    }
    if let Some(extensions) = type_extensions {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !extensions.contains(&ext) {
            return false;
        }
    }
    true
}

/// 从 glob 字符串构建 GlobSet 匹配器。
fn build_glob_matcher(glob: Option<&str>) -> Result<Option<globset::GlobSet>> {
    let Some(pattern) = glob else {
        return Ok(None);
    };
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| AstrError::Validation(format!("invalid glob pattern '{}': {}", pattern, e)))?;
    // We return a GlobSet instead of a one-off matcher so the rest of the search pipeline keeps a
    // single matching API regardless of whether more include patterns are added later.
    let mut builder = globset::GlobSetBuilder::new();
    builder.add(glob);
    let globset = builder.build().map_err(|e| {
        AstrError::Validation(format!("failed to build glob matcher '{}': {}", pattern, e))
    })?;
    Ok(Some(globset))
}

/// 将常见参数错误改写为可执行的恢复提示。
///
/// 纯粹返回“invalid args”对 agent 没帮助；这里直接指出缺了什么，
/// 并告诉模型什么时候应该改用 `findFiles`。
fn explain_grep_args_error(error: &serde_json::Error) -> String {
    let detail = error.to_string();

    if detail.contains("missing field `pattern`") {
        return "invalid args for grep: missing required field `pattern`. If you only know a \
                filename or glob, use `findFiles` first."
            .to_string();
    }

    // `path` 现在是可选的，此错误不再出现，但保留防御性处理
    if detail.contains("missing field `path`") {
        return "invalid args for grep: `path` is optional and defaults to the current working \
                directory."
            .to_string();
    }

    format!("invalid args for grep: {detail}")
}

/// 将文件类型字符串映射到扩展名列表。
///
/// 仅包含常见语言/格式，无需外部依赖。
/// 返回 None 表示未知类型（不做过滤）。
fn extensions_for_file_type(file_type: &str) -> Option<&'static [&'static str]> {
    match file_type {
        "rust" => Some(&["rs"]),
        "python" | "py" => Some(&["py", "pyi", "pyw"]),
        "javascript" | "js" => Some(&["js", "mjs", "cjs"]),
        "typescript" | "ts" => Some(&["ts", "tsx", "mts", "cts"]),
        "go" => Some(&["go"]),
        "java" => Some(&["java"]),
        "c" => Some(&["c", "h"]),
        "cpp" | "c++" => Some(&["cpp", "hpp", "cc", "hh", "cxx", "hxx"]),
        "ruby" | "rb" => Some(&["rb"]),
        "swift" => Some(&["swift"]),
        "kotlin" | "kt" => Some(&["kt", "kts"]),
        "css" => Some(&["css", "scss", "sass", "less"]),
        "html" => Some(&["html", "htm"]),
        "json" => Some(&["json"]),
        "yaml" => Some(&["yaml", "yml"]),
        "markdown" | "md" => Some(&["md", "mdx"]),
        "shell" | "sh" => Some(&["sh", "bash", "zsh"]),
        "toml" => Some(&["toml"]),
        "xml" => Some(&["xml", "xsl", "xsd", "svg"]),
        "sql" => Some(&["sql"]),
        _ => None,
    }
}

/// 截断超长行到 MAX_LINE_DISPLAY 字符，追加 `...`。
///
/// 使用 `floor_char_boundary` 确保 UTF-8 安全截断。
fn truncate_line(line: &str) -> String {
    if line.len() <= MAX_LINE_DISPLAY {
        return line.to_string();
    }
    let truncated = line.floor_char_boundary(MAX_LINE_DISPLAY.saturating_sub(3));
    format!("{truncated}...")
}

/// 从匹配行中提取精确匹配到的子串。
///
/// 提取策略：
/// - 正则包含捕获组 → 返回第一个捕获组 (`caps[1]`) 的内容
/// - 正则无捕获组 → 返回整个匹配 (`caps[0]`)，即行内第一个匹配片段
///
/// 注意：这里只取行的第一个匹配位置（`captures` 只找第一个），
/// 同一行的后续匹配不在此字段体现。
fn extract_match_text(re: &regex::Regex, line: &str) -> Option<String> {
    re.captures(line).and_then(|caps| {
        if caps.len() > 1 {
            caps.get(1).map(|m| m.as_str().to_string())
        } else {
            caps.get(0).map(|m| m.as_str().to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        builtin_tools::read_file::ReadFileTool,
        test_support::{canonical_tool_path, test_tool_context_for},
    };

    #[tokio::test]
    async fn grep_finds_matches_with_line_numbers() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "pub fn a() {}\nlet x = 1;\npub fn b() {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-found".to_string(),
                json!({
                    "pattern": "pub fn",
                    "path": file.to_string_lossy(),
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should execute");

        assert!(result.ok);
        assert!(!result.output.starts_with("No matches found"));
        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line_no, 1);
        assert_eq!(matches[1].line_no, 3);
        assert_eq!(
            matches[0].file,
            canonical_tool_path(&file).to_string_lossy().to_string()
        );
        assert_eq!(matches[0].match_text, Some("pub fn".to_string()));
    }

    #[tokio::test]
    async fn grep_directory_path_recurses_by_default() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let nested_dir = temp.path().join("nested");
        tokio::fs::create_dir_all(&nested_dir)
            .await
            .expect("nested dir should be created");
        let nested_file = nested_dir.join("lib.rs");
        tokio::fs::write(&nested_file, "pub fn nested() {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-recursive-default".to_string(),
                json!({
                    "pattern": "nested",
                    "path": temp.path().to_string_lossy(),
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should recurse into directory by default");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert!(
            matches[0].file.ends_with("nested\\lib.rs")
                || matches[0].file.ends_with("nested/lib.rs")
        );
    }

    #[tokio::test]
    async fn grep_recursive_false_limits_search_to_top_level_directory() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let nested_dir = temp.path().join("nested");
        tokio::fs::create_dir_all(&nested_dir)
            .await
            .expect("nested dir should be created");
        let nested_file = nested_dir.join("lib.rs");
        tokio::fs::write(&nested_file, "pub fn nested() {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-non-recursive-explicit".to_string(),
                json!({
                    "pattern": "nested",
                    "path": temp.path().to_string_lossy(),
                    "recursive": false,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn grep_returns_friendly_text_when_no_matches() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "let x = 1;\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-empty".to_string(),
                json!({
                    "pattern": "pub fn",
                    "path": file.to_string_lossy(),
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should execute");

        assert!(result.ok);
        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should remain valid json");
        assert!(matches.is_empty());
        let meta = result.metadata.expect("metadata should exist");
        assert_eq!(
            meta["message"],
            json!("No matches found for the given pattern.")
        );
    }

    #[tokio::test]
    async fn grep_supports_offset_pagination() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(
            &file,
            "pub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn d() {}\n",
        )
        .await
        .expect("seed write should work");
        let tool = GrepTool;

        // 第一页：maxMatches=2
        let result = tool
            .execute(
                "tc-grep-p1".to_string(),
                json!({
                    "pattern": "pub fn",
                    "path": file.to_string_lossy(),
                    "maxMatches": 2,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 2);
        assert!(result.truncated);
        let meta = result.metadata.as_ref().expect("metadata should exist");
        assert_eq!(meta["has_more"], json!(true));

        // 第二页：offset=2, 使用更大的 maxMatches 以确保不触发 hit_limit
        let result2 = tool
            .execute(
                "tc-grep-p2".to_string(),
                json!({
                    "pattern": "pub fn",
                    "path": file.to_string_lossy(),
                    "maxMatches": 10,
                    "offset": 2,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches2: Vec<GrepMatch> =
            serde_json::from_str(&result2.output).expect("output should be valid json");
        assert_eq!(matches2.len(), 2);
        assert_eq!(matches2[0].line_no, 3);
        assert_eq!(matches2[1].line_no, 4);
        assert!(!result2.truncated);
    }

    #[tokio::test]
    async fn grep_errors_for_invalid_regex() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "let x = 1;\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let err = tool
            .execute(
                "tc-grep-invalid".to_string(),
                json!({
                    "pattern": "(",
                    "path": file.to_string_lossy()
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect_err("grep should fail for invalid regex");

        let msg = format!("{err}");
        assert!(msg.contains("invalid regex"));
        assert!(msg.contains("literal: true"));
    }

    #[tokio::test]
    async fn grep_literal_mode_searches_punctuation_heavy_text() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "#[cfg(test)]\nmod tests {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-literal".to_string(),
                json!({
                    "pattern": "#[cfg(test)]",
                    "literal": true,
                    "path": file.to_string_lossy(),
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should search exact text");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_no, 1);
        assert_eq!(matches[0].match_text, Some("#[cfg(test)]".to_string()));
        let meta = result.metadata.expect("metadata should exist");
        assert_eq!(meta["literal"], json!(true));
    }

    #[tokio::test]
    async fn grep_missing_required_fields_returns_recovery_hint() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let tool = GrepTool;

        let err = tool
            .execute(
                "tc-grep-missing-pattern".to_string(),
                json!({
                    "glob": "**/*.rs"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect_err("grep should fail when required fields are missing");

        let message = err.to_string();
        assert!(message.contains("missing required field `pattern`"));
        assert!(message.contains("use `findFiles` first"));
    }

    #[tokio::test]
    async fn grep_offset_exhausted_returns_friendly_text() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "pub fn a() {}\npub fn b() {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-exhausted".to_string(),
                json!({
                    "pattern": "pub fn",
                    "path": file.to_string_lossy(),
                    "offset": 5,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should remain valid json");
        assert!(matches.is_empty());
        let meta = result.metadata.expect("metadata should exist");
        assert_eq!(
            meta["message"],
            json!(
                "No more matches found (all remaining results after offset have been exhausted)."
            )
        );
    }

    #[tokio::test]
    async fn grep_files_with_matches_mode_returns_only_file_paths() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file1 = temp.path().join("a.rs");
        let file2 = temp.path().join("b.rs");
        tokio::fs::write(&file1, "pub fn a() {}\npub fn b() {}\n")
            .await
            .expect("seed write should work");
        tokio::fs::write(&file2, "let x = 1;\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-files".to_string(),
                json!({
                    "pattern": "pub fn",
                    "path": temp.path().to_string_lossy(),
                    "outputMode": "files_with_matches"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        assert!(result.ok);
        let files: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("a.rs"));
    }

    #[tokio::test]
    async fn grep_defaults_to_files_with_matches_for_exploration() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("main.rs");
        tokio::fs::write(&file, "fn main() {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-default-files".to_string(),
                json!({
                    "pattern": "main",
                    "path": temp.path().to_string_lossy()
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        assert!(result.ok);
        let files: Vec<String> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("main.rs"));
        let metadata = result.metadata.expect("metadata should exist");
        assert_eq!(metadata["output_mode"], json!("files_with_matches"));
    }

    #[tokio::test]
    async fn grep_count_mode_returns_match_counts() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "pub fn a() {}\nlet x = 1;\npub fn b() {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-count".to_string(),
                json!({
                    "pattern": "pub fn",
                    "path": file.to_string_lossy(),
                    "outputMode": "count"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        assert!(result.ok);
        let counts: Vec<GrepFileCount> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].count, 2);
    }

    #[tokio::test]
    async fn grep_accepts_ripgrep_style_aliases_and_string_scalars() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "before\nTARGET\nmatch target\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-aliases".to_string(),
                json!({
                    "pattern": "target",
                    "path": temp.path().to_string_lossy(),
                    "output_mode": "content",
                    "-i": "true",
                    "-B": "1",
                    "head_limit": "1"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should accept aliases");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0]
                .before
                .as_ref()
                .expect("before context should exist")[0],
            "before"
        );
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn grep_glob_filter_excludes_non_matching_files() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let rs_file = temp.path().join("code.rs");
        let py_file = temp.path().join("code.py");
        tokio::fs::write(&rs_file, "pub fn main() {}\n")
            .await
            .expect("seed write should work");
        tokio::fs::write(&py_file, "def main(): pass\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-glob".to_string(),
                json!({
                    "pattern": "main",
                    "path": temp.path().to_string_lossy(),
                    "glob": "*.rs",
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].file.ends_with("code.rs"));
    }

    #[test]
    fn grep_prompt_metadata_explicitly_describes_required_shape() {
        let prompt = GrepTool
            .capability_metadata()
            .prompt
            .expect("grep should expose prompt metadata");

        assert!(prompt.summary.contains("Defaults to"));
        assert!(prompt.guide.contains("`outputMode: \"content\"`"));
        assert!(prompt.guide.contains("glob"));
        assert!(prompt.guide.contains("findFiles"));
        assert!(prompt.guide.contains("literal: true"));
        assert!(
            prompt
                .caveats
                .iter()
                .any(|caveat| caveat.contains("literal: true"))
        );
    }

    #[tokio::test]
    async fn grep_file_type_filter_works() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let rs_file = temp.path().join("code.rs");
        let ts_file = temp.path().join("code.ts");
        tokio::fs::write(&rs_file, "pub fn hello() {}\n")
            .await
            .expect("seed write should work");
        tokio::fs::write(&ts_file, "function hello() {}\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-type".to_string(),
                json!({
                    "pattern": "hello",
                    "path": temp.path().to_string_lossy(),
                    "fileType": "rust",
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].file.ends_with("code.rs"));
    }

    #[tokio::test]
    async fn grep_context_lines_are_included() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("lib.rs");
        tokio::fs::write(&file, "line1\nline2\nTARGET line3\nline4\nline5\n")
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-ctx".to_string(),
                json!({
                    "pattern": "TARGET",
                    "path": file.to_string_lossy(),
                    "beforeContext": 2,
                    "afterContext": 2,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0]
                .before
                .as_ref()
                .expect("before context should exist")
                .len(),
            2
        );
        assert_eq!(
            matches[0]
                .before
                .as_ref()
                .expect("before context should exist")[0],
            "line1"
        );
        assert_eq!(
            matches[0]
                .before
                .as_ref()
                .expect("before context should exist")[1],
            "line2"
        );
        assert_eq!(
            matches[0]
                .after
                .as_ref()
                .expect("after context should exist")
                .len(),
            2
        );
        assert_eq!(
            matches[0]
                .after
                .as_ref()
                .expect("after context should exist")[0],
            "line4"
        );
        assert_eq!(
            matches[0]
                .after
                .as_ref()
                .expect("after context should exist")[1],
            "line5"
        );
    }

    #[tokio::test]
    async fn grep_truncates_long_lines() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("min.js");
        let long_line = "x".repeat(1000);
        tokio::fs::write(&file, format!("{long_line}\n"))
            .await
            .expect("seed write should work");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-trunc".to_string(),
                json!({
                    "pattern": "x",
                    "path": file.to_string_lossy(),
                    "maxMatches": 1,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        // 500 - 3("...") = 497 个字符 + "..."
        assert!(matches[0].line.len() <= 503);
        assert!(matches[0].line.ends_with("..."));
    }

    #[tokio::test]
    async fn grep_respects_gitignore() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        // 创建 .gitignore 排除 *.log
        tokio::fs::write(temp.path().join(".gitignore"), "*.log\n")
            .await
            .expect("write gitignore");
        // 创建 .git 目录使 ignore crate 认为这是 git 仓库
        tokio::fs::create_dir(temp.path().join(".git"))
            .await
            .expect("create .git dir");
        let log_file = temp.path().join("app.log");
        let rs_file = temp.path().join("main.rs");
        tokio::fs::write(&log_file, "error: TARGET\n")
            .await
            .expect("write log");
        tokio::fs::write(&rs_file, "// TARGET\n")
            .await
            .expect("write rs");

        let tool = GrepTool;
        let result = tool
            .execute(
                "tc-grep-gitignore".to_string(),
                json!({
                    "pattern": "TARGET",
                    "path": temp.path().to_string_lossy(),
                    "recursive": true,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        // .log 文件应被 .gitignore 排除
        assert_eq!(matches.len(), 1);
        assert!(matches[0].file.ends_with("main.rs"));
    }

    #[tokio::test]
    async fn grep_skips_git_internal_objects_when_hidden_files_are_enabled() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        tokio::fs::create_dir_all(temp.path().join(".git").join("objects").join("91"))
            .await
            .expect("create git objects dir");
        tokio::fs::write(
            temp.path()
                .join(".git")
                .join("objects")
                .join("91")
                .join("bad-object"),
            vec![0xff, 0xfe, 0xfd, 0xfc],
        )
        .await
        .expect("write invalid git object");
        let rs_file = temp.path().join("main.rs");
        tokio::fs::write(&rs_file, "// TARGET\n")
            .await
            .expect("write rs");

        let tool = GrepTool;
        let result = tool
            .execute(
                "tc-grep-skip-git-objects".to_string(),
                json!({
                    "pattern": "TARGET",
                    "path": temp.path().to_string_lossy(),
                    "recursive": true,
                    "outputMode": "content"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("grep should succeed");

        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].file.ends_with("main.rs"));

        let metadata = result.metadata.expect("grep should return metadata");
        assert_eq!(
            metadata
                .get("skipped_files")
                .and_then(|value| value.as_u64()),
            Some(0),
        );
    }

    #[tokio::test]
    async fn grep_allows_path_outside_working_dir() {
        let parent = tempfile::tempdir().expect("tempdir should be created");
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside.txt");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace should be created");
        tokio::fs::write(&outside, "needle outside\n")
            .await
            .expect("outside file should be written");
        let tool = GrepTool;

        let result = tool
            .execute(
                "tc-grep-outside".to_string(),
                json!({
                    "pattern": "needle",
                    "path": "../outside.txt",
                    "outputMode": "content"
                }),
                &test_tool_context_for(&workspace),
            )
            .await
            .expect("grep should succeed");

        assert!(result.ok);
        let matches: Vec<GrepMatch> =
            serde_json::from_str(&result.output).expect("output should be valid json");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].file.ends_with("outside.txt"));
    }

    #[tokio::test]
    async fn grep_persists_large_results_and_read_file_can_open_them() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("huge.rs");
        let mut content = String::new();
        for i in 0..700 {
            content.push_str(&format!("pub fn target_{i}() {{}}\n"));
        }
        tokio::fs::write(&file, content)
            .await
            .expect("seed write should work");

        let ctx = test_tool_context_for(temp.path());
        let grep_tool = GrepTool;
        let result = grep_tool
            .execute(
                "tc-grep-persist".to_string(),
                json!({
                    "pattern": "target_",
                    "path": file.to_string_lossy(),
                    "maxMatches": 700,
                    "outputMode": "content"
                }),
                &ctx,
            )
            .await
            .expect("grep should succeed");

        assert!(result.output.starts_with("<persisted-output>"));
        let metadata = result.metadata.as_ref().expect("metadata should exist");
        let persisted_absolute = metadata["persistedOutput"]["absolutePath"]
            .as_str()
            .expect("persisted absolute path should be present");
        assert!(result.output.contains(persisted_absolute));

        let read_tool = ReadFileTool;
        let read_result = read_tool
            .execute(
                "tc-read-persisted".to_string(),
                json!({
                    "path": persisted_absolute,
                    "charOffset": 0,
                    "maxChars": 200000
                }),
                &ctx,
            )
            .await
            .expect("readFile should open persisted grep result");

        assert!(read_result.ok);
        assert!(read_result.output.starts_with('['));
        assert!(read_result.output.contains("target_0"));
        let read_metadata = read_result.metadata.expect("metadata should exist");
        assert_eq!(read_metadata["persistedRead"], json!(true));
    }

    #[test]
    fn extract_match_text_returns_first_capture_group() {
        let re = regex::Regex::new(r"fn\s+(\w+)").expect("regex should compile");
        let text = extract_match_text(&re, "pub fn greet(name: &str)");
        assert_eq!(text, Some("greet".to_string()));
    }

    #[test]
    fn extract_match_text_returns_full_match_when_no_groups() {
        let re = regex::Regex::new(r"pub fn").expect("regex should compile");
        let text = extract_match_text(&re, "pub fn main()");
        assert_eq!(text, Some("pub fn".to_string()));
    }

    #[test]
    fn truncate_line_short_lines_unchanged() {
        assert_eq!(truncate_line("hello"), "hello");
        assert_eq!(truncate_line(&"x".repeat(500)), "x".repeat(500));
    }

    #[test]
    fn truncate_line_long_lines_get_ellipsis() {
        let input = "x".repeat(1000);
        let truncated = truncate_line(&input);
        assert!(truncated.len() <= 503);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn file_type_mapping_returns_known_types() {
        assert_eq!(extensions_for_file_type("rust"), Some(&["rs"][..]));
        assert_eq!(
            extensions_for_file_type("typescript"),
            Some(&["ts", "tsx", "mts", "cts"][..])
        );
        assert_eq!(extensions_for_file_type("unknown"), None);
    }
}

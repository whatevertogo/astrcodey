use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::{is_path_within, resolve_path};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::shared::{collect_grep_files, error_result, is_binary, tool_call_id, trunc};
// ─── grep ────────────────────────────────────────────────────────────────

/// 内容搜索工具，使用正则或字面量在文件内容中搜索匹配。
///
/// 默认返回匹配的文件列表（`files_with_matches` 模式），可切换为返回匹配行内容或计数。
pub struct GrepTool {
    /// 工具的工作目录
    pub working_dir: PathBuf,
}

/// grep 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    /// 搜索模式（正则表达式，除非 literal 为 true）
    pattern: String,
    /// 是否将 pattern 视为字面量文本（自动转义特殊字符）
    #[serde(default)]
    literal: bool,
    /// 搜索的文件或目录路径（默认为工作目录）
    #[serde(default)]
    path: Option<PathBuf>,
    /// 是否递归搜索子目录（默认对目录为 true）
    #[serde(default)]
    recursive: Option<bool>,
    /// 是否大小写不敏感
    #[serde(default, alias = "case_insensitive")]
    case_insensitive: bool,
    /// 最大匹配数/文件数（默认 250）
    #[serde(default, alias = "max_matches")]
    max_matches: Option<usize>,
    /// 跳过的匹配数量（用于分页）
    #[serde(default)]
    offset: Option<usize>,
    /// 路径过滤 glob 模式，如 `*.rs`
    #[serde(default)]
    glob: Option<String>,
    /// 文件类型过滤，如 `rust`、`typescript`
    #[serde(default, alias = "file_type")]
    file_type: Option<String>,
    /// 匹配行前的上下文行数
    #[serde(default, alias = "before_context")]
    before_context: Option<usize>,
    /// 匹配行后的上下文行数
    #[serde(default, alias = "after_context")]
    after_context: Option<usize>,
    /// 输出模式
    #[serde(default, alias = "output_mode")]
    output_mode: GrepOutputMode,
}

/// grep 的输出模式。
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrepOutputMode {
    /// 返回匹配行的内容（含行号和上下文）
    Content,
    /// 仅返回包含匹配的文件路径（默认）
    #[default]
    FilesWithMatches,
    /// 返回每个文件的匹配计数
    Count,
}

impl GrepOutputMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }
}

/// 单个 grep 匹配结果。
#[derive(Debug)]
struct GrepMatch {
    /// 匹配所在的文件路径
    file: String,
    /// 匹配的行号（1-based）
    line_no: usize,
    /// 匹配的行内容
    line: String,
    /// 匹配行前的上下文行
    before: Vec<String>,
    /// 匹配行后的上下文行
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
            origin: ToolOrigin::Builtin,
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

    /// 执行内容搜索：标准化参数 → 编译正则 → 遍历文件 → 收集匹配 → 格式化输出。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
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
        if !is_path_within(&root, &self.working_dir) {
            return Ok(error_result(
                ctx,
                started_at,
                format!("path escapes working directory: {}", root.display()),
                BTreeMap::from([
                    ("path".into(), serde_json::json!(root.display().to_string())),
                    ("pathEscapesWorkingDir".into(), serde_json::json!(true)),
                ]),
            ));
        }
        let max_matches = args.max_matches.unwrap_or(250);
        let offset = args.offset.unwrap_or(0);
        let files = collect_grep_files(
            &self.working_dir,
            &root,
            args.glob.as_deref(),
            args.file_type.as_deref(),
            args.recursive.unwrap_or_else(|| root.is_dir()),
        )
        .map_err(|e| ToolError::Execution(format!("grep: {e}")))?;
        let mut state = GrepState {
            seen: 0,
            max_matches,
            offset,
            output_mode: args.output_mode,
            matches: Vec::new(),
            counts: BTreeMap::new(),
            files: Vec::new(),
            skipped_files: 0,
            has_more: false,
        };
        let options = GrepOptions {
            before_context: args.before_context.unwrap_or(0),
            after_context: args.after_context.unwrap_or(0),
        };
        run_grep(&files, &re, &options, &mut state)
            .map_err(|e| ToolError::Execution(format!("grep: {e}")))?;
        let matches = render_grep_output(args.output_mode, &state);
        let mut meta = BTreeMap::new();
        meta.insert("pattern".into(), serde_json::json!(args.pattern));
        meta.insert("literal".into(), serde_json::json!(args.literal));
        meta.insert("returned".into(), serde_json::json!(matches.len()));
        meta.insert("hasMore".into(), serde_json::json!(state.has_more));
        meta.insert(
            "skippedFiles".into(),
            serde_json::json!(state.skipped_files),
        );
        meta.insert("truncated".into(), serde_json::json!(state.has_more));
        meta.insert(
            "outputMode".into(),
            serde_json::json!(args.output_mode.as_str()),
        );
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content: matches.join("\n"),
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

/// 标准化 grep 参数：将各种别名（如 `-i`、`-A`、`head_limit`）映射到规范字段名，
/// 并将字符串形式的布尔值/数字转换为正确的 JSON 类型。
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

/// 将别名键移动到规范键（如果规范键已存在则删除别名键）。
fn move_alias(object: &mut Map<String, Value>, from: &str, to: &str) {
    if object.contains_key(to) {
        object.remove(from);
        return;
    }
    if let Some(value) = object.remove(from) {
        object.insert(to.to_string(), value);
    }
}

/// 将字符串形式的布尔值（"true"/"1"/"yes"/"on" 等）转换为 JSON bool。
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

/// 将字符串形式的数字转换为 JSON 数字，对 maxMatches 为 0 的情况重置为默认值 250。
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

/// grep 搜索配置。
struct GrepOptions {
    /// 匹配行前的上下文行数
    before_context: usize,
    /// 匹配行后的上下文行数
    after_context: usize,
}

/// grep 搜索过程中的累积状态。
struct GrepState {
    /// 已发现的匹配总数（含被 offset 跳过的）
    seen: usize,
    /// 最大返回匹配数
    max_matches: usize,
    /// 跳过的匹配数（用于分页）
    offset: usize,
    /// 输出模式，决定 maxMatches/offset 的单位。
    output_mode: GrepOutputMode,
    /// 收集到的匹配详情
    matches: Vec<GrepMatch>,
    /// 每个文件的匹配计数
    counts: BTreeMap<String, usize>,
    /// 包含匹配的文件路径集合
    files: Vec<String>,
    /// 跳过的不可读或二进制文件数
    skipped_files: usize,
    /// 是否还有更多结果
    has_more: bool,
}

/// 对候选文件执行 grep 搜索。
fn run_grep(
    files: &[PathBuf],
    re: &regex::Regex,
    options: &GrepOptions,
    state: &mut GrepState,
) -> std::io::Result<()> {
    for file in files {
        let Some(result) = grep_file(file, re, options, state)? else {
            continue;
        };

        match state.output_mode {
            GrepOutputMode::Content => {
                for matched in result.matches {
                    state.seen += 1;
                    if state.seen <= state.offset {
                        continue;
                    }
                    if state.matches.len() >= state.max_matches {
                        state.has_more = true;
                        return Ok(());
                    }
                    state.matches.push(matched);
                }
            },
            GrepOutputMode::FilesWithMatches => {
                state.seen += 1;
                if state.seen <= state.offset {
                    continue;
                }
                if state.files.len() >= state.max_matches {
                    state.has_more = true;
                    return Ok(());
                }
                state.files.push(result.file);
            },
            GrepOutputMode::Count => {
                state.seen += 1;
                if state.seen <= state.offset {
                    continue;
                }
                if state.counts.len() >= state.max_matches {
                    state.has_more = true;
                    return Ok(());
                }
                state.counts.insert(result.file, result.count);
            },
        }
    }
    Ok(())
}

struct GrepFileResult {
    file: String,
    count: usize,
    matches: Vec<GrepMatch>,
}

/// 对单个文件执行 grep 搜索，收集匹配行及其上下文。
fn grep_file(
    path: &Path,
    re: &regex::Regex,
    options: &GrepOptions,
    state: &mut GrepState,
) -> std::io::Result<Option<GrepFileResult>> {
    if is_binary(path) {
        state.skipped_files += 1;
        return Ok(None);
    }
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            state.skipped_files += 1;
            return Ok(None);
        },
        Err(error) => return Err(error),
    };
    let lines: Vec<&str> = content.lines().collect();
    let file = path.display().to_string();
    let mut count = 0usize;
    let mut matches = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !re.is_match(line) {
            continue;
        }

        count += 1;
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
        matches.push(GrepMatch {
            file: file.clone(),
            line_no: i + 1,
            line: trunc(line, 500),
            before,
            after,
        });
    }
    Ok((count > 0).then_some(GrepFileResult {
        file,
        count,
        matches,
    }))
}

/// 根据输出模式将搜索状态渲染为文本行列表。
fn render_grep_output(mode: GrepOutputMode, state: &GrepState) -> Vec<String> {
    match mode {
        GrepOutputMode::FilesWithMatches => state.files.clone(),
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

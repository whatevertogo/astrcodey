use std::{collections::BTreeMap, path::PathBuf, time::Instant};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::{is_path_within, resolve_path};
use serde::Deserialize;

use super::shared::{
    DEFAULT_MAX_CHARS, binary, directory, error_result, image_media_type, is_binary, not_found,
    read_image_file, slice_chars, tool_call_id,
};
// ─── readFile ────────────────────────────────────────────────────────────

/// 文件读取工具，读取已知路径的文件内容并返回带行号的文本。
///
/// 支持行偏移/限制和字符级别的截断，适用于大文件的分页读取。
pub struct ReadFileTool {
    /// 工具的工作目录，用于解析相对路径和做路径遍历防护
    pub working_dir: PathBuf,
}

/// readFile 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadFileArgs {
    /// 要读取的文件路径（绝对或相对路径）
    path: PathBuf,
    /// 返回内容的最大字符数（默认 20000）
    #[serde(default)]
    max_chars: Option<usize>,
    /// 字符偏移量，用于续读被截断的内容
    #[serde(default)]
    char_offset: Option<usize>,
    /// 起始行偏移（0-based）
    #[serde(default)]
    offset: Option<usize>,
    /// 从 offset 开始返回的最大行数
    #[serde(default)]
    limit: Option<usize>,
}


#[async_trait::async_trait]
impl Tool for ReadFileTool {
    /// 返回 readFile 工具的定义，包含参数 schema。
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

    /// 执行文件读取：解析路径 → 安全校验 → 读取内容 → 按行编号格式化输出。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
        let args: ReadFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid readFile args: {e}")))?;
        let path = resolve_path(&self.working_dir, &args.path);
        // 拒绝工作目录外的路径，防止 LLM 构造 ../ 等路径遍历读取敏感文件
        if !is_path_within(&path, &self.working_dir) {
            return Ok(error_result(
                ctx,
                started_at,
                format!("path escapes working directory: {}", path.display()),
                BTreeMap::from([
                    ("path".into(), serde_json::json!(path.display().to_string())),
                    ("pathEscapesWorkingDir".into(), serde_json::json!(true)),
                ]),
            ));
        }
        if !path.exists() {
            return Ok(not_found(ctx, started_at, &path));
        }
        if path.is_dir() {
            return Ok(directory(ctx, started_at, &path));
        }
        if let Some(media_type) = image_media_type(&path) {
            return read_image_file(ctx, started_at, &path, media_type);
        }
        if is_binary(&path) {
            return Ok(binary(ctx, started_at, &path));
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(usize::MAX);
        let char_offset = args.char_offset.unwrap_or(0);
        let max_chars = args.max_chars.unwrap_or(DEFAULT_MAX_CHARS);

        let total_lines = content.lines().count();
        let lines: Vec<String> = content
            .lines()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, l)| format!("{:>6}\t{}", i + offset + 1, l))
            .collect();
        let rendered = lines.join("\n");
        let rendered = slice_chars(&rendered, char_offset, max_chars);
        let line_truncated = offset.saturating_add(lines.len()) < total_lines;

        let mut meta = BTreeMap::new();
        meta.insert("path".into(), serde_json::json!(path.display().to_string()));
        meta.insert("totalLines".into(), serde_json::json!(total_lines));
        meta.insert("shownLines".into(), serde_json::json!(lines.len()));
        meta.insert("charOffset".into(), serde_json::json!(char_offset));
        meta.insert("maxChars".into(), serde_json::json!(max_chars));
        meta.insert(
            "returnedChars".into(),
            serde_json::json!(rendered.returned_chars),
        );
        meta.insert(
            "nextCharOffset".into(),
            serde_json::json!(rendered.next_char_offset),
        );
        meta.insert(
            "hasMore".into(),
            serde_json::json!(rendered.has_more || line_truncated),
        );
        meta.insert(
            "truncated".into(),
            serde_json::json!(rendered.has_more || line_truncated),
        );
        if line_truncated {
            meta.insert(
                "nextOffset".into(),
                serde_json::json!(offset.saturating_add(lines.len())),
            );
        }

        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content: rendered.text,
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

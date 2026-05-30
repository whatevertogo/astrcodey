use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock, time::Instant};

use astrcode_core::{storage::StorageError, tool::*};
use astrcode_support::hostpaths::resolve_path;
use serde::Deserialize;

use super::shared::{
    DEFAULT_MAX_CHARS, MAX_UNPAGINATED_READ_BYTES, binary_result, directory_result,
    error_result_with_call_id, image_media_type, is_binary, not_found_result,
    read_image_file_result, read_lines_segment, remember_file_observation_with_store, run_blocking,
    slice_chars, tool_call_id,
};

const MAX_TOOL_RESULT_READ_CHARS: usize = 60_000;
// ─── read ────────────────────────────────────────────────────────────────

/// 文件读取工具，读取已知路径的文件内容并返回带行号的文本。
///
/// 支持行偏移/限制和字符级别的截断，适用于大文件的分页读取。
pub struct ReadFileTool {
    /// 工具的工作目录，用于解析相对路径
    pub working_dir: PathBuf,
}

/// read 工具的参数。
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
    /// 返回 read 工具的定义，包含参数 schema。
    fn definition(&self) -> ToolDefinition {
        read_file_tool_definition().clone()
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
            .map_err(|e| ToolError::InvalidArguments(format!("invalid read args: {e}")))?;
        let path = resolve_path(&self.working_dir, &args.path);
        if !path.exists() {
            if let Some(result) =
                read_persisted_tool_result_path(ctx, started_at, &path, &args).await?
            {
                return Ok(result);
            }
            return Ok(not_found_result(tool_call_id(ctx), started_at, &path));
        }

        let call_id = tool_call_id(ctx);
        let file_observation_store = ctx.capabilities.file_observation_store.clone();
        let working_dir = self.working_dir.clone();
        run_blocking(move || {
            read_existing_file_sync(
                working_dir,
                args,
                call_id,
                file_observation_store,
                started_at,
            )
        })
        .await
    }

    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        Some(ToolPromptMetadata::new("").prompt_tag(ToolPromptTag::Filesystem))
    }
}

fn read_existing_file_sync(
    working_dir: PathBuf,
    args: ReadFileArgs,
    call_id: String,
    file_observation_store: Option<std::sync::Arc<dyn FileObservationStore>>,
    started_at: Instant,
) -> Result<ToolResult, ToolError> {
    let path = resolve_path(&working_dir, &args.path);
    if path.is_dir() {
        return Ok(directory_result(call_id.clone(), started_at, &path));
    }
    if let Some(media_type) = image_media_type(&path) {
        return read_image_file_result(call_id, started_at, &path, media_type);
    }
    if is_binary(&path) {
        return Ok(binary_result(call_id.clone(), started_at, &path));
    }

    let offset = args.offset.unwrap_or(0);
    let limit = args.limit.unwrap_or(usize::MAX);
    let char_offset = args.char_offset.unwrap_or(0);
    let max_chars = args.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
    let use_line_pagination = args.offset.is_some() || args.limit.is_some();

    let file_len = std::fs::metadata(&path)
        .map_err(|e| ToolError::Execution(format!("stat: {e}")))?
        .len();
    if !use_line_pagination && file_len > MAX_UNPAGINATED_READ_BYTES {
        return Ok(error_result_with_call_id(
            call_id.clone(),
            started_at,
            format!(
                "file is {file_len} bytes; use offset/limit to paginate reads over \
                 {MAX_UNPAGINATED_READ_BYTES} bytes"
            ),
            BTreeMap::from([
                ("path".into(), serde_json::json!(path.display().to_string())),
                ("bytes".into(), serde_json::json!(file_len)),
                (
                    "maxUnpaginatedBytes".into(),
                    serde_json::json!(MAX_UNPAGINATED_READ_BYTES),
                ),
            ]),
        ));
    }

    let (raw_lines, total_lines) = if use_line_pagination {
        read_lines_segment(&path, offset, limit)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?
    } else {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let total_lines = content.lines().count();
        let lines: Vec<String> = content
            .lines()
            .skip(offset)
            .take(limit)
            .map(str::to_string)
            .collect();
        (lines, total_lines)
    };

    let lines: Vec<String> = raw_lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{}", i + offset + 1, l))
        .collect();
    let rendered = lines.join("\n");
    let rendered = slice_chars(&rendered, char_offset, max_chars);
    let line_truncated = offset.saturating_add(lines.len()) < total_lines;

    let _ = remember_file_observation_with_store(file_observation_store.as_ref(), &path);

    let mut meta = BTreeMap::new();
    meta.insert("path".into(), serde_json::json!(path.display().to_string()));
    meta.insert("totalLines".into(), serde_json::json!(total_lines));
    meta.insert("shownLines".into(), serde_json::json!(lines.len()));
    meta.insert("offset".into(), serde_json::json!(offset));
    if args.limit.is_some() {
        meta.insert("limit".into(), serde_json::json!(limit));
    }
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
    meta.insert("lineTruncated".into(), serde_json::json!(line_truncated));
    meta.insert("charTruncated".into(), serde_json::json!(rendered.has_more));
    if line_truncated {
        meta.insert(
            "nextOffset".into(),
            serde_json::json!(offset.saturating_add(lines.len())),
        );
    }

    Ok(ToolResult {
        call_id,
        content: rendered.text,
        is_error: false,
        error: None,
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    })
}

fn read_file_tool_definition() -> &'static ToolDefinition {
    static DEFINITION: OnceLock<ToolDefinition> = OnceLock::new();
    DEFINITION.get_or_init(|| ToolDefinition {
        name: "read".into(),
        description: concat!(
            "Read a file with line numbers. MUST `read` before `edit`.\n\n",
            "When NOT to use:\n",
            "- Listing paths → `glob`\n",
            "- Repo-wide content search → `grep` first\n\n",
            "Tips:\n",
            "- Known file path (or persisted tool-result path)\n",
            "- Multiple files may be read together when helpful\n\n",
            "Notes: copy text without line-number prefixes; paginate large files via parameters.",
        )
        .into(),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Parallel,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path, or a persisted tool-result path from a prior result. Supports text, code, images, and binary detection."
                },
                "maxChars": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Default 20000 (60000 for persisted results). Use with charOffset to paginate large files."
                },
                "charOffset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Continue a truncated read (character offset)."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Start line (0-based). Use with limit for line pagination."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Max lines from offset."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    })
}

async fn read_persisted_tool_result_path(
    ctx: &ToolExecutionContext,
    started_at: Instant,
    path: &std::path::Path,
    args: &ReadFileArgs,
) -> Result<Option<ToolResult>, ToolError> {
    let Some(reader) = ctx.capabilities.tool_result_reader.as_ref() else {
        return Ok(None);
    };
    let char_offset = args.char_offset.unwrap_or(0);
    let max_chars = args
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .min(MAX_TOOL_RESULT_READ_CHARS);
    let path = path.display().to_string();
    let slice = match reader
        .read_tool_result_artifact_by_path(&ctx.session_id, &path, char_offset, max_chars)
        .await
    {
        Ok(slice) => slice,
        Err(StorageError::InvalidId(_) | StorageError::Unsupported(_)) => return Ok(None),
        Err(StorageError::NotFound(_)) => {
            return Ok(Some(error_result_with_call_id(
                tool_call_id(ctx),
                started_at,
                format!("tool result path not found: {path}"),
                BTreeMap::from([
                    ("path".into(), serde_json::json!(path)),
                    ("source".into(), serde_json::json!("toolResultArtifact")),
                ]),
            )));
        },
        Err(error) => return Err(ToolError::Execution(format!("read tool result: {error}"))),
    };

    let mut meta = BTreeMap::new();
    meta.insert("path".into(), serde_json::json!(slice.path));
    meta.insert("source".into(), serde_json::json!("toolResultArtifact"));
    meta.insert("bytes".into(), serde_json::json!(slice.bytes));
    meta.insert("charOffset".into(), serde_json::json!(slice.char_offset));
    meta.insert(
        "returnedChars".into(),
        serde_json::json!(slice.returned_chars),
    );
    meta.insert("maxChars".into(), serde_json::json!(max_chars));
    meta.insert("hasMore".into(), serde_json::json!(slice.has_more));
    meta.insert("truncated".into(), serde_json::json!(slice.has_more));
    if let Some(next_char_offset) = slice.next_char_offset {
        meta.insert("nextCharOffset".into(), serde_json::json!(next_char_offset));
    }

    Ok(Some(ToolResult {
        call_id: tool_call_id(ctx),
        content: slice.content,
        is_error: false,
        error: None,
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }))
}

use std::{collections::BTreeMap, path::PathBuf, time::Instant};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::{is_path_within, resolve_path};
use serde::Deserialize;

use super::shared::{error_result, tool_call_id};
// ─── writeFile ───────────────────────────────────────────────────────────

/// 文件写入工具，创建新文件或完整覆盖已有文件。
///
/// 当已知完整的目标内容时使用此工具；对于小范围编辑，优先使用 `EditFileTool`。
pub struct WriteFileTool {
    /// 工具的工作目录
    pub working_dir: PathBuf,
}

/// writeFile 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteFileArgs {
    /// 目标文件路径
    path: PathBuf,
    /// 要写入的完整 UTF-8 内容（覆盖整个文件）
    content: String,
    /// 是否自动创建缺失的父目录
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

    /// 执行文件写入：解析路径 → 安全校验 → 可选创建目录 → 写入文件。
    ///
    /// 如果文件已存在则覆盖，返回旧/新文件大小的变化信息。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
        let args: WriteFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid writeFile args: {e}")))?;
        let path = resolve_path(&self.working_dir, &args.path);
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

        let old_bytes = old.as_ref().map(|old| old.len());
        let msg = if let Some(old_bytes) = old_bytes {
            format!(
                "Updated {} ({}→{} bytes)",
                path.display(),
                old_bytes,
                args.content.len()
            )
        } else {
            format!("Created {} ({} bytes)", path.display(), args.content.len())
        };
        let mut metadata = BTreeMap::new();
        metadata.insert("path".into(), serde_json::json!(path.display().to_string()));
        metadata.insert("newBytes".into(), serde_json::json!(args.content.len()));
        metadata.insert("created".into(), serde_json::json!(old_bytes.is_none()));
        if let Some(old_bytes) = old_bytes {
            metadata.insert("oldBytes".into(), serde_json::json!(old_bytes));
        }
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content: msg,
            is_error: false,
            error: None,
            metadata,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

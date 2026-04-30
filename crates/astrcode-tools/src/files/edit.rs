use std::{collections::BTreeMap, path::PathBuf, time::Instant};

use astrcode_core::tool::*;
use astrcode_support::hostpaths::{is_path_within, resolve_path};
use serde::Deserialize;

use super::shared::{clean_quotes, error_result, find_unique_occurrence, tool_call_id};
// ─── editFile ────────────────────────────────────────────────────────────

/// 文件精确编辑工具，对已有文件执行窄范围的字符串替换。
///
/// `oldStr` 在文件中必须唯一匹配（除非启用 `replaceAll`），适用于小范围精确修改。
pub struct EditFileTool {
    /// 工具的工作目录
    pub working_dir: PathBuf,
}

/// editFile 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditFileArgs {
    /// 要编辑的文件路径
    path: PathBuf,
    /// 要被替换的原始文本（需包含足够的上下文以确保唯一匹配）
    #[serde(rename = "oldStr", alias = "old_string")]
    old_str: String,
    /// 替换后的新文本
    #[serde(rename = "newStr", alias = "new_string")]
    new_str: String,
    /// 是否替换所有匹配项（默认仅替换第一个）
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
            origin: ToolOrigin::Builtin,
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

    /// 执行文件编辑：解析参数 → 清理引号 → 查找匹配 → 执行替换 → 写回文件。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
        let args: EditFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid editFile args: {e}")))?;
        let old_str = clean_quotes(&args.old_str);
        let new_str = clean_quotes(&args.new_str);
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
        if old_str.is_empty() {
            return Err(ToolError::InvalidArguments("oldStr cannot be empty".into()));
        }

        let original = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let replacements;
        let updated = if args.replace_all {
            if !original.contains(&old_str) {
                return Err(ToolError::Execution(format!(
                    "oldStr not found in {}",
                    path.display()
                )));
            }
            replacements = original.matches(&old_str).count();
            original.replace(&old_str, &new_str)
        } else {
            let Some(pos) = find_unique_occurrence(&original, &old_str)? else {
                return Err(ToolError::Execution(format!(
                    "oldStr not found in {}",
                    path.display()
                )));
            };
            replacements = 1;
            let mut updated = String::with_capacity(original.len() - old_str.len() + new_str.len());
            updated.push_str(&original[..pos]);
            updated.push_str(&new_str);
            updated.push_str(&original[pos + old_str.len()..]);
            updated
        };
        std::fs::write(&path, &updated).map_err(|e| ToolError::Execution(format!("write: {e}")))?;
        let metadata = BTreeMap::from([
            ("path".into(), serde_json::json!(path.display().to_string())),
            ("replaceAll".into(), serde_json::json!(args.replace_all)),
            ("replacements".into(), serde_json::json!(replacements)),
            ("oldBytes".into(), serde_json::json!(original.len())),
            ("newBytes".into(), serde_json::json!(updated.len())),
        ]);
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content: format!("Edited {}", path.display()),
            is_error: false,
            error: None,
            metadata,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

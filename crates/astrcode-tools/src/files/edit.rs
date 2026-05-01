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
    #[serde(default, rename = "oldStr", alias = "old_string")]
    old_str: Option<String>,
    /// 替换后的新文本
    #[serde(default, rename = "newStr", alias = "new_string")]
    new_str: Option<String>,
    /// 是否替换所有匹配项（默认仅替换第一个）
    #[serde(default, alias = "replace_all")]
    replace_all: bool,
    /// 批量编辑操作，按顺序应用且整体成功后才写回文件
    #[serde(default)]
    edits: Vec<EditOperation>,
}

/// 单个精确替换操作。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditOperation {
    /// 要被替换的原始文本
    #[serde(rename = "oldStr", alias = "old_string")]
    old_str: String,
    /// 替换后的新文本
    #[serde(rename = "newStr", alias = "new_string")]
    new_str: String,
    /// 是否替换所有匹配项
    #[serde(default, alias = "replace_all")]
    replace_all: bool,
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "editFile".into(),
            description: "Apply one or more narrow exact string replacements inside an existing \
                          file. oldStr must appear exactly once unless replaceAll is true. Use \
                          edits for atomic multiEdit-style changes."
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
                    },
                    "edits": {
                        "type": "array",
                        "description": "Atomic multiEdit operations applied in order. Do not combine with top-level oldStr/newStr.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldStr": {
                                    "type": "string",
                                    "description": "Exact text to replace."
                                },
                                "newStr": {
                                    "type": "string",
                                    "description": "Replacement text."
                                },
                                "replaceAll": {
                                    "type": "boolean",
                                    "description": "Replace every occurrence for this operation."
                                }
                            },
                            "required": ["oldStr", "newStr"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path"],
                "anyOf": [
                    { "required": ["oldStr", "newStr"] },
                    { "required": ["edits"] }
                ],
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
        let operations = normalize_edit_operations(args)?;

        let original = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Execution(format!("read: {e}")))?;
        let (updated, replacements) = apply_edit_operations(&original, &path, &operations)?;
        std::fs::write(&path, &updated).map_err(|e| ToolError::Execution(format!("write: {e}")))?;
        let metadata = BTreeMap::from([
            ("path".into(), serde_json::json!(path.display().to_string())),
            ("operationCount".into(), serde_json::json!(operations.len())),
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

fn normalize_edit_operations(args: EditFileArgs) -> Result<Vec<EditOperation>, ToolError> {
    let has_top_level = args.old_str.is_some() || args.new_str.is_some();
    if has_top_level && !args.edits.is_empty() {
        return Err(ToolError::InvalidArguments(
            "use either oldStr/newStr or edits, not both".into(),
        ));
    }

    let operations = if !args.edits.is_empty() {
        args.edits
    } else {
        let old_str = args
            .old_str
            .ok_or_else(|| ToolError::InvalidArguments("oldStr is required".into()))?;
        let new_str = args
            .new_str
            .ok_or_else(|| ToolError::InvalidArguments("newStr is required".into()))?;
        vec![EditOperation {
            old_str,
            new_str,
            replace_all: args.replace_all,
        }]
    };

    for (index, operation) in operations.iter().enumerate() {
        if operation.old_str.is_empty() {
            return Err(ToolError::InvalidArguments(format!(
                "edits[{index}].oldStr cannot be empty"
            )));
        }
    }
    Ok(operations)
}

fn apply_edit_operations(
    original: &str,
    path: &std::path::Path,
    operations: &[EditOperation],
) -> Result<(String, usize), ToolError> {
    let mut updated = original.to_string();
    let mut replacements = 0usize;

    for operation in operations {
        let old_str = clean_quotes(&operation.old_str);
        let new_str = clean_quotes(&operation.new_str);
        let count = apply_one_edit(
            &mut updated,
            &old_str,
            &new_str,
            operation.replace_all,
            path,
        )?;
        replacements = replacements.saturating_add(count);
    }

    Ok((updated, replacements))
}

fn apply_one_edit(
    content: &mut String,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
    path: &std::path::Path,
) -> Result<usize, ToolError> {
    if replace_all {
        if !content.contains(old_str) {
            return Err(ToolError::Execution(format!(
                "oldStr not found in {}",
                path.display()
            )));
        }
        let replacements = content.matches(old_str).count();
        *content = content.replace(old_str, new_str);
        return Ok(replacements);
    }

    let Some(pos) = find_unique_occurrence(content, old_str)? else {
        return Err(ToolError::Execution(format!(
            "oldStr not found in {}",
            path.display()
        )));
    };
    let mut next = String::with_capacity(content.len() - old_str.len() + new_str.len());
    next.push_str(&content[..pos]);
    next.push_str(new_str);
    next.push_str(&content[pos + old_str.len()..]);
    *content = next;
    Ok(1)
}

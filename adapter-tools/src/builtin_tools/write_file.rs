//! # WriteFile 工具
//!
//! 实现 `writeFile` 工具，用于创建或完全覆盖文本文件。
//!
//! ## 设计要点
//!
//! - 支持 `createDirs` 参数自动创建父目录
//! - 写入前读取现有内容以生成 diff 报告
//! - 如果现有文件无法作为 UTF-8 读取，diff 不可用但不影响写入操作

use std::{path::PathBuf, time::Instant};

use astrcode_core::{AstrError, Result, SideEffect};
use astrcode_runtime_contract::tool::{
    Tool, ToolCapabilityMetadata, ToolContext, ToolDefinition, ToolExecutionResult,
    ToolPromptMetadata,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::builtin_tools::fs_common::{
    TextChangeReport, build_text_change_report, check_cancel,
    ensure_not_canonical_session_plan_write_target, is_symlink, is_unc_path, read_utf8_file,
    resolve_path, write_text_file,
};

/// WriteFile 工具实现。
///
/// 创建新文件或完全覆盖现有文件，自动生成变更 diff 报告。
#[derive(Default)]
pub struct WriteFileTool;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteFileArgs {
    path: PathBuf,
    content: String,
    #[serde(default)]
    create_dirs: bool,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "writeFile".to_string(),
            description: "Create or overwrite a file with the given UTF-8 text content."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file. Creates the parent directory chain if createDirs is true."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full UTF-8 text content to write. Will entirely replace any existing content."
                    },
                    "createDirs": {
                        "type": "boolean",
                        "description": "Create missing intermediate directories (default false). Set true when path contains directories that may not exist yet."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn capability_metadata(&self) -> ToolCapabilityMetadata {
        ToolCapabilityMetadata::builtin()
            .tags(["filesystem", "write"])
            .permission("filesystem.write")
            .side_effect(SideEffect::Local)
            .prompt(
                ToolPromptMetadata::new(
                    "Create or fully replace a text file when the whole target content is known.",
                    "Use `writeFile` for file creation, regeneration, or full rewrites. Prefer \
                     `editFile` or `apply_patch` for narrow edits to existing files.",
                )
                .caveat(
                    "Overwrites the entire file. `createDirs` defaults to false — parent \
                     directories must exist or set it to true.",
                )
                .prompt_tag("filesystem")
                .always_include(true),
            )
    }

    async fn execute(
        &self,
        tool_call_id: String,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolExecutionResult> {
        check_cancel(ctx.cancel())?;

        let args: WriteFileArgs = serde_json::from_value(args)
            .map_err(|e| AstrError::parse("invalid args for writeFile", e))?;
        let started_at = Instant::now();
        let path = resolve_path(ctx, &args.path)?;
        ensure_not_canonical_session_plan_write_target(ctx, &path, "writeFile")?;

        // UNC 路径检查：防止 Windows NTLM 凭据泄露
        if is_unc_path(&path) {
            return Ok(ToolExecutionResult {
                tool_call_id,
                tool_name: "writeFile".to_string(),
                ok: false,
                output: String::new(),
                error: Some(format!(
                    "UNC paths are not supported for security reasons (potential NTLM credential \
                     leak on Windows). Path: '{}'",
                    path.display()
                )),
                metadata: Some(json!({
                    "path": path.to_string_lossy(),
                    "uncPath": true,
                })),
                continuation: None,
                duration_ms: started_at.elapsed().as_millis() as u64,
                truncated: false,
            });
        }

        // 符号链接检查：防止绕过路径沙箱
        if is_symlink(&path)? {
            return Ok(ToolExecutionResult {
                tool_call_id,
                tool_name: "writeFile".to_string(),
                ok: false,
                output: String::new(),
                error: Some(format!(
                    "refusing to write to symlink '{}' (symlinks may point outside the intended \
                     target path)",
                    path.display()
                )),
                metadata: Some(json!({
                    "path": path.to_string_lossy(),
                    "isSymlink": true,
                })),
                continuation: None,
                duration_ms: started_at.elapsed().as_millis() as u64,
                truncated: false,
            });
        }

        let file_existed = path.exists();
        let existing_content = if file_existed {
            match read_utf8_file(&path).await {
                Ok(content) => Some(content),
                Err(error) => {
                    log::warn!(
                        "writeFile skipped diff metadata for '{}' because the existing file could \
                         not be read as UTF-8: {}",
                        path.display(),
                        error
                    );
                    None
                },
            }
        } else {
            None
        };
        let change_type = if file_existed { "updated" } else { "created" };
        let bytes = write_text_file(&path, &args.content, args.create_dirs).await?;
        let report = if file_existed && existing_content.is_none() {
            TextChangeReport {
                summary: format!("{change_type} {} (diff unavailable)", path.display()),
                metadata: json!({
                    "path": path.to_string_lossy(),
                    "changeType": change_type,
                }),
            }
        } else {
            build_text_change_report(
                &path,
                change_type,
                existing_content.as_deref(),
                &args.content,
            )
        };
        let mut metadata = report.metadata;
        if let Some(object) = metadata.as_object_mut() {
            object.insert("bytes".to_string(), json!(bytes));
        }

        Ok(ToolExecutionResult {
            tool_call_id,
            tool_name: "writeFile".to_string(),
            ok: true,
            output: report.summary,
            error: None,
            metadata: Some(metadata),
            continuation: None,
            duration_ms: started_at.elapsed().as_millis() as u64,
            truncated: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{canonical_tool_path, test_tool_context_for};

    #[tokio::test]
    async fn write_file_creates_new_file() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("hello.txt");
        let tool = WriteFileTool;

        let result = tool
            .execute(
                "tc-write-new".to_string(),
                json!({
                    "path": file.to_string_lossy(),
                    "content": "hello"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("writeFile should execute");

        assert!(result.ok);
        let content = tokio::fs::read_to_string(&file)
            .await
            .expect("file should be readable");
        assert_eq!(content, "hello");
        assert_eq!(
            result.metadata.expect("metadata should exist")["path"],
            json!(canonical_tool_path(&file).to_string_lossy().to_string())
        );
    }

    #[tokio::test]
    async fn write_file_overwrites_existing_file() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("hello.txt");
        tokio::fs::write(&file, "old")
            .await
            .expect("seed write should work");
        let tool = WriteFileTool;

        let result = tool
            .execute(
                "tc-write-overwrite".to_string(),
                json!({
                    "path": file.to_string_lossy(),
                    "content": "new"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("writeFile should execute");

        assert!(result.ok);
        let content = tokio::fs::read_to_string(&file)
            .await
            .expect("file should be readable");
        assert_eq!(content, "new");
        let metadata = result.metadata.expect("metadata should exist");
        assert!(
            metadata["diff"]["patch"]
                .as_str()
                .expect("patch should exist")
                .contains("+new")
        );
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories_when_requested() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("nested").join("hello.txt");
        let tool = WriteFileTool;

        let result = tool
            .execute(
                "tc-write-create-dirs".to_string(),
                json!({
                    "path": file.to_string_lossy(),
                    "content": "hello",
                    "createDirs": true
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("writeFile should execute");

        assert!(result.ok);
        assert!(file.exists());
    }

    #[tokio::test]
    async fn write_file_errors_when_parent_directory_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let file = temp.path().join("nested").join("hello.txt");
        let tool = WriteFileTool;

        let err = tool
            .execute(
                "tc-write-missing-parent".to_string(),
                json!({
                    "path": file.to_string_lossy(),
                    "content": "hello"
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect_err("writeFile should fail");

        assert!(err.to_string().contains("failed writing file"));
    }

    #[tokio::test]
    async fn write_file_allows_relative_path_outside_working_dir() {
        let parent = tempfile::tempdir().expect("tempdir should be created");
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside.txt");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace should be created");
        let tool = WriteFileTool;

        let result = tool
            .execute(
                "tc-write-outside".to_string(),
                json!({
                    "path": "../outside.txt",
                    "content": "outside"
                }),
                &test_tool_context_for(&workspace),
            )
            .await
            .expect("writeFile should execute");

        assert!(result.ok);
        let content = tokio::fs::read_to_string(&outside)
            .await
            .expect("outside file should be readable");
        assert_eq!(content, "outside");
    }

    #[tokio::test]
    async fn write_file_rejects_canonical_session_plan_targets() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let tool = WriteFileTool;
        let target = temp
            .path()
            .join(".astrcode-test-state")
            .join("sessions")
            .join("session-test")
            .join("plan")
            .join("cleanup-crates.md");

        let err = tool
            .execute(
                "tc-write-plan".to_string(),
                json!({
                    "path": target.to_string_lossy(),
                    "content": "# Plan: Cleanup crates",
                    "createDirs": true
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect_err("canonical plan writes should be rejected");

        assert!(err.to_string().contains("upsertSessionPlan"));
    }
}

//! `taskWrite` 工具。
//!
//! 该工具只维护执行期 task 快照，不触碰 canonical session plan。

use std::time::Instant;

use astrcode_core::{
    AstrError, ExecutionTaskItem, ExecutionTaskSnapshotMetadata, ExecutionTaskStatus, Result,
    SideEffect, TaskSnapshot,
};
use astrcode_runtime_contract::tool::{
    Tool, ToolCapabilityMetadata, ToolContext, ToolDefinition, ToolExecutionResult,
    ToolPromptMetadata,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::builtin_tools::fs_common::check_cancel;

const MAX_TASK_ITEMS: usize = 20;

#[derive(Default)]
pub struct TaskWriteTool;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskWriteArgs {
    #[serde(default)]
    items: Vec<ExecutionTaskItem>,
}

#[async_trait]
impl Tool for TaskWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "taskWrite".to_string(),
            description: "Persist the current execution-task snapshot for this execution owner."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "Full execution-task snapshot for the current owner. Pass an empty array to clear active tasks.",
                        "maxItems": MAX_TASK_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "Imperative task title, for example 'Update runtime projection tests'."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                },
                                "activeForm": {
                                    "type": "string",
                                    "description": "Present-progress phrase used while the task is actively being worked on."
                                }
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        }
    }

    fn capability_metadata(&self) -> ToolCapabilityMetadata {
        ToolCapabilityMetadata::builtin()
            .tags(["task", "execution", "progress"])
            .side_effect(SideEffect::Local)
            .prompt(
                ToolPromptMetadata::new(
                    "Maintain the current execution-task snapshot for this branch of work.",
                    "Use `taskWrite` for non-trivial execution that benefits from an externalized \
                     task list. Always send the full current snapshot, not a patch. Prefer it for \
                     multi-step implementation, user requests with multiple deliverables, or work \
                     that must survive long turns and session recovery.",
                )
                .caveat(
                    "Do not use `taskWrite` for trivial one-step work, pure Q&A, or tasks that \
                     can be completed in roughly three straightforward actions.",
                )
                .caveat(
                    "Keep exactly one item in `in_progress` at a time. Mark an item `in_progress` \
                     before starting it, and mark it `completed` immediately after it is truly \
                     finished.",
                )
                .caveat(
                    "Every item should have a concise imperative `content`. Any `in_progress` \
                     item must also include `activeForm`, such as '正在补充会话投影测试'.",
                )
                .caveat(
                    "Passing an empty array clears active tasks. Completed-only snapshots are \
                     also treated as cleared by the runtime projection.",
                )
                .example(
                    "{ items: [{ content: \"补齐 runtime task 投影\", status: \"in_progress\", \
                     activeForm: \"正在补齐 runtime task 投影\" }, { content: \"验证 prompt \
                     注入\", status: \"pending\", activeForm: \"准备验证 prompt 注入\" }] }",
                )
                .prompt_tag("task")
                .always_include(true),
            )
    }

    async fn execute(
        &self,
        tool_call_id: String,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolExecutionResult> {
        check_cancel(ctx.cancel())?;

        let args: TaskWriteArgs = serde_json::from_value(input)
            .map_err(|error| AstrError::parse("invalid args for taskWrite", error))?;
        validate_items(&args.items)?;

        let started_at = Instant::now();
        let snapshot = TaskSnapshot {
            owner: resolve_task_owner(ctx),
            items: args.items,
        };
        let metadata = ExecutionTaskSnapshotMetadata::from_snapshot(&snapshot);
        let (pending_count, in_progress_count, completed_count) = count_statuses(&snapshot.items);
        let cleared = metadata.cleared;

        Ok(ToolExecutionResult {
            tool_call_id,
            tool_name: "taskWrite".to_string(),
            ok: true,
            output: if cleared {
                "cleared active execution tasks".to_string()
            } else {
                format!(
                    "updated execution tasks: {in_progress_count} in progress, {pending_count} \
                     pending, {completed_count} completed"
                )
            },
            error: None,
            metadata: Some(
                serde_json::to_value(metadata)
                    .map_err(|error| AstrError::parse("invalid taskWrite metadata", error))?,
            ),
            continuation: None,
            duration_ms: started_at.elapsed().as_millis() as u64,
            truncated: false,
        })
    }
}

fn validate_items(items: &[ExecutionTaskItem]) -> Result<()> {
    if items.len() > MAX_TASK_ITEMS {
        return Err(AstrError::Validation(format!(
            "taskWrite accepts at most {MAX_TASK_ITEMS} items per snapshot"
        )));
    }

    let in_progress_count = items
        .iter()
        .filter(|item| item.status == ExecutionTaskStatus::InProgress)
        .count();
    if in_progress_count > 1 {
        return Err(AstrError::Validation(
            "taskWrite snapshot must contain at most one in_progress item".to_string(),
        ));
    }

    for (index, item) in items.iter().enumerate() {
        if item.content.trim().is_empty() {
            return Err(AstrError::Validation(format!(
                "taskWrite item #{index} content must not be empty"
            )));
        }
        if item
            .active_form
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(AstrError::Validation(format!(
                "taskWrite item #{} activeForm must not be blank when provided",
                index
            )));
        }
        if item.status == ExecutionTaskStatus::InProgress
            && item
                .active_form
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            return Err(AstrError::Validation(format!(
                "taskWrite item #{} with status in_progress must include activeForm",
                index
            )));
        }
    }

    Ok(())
}

fn resolve_task_owner(ctx: &ToolContext) -> String {
    ctx.agent_context()
        .agent_id
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| ctx.session_id().to_string())
}

fn count_statuses(items: &[ExecutionTaskItem]) -> (usize, usize, usize) {
    items
        .iter()
        .fold((0usize, 0usize, 0usize), |counts, item| match item.status {
            ExecutionTaskStatus::Pending => (counts.0 + 1, counts.1, counts.2),
            ExecutionTaskStatus::InProgress => (counts.0, counts.1 + 1, counts.2),
            ExecutionTaskStatus::Completed => (counts.0, counts.1, counts.2 + 1),
        })
}

#[cfg(test)]
mod tests {
    use astrcode_core::AgentEventContext;
    use serde_json::Value;

    use super::*;
    use crate::test_support::test_tool_context_for;

    fn metadata_snapshot(result: &ToolExecutionResult) -> ExecutionTaskSnapshotMetadata {
        serde_json::from_value(result.metadata.clone().expect("metadata should exist"))
            .expect("task metadata should decode")
    }

    #[tokio::test]
    async fn task_write_accepts_valid_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let tool = TaskWriteTool;
        let ctx = test_tool_context_for(temp.path()).with_agent_context(AgentEventContext {
            agent_id: Some("agent-main".into()),
            ..AgentEventContext::default()
        });

        let result = tool
            .execute(
                "tc-task-valid".to_string(),
                json!({
                    "items": [
                        {
                            "content": "实现 task 投影",
                            "status": "in_progress",
                            "activeForm": "正在实现 task 投影"
                        },
                        {
                            "content": "补充服务端映射测试",
                            "status": "pending"
                        }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("taskWrite should execute");

        assert!(result.ok);
        let metadata = metadata_snapshot(&result);
        assert_eq!(metadata.owner, "agent-main");
        assert!(!metadata.cleared);
        assert_eq!(metadata.items.len(), 2);
    }

    #[tokio::test]
    async fn task_write_rejects_invalid_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let tool = TaskWriteTool;

        let error = tool
            .execute(
                "tc-task-invalid".to_string(),
                json!({
                    "items": [
                        {
                            "content": "任务 A",
                            "status": "in_progress",
                            "activeForm": "正在处理任务 A"
                        },
                        {
                            "content": "任务 B",
                            "status": "in_progress",
                            "activeForm": "正在处理任务 B"
                        }
                    ]
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect_err("multiple in_progress items should be rejected");

        assert!(error.to_string().contains("at most one in_progress item"));
    }

    #[tokio::test]
    async fn task_write_rejects_oversized_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let tool = TaskWriteTool;
        let items = (0..=MAX_TASK_ITEMS)
            .map(|index| {
                json!({
                    "content": format!("任务 {index}"),
                    "status": "pending"
                })
            })
            .collect::<Vec<Value>>();

        let error = tool
            .execute(
                "tc-task-oversized".to_string(),
                json!({ "items": items }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect_err("oversized snapshot should be rejected");

        assert!(error.to_string().contains("at most 20 items"));
    }

    #[tokio::test]
    async fn task_write_falls_back_to_session_owner_without_agent_id() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let tool = TaskWriteTool;

        let result = tool
            .execute(
                "tc-task-owner".to_string(),
                json!({ "items": [] }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("taskWrite should execute");

        let metadata = metadata_snapshot(&result);
        assert_eq!(metadata.owner, "session-test");
        assert!(metadata.cleared);
    }
}

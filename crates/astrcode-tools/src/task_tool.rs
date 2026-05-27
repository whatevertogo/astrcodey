//! Background task management tool — list running tasks and cancel them.

use std::{collections::BTreeMap, sync::OnceLock};

use astrcode_core::{tool::*, types::BackgroundTaskId};
use serde::Deserialize;

/// task 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskArgs {
    /// 操作：list 列出所有后台任务，cancel 取消指定任务。
    action: String,
    /// 要取消的任务 ID（cancel 操作必填）。
    #[serde(default)]
    task_id: Option<String>,
}

/// 后台任务管理工具。
///
/// LLM 通过此工具查看和操控当前会话的后台任务。
/// 典型场景：检查 dev server/watcher 是否仍在运行，或取消不再需要的长任务。
pub struct TaskTool;

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn definition(&self) -> ToolDefinition {
        task_tool_definition().clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let args: TaskArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid task args: {e}")))?;

        let reader = ctx
            .capabilities
            .background_task_reader
            .as_ref()
            .ok_or_else(|| ToolError::Execution("background task reader not available".into()))?;

        match args.action.as_str() {
            "list" => {
                let tasks = reader.list_active(&ctx.session_id);
                let content = if tasks.is_empty() {
                    "No active background tasks.".into()
                } else {
                    let lines: Vec<String> = tasks
                        .iter()
                        .map(|task_id| format!("- task: {task_id}"))
                        .collect();
                    format!("Active background tasks:\n{}", lines.join("\n"))
                };
                Ok(ToolResult::text(content, false, BTreeMap::new()))
            },
            "cancel" => {
                let task_id_str = args.task_id.unwrap_or_default();
                if task_id_str.is_empty() {
                    return Err(ToolError::InvalidArguments(
                        "taskId is required for cancel action".into(),
                    ));
                }
                let task_id = BackgroundTaskId::new(task_id_str);
                let cancelled = reader.cancel(&ctx.session_id, &task_id);
                let content = if cancelled {
                    format!("Task {task_id} cancelled.")
                } else {
                    format!("Task {task_id} not found or already completed.")
                };
                Ok(ToolResult::text(content, false, BTreeMap::new()))
            },
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}', expected 'list' or 'cancel'"
            ))),
        }
    }

    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        Some(ToolPromptMetadata::new("").prompt_tag(ToolPromptTag::System))
    }
}

fn task_tool_definition() -> &'static ToolDefinition {
    static DEFINITION: OnceLock<ToolDefinition> = OnceLock::new();
    DEFINITION.get_or_init(|| ToolDefinition {
        name: "task".into(),
        description: concat!(
            "Manages background shell tasks.\n",
            "- `action=list`: show running tasks with status and output.\n",
            "- `action=cancel`: stop a task by ID.\n",
            "- Use to check progress or clean up after `shell(runInBackground=true)`.",
        )
        .into(),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Parallel,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "cancel"],
                    "description": "Must Required.list: show all tasks. cancel: stop one."
                },
                "taskId": {
                    "type": "string",
                    "description": "Required for cancel."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    })
}

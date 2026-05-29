//! Background task management tool — list running tasks, cancel them, and read output.
//!
//! TODO: 目前只展示 shell 后台任务（BackgroundTaskManager），agent 后台的子会话不可见。
//! 考虑在 list 中也展示 wait_for_result=false 的 agent 子会话状态，
//! 让 LLM 能统一查询所有后台活动。

use std::{collections::BTreeMap, sync::OnceLock, time::Instant};

use astrcode_core::{tool::*, types::BackgroundTaskId};
use serde::Deserialize;

use crate::files::tool_call_id;

/// task 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskArgs {
    /// 操作：list / cancel / result。
    action: String,
    /// 任务 ID（cancel 和 result 操作必填）。
    #[serde(default)]
    task_id: Option<String>,
    /// result 操作的字符偏移（默认 0）。
    #[serde(default)]
    char_offset: usize,
    /// result 操作的最大读取字符数（默认 20000）。
    #[serde(default)]
    max_chars: Option<usize>,
}

/// 后台任务管理工具。
///
/// LLM 通过此工具查看和操控当前会话的后台任务。
/// 典型场景：检查 dev server/watcher 是否仍在运行，或取消不再需要的长任务。
pub struct TaskTool;

const DEFAULT_RESULT_MAX_CHARS: usize = 20_000;

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
        let started_at = Instant::now();
        let args: TaskArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid task args: {e}")))?;

        let reader = ctx
            .capabilities
            .background_task_reader
            .as_ref()
            .ok_or_else(|| ToolError::Execution("background task reader not available".into()))?;

        let call_id = tool_call_id(ctx);
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
                Ok(task_result(
                    call_id,
                    started_at,
                    content,
                    false,
                    BTreeMap::new(),
                ))
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
                Ok(task_result(
                    call_id,
                    started_at,
                    content,
                    false,
                    BTreeMap::new(),
                ))
            },
            "result" => {
                let task_id_str = args.task_id.unwrap_or_default();
                if task_id_str.is_empty() {
                    return Err(ToolError::InvalidArguments(
                        "taskId is required for result action".into(),
                    ));
                }
                let task_id = BackgroundTaskId::new(task_id_str);
                let max_chars = args.max_chars.unwrap_or(DEFAULT_RESULT_MAX_CHARS);
                let slice = reader
                    .read_output(&ctx.session_id, &task_id, args.char_offset, max_chars)
                    .map_err(|e| {
                        ToolError::Execution(format!(
                            "output for task {task_id} not available: {e}"
                        ))
                    })?;

                let mut content =
                    format!("Output of task {task_id} ({} bytes total):\n", slice.bytes);
                content.push_str(&slice.content);
                if slice.has_more {
                    content.push_str(&format!(
                        "\n\n... (use charOffset={} to continue reading)",
                        slice
                            .next_char_offset
                            .expect("has_more implies next_char_offset is set")
                    ));
                }
                Ok(task_result(
                    call_id,
                    started_at,
                    content,
                    false,
                    BTreeMap::new(),
                ))
            },
            other => Err(ToolError::InvalidArguments(format!(
                "unknown action '{other}', expected 'list', 'cancel', or 'result'"
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
            "- `action=result`: read the persisted output of a completed task by ID.\n",
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
                    "enum": ["list", "cancel", "result"],
                    "description": "Must be one of: list (show all tasks), cancel (stop one), result (read output)."
                },
                "taskId": {
                    "type": "string",
                    "description": "Required for cancel and result actions."
                },
                "charOffset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Char offset for result action pagination. Default 0."
                },
                "maxChars": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Max chars to read for result action. Default 20000."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    })
}

fn task_result(
    call_id: String,
    started_at: Instant,
    content: String,
    is_error: bool,
    metadata: BTreeMap<String, serde_json::Value>,
) -> ToolResult {
    ToolResult {
        call_id,
        content: content.clone(),
        is_error,
        error: is_error.then_some(content),
        metadata,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

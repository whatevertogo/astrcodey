//! astrcode-extension-task-tools — 任务跟踪工具。
//!
//! 注册的工具：
//! - `taskCreate`: 创建跟踪任务
//! - `taskList`: 列出所有跟踪任务
//! - `taskUpdate`: 更新任务状态

mod task;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect, HookMode,
    },
    tool::{ToolDefinition, ToolResult},
    types::project_hash_from_path,
};
use astrcode_support::hostpaths;
use task::TaskStore;

/// 计算 session 级任务存储根目录：`~/.astrcode/projects/<hash>/sessions/<sid>/tasks/`
fn task_store_root(session_id: &str, working_dir: &str) -> PathBuf {
    let hash = project_hash_from_path(&PathBuf::from(working_dir));
    hostpaths::sessions_dir(&hash)
        .join(session_id)
        .join("tasks")
}

// ─── 内置扩展入口 ─────────────────────────────────────────────────────

/// 返回内置任务工具扩展。
pub fn extension() -> Arc<dyn Extension> {
    Arc::new(TaskToolsExtension)
}

struct TaskToolsExtension;

#[async_trait::async_trait]
impl Extension for TaskToolsExtension {
    fn id(&self) -> &str {
        "astrcode-task-tools"
    }

    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
        vec![]
    }

    async fn on_event(
        &self,
        _event: ExtensionEvent,
        _ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        Ok(HookEffect::Allow)
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            task_create_tool_definition(),
            task_list_tool_definition(),
            task_update_tool_definition(),
        ]
    }

    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let session_id = &ctx.session_id;
        let result = match tool_name {
            "taskCreate" => handle_task_create(&arguments.to_string(), session_id, working_dir),
            "taskList" => handle_task_list(session_id, working_dir),
            "taskUpdate" => handle_task_update(&arguments.to_string(), session_id, working_dir),
            _ => return Err(ExtensionError::NotFound(tool_name.into())),
        };

        match result {
            Ok(content) => Ok(text_result(content, false)),
            Err(error) => Ok(text_result(error, true)),
        }
    }
}

// ─── 工具实现 ────────────────────────────────────────────────────────

fn handle_task_create(
    input_json: &str,
    session_id: &str,
    working_dir: &str,
) -> Result<String, String> {
    let input: serde_json::Value =
        serde_json::from_str(input_json).map_err(|e| format!("parse: {e}"))?;
    let subject = input["subject"].as_str().ok_or("subject required")?;
    let desc = input["description"].as_str().unwrap_or("");
    let blocks: Vec<String> = input["blocks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let store = TaskStore::new(task_store_root(session_id, working_dir));
    let task = store
        .create(subject, desc, &blocks)
        .map_err(|e| format!("failed to create task: {e}"))?;
    Ok(format!("Created task {}: {}", task.id, task.subject))
}

fn handle_task_list(session_id: &str, working_dir: &str) -> Result<String, String> {
    let store = TaskStore::new(task_store_root(session_id, working_dir));
    let tasks = store.list();
    if tasks.is_empty() {
        return Ok("No tasks.".into());
    }
    let lines: Vec<String> = tasks
        .iter()
        .map(|task| {
            format!(
                "[{}] {} - {}",
                task.id,
                status_icon(&task.status),
                task.subject
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

fn handle_task_update(
    input_json: &str,
    session_id: &str,
    working_dir: &str,
) -> Result<String, String> {
    let input: serde_json::Value =
        serde_json::from_str(input_json).map_err(|e| format!("parse: {e}"))?;
    let id = input["id"].as_str().ok_or("id required")?;
    let status = match input["status"].as_str() {
        Some("in_progress") => Some(task::TaskStatus::InProgress),
        Some("completed") => Some(task::TaskStatus::Completed),
        Some("pending") => Some(task::TaskStatus::Pending),
        _ => None,
    };
    let store = TaskStore::new(task_store_root(session_id, working_dir));
    let task = store
        .update(
            id,
            status,
            input["subject"].as_str(),
            input["description"].as_str(),
        )
        .map_err(|e| format!("failed to update task: {e}"))?;
    Ok(format!(
        "Updated task {}: {} - {}",
        task.id,
        task.subject,
        status_icon(&task.status)
    ))
}

fn status_icon(status: &task::TaskStatus) -> &str {
    match status {
        task::TaskStatus::Pending => "pending",
        task::TaskStatus::InProgress => "in_progress",
        task::TaskStatus::Completed => "completed",
    }
}

fn text_result(content: String, is_error: bool) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.clone(),
        is_error,
        error: is_error.then_some(content),
        metadata: BTreeMap::new(),
        duration_ms: None,
    }
}

// ─── 工具定义 ────────────────────────────────────────────────────────

const TASK_CREATE_DESCRIPTION: &str = "Create a tracked task";
const TASK_CREATE_PARAMETERS: &str = r#"{"type":"object","properties":{"subject":{"type":"string"},"description":{"type":"string"},"blocks":{"type":"array","items":{"type":"string"}}},"required":["subject","description"]}"#;

const TASK_LIST_DESCRIPTION: &str = "List all tracked tasks";
const TASK_LIST_PARAMETERS: &str = r#"{"type":"object","properties":{}}"#;

const TASK_UPDATE_DESCRIPTION: &str = "Update task status";
const TASK_UPDATE_PARAMETERS: &str = r#"{"type":"object","properties":{"id":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]},"subject":{"type":"string"},"description":{"type":"string"}},"required":["id"]}"#;

fn task_create_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "taskCreate".into(),
        description: TASK_CREATE_DESCRIPTION.into(),
        parameters: parse_parameters(TASK_CREATE_PARAMETERS),
        is_builtin: false,
    }
}

fn task_list_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "taskList".into(),
        description: TASK_LIST_DESCRIPTION.into(),
        parameters: parse_parameters(TASK_LIST_PARAMETERS),
        is_builtin: false,
    }
}

fn task_update_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "taskUpdate".into(),
        description: TASK_UPDATE_DESCRIPTION.into(),
        parameters: parse_parameters(TASK_UPDATE_PARAMETERS),
        is_builtin: false,
    }
}

fn parse_parameters(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    })
}

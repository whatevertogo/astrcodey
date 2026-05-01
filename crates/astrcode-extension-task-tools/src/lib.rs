//! astrcode-extension-task-tools — session-local 任务跟踪工具。
//!
//! 注册的工具：
//! - `taskCreate`: 创建跟踪任务
//! - `taskList`: 列出所有跟踪任务
//! - `taskGet`: 读取单个任务
//! - `taskUpdate`: 更新或删除任务

mod task;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect, HookMode,
    },
    tool::{ToolDefinition, ToolOrigin, ToolResult},
    types::project_hash_from_path,
};
use astrcode_support::hostpaths;
use serde::Deserialize;
use serde_json::{Value, json};
use task::{DependencyChange, DependencyCleanup, Task, TaskStatus, TaskStore, TaskUpdate};

const TOOL_TASK_CREATE: &str = "taskCreate";
const TOOL_TASK_LIST: &str = "taskList";
const TOOL_TASK_GET: &str = "taskGet";
const TOOL_TASK_UPDATE: &str = "taskUpdate";

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
        tool_definitions()
    }

    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let store = TaskStore::new(task_store_root(&ctx.session_id, working_dir));
        let result = match tool_name {
            TOOL_TASK_CREATE => handle_task_create(arguments, &store),
            TOOL_TASK_LIST => handle_task_list(arguments, &store),
            TOOL_TASK_GET => handle_task_get(arguments, &store),
            TOOL_TASK_UPDATE => handle_task_update(arguments, &store),
            _ => return Err(ExtensionError::NotFound(tool_name.into())),
        };

        Ok(match result {
            Ok(result) => result,
            Err(error) => text_result(error.clone(), true, metadata([("error", json!(error))])),
        })
    }
}

// ─── 工具输入 ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskCreateArgs {
    subject: String,
    description: String,
    #[serde(default)]
    active_form: Option<String>,
    #[serde(default)]
    blocks: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskListArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskGetArgs {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskUpdateArgs {
    id: String,
    #[serde(default)]
    action: Option<TaskAction>,
    #[serde(default)]
    status: Option<TaskStatus>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    active_form: Option<Option<String>>,
    #[serde(default)]
    blocks: Option<DependencyArgs>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskAction {
    Delete,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DependencyArgs {
    #[serde(default)]
    replace: Option<Vec<String>>,
    #[serde(default)]
    add: Option<Vec<String>>,
    #[serde(default)]
    remove: Option<Vec<String>>,
}

// ─── 工具实现 ────────────────────────────────────────────────────────

fn handle_task_create(arguments: Value, store: &TaskStore) -> Result<ToolResult, String> {
    let args = parse_args::<TaskCreateArgs>(arguments, TOOL_TASK_CREATE)?;
    let task = store.create(
        &args.subject,
        &args.description,
        args.active_form,
        args.blocks,
    )?;
    Ok(task_result(
        format!("Created task {}: {}", task.id, task.subject),
        "create",
        task,
    ))
}

fn handle_task_list(arguments: Value, store: &TaskStore) -> Result<ToolResult, String> {
    let _args = parse_args::<TaskListArgs>(arguments, TOOL_TASK_LIST)?;
    let tasks = store.list()?;
    let content = if tasks.is_empty() {
        "No tasks.".to_string()
    } else {
        tasks
            .iter()
            .map(|task| {
                let blocked = if task.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" [blocked by {}]", task.blocked_by.join(", "))
                };
                format!(
                    "[{}] {} - {}{}",
                    task.id,
                    status_label(task.status),
                    task.subject,
                    blocked
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let count = tasks.len();
    Ok(text_result(
        content,
        false,
        metadata([("tasks", json!(tasks)), ("count", json!(count))]),
    ))
}

fn handle_task_get(arguments: Value, store: &TaskStore) -> Result<ToolResult, String> {
    let args = parse_args::<TaskGetArgs>(arguments, TOOL_TASK_GET)?;
    let task = store
        .get(&args.id)?
        .ok_or_else(|| format!("task '{}' not found", args.id))?;
    Ok(task_result(
        format!(
            "Task {}: {} ({})",
            task.id,
            task.subject,
            status_label(task.status)
        ),
        "get",
        task,
    ))
}

fn handle_task_update(arguments: Value, store: &TaskStore) -> Result<ToolResult, String> {
    let args = parse_args::<TaskUpdateArgs>(arguments, TOOL_TASK_UPDATE)?;
    if args.action == Some(TaskAction::Delete) {
        validate_delete_args(&args)?;
        let cleanup = store.delete(&args.id)?;
        return Ok(delete_result(args.id, cleanup));
    }

    let update = args.into_update()?;
    let id = update.id.clone();
    let task = store.update(&id, update.update)?;
    let mut content = format!(
        "Updated task {}: {} - {}",
        task.id,
        task.subject,
        status_label(task.status)
    );
    if task.status == TaskStatus::Completed {
        let remaining = store
            .list()?
            .into_iter()
            .filter(|task| task.status != TaskStatus::Completed)
            .count();
        if remaining > 0 {
            content.push_str("\n\nTask completed. Call taskList to find the next open task.");
        }
    }
    Ok(task_result(content, "update", task))
}

struct TaskUpdateWithId {
    id: String,
    update: TaskUpdate,
}

trait IntoTaskUpdate {
    fn into_update(self) -> Result<TaskUpdateWithId, String>;
}

impl IntoTaskUpdate for TaskUpdateArgs {
    fn into_update(self) -> Result<TaskUpdateWithId, String> {
        if self.action.is_some() {
            return Err("unsupported action for update".to_string());
        }

        let has_update = self.status.is_some()
            || self.subject.is_some()
            || self.description.is_some()
            || self.active_form.is_some()
            || self.blocks.is_some();
        if !has_update {
            return Err("taskUpdate requires at least one field to update".to_string());
        }

        let blocks = self.blocks.map(DependencyArgs::into_change).transpose()?;
        Ok(TaskUpdateWithId {
            id: self.id,
            update: TaskUpdate {
                status: self.status,
                subject: self.subject,
                description: self.description,
                active_form: self.active_form,
                blocks,
            },
        })
    }
}

impl DependencyArgs {
    fn into_change(self) -> Result<DependencyChange, String> {
        match (self.replace, self.add, self.remove) {
            (Some(replace), None, None) => Ok(DependencyChange::Replace(replace)),
            (None, add, remove) if add.is_some() || remove.is_some() => {
                Ok(DependencyChange::Patch {
                    add: add.unwrap_or_default(),
                    remove: remove.unwrap_or_default(),
                })
            },
            (None, None, None) => Err("blocks update requires replace or add/remove".to_string()),
            _ => Err(
                "blocks.replace cannot be combined with blocks.add or blocks.remove".to_string(),
            ),
        }
    }
}

fn validate_delete_args(args: &TaskUpdateArgs) -> Result<(), String> {
    if args.status.is_some()
        || args.subject.is_some()
        || args.description.is_some()
        || args.active_form.is_some()
        || args.blocks.is_some()
    {
        return Err("action=delete cannot be combined with update fields".to_string());
    }
    Ok(())
}

fn parse_args<T>(arguments: Value, tool_name: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments)
        .map_err(|error| format!("invalid args for {tool_name}: {error}"))
}

fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
    }
}

fn task_result(content: String, operation: &str, task: Task) -> ToolResult {
    text_result(
        content,
        false,
        metadata([("operation", json!(operation)), ("task", json!(task))]),
    )
}

fn delete_result(id: String, cleanup: DependencyCleanup) -> ToolResult {
    text_result(
        format!("Deleted task {id}"),
        false,
        metadata([
            ("operation", json!("delete")),
            ("deletedTaskId", json!(id)),
            ("dependencyCleanup", json!(cleanup)),
        ]),
    )
}

fn text_result(
    content: String,
    is_error: bool,
    metadata: BTreeMap<String, serde_json::Value>,
) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.clone(),
        is_error,
        error: is_error.then_some(content),
        metadata,
        duration_ms: None,
    }
}

fn metadata<const N: usize>(entries: [(&str, serde_json::Value); N]) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

// ─── 工具定义 ────────────────────────────────────────────────────────

const TASK_CREATE_DESCRIPTION: &str = "\
Create a session-local tracked task. Use it for complex multi-step work; mark tasks in_progress \
                                       before starting and completed only when done.";
const TASK_LIST_DESCRIPTION: &str = "List session-local tracked tasks and their dependency status.";
const TASK_GET_DESCRIPTION: &str = "Get a session-local tracked task by id.";
const TASK_UPDATE_DESCRIPTION: &str = "\
Update or delete a session-local tracked task. Use action=\"delete\" for deletion; deletion is not \
                                       a TaskStatus.";

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        task_create_tool_definition(),
        task_list_tool_definition(),
        task_get_tool_definition(),
        task_update_tool_definition(),
    ]
}

fn task_create_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_TASK_CREATE.into(),
        description: TASK_CREATE_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "subject": { "type": "string", "description": "Brief imperative task title." },
                "description": { "type": "string", "description": "Detailed task requirements." },
                "activeForm": { "type": "string", "description": "Present-progress phrase used when the task is in_progress." },
                "blocks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task IDs this task blocks."
                }
            },
            "required": ["subject", "description"]
        }),
        origin: ToolOrigin::Bundled,
    }
}

fn task_list_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_TASK_LIST.into(),
        description: TASK_LIST_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
        origin: ToolOrigin::Bundled,
    }
}

fn task_get_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_TASK_GET.into(),
        description: TASK_GET_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": { "type": "string", "description": "Task ID." }
            },
            "required": ["id"]
        }),
        origin: ToolOrigin::Bundled,
    }
}

fn task_update_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_TASK_UPDATE.into(),
        description: TASK_UPDATE_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": { "type": "string", "description": "Task ID." },
                "action": { "type": "string", "enum": ["delete"], "description": "Use delete to remove the task and clean dependency edges." },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                "subject": { "type": "string" },
                "description": { "type": "string" },
                "activeForm": { "type": ["string", "null"], "description": "Required when status is in_progress. Null clears it." },
                "blocks": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "replace": { "type": "array", "items": { "type": "string" } },
                        "add": { "type": "array", "items": { "type": "string" } },
                        "remove": { "type": "array", "items": { "type": "string" } }
                    },
                    "description": "Either replace the full blocks set or patch it with add/remove."
                }
            },
            "required": ["id"]
        }),
        origin: ToolOrigin::Bundled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(name: &str) -> TaskStore {
        let root = std::env::temp_dir()
            .join("astrcode-task-tool-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        TaskStore::new(root)
    }

    #[test]
    fn manifest_matches_rust_tool_definitions() {
        let manifest: Value =
            serde_json::from_str(include_str!("../extension.json")).expect("manifest json");
        let manifest_tools = manifest["tools"]
            .as_array()
            .expect("manifest tools should be an array");
        let definitions = tool_definitions();

        assert_eq!(manifest_tools.len(), definitions.len());
        for definition in definitions {
            let manifest_tool = manifest_tools
                .iter()
                .find(|tool| tool["name"] == definition.name)
                .unwrap_or_else(|| panic!("manifest missing {}", definition.name));

            assert_eq!(manifest_tool["description"], definition.description);
            assert_eq!(manifest_tool["parameters"], definition.parameters);
        }
    }

    #[test]
    fn update_args_reject_delete_with_update_fields() {
        let args = TaskUpdateArgs {
            id: "1".to_string(),
            action: Some(TaskAction::Delete),
            status: Some(TaskStatus::Completed),
            subject: None,
            description: None,
            active_form: None,
            blocks: None,
        };

        assert!(validate_delete_args(&args).is_err());
    }

    #[test]
    fn dependency_args_require_one_mode() {
        let mixed = DependencyArgs {
            replace: Some(vec!["1".to_string()]),
            add: Some(vec!["2".to_string()]),
            remove: None,
        };

        assert!(mixed.into_change().is_err());
    }

    #[test]
    fn tool_results_expose_stable_metadata() {
        let store = test_store("metadata");
        let created = handle_task_create(
            json!({
                "subject": "Build feature",
                "description": "Implement the task tool contract"
            }),
            &store,
        )
        .expect("create should succeed");
        let task_id = created.metadata["task"]["id"]
            .as_str()
            .expect("task id should be serialized")
            .to_string();
        assert_eq!(created.metadata["operation"], "create");

        let fetched =
            handle_task_get(json!({ "id": task_id }), &store).expect("get should succeed");
        assert_eq!(fetched.metadata["operation"], "get");
        assert_eq!(fetched.metadata["task"]["subject"], "Build feature");

        let updated = handle_task_update(
            json!({
                "id": task_id,
                "status": "completed"
            }),
            &store,
        )
        .expect("update should succeed");
        assert_eq!(updated.metadata["operation"], "update");
        assert_eq!(updated.metadata["task"]["status"], "completed");

        let deleted = handle_task_update(
            json!({
                "id": task_id,
                "action": "delete"
            }),
            &store,
        )
        .expect("delete should succeed");
        assert_eq!(deleted.metadata["operation"], "delete");
        assert_eq!(deleted.metadata["deletedTaskId"], "1");
        assert!(deleted.metadata["dependencyCleanup"].is_object());
    }
}

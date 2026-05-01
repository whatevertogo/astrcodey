//! astrcode-extension-todo-tool — session-local progress todo list.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect, HookMode,
    },
    tool::{ToolDefinition, ToolOrigin, ToolResult},
    types::project_hash_from_path,
};
use astrcode_support::hostpaths;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TODO_WRITE_TOOL_NAME: &str = "todoWrite";

const TODO_WRITE_DESCRIPTION: &str = "\
Update the progress todo list for the current session. Use it proactively for complex multi-step \
                                      work. Keep at most one item in_progress while actively \
                                      working. Provide both content and activeForm for each item.";
const PROGRESS_SCHEMA_VERSION: u32 = 1;
const PROGRESS_FILE: &str = "progress.json";

/// Compute session-local progress todo storage root.
pub fn progress_store_root(session_id: &str, working_dir: &str) -> PathBuf {
    let hash = project_hash_from_path(&PathBuf::from(working_dir));
    hostpaths::sessions_dir(&hash)
        .join(session_id)
        .join("todos")
}

/// Return bundled todo extension.
pub fn extension() -> Arc<dyn Extension> {
    Arc::new(TodoToolExtension)
}

struct TodoToolExtension;

#[async_trait::async_trait]
impl Extension for TodoToolExtension {
    fn id(&self) -> &str {
        "astrcode-todo-tool"
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
        vec![todo_write_tool_definition()]
    }

    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != TODO_WRITE_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let store = ProgressListStore::new(progress_store_root(&ctx.session_id, working_dir));
        Ok(match handle_todo_write(arguments, &store) {
            Ok(result) => result,
            Err(error) => text_result(error.clone(), true, metadata([("error", json!(error))])),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TodoWriteArgs {
    todos: Vec<TodoInputItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TodoInputItem {
    content: String,
    active_form: String,
    status: ProgressStatus,
}

/// Progress item status for the single-agent todo list.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStatus {
    Pending,
    InProgress,
    Completed,
}

/// A single progress todo item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProgressItem {
    pub content: String,
    pub active_form: String,
    pub status: ProgressStatus,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

/// Persisted session-local progress list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProgressList {
    pub schema_version: u32,
    pub items: Vec<ProgressItem>,
    pub updated_at: String,
}

/// Result of replacing the todo list.
#[derive(Debug, Clone, PartialEq)]
pub struct TodoWriteOutcome {
    pub old_todos: Vec<ProgressItem>,
    pub new_todos: Vec<ProgressItem>,
    pub verification_nudge_needed: bool,
}

/// Session-local progress todo store.
pub struct ProgressListStore {
    root: PathBuf,
}

impl ProgressListStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn load_items(&self) -> Result<Vec<ProgressItem>, String> {
        self.load_progress().map(|progress| progress.items)
    }

    pub fn replace(&self, submitted: Vec<ProgressItem>) -> Result<TodoWriteOutcome, String> {
        validate_items(&submitted)?;

        let old_todos = self.load_items()?;
        let verification_nudge_needed = needs_verification_nudge(&submitted);
        let new_todos = if submitted
            .iter()
            .all(|item| item.status == ProgressStatus::Completed)
        {
            Vec::new()
        } else {
            submitted
        };

        self.save_items(&new_todos)?;

        Ok(TodoWriteOutcome {
            old_todos,
            new_todos,
            verification_nudge_needed,
        })
    }

    fn load_progress(&self) -> Result<ProgressList, String> {
        let path = self.root.join(PROGRESS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let progress = serde_json::from_str::<ProgressList>(&content)
                    .map_err(|error| format!("parse progress list: {error}"))?;
                if progress.schema_version != PROGRESS_SCHEMA_VERSION {
                    return Err(format!(
                        "unsupported progress list schema version {}",
                        progress.schema_version
                    ));
                }
                Ok(progress)
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ProgressList {
                schema_version: PROGRESS_SCHEMA_VERSION,
                items: Vec::new(),
                updated_at: now_utc(),
            }),
            Err(error) => Err(format!("read progress list: {error}")),
        }
    }

    fn save_items(&self, items: &[ProgressItem]) -> Result<(), String> {
        self.ensure_dir()?;
        let progress = ProgressList {
            schema_version: PROGRESS_SCHEMA_VERSION,
            items: items.to_vec(),
            updated_at: now_utc(),
        };
        self.write_json(PROGRESS_FILE, &progress)
    }

    fn write_json<T: Serialize>(&self, file_name: &str, value: &T) -> Result<(), String> {
        let path = self.root.join(file_name);
        let tmp = self.root.join(format!("{file_name}.tmp"));
        let json = serde_json::to_string_pretty(value)
            .map_err(|error| format!("serialize {file_name}: {error}"))?;
        std::fs::write(&tmp, json).map_err(|error| format!("write {file_name}: {error}"))?;
        std::fs::rename(&tmp, &path).map_err(|error| format!("save {file_name}: {error}"))?;
        Ok(())
    }

    fn ensure_dir(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|error| format!("create todo directory {}: {error}", self.root.display()))
    }
}

fn handle_todo_write(arguments: Value, store: &ProgressListStore) -> Result<ToolResult, String> {
    let args = serde_json::from_value::<TodoWriteArgs>(arguments)
        .map_err(|error| format!("invalid args for {TODO_WRITE_TOOL_NAME}: {error}"))?;
    let outcome = store.replace(args.todos.into_iter().map(ProgressItem::from).collect())?;

    let mut content = String::from(
        "Todos have been modified successfully. Continue to use the todo list to track your \
         progress. Proceed with the current task if applicable.",
    );
    if outcome.verification_nudge_needed {
        content.push_str(
            "\n\nNOTE: You just completed a multi-step todo list without an explicit verification \
             step. Before final response, run the relevant verification or explain why it cannot \
             be run.",
        );
    }

    Ok(text_result(
        content,
        false,
        metadata([
            ("oldTodos", json!(outcome.old_todos)),
            ("newTodos", json!(outcome.new_todos)),
            (
                "verificationNudgeNeeded",
                json!(outcome.verification_nudge_needed),
            ),
        ]),
    ))
}

fn validate_items(items: &[ProgressItem]) -> Result<(), String> {
    let mut in_progress = 0;
    for item in items {
        validate_text("content", &item.content)?;
        validate_text("activeForm", &item.active_form)?;
        if item.status == ProgressStatus::InProgress {
            in_progress += 1;
        }
    }

    if in_progress > 1 {
        return Err("at most one todo can be in_progress".to_string());
    }

    Ok(())
}

fn validate_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(())
}

impl From<TodoInputItem> for ProgressItem {
    fn from(item: TodoInputItem) -> Self {
        Self {
            content: item.content,
            active_form: item.active_form,
            status: item.status,
            metadata: BTreeMap::new(),
        }
    }
}

fn needs_verification_nudge(items: &[ProgressItem]) -> bool {
    items.len() >= 3
        && items
            .iter()
            .all(|item| item.status == ProgressStatus::Completed)
        && !items.iter().any(|item| {
            let text = format!("{} {}", item.content, item.active_form).to_ascii_lowercase();
            ["verif", "test", "check"]
                .iter()
                .any(|needle| text.contains(needle))
        })
}

fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn todo_write_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TODO_WRITE_TOOL_NAME.into(),
        description: TODO_WRITE_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Imperative form describing what needs to be done."
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Present continuous form shown while the item is in_progress."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "activeForm", "status"]
                    },
                    "description": "The full replacement progress todo list."
                }
            },
            "required": ["todos"]
        }),
        origin: ToolOrigin::Bundled,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, active_form: &str, status: ProgressStatus) -> ProgressItem {
        ProgressItem {
            content: content.to_string(),
            active_form: active_form.to_string(),
            status,
            metadata: BTreeMap::new(),
        }
    }

    fn test_store(name: &str) -> ProgressListStore {
        let root = std::env::temp_dir()
            .join("astrcode-todo-tool-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        ProgressListStore::new(root)
    }

    #[test]
    fn todo_write_replaces_list_and_returns_metadata() {
        let store = test_store("replace");
        let first = handle_todo_write(
            json!({
                "todos": [
                    {
                        "content": "Inspect files",
                        "activeForm": "Inspecting files",
                        "status": "in_progress"
                    }
                ]
            }),
            &store,
        )
        .expect("write should succeed");
        assert!(first.metadata["oldTodos"].as_array().unwrap().is_empty());
        assert_eq!(first.metadata["newTodos"][0]["content"], "Inspect files");

        let second = handle_todo_write(
            json!({
                "todos": [
                    {
                        "content": "Run tests",
                        "activeForm": "Running tests",
                        "status": "pending"
                    }
                ]
            }),
            &store,
        )
        .expect("replace should succeed");
        assert_eq!(second.metadata["oldTodos"][0]["content"], "Inspect files");
        assert_eq!(second.metadata["newTodos"][0]["content"], "Run tests");
    }

    #[test]
    fn rejects_multiple_in_progress_items() {
        let store = test_store("multiple-in-progress");
        let result = store.replace(vec![
            item(
                "Inspect files",
                "Inspecting files",
                ProgressStatus::InProgress,
            ),
            item("Run tests", "Running tests", ProgressStatus::InProgress),
        ]);

        assert_eq!(
            result.expect_err("multiple in_progress should fail"),
            "at most one todo can be in_progress"
        );
    }

    #[test]
    fn rejects_blank_content_and_active_form() {
        let store = test_store("blank-fields");
        let blank_content =
            store.replace(vec![item(" ", "Running tests", ProgressStatus::InProgress)]);
        assert_eq!(
            blank_content.expect_err("blank content should fail"),
            "content must not be empty"
        );

        let blank_active_form =
            store.replace(vec![item("Run tests", " ", ProgressStatus::InProgress)]);
        assert_eq!(
            blank_active_form.expect_err("blank active form should fail"),
            "activeForm must not be empty"
        );
    }

    #[test]
    fn clears_store_when_all_items_are_completed() {
        let store = test_store("clear-completed");
        store
            .replace(vec![
                item(
                    "Inspect files",
                    "Inspecting files",
                    ProgressStatus::Completed,
                ),
                item("Run tests", "Running tests", ProgressStatus::Completed),
            ])
            .expect("completed write should succeed");

        assert!(store.load_items().unwrap().is_empty());
    }

    #[test]
    fn manifest_matches_rust_tool_definition() {
        let manifest: Value =
            serde_json::from_str(include_str!("../extension.json")).expect("manifest json");
        let tool = manifest["tools"][0].clone();
        let definition = todo_write_tool_definition();

        assert_eq!(manifest["id"], "astrcode-todo-tool");
        assert_eq!(manifest["tools"].as_array().unwrap().len(), 1);
        assert_eq!(tool["name"], definition.name);
        assert_eq!(tool["description"], definition.description);
        assert_eq!(tool["parameters"], definition.parameters);
    }

    #[test]
    fn verification_nudge_fires_for_completed_multi_step_list_without_verification() {
        let store = test_store("verification-nudge");
        let result = store
            .replace(vec![
                item(
                    "Inspect files",
                    "Inspecting files",
                    ProgressStatus::Completed,
                ),
                item("Edit code", "Editing code", ProgressStatus::Completed),
                item(
                    "Write summary",
                    "Writing summary",
                    ProgressStatus::Completed,
                ),
            ])
            .expect("write should succeed");

        assert!(result.verification_nudge_needed);
    }
}

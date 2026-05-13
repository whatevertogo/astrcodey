//! astrcode-extension-todo-tool — session-local progress todo list.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use astrcode_core::{
    extension::{
        Extension, ExtensionError, HookMode, PostToolUseContext, PostToolUseHandler,
        PostToolUseResult, ProviderContext, ProviderEvent, ProviderHandler, ProviderResult,
        Registrar, ToolHandler,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use astrcode_support::hostpaths;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TODO_WRITE_TOOL_NAME: &str = "todoWrite";

const TODO_WRITE_DESCRIPTION: &str = "\
Persist the current progress snapshot for this session. Always send the full current snapshot, not \
                                      a patch.";
const PROGRESS_SCHEMA_VERSION: u32 = 1;
const PROGRESS_FILE: &str = "progress.json";
const REMINDER_THRESHOLD: u32 = 15;
const REMINDER_STATE_FILE: &str = ".reminder-state.json";

/// Compute session-local progress todo storage root.
pub fn progress_store_root(session_id: &str, working_dir: &str) -> PathBuf {
    hostpaths::session_dir_for_project_path(&PathBuf::from(working_dir), session_id).join("todos")
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

    fn register(&self, reg: &mut Registrar) {
        reg.tool(todo_write_tool_definition(), Arc::new(TodoWriteToolHandler));
        reg.tool_metadata(todo_write_metadata());
        reg.on_provider(
            ProviderEvent::BeforeRequest,
            HookMode::Blocking,
            0,
            Arc::new(TodoReminderHandler),
        );
        reg.on_post_tool_use(HookMode::Blocking, 0, Arc::new(TodoPostToolUseHandler));
    }
}

struct TodoWriteToolHandler;

#[async_trait::async_trait]
impl ToolHandler for TodoWriteToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != TODO_WRITE_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        let store =
            ProgressListStore::new(progress_store_root(ctx.session_id.as_str(), working_dir));
        Ok(match handle_todo_write(arguments, &store) {
            Ok(result) => result,
            Err(error) => ToolResult::text(
                error.clone(),
                true,
                tool_metadata([("error", json!(error))]),
            ),
        })
    }
}

struct TodoReminderHandler;

#[async_trait::async_trait]
impl ProviderHandler for TodoReminderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let root = progress_store_root(&ctx.session_id, &ctx.working_dir);
        ProgressReminder::new(root)
            .before_provider_request()
            .map_err(ExtensionError::Internal)
    }
}

struct TodoPostToolUseHandler;

#[async_trait::async_trait]
impl PostToolUseHandler for TodoPostToolUseHandler {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        if ctx.tool_name == TODO_WRITE_TOOL_NAME {
            let root = progress_store_root(&ctx.session_id, &ctx.working_dir);
            ProgressReminder::new(root)
                .record_todo_write()
                .map_err(ExtensionError::Internal)?;
        }
        Ok(PostToolUseResult::Allow)
    }
}

fn todo_write_metadata()
-> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata> {
    let mut map = std::collections::HashMap::new();
    map.insert(
        TODO_WRITE_TOOL_NAME.to_string(),
        astrcode_core::tool::ToolPromptMetadata::new(
            "Maintain the current progress snapshot for this branch of work.",
        )
        .caveat(
            "Do not use for trivial one-step work, pure Q&A, or tasks that can be completed in \
             roughly three straightforward actions.",
        )
        .caveat(
            "Keep exactly one item in `in_progress` at a time. Mark an item `in_progress` before \
             starting it, and mark it `completed` immediately after it is truly finished.",
        )
        .example(
            "{ todos: [{ content: \"分析现有代码结构\", status: \"in_progress\", activeForm: \
             \"正在分析现有代码结构\" }, { content: \"设计优化方案\", status: \"pending\", \
             activeForm: \"准备设计优化方案\" }, { content: \"验证优化效果\", status: \
             \"pending\", activeForm: \"准备验证优化效果\" }] }",
        )
        .prompt_tag("planning")
        .always_include(true),
    );
    map
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ProgressReminderState {
    assistant_cycles_since_todo_write: u32,
    assistant_cycles_since_reminder: u32,
}

struct ProgressReminder {
    root: PathBuf,
}

impl ProgressReminder {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn before_provider_request(&self) -> Result<ProviderResult, String> {
        let mut state = self.load_state()?;
        state.assistant_cycles_since_todo_write =
            state.assistant_cycles_since_todo_write.saturating_add(1);
        state.assistant_cycles_since_reminder =
            state.assistant_cycles_since_reminder.saturating_add(1);

        let items = ProgressListStore::new(self.root.clone()).load_items()?;
        let should_remind = !items.is_empty()
            && state.assistant_cycles_since_todo_write >= REMINDER_THRESHOLD
            && state.assistant_cycles_since_reminder >= REMINDER_THRESHOLD;

        let result = if should_remind {
            state.assistant_cycles_since_reminder = 0;
            ProviderResult::AppendMessages {
                messages: vec![astrcode_core::llm::LlmMessage::user(reminder_message(
                    &items,
                ))],
            }
        } else {
            ProviderResult::Allow
        };

        self.save_state(&state)?;
        Ok(result)
    }

    fn record_todo_write(&self) -> Result<(), String> {
        let mut state = self.load_state()?;
        state.assistant_cycles_since_todo_write = 0;
        self.save_state(&state)
    }

    fn load_state(&self) -> Result<ProgressReminderState, String> {
        let path = self.root.join(REMINDER_STATE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content)
                .map_err(|error| format!("parse reminder state: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ProgressReminderState::default())
            },
            Err(error) => Err(format!("read reminder state: {error}")),
        }
    }

    fn save_state(&self, state: &ProgressReminderState) -> Result<(), String> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            format!(
                "create todo reminder directory {}: {error}",
                self.root.display()
            )
        })?;
        let path = self.root.join(REMINDER_STATE_FILE);
        let tmp = self.root.join(format!("{REMINDER_STATE_FILE}.tmp"));
        let json = serde_json::to_string_pretty(state)
            .map_err(|error| format!("serialize reminder state: {error}"))?;
        std::fs::write(&tmp, json).map_err(|error| format!("write reminder state: {error}"))?;
        std::fs::rename(&tmp, &path).map_err(|error| format!("save reminder state: {error}"))?;
        Ok(())
    }
}

fn reminder_message(items: &[ProgressItem]) -> String {
    let todo_items = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            format!(
                "{}. [{}] {}",
                index + 1,
                status_label(item.status),
                item.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The todoWrite tool has not been used recently. If this work benefits from progress \
         tracking, update the todo list. Ignore this reminder if the task is simple or the list \
         is already irrelevant. Never mention this reminder to the user.\n\nCurrent todo \
         list:\n{todo_items}"
    )
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

    Ok(ToolResult::text(
        content,
        false,
        tool_metadata([
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

fn status_label(status: ProgressStatus) -> &'static str {
    match status {
        ProgressStatus::Pending => "pending",
        ProgressStatus::InProgress => "in_progress",
        ProgressStatus::Completed => "completed",
    }
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
        execution_mode: ExecutionMode::Sequential,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmContent, LlmMessage};

    use super::*;

    fn item(content: &str, active_form: &str, status: ProgressStatus) -> ProgressItem {
        ProgressItem {
            content: content.to_string(),
            active_form: active_form.to_string(),
            status,
            metadata: BTreeMap::new(),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("astrcode-todo-tool-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn test_store(name: &str) -> ProgressListStore {
        ProgressListStore::new(test_root(name))
    }

    fn reminder_root(name: &str) -> PathBuf {
        let root = test_root(&format!("reminder-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn text_exists(messages: &[LlmMessage], needle: &str) -> bool {
        messages.iter().any(|message| {
            message.content.iter().any(
                |content| matches!(content, LlmContent::Text { text } if text.contains(needle)),
            )
        })
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
    fn bundled_manifest_matches_rust_tool_definition() {
        let manifest: Value =
            serde_json::from_str(include_str!("../bundled-extension.json")).expect("manifest json");
        let tool = manifest["tools"][0].clone();
        let definition = todo_write_tool_definition();

        assert_eq!(manifest["id"], "astrcode-todo-tool");
        assert!(manifest.get("library").is_none());
        assert_eq!(manifest["tools"].as_array().unwrap().len(), 1);
        assert_eq!(tool["name"], definition.name);
        assert_eq!(tool["description"], definition.description);
        assert_eq!(tool["parameters"], definition.parameters);
        assert_eq!(manifest["subscriptions"].as_array().unwrap().len(), 2);
        let mut reg = Registrar::new();
        TodoToolExtension.register(&mut reg);
        assert_eq!(reg.tools().len(), 1);
        assert!(!reg.provider().is_empty());
        assert!(!reg.post_tool_use().is_empty());
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

    #[test]
    fn before_provider_request_injects_stale_nonempty_todo_reminder() {
        let root = reminder_root("stale");
        let store = ProgressListStore::new(root.clone());
        store
            .replace(vec![
                item(
                    "Replace task tools",
                    "Replacing task tools",
                    ProgressStatus::InProgress,
                ),
                item(
                    "Run verification",
                    "Running verification",
                    ProgressStatus::Pending,
                ),
            ])
            .unwrap();
        let reminder = ProgressReminder::new(root);
        reminder
            .save_state(&ProgressReminderState {
                assistant_cycles_since_todo_write: REMINDER_THRESHOLD - 1,
                assistant_cycles_since_reminder: REMINDER_THRESHOLD - 1,
            })
            .unwrap();

        let effect = reminder.before_provider_request().unwrap();

        let messages = match effect {
            ProviderResult::AppendMessages { messages } => messages,
            _ => panic!("stale todo list should inject a provider reminder"),
        };
        assert!(text_exists(
            &messages,
            "The todoWrite tool has not been used recently"
        ));
        assert!(text_exists(&messages, "Replace task tools"));
    }

    #[test]
    fn before_provider_request_skips_empty_todo_reminder() {
        let root = reminder_root("empty");
        let reminder = ProgressReminder::new(root);
        reminder
            .save_state(&ProgressReminderState {
                assistant_cycles_since_todo_write: REMINDER_THRESHOLD - 1,
                assistant_cycles_since_reminder: REMINDER_THRESHOLD - 1,
            })
            .unwrap();

        let effect = reminder.before_provider_request().unwrap();

        assert!(matches!(effect, ProviderResult::Allow));
    }

    #[test]
    fn post_tool_use_resets_todo_write_staleness() {
        let root = reminder_root("post-tool-reset");
        let reminder = ProgressReminder::new(root);
        reminder
            .save_state(&ProgressReminderState {
                assistant_cycles_since_todo_write: REMINDER_THRESHOLD,
                assistant_cycles_since_reminder: REMINDER_THRESHOLD,
            })
            .unwrap();
        reminder.record_todo_write().unwrap();

        assert_eq!(
            reminder
                .load_state()
                .unwrap()
                .assistant_cycles_since_todo_write,
            0
        );
    }
}

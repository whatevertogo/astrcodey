//! astrcode-extension-todo-tool — session-local progress todo list.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use astrcode_extension_sdk::{
    builder::{ExtensionToolDefinition, manifest},
    extension::{
        Extension, ExtensionCall, ExtensionCapability, ExtensionError, ExtensionManifest,
        ExtensionPaths, HookMode, PostToolUseContext, PostToolUseHandler, PostToolUseResult,
        PreparedProviderContribution, PreparedProviderEffect, ProviderContext,
        ProviderContributionHandler, ProviderContributionId, ProviderSettlementContext, Registrar,
        ToolContext, ToolHandler, ToolPlanContext,
    },
    tool::{
        ExecutionMode, HostResource, ToolDefinition, ToolOrigin, ToolPlan, ToolPromptMetadata,
        ToolPromptTag, ToolResult, tool_metadata,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const TODO_WRITE_TOOL_NAME: &str = "todoWrite";

const TODO_WRITE_DESCRIPTION: &str =
    "Update the session todo list to track multi-step task progress.\n\nWhen NOT to use:\n- \
     Simple Q&A or single straightforward task\n- One file, one edit, no progress tracking \
     needed\n\nTips:\n- Multi-step work, task lists, or when progress tracking helps\n- Every \
     item must declare `executor`: `self` (main agent does it directly) or `agent` (delegate; \
     then `agentType` is required). For agent steps set `mode`: `parallel` when independent of \
     other steps, `serial` otherwise. Default to `self` — delegate only when the step is an \
     isolated non-trivial subtask, parallel investigation clearly pays off, or independent \
     verification is warranted (see `agent` guidance). Revisit executors when new evidence \
     changes dependencies.\n\nRules:\n- Send the full list every time (not a patch). Keep exactly \
     one `in_progress`.\n- Mark `in_progress` BEFORE starting work. Mark `completed` only when \
     fully done (tests pass, implementation complete).\n- After receiving new instructions, \
     immediately add them as todos.\n- Each item: `content` (imperative: \"Fix auth bug\") + \
     `activeForm` (continuous: \"Fixing auth bug\").";
const PROGRESS_SCHEMA_VERSION: u32 = 2;
const PROGRESS_FILE: &str = "progress.json";
const REMINDER_THRESHOLD: u32 = 15;
const REMINDER_STATE_FILE: &str = ".reminder-state.json";

/// Return bundled todo extension.
pub fn extension() -> Arc<dyn Extension> {
    Arc::new(TodoToolExtension)
}

struct TodoToolExtension;

#[async_trait::async_trait]
impl Extension for TodoToolExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("astrcode-todo-tool")
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::ProviderRequest)
            .capability(ExtensionCapability::ToolIntercept)
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ExtensionToolDefinition::from_definition(todo_write_tool_definition())
                .with_prompt(todo_write_prompt()),
            Arc::new(TodoWriteToolHandler),
        );
        reg.on_provider_contribution(0, Arc::new(TodoReminderHandler));
        reg.on_post_tool_use(HookMode::Blocking, 0, Arc::new(TodoPostToolUseHandler));
    }
}

struct TodoWriteToolHandler;

#[async_trait::async_trait]
impl ToolHandler for TodoWriteToolHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Session))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name();
        if tool_name != TODO_WRITE_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        let root = todo_root(ctx.paths())?;
        let store = ProgressListStore::new(root);
        Ok(match handle_todo_write(ctx.arguments()?, &store) {
            Ok(result) => result,
            Err(error) => {
                let meta = tool_metadata([("error", json!(&error))]);
                ToolResult::text(error, true, meta)
            },
        }
        .into())
    }
}

struct TodoReminderHandler;

#[async_trait::async_trait]
impl ProviderContributionHandler for TodoReminderHandler {
    async fn prepare(
        &self,
        ctx: ProviderContext,
    ) -> Result<Option<PreparedProviderContribution>, ExtensionError> {
        let root = todo_root(ctx.paths())?;
        ProgressReminder::new(root)
            .prepare_provider_request()
            .map(Some)
            .map_err(ExtensionError::Internal)
    }

    async fn acknowledge(&self, ctx: ProviderSettlementContext) -> Result<(), ExtensionError> {
        let root = todo_root(ctx.paths())?;
        ProgressReminder::new(root)
            .acknowledge_provider_cycle(ctx.contribution_id().as_str())
            .map_err(ExtensionError::Internal)
    }
}

struct TodoPostToolUseHandler;

#[async_trait::async_trait]
impl PostToolUseHandler for TodoPostToolUseHandler {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        if ctx.tool_name() == TODO_WRITE_TOOL_NAME && !ctx.tool_result().is_error {
            let root = todo_root(ctx.paths())?;
            ProgressReminder::new(root)
                .record_todo_write()
                .map_err(ExtensionError::Internal)?;
        }
        Ok(PostToolUseResult::Allow)
    }
}

fn todo_root(paths: &ExtensionPaths) -> Result<PathBuf, ExtensionError> {
    paths
        .session_data_dir()
        .map(|path| path.join("todos"))
        .map_err(|error| ExtensionError::Internal(error.to_string()))
}

fn todo_write_prompt() -> ToolPromptMetadata {
    ToolPromptMetadata::new(String::new())
        .example(
            "{ todos: [{ content: \"分析现有代码结构\", executor: \"self\", status: \
             \"in_progress\", activeForm: \"正在分析现有代码结构\" }, { content: \
             \"审查安全相关改动\", executor: \"agent\", agentType: \"reviewer\", mode: \
             \"parallel\", status: \"pending\", activeForm: \"准备审查安全相关改动\" }] }",
        )
        .prompt_tag(ToolPromptTag::Planning)
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
    executor: TodoExecutor,
    agent_type: Option<String>,
    mode: Option<StepMode>,
}

/// Step executor: the main agent itself or a delegated subagent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum TodoExecutor {
    #[serde(rename = "self")]
    #[default]
    MainAgent,
    #[serde(rename = "agent")]
    SubAgent,
}

/// Execution order of a delegated step relative to other steps.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepMode {
    Serial,
    Parallel,
}

/// Progress item status for the single-agent todo list.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProgressStatus {
    Pending,
    InProgress,
    Completed,
}

/// A single progress todo item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProgressItem {
    pub content: String,
    pub active_form: String,
    pub status: ProgressStatus,
    pub executor: TodoExecutor,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub mode: Option<StepMode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

/// Persisted session-local progress list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProgressList {
    pub schema_version: u32,
    pub items: Vec<ProgressItem>,
    pub updated_at: String,
}

/// Result of replacing the todo list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TodoWriteOutcome {
    pub old_todos: Vec<ProgressItem>,
    pub new_todos: Vec<ProgressItem>,
    pub verification_nudge_needed: bool,
}

/// Session-local progress todo store.
pub(crate) struct ProgressListStore {
    root: PathBuf,
}

impl ProgressListStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn load_items(&self) -> Result<Vec<ProgressItem>, String> {
        self.load_progress().map(|progress| progress.items)
    }

    pub(crate) fn replace(&self, submitted: Vec<ProgressItem>) -> Result<TodoWriteOutcome, String> {
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
        let progress = astrcode_extension_sdk::hostpaths::read_json_state::<ProgressList>(&path)
            .map_err(|error| format!("read progress list: {error}"))?;
        match progress {
            Some(progress) => {
                if progress.schema_version != PROGRESS_SCHEMA_VERSION {
                    return Err(format!(
                        "unsupported progress list schema version {}",
                        progress.schema_version
                    ));
                }
                Ok(progress)
            },
            None => Ok(ProgressList {
                schema_version: PROGRESS_SCHEMA_VERSION,
                items: Vec::new(),
                updated_at: now_utc(),
            }),
        }
    }

    fn save_items(&self, items: &[ProgressItem]) -> Result<(), String> {
        self.ensure_dir()?;
        let progress = ProgressList {
            schema_version: PROGRESS_SCHEMA_VERSION,
            items: items.to_vec(),
            updated_at: now_utc(),
        };
        astrcode_extension_sdk::hostpaths::write_json_state(
            &self.root.join(PROGRESS_FILE),
            &progress,
        )
        .map_err(|error| format!("save progress list: {error}"))
    }

    fn ensure_dir(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|error| format!("create todo directory {}: {error}", self.root.display()))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ProgressReminderState {
    revision: u64,
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

    fn prepare_provider_request(&self) -> Result<PreparedProviderContribution, String> {
        let state = self.load_state()?;
        let next_todo_cycles = state.assistant_cycles_since_todo_write.saturating_add(1);
        let next_reminder_cycles = state.assistant_cycles_since_reminder.saturating_add(1);
        let items = ProgressListStore::new(self.root.clone()).load_items()?;
        let should_remind = !items.is_empty()
            && next_todo_cycles >= REMINDER_THRESHOLD
            && next_reminder_cycles >= REMINDER_THRESHOLD;
        let effect = if should_remind {
            PreparedProviderEffect::AppendMessages(vec![
                astrcode_extension_sdk::llm::LlmMessage::user(reminder_message(&items)),
            ])
        } else {
            PreparedProviderEffect::Unchanged
        };
        Ok(PreparedProviderContribution::new(
            ProviderContributionId::new(format!("todo-cycle:{}", state.revision)),
            effect,
        ))
    }

    fn acknowledge_provider_cycle(&self, contribution_id: &str) -> Result<(), String> {
        let items = ProgressListStore::new(self.root.clone()).load_items()?;
        astrcode_extension_sdk::hostpaths::update_json_state(
            &self.root.join(REMINDER_STATE_FILE),
            |state: Option<ProgressReminderState>| {
                let mut state = state.unwrap_or_default();
                if contribution_id != format!("todo-cycle:{}", state.revision) {
                    return Ok((None, ()));
                }
                state.assistant_cycles_since_todo_write =
                    state.assistant_cycles_since_todo_write.saturating_add(1);
                state.assistant_cycles_since_reminder =
                    state.assistant_cycles_since_reminder.saturating_add(1);
                if !items.is_empty()
                    && state.assistant_cycles_since_todo_write >= REMINDER_THRESHOLD
                    && state.assistant_cycles_since_reminder >= REMINDER_THRESHOLD
                {
                    state.assistant_cycles_since_reminder = 0;
                }
                state.revision = state.revision.saturating_add(1);
                Ok((Some(state), ()))
            },
        )
        .map_err(|error| format!("ack reminder state: {error}"))
    }

    fn record_todo_write(&self) -> Result<(), String> {
        astrcode_extension_sdk::hostpaths::update_json_state(
            &self.root.join(REMINDER_STATE_FILE),
            |state: Option<ProgressReminderState>| {
                let mut state = state.unwrap_or_default();
                state.assistant_cycles_since_todo_write = 0;
                state.revision = state.revision.saturating_add(1);
                Ok((Some(state), ()))
            },
        )
        .map_err(|error| format!("update reminder state after todo write: {error}"))
    }

    fn load_state(&self) -> Result<ProgressReminderState, String> {
        let path = self.root.join(REMINDER_STATE_FILE);
        Ok(astrcode_extension_sdk::hostpaths::read_json_state(&path)
            .map_err(|error| format!("read reminder state: {error}"))?
            .unwrap_or_default())
    }

    #[cfg(test)]
    fn save_state(&self, state: &ProgressReminderState) -> Result<(), String> {
        astrcode_extension_sdk::hostpaths::write_json_state(
            &self.root.join(REMINDER_STATE_FILE),
            state,
        )
        .map_err(|error| format!("save reminder state: {error}"))
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

fn handle_todo_write(args: TodoWriteArgs, store: &ProgressListStore) -> Result<ToolResult, String> {
    let items = args
        .todos
        .into_iter()
        .map(TodoInputItem::into_progress_item)
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = store.replace(items)?;

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

impl TodoInputItem {
    fn into_progress_item(self) -> Result<ProgressItem, String> {
        let agent_type = self.agent_type.map(|value| value.trim().to_string());
        match self.executor {
            TodoExecutor::SubAgent => {
                let agent_type = agent_type
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "executor \"agent\" requires non-empty agentType".to_string())?;
                Ok(ProgressItem {
                    content: self.content,
                    active_form: self.active_form,
                    status: self.status,
                    executor: self.executor,
                    agent_type: Some(agent_type),
                    mode: self.mode,
                    metadata: BTreeMap::new(),
                })
            },
            TodoExecutor::MainAgent => {
                if agent_type.is_some() {
                    return Err("agentType is only allowed when executor is \"agent\"".to_string());
                }
                if self.mode.is_some() {
                    return Err("mode is only allowed when executor is \"agent\"".to_string());
                }
                Ok(ProgressItem {
                    content: self.content,
                    active_form: self.active_form,
                    status: self.status,
                    executor: self.executor,
                    agent_type: None,
                    mode: None,
                    metadata: BTreeMap::new(),
                })
            },
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
                                "description": "Imperative form."
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Present continuous form shown while the item is in_progress."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            },
                            "executor": {
                                "type": "string",
                                "enum": ["self", "agent"],
                                "description": "Who executes this step: `self` for the main agent directly, `agent` to delegate to a subagent."
                            },
                            "agentType": {
                                "type": "string",
                                "description": "Subagent type (e.g. explore, execute, reviewer). Required when executor is `agent`."
                            },
                            "mode": {
                                "type": "string",
                                "enum": ["serial", "parallel"],
                                "description": "For agent steps: `parallel` when independent, `serial` when dependent."
                            }
                        },
                        "required": ["content", "activeForm", "status", "executor"]
                    },
                    "description": "The full replacement progress todo list."
                }
            },
            "required": ["todos"]
        }),
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
        timeout_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::llm::{LlmContent, LlmMessage};

    use super::*;

    fn item(content: &str, active_form: &str, status: ProgressStatus) -> ProgressItem {
        ProgressItem {
            content: content.to_string(),
            active_form: active_form.to_string(),
            status,
            executor: TodoExecutor::MainAgent,
            agent_type: None,
            mode: None,
            metadata: BTreeMap::new(),
        }
    }

    fn input_item(
        executor: TodoExecutor,
        agent_type: Option<&str>,
        mode: Option<StepMode>,
    ) -> TodoInputItem {
        TodoInputItem {
            content: "Run tests".into(),
            active_form: "Running tests".into(),
            status: ProgressStatus::Pending,
            executor,
            agent_type: agent_type.map(String::from),
            mode,
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
            message
                .content
                .iter()
                .filter_map(LlmContent::as_text)
                .any(|text| text.contains(needle))
        })
    }

    #[test]
    fn todo_write_replaces_list_and_returns_metadata() {
        let store = test_store("replace");
        let first = handle_todo_write(
            serde_json::from_value(json!({
                "todos": [
                    {
                        "content": "Inspect files",
                        "activeForm": "Inspecting files",
                        "status": "in_progress",
                        "executor": "self"
                    }
                ]
            }))
            .expect("parse args"),
            &store,
        )
        .expect("write should succeed");
        assert!(first.metadata["oldTodos"].as_array().unwrap().is_empty());
        assert_eq!(first.metadata["newTodos"][0]["content"], "Inspect files");
        assert_eq!(first.metadata["newTodos"][0]["executor"], "self");

        let second = handle_todo_write(
            serde_json::from_value(json!({
                "todos": [
                    {
                        "content": "Run tests",
                        "activeForm": "Running tests",
                        "status": "pending",
                        "executor": "agent",
                        "agentType": "execute",
                        "mode": "serial"
                    }
                ]
            }))
            .expect("parse args"),
            &store,
        )
        .expect("replace should succeed");
        assert_eq!(second.metadata["oldTodos"][0]["content"], "Inspect files");
        assert_eq!(second.metadata["newTodos"][0]["content"], "Run tests");
        assert_eq!(second.metadata["newTodos"][0]["executor"], "agent");
        assert_eq!(second.metadata["newTodos"][0]["agentType"], "execute");
        assert_eq!(second.metadata["newTodos"][0]["mode"], "serial");
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
    fn rejects_agent_executor_without_agent_type() {
        assert_eq!(
            input_item(TodoExecutor::SubAgent, None, None)
                .into_progress_item()
                .expect_err("agent without agentType should fail"),
            "executor \"agent\" requires non-empty agentType"
        );

        let blank = input_item(TodoExecutor::SubAgent, Some("   "), None)
            .into_progress_item()
            .expect_err("blank agentType should fail");
        assert!(blank.contains("agentType"));
    }

    #[test]
    fn rejects_self_executor_with_agent_type_or_mode() {
        assert_eq!(
            input_item(TodoExecutor::MainAgent, Some("explore"), None)
                .into_progress_item()
                .expect_err("self with agentType should fail"),
            "agentType is only allowed when executor is \"agent\""
        );
        assert_eq!(
            input_item(TodoExecutor::MainAgent, None, Some(StepMode::Parallel))
                .into_progress_item()
                .expect_err("self with mode should fail"),
            "mode is only allowed when executor is \"agent\""
        );
    }

    #[test]
    fn accepts_agent_executor_with_agent_type() {
        let item = input_item(
            TodoExecutor::SubAgent,
            Some(" reviewer "),
            Some(StepMode::Parallel),
        )
        .into_progress_item()
        .expect("agent with agentType should succeed");
        assert_eq!(item.executor, TodoExecutor::SubAgent);
        assert_eq!(item.agent_type.as_deref(), Some("reviewer"));
        assert_eq!(item.mode, Some(StepMode::Parallel));
    }

    #[test]
    fn rejects_todo_without_executor_field() {
        let error = serde_json::from_value::<TodoWriteArgs>(json!({
            "todos": [
                {
                    "content": "Inspect files",
                    "activeForm": "Inspecting files",
                    "status": "in_progress"
                }
            ]
        }))
        .expect_err("missing executor must fail");
        assert!(error.to_string().contains("executor"));
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
    fn provider_cycle_is_retryable_and_acknowledges_only_its_exact_revision() {
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
                revision: 7,
                assistant_cycles_since_todo_write: REMINDER_THRESHOLD - 1,
                assistant_cycles_since_reminder: REMINDER_THRESHOLD - 1,
            })
            .unwrap();

        let first = reminder.prepare_provider_request().unwrap();
        let retry = reminder.prepare_provider_request().unwrap();
        let (first_id, effect) = first.into_parts();
        let (retry_id, _) = retry.into_parts();
        assert_eq!(retry_id, first_id, "failure must leave the cycle retryable");
        assert_eq!(
            reminder.load_state().unwrap().revision,
            7,
            "prepare must not commit counters"
        );

        let messages = match effect {
            PreparedProviderEffect::AppendMessages(messages) => messages,
            _ => panic!("stale todo list should inject a provider reminder"),
        };
        assert!(text_exists(
            &messages,
            "The todoWrite tool has not been used recently"
        ));
        assert!(text_exists(&messages, "Replace task tools"));

        reminder.record_todo_write().unwrap();
        let after_write = reminder.load_state().unwrap();
        assert_eq!(after_write.revision, 8);
        assert_eq!(after_write.assistant_cycles_since_todo_write, 0);
        reminder
            .acknowledge_provider_cycle(first_id.as_str())
            .unwrap();
        assert_eq!(
            reminder.load_state().unwrap(),
            after_write,
            "an old ack must not clear or advance state updated in flight"
        );

        let current = reminder.prepare_provider_request().unwrap();
        let (current_id, _) = current.into_parts();
        reminder
            .acknowledge_provider_cycle(current_id.as_str())
            .unwrap();
        let settled = reminder.load_state().unwrap();
        assert_eq!(settled.revision, 9);
        assert_eq!(settled.assistant_cycles_since_todo_write, 1);
    }

    #[test]
    fn before_provider_request_skips_empty_todo_reminder() {
        let root = reminder_root("empty");
        let reminder = ProgressReminder::new(root);
        reminder
            .save_state(&ProgressReminderState {
                revision: 0,
                assistant_cycles_since_todo_write: REMINDER_THRESHOLD - 1,
                assistant_cycles_since_reminder: REMINDER_THRESHOLD - 1,
            })
            .unwrap();

        let (_, effect) = reminder.prepare_provider_request().unwrap().into_parts();

        assert!(matches!(effect, PreparedProviderEffect::Unchanged));
    }

    #[test]
    fn tool_contract_forces_executor_decision() {
        let definition = todo_write_tool_definition();
        assert!(definition.description.contains("`executor`"));
        assert!(definition.description.contains("`agentType`"));
        assert!(definition.description.contains("Default to `self`"));
        let properties = &definition.parameters["properties"]["todos"]["items"]["properties"];
        assert_eq!(properties["executor"]["enum"], json!(["self", "agent"]));
        assert_eq!(properties["mode"]["enum"], json!(["serial", "parallel"]));
        let required = definition.parameters["properties"]["todos"]["items"]["required"]
            .as_array()
            .unwrap();
        assert!(required.contains(&json!("executor")));
        assert_eq!(
            properties["content"]["description"].as_str().unwrap(),
            "Imperative form."
        );
    }
}

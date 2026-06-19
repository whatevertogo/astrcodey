//! astrcode-extension-goal — Codex-style session goal tracking.
//!
//! State is stored at `<session>/extension_data/astrcode-goal/goal/goal-state.json`.

mod store;

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use astrcode_extension_sdk::{
    event::EventPayload,
    extension::{
        CommandContext, CommandHandler, ContinueAfterStopContext, ContinueAfterStopHandler,
        ContinueAfterStopOptions, ContinueAfterStopResult, Extension, ExtensionCapability,
        ExtensionCommandResult, ExtensionCtx, ExtensionError, HookMode, ProviderContext,
        ProviderEvent, ProviderHandler, ProviderResult, Registrar, SlashCommand, ToolHandler,
    },
    llm::LlmMessage,
    state,
    storage::EventReader,
    tool::{
        ExecutionMode, ToolDefinition, ToolOrigin, ToolPromptMetadata, ToolResult, tool_metadata,
    },
    types::SessionId,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::store::{GoalState, GoalStatus, GoalStore, GoalUpdateStatus, goal_dir_from_base};

const EXTENSION_ID: &str = "astrcode-goal";
const GET_GOAL_TOOL_NAME: &str = "getGoal";
const CREATE_GOAL_TOOL_NAME: &str = "createGoal";
const UPDATE_GOAL_TOOL_NAME: &str = "updateGoal";

const CAPABILITIES: &[ExtensionCapability] = &[ExtensionCapability::SessionHistory];

const CREATE_GOAL_DESCRIPTION: &str =
    "Create a session goal for multi-turn autonomous work. Use this only when the user asks for a \
     concrete objective that may require continued work across multiple model steps. A new goal \
     can be created only when no unfinished goal exists. The optional tokenBudget limits \
     automatic goal continuation using session token usage recorded by the host.";

const GET_GOAL_DESCRIPTION: &str = "Return the current session goal, status, elapsed time, token \
                                    usage, and remaining token budget. Use before deciding \
                                    whether an existing goal is still active or blocked.";

const UPDATE_GOAL_DESCRIPTION: &str =
    "Mark the current goal complete or blocked. Use complete only when the objective is genuinely \
     achieved. Use blocked only when the same blocking condition has repeated and no useful \
     progress can be made without user input or an external change.";

/// Return the bundled goal extension.
pub fn extension() -> Arc<dyn Extension> {
    Arc::new(GoalExtension::default())
}

#[derive(Default)]
struct GoalRuntime {
    session_read: RwLock<Option<Arc<dyn EventReader>>>,
}

impl GoalRuntime {
    fn set_session_read(&self, reader: Option<Arc<dyn EventReader>>) {
        *self.session_read.write() = reader;
    }

    fn session_read(&self) -> Option<Arc<dyn EventReader>> {
        self.session_read.read().clone()
    }

    async fn session_store_dir(&self, session_id: &str) -> Result<Option<PathBuf>, String> {
        let Some(reader) = self.session_read() else {
            return Ok(None);
        };
        reader
            .session_store_dir(&SessionId::from(session_id))
            .await
            .map_err(|error| format!("read session store dir: {error}"))
    }

    async fn total_token_usage(
        &self,
        session_id: &str,
    ) -> Result<Option<TokenUsageSnapshot>, String> {
        let Some(reader) = self.session_read() else {
            return Ok(None);
        };
        let events = reader
            .replay_events(&SessionId::from(session_id))
            .await
            .map_err(|error| format!("replay session events: {error}"))?;

        let mut total_tokens = 0u64;
        let mut saw_usage = false;
        let mut model_context_window = None;
        for event in events {
            if let EventPayload::TokenUsageRecorded {
                usage,
                model_context_window: window,
            } = event.payload
            {
                if let Some(tokens) = token_total(&usage) {
                    total_tokens = total_tokens.saturating_add(tokens);
                    saw_usage = true;
                }
                model_context_window = Some(window);
            }
        }

        Ok(saw_usage.then_some(TokenUsageSnapshot {
            total_tokens,
            model_context_window,
        }))
    }

    async fn usage_for_goal(&self, session_id: &str, goal: &GoalState) -> GoalUsage {
        let snapshot = self.total_token_usage(session_id).await.ok().flatten();
        let tokens_used = match (snapshot.as_ref(), goal.token_usage_baseline) {
            (Some(snapshot), Some(baseline)) => {
                Some(snapshot.total_tokens.saturating_sub(baseline))
            },
            _ => None,
        };
        let remaining_tokens = match (goal.token_budget, tokens_used) {
            (Some(budget), Some(used)) => Some(budget.saturating_sub(used)),
            _ => None,
        };

        GoalUsage {
            tokens_used,
            token_budget: goal.token_budget,
            remaining_tokens,
            model_context_window: snapshot.and_then(|snapshot| snapshot.model_context_window),
            elapsed_seconds: goal.elapsed_seconds(),
        }
    }
}

#[derive(Default)]
struct GoalExtension {
    runtime: Arc<GoalRuntime>,
}

#[async_trait::async_trait]
impl Extension for GoalExtension {
    fn id(&self) -> &str {
        EXTENSION_ID
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        CAPABILITIES
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        self.runtime.set_session_read(
            ctx.host_services()
                .and_then(|services| services.session_read.clone()),
        );
        Ok(())
    }

    fn register(&self, reg: &mut Registrar) {
        let runtime = Arc::clone(&self.runtime);
        reg.tool(
            get_goal_tool_definition(),
            Arc::new(GoalToolHandler {
                runtime: Arc::clone(&runtime),
            }),
        );
        reg.tool(
            create_goal_tool_definition(),
            Arc::new(GoalToolHandler {
                runtime: Arc::clone(&runtime),
            }),
        );
        reg.tool(
            update_goal_tool_definition(),
            Arc::new(GoalToolHandler {
                runtime: Arc::clone(&runtime),
            }),
        );
        reg.tool_metadata(goal_tool_metadata());
        reg.on_provider(
            ProviderEvent::BeforeRequest,
            HookMode::Blocking,
            40,
            Arc::new(GoalProviderHandler {
                runtime: Arc::clone(&runtime),
            }),
        );
        reg.on_continue_after_stop(
            40,
            ContinueAfterStopOptions::unlimited(),
            Arc::new(GoalContinueAfterStopHandler {
                runtime: Arc::clone(&runtime),
            }),
        );
        reg.command(
            SlashCommand {
                name: "goal".into(),
                description: "Show or manage the current session goal.".into(),
                args_schema: None,
            },
            Arc::new(GoalSlashCommandHandler { runtime }),
        );
    }
}

struct GoalToolHandler {
    runtime: Arc<GoalRuntime>,
}

#[async_trait::async_trait]
impl ToolHandler for GoalToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        _working_dir: &str,
        ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let root = ctx
            .capabilities
            .paths
            .store_dir
            .as_deref()
            .map(goal_root_from_session_base)
            .ok_or_else(|| ExtensionError::Internal("session_store_dir not injected".into()))?;
        let store = GoalStore::new(root);

        Ok(match tool_name {
            GET_GOAL_TOOL_NAME => {
                handle_get_goal(&store, &self.runtime, ctx.scope.session_id.as_str()).await
            },
            CREATE_GOAL_TOOL_NAME => {
                handle_create_goal(
                    &store,
                    &self.runtime,
                    ctx.scope.session_id.as_str(),
                    arguments,
                )
                .await
            },
            UPDATE_GOAL_TOOL_NAME => {
                handle_update_goal(
                    &store,
                    &self.runtime,
                    ctx.scope.session_id.as_str(),
                    arguments,
                )
                .await
            },
            _ => return Err(ExtensionError::NotFound(tool_name.into())),
        })
    }
}

struct GoalProviderHandler {
    runtime: Arc<GoalRuntime>,
}

#[async_trait::async_trait]
impl ProviderHandler for GoalProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let root = ctx
            .session_store_dir
            .as_deref()
            .map(goal_root_from_session_base)
            .ok_or_else(|| ExtensionError::Internal("session_store_dir not injected".into()))?;
        let store = GoalStore::new(root);
        let Some(mut goal) = store.load().map_err(ExtensionError::Internal)? else {
            return Ok(ProviderResult::Allow);
        };

        if !goal.status.can_auto_continue() {
            return Ok(ProviderResult::Allow);
        }

        let usage = self.runtime.usage_for_goal(&ctx.session_id, &goal).await;
        if apply_budget_limit(&mut goal, &usage) {
            store.save(&goal).map_err(ExtensionError::Internal)?;
            return Ok(ProviderResult::AppendMessages {
                messages: vec![LlmMessage::user(budget_limited_message(&goal, &usage))],
            });
        }

        let continuation = goal.take_continuation_prompt_pending();
        store.save(&goal).map_err(ExtensionError::Internal)?;
        Ok(ProviderResult::AppendMessages {
            messages: vec![LlmMessage::user(goal_context_message(
                &goal,
                &usage,
                continuation,
            ))],
        })
    }
}

struct GoalContinueAfterStopHandler {
    runtime: Arc<GoalRuntime>,
}

#[async_trait::async_trait]
impl ContinueAfterStopHandler for GoalContinueAfterStopHandler {
    async fn handle(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        let Some(session_store_dir) = self
            .runtime
            .session_store_dir(&ctx.session_id)
            .await
            .map_err(ExtensionError::Internal)?
        else {
            return Ok(ContinueAfterStopResult::EndTurn);
        };
        let store = GoalStore::new(goal_root_from_session_base(&session_store_dir));
        let Some(mut goal) = store.load().map_err(ExtensionError::Internal)? else {
            return Ok(ContinueAfterStopResult::EndTurn);
        };
        if !goal.status.can_auto_continue() {
            return Ok(ContinueAfterStopResult::EndTurn);
        }

        let usage = self.runtime.usage_for_goal(&ctx.session_id, &goal).await;
        if apply_budget_limit(&mut goal, &usage) {
            store.save(&goal).map_err(ExtensionError::Internal)?;
            return Ok(ContinueAfterStopResult::EndTurn);
        }

        goal.mark_continuation_pending();
        store.save(&goal).map_err(ExtensionError::Internal)?;
        Ok(ContinueAfterStopResult::ContinueOneStep)
    }
}

struct GoalSlashCommandHandler {
    runtime: Arc<GoalRuntime>,
}

#[async_trait::async_trait]
impl CommandHandler for GoalSlashCommandHandler {
    async fn execute(
        &self,
        _command_name: &str,
        arguments: &str,
        _working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let root = ctx
            .session_store_dir
            .as_deref()
            .map(goal_root_from_session_base)
            .ok_or_else(|| ExtensionError::Internal("session_store_dir not injected".into()))?;
        let store = GoalStore::new(root);
        let args = arguments.trim();

        match args {
            "" | "show" => {
                let content = goal_report_text(
                    &build_goal_report(&store, &self.runtime, &ctx.session_id).await,
                );
                Ok(ExtensionCommandResult::display(content, false))
            },
            "clear" => {
                store.clear().map_err(ExtensionError::Internal)?;
                Ok(ExtensionCommandResult::display("Goal cleared", false))
            },
            "pause" => match store.pause() {
                Ok(goal) => Ok(ExtensionCommandResult::display(
                    format!("Goal paused: {}", goal.objective),
                    false,
                )),
                Err(error) => Ok(ExtensionCommandResult::display(error, true)),
            },
            "resume" => match store.resume() {
                Ok(goal) => Ok(ExtensionCommandResult::start_turn(format!(
                    "Resume working toward this active goal: {}",
                    goal.objective
                ))),
                Err(error) => Ok(ExtensionCommandResult::display(error, true)),
            },
            "complete" => match store.update_status(GoalUpdateStatus::Complete) {
                Ok(goal) => Ok(ExtensionCommandResult::display(
                    format!("Goal marked complete: {}", goal.objective),
                    false,
                )),
                Err(error) => Ok(ExtensionCommandResult::display(error, true)),
            },
            "blocked" => match store.update_status(GoalUpdateStatus::Blocked) {
                Ok(goal) => Ok(ExtensionCommandResult::display(
                    format!("Goal marked blocked: {}", goal.objective),
                    false,
                )),
                Err(error) => Ok(ExtensionCommandResult::display(error, true)),
            },
            objective => {
                let baseline = self
                    .runtime
                    .total_token_usage(&ctx.session_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|snapshot| snapshot.total_tokens);
                match store.create(objective.to_string(), None, baseline) {
                    Ok(goal) => Ok(ExtensionCommandResult::start_turn(format!(
                        "Work toward this new active goal: {}",
                        goal.objective
                    ))),
                    Err(error) => Ok(ExtensionCommandResult::display(error, true)),
                }
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateGoalArgs {
    objective: String,
    #[serde(default)]
    token_budget: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateGoalArgs {
    status: GoalUpdateStatus,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct GoalUsage {
    tokens_used: Option<u64>,
    token_budget: Option<u64>,
    remaining_tokens: Option<u64>,
    model_context_window: Option<usize>,
    elapsed_seconds: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct GoalReport {
    goal: Option<GoalState>,
    usage: Option<GoalUsage>,
    automation: GoalAutomation,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct GoalAutomation {
    automatic_continuation_enabled: bool,
    continuation_count: u64,
}

struct TokenUsageSnapshot {
    total_tokens: u64,
    model_context_window: Option<usize>,
}

async fn handle_get_goal(store: &GoalStore, runtime: &GoalRuntime, session_id: &str) -> ToolResult {
    let report = build_goal_report(store, runtime, session_id).await;
    ToolResult::text(
        goal_report_text(&report),
        false,
        tool_metadata([("goalReport", json!(report))]),
    )
}

async fn handle_create_goal(
    store: &GoalStore,
    runtime: &GoalRuntime,
    session_id: &str,
    arguments: Value,
) -> ToolResult {
    let args = match serde_json::from_value::<CreateGoalArgs>(arguments) {
        Ok(args) => args,
        Err(error) => {
            let message = format!("invalid args for {CREATE_GOAL_TOOL_NAME}: {error}");
            return ToolResult::text(
                message.clone(),
                true,
                tool_metadata([("error", json!(message))]),
            );
        },
    };
    let baseline = runtime
        .total_token_usage(session_id)
        .await
        .ok()
        .flatten()
        .map(|snapshot| snapshot.total_tokens);

    match store.create(args.objective, args.token_budget, baseline) {
        Ok(goal) => {
            let usage = runtime.usage_for_goal(session_id, &goal).await;
            let report = GoalReport {
                automation: automation_for_goal(Some(&goal)),
                goal: Some(goal.clone()),
                usage: Some(usage),
            };
            ToolResult::text(
                format!(
                    "Goal created: {}\n\nContinue working toward this objective. Call \
                     {UPDATE_GOAL_TOOL_NAME} with status complete when it is fully achieved, or \
                     blocked when progress is genuinely blocked.",
                    goal.objective
                ),
                false,
                tool_metadata([("goalReport", json!(report))]),
            )
        },
        Err(error) => ToolResult::text(
            error.clone(),
            true,
            tool_metadata([("error", json!(error))]),
        ),
    }
}

async fn handle_update_goal(
    store: &GoalStore,
    runtime: &GoalRuntime,
    session_id: &str,
    arguments: Value,
) -> ToolResult {
    let args = match serde_json::from_value::<UpdateGoalArgs>(arguments) {
        Ok(args) => args,
        Err(error) => {
            let message = format!("invalid args for {UPDATE_GOAL_TOOL_NAME}: {error}");
            return ToolResult::text(
                message.clone(),
                true,
                tool_metadata([("error", json!(message))]),
            );
        },
    };

    match store.update_status(args.status) {
        Ok(goal) => {
            let usage = runtime.usage_for_goal(session_id, &goal).await;
            let report = GoalReport {
                automation: automation_for_goal(Some(&goal)),
                goal: Some(goal.clone()),
                usage: Some(usage),
            };
            ToolResult::text(
                format!(
                    "Goal status updated to {}: {}",
                    goal.status.label(),
                    goal.objective
                ),
                false,
                tool_metadata([("goalReport", json!(report))]),
            )
        },
        Err(error) => ToolResult::text(
            error.clone(),
            true,
            tool_metadata([("error", json!(error))]),
        ),
    }
}

async fn build_goal_report(
    store: &GoalStore,
    runtime: &GoalRuntime,
    session_id: &str,
) -> GoalReport {
    match store.load() {
        Ok(Some(goal)) => {
            let usage = runtime.usage_for_goal(session_id, &goal).await;
            GoalReport {
                automation: automation_for_goal(Some(&goal)),
                goal: Some(goal),
                usage: Some(usage),
            }
        },
        _ => GoalReport {
            goal: None,
            usage: None,
            automation: automation_for_goal(None),
        },
    }
}

fn automation_for_goal(goal: Option<&GoalState>) -> GoalAutomation {
    GoalAutomation {
        automatic_continuation_enabled: goal.is_some_and(|goal| goal.status.can_auto_continue()),
        continuation_count: goal.map_or(0, |goal| goal.continuation_count),
    }
}

fn goal_report_text(report: &GoalReport) -> String {
    let Some(goal) = &report.goal else {
        return "No goal exists for this session.".to_string();
    };
    let mut lines = vec![
        format!("Goal: {}", goal.objective),
        format!("Status: {}", goal.status.label()),
        format!("Elapsed: {}s", goal.elapsed_seconds()),
    ];

    if let Some(usage) = &report.usage {
        match (
            usage.tokens_used,
            usage.token_budget,
            usage.remaining_tokens,
        ) {
            (Some(used), Some(budget), Some(remaining)) => {
                lines.push(format!("Tokens: {used}/{budget} ({remaining} remaining)"));
            },
            (Some(used), None, _) => {
                lines.push(format!("Tokens: {used}"));
            },
            _ => {
                if goal.token_budget.is_some() {
                    lines.push("Tokens: unavailable".to_string());
                }
            },
        }
        if let Some(window) = usage.model_context_window {
            lines.push(format!("Model context window: {window}"));
        }
    }

    lines.push(format!(
        "Automatic continuation: {}",
        if report.automation.automatic_continuation_enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
    lines.join("\n")
}

fn apply_budget_limit(goal: &mut GoalState, usage: &GoalUsage) -> bool {
    if goal.status != GoalStatus::Active {
        return false;
    }
    let (Some(budget), Some(used)) = (goal.token_budget, usage.tokens_used) else {
        return false;
    };
    if used < budget {
        return false;
    }
    goal.set_status(GoalStatus::BudgetLimited);
    true
}

fn goal_context_message(goal: &GoalState, usage: &GoalUsage, continuation: bool) -> String {
    let mut lines = vec![
        "Goal context for this request. Do not mention this hidden context unless it is directly \
         relevant to the user's task."
            .to_string(),
        format!("Objective: {}", goal.objective),
        format!("Status: {}", goal.status.label()),
    ];
    if let (Some(used), Some(budget), Some(remaining)) = (
        usage.tokens_used,
        usage.token_budget,
        usage.remaining_tokens,
    ) {
        lines.push(format!(
            "Goal token budget: {used}/{budget} tokens used, {remaining} remaining."
        ));
    } else if let Some(budget) = goal.token_budget {
        lines.push(format!(
            "Goal token budget: {budget} tokens. Current usage is unavailable."
        ));
    }
    if continuation {
        lines.push(
            "This is an automatic continuation step requested by the goal plugin.".to_string(),
        );
    }
    lines.push(format!(
        "Continue making concrete progress toward the objective. Call {UPDATE_GOAL_TOOL_NAME} \
         with status complete before the final response once the objective is fully achieved. \
         Call {UPDATE_GOAL_TOOL_NAME} with status blocked only when useful progress is impossible \
         without user input or an external state change."
    ));
    lines.join("\n")
}

fn budget_limited_message(goal: &GoalState, usage: &GoalUsage) -> String {
    let token_line = match (usage.tokens_used, goal.token_budget) {
        (Some(used), Some(budget)) => format!("{used}/{budget} goal tokens have been used."),
        _ => "The goal token budget has been reached.".to_string(),
    };
    format!(
        "Goal automation is now budget_limited for objective: {}\n{token_line}\nDo not request \
         more automatic continuation for this goal. Summarize the current state concisely if the \
         user needs a response.",
        goal.objective
    )
}

fn token_total(usage: &astrcode_extension_sdk::llm::LlmTokenUsage) -> Option<u64> {
    if let Some(total) = usage.total_tokens {
        return Some(total);
    }
    let parts = [
        usage.input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
    ];
    parts
        .iter()
        .copied()
        .flatten()
        .reduce(|acc, value| acc.saturating_add(value))
}

fn goal_root_from_session_base(session_base: &std::path::Path) -> PathBuf {
    goal_dir_from_base(&state::session_data_dir(session_base, EXTENSION_ID))
}

fn goal_tool_metadata() -> HashMap<String, ToolPromptMetadata> {
    let mut map = HashMap::new();
    let planning = ToolPromptMetadata::new(String::new())
        .prompt_tag(astrcode_extension_sdk::tool::ToolPromptTag::Planning);
    map.insert(GET_GOAL_TOOL_NAME.to_string(), planning.clone());
    map.insert(CREATE_GOAL_TOOL_NAME.to_string(), planning.clone());
    map.insert(UPDATE_GOAL_TOOL_NAME.to_string(), planning);
    map
}

fn get_goal_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: GET_GOAL_TOOL_NAME.into(),
        description: GET_GOAL_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
            "required": []
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

fn create_goal_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CREATE_GOAL_TOOL_NAME.into(),
        description: CREATE_GOAL_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Concrete objective this session should continue pursuing."
                },
                "tokenBudget": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum tokens to spend on automatic goal continuation."
                }
            },
            "required": ["objective"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

fn update_goal_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: UPDATE_GOAL_TOOL_NAME.into(),
        description: UPDATE_GOAL_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked"],
                    "description": "Terminal status for the current goal."
                }
            },
            "required": ["status"]
        }),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(objective: &str) -> GoalState {
        GoalState::new(objective.to_string(), Some(100), Some(10))
    }

    #[test]
    fn budget_limit_marks_active_goal_budget_limited() {
        let mut goal = goal("Finish work");
        let usage = GoalUsage {
            tokens_used: Some(100),
            token_budget: Some(100),
            remaining_tokens: Some(0),
            model_context_window: Some(8192),
            elapsed_seconds: 1,
        };

        assert!(apply_budget_limit(&mut goal, &usage));
        assert_eq!(goal.status, GoalStatus::BudgetLimited);
        assert!(!goal.continuation_prompt_pending);
    }

    #[test]
    fn budget_limit_ignores_unavailable_usage() {
        let mut goal = goal("Finish work");
        let usage = GoalUsage {
            tokens_used: None,
            token_budget: Some(100),
            remaining_tokens: None,
            model_context_window: None,
            elapsed_seconds: 1,
        };

        assert!(!apply_budget_limit(&mut goal, &usage));
        assert_eq!(goal.status, GoalStatus::Active);
    }

    #[test]
    fn context_message_marks_automatic_continuation() {
        let goal = goal("Finish work");
        let usage = GoalUsage {
            tokens_used: Some(25),
            token_budget: Some(100),
            remaining_tokens: Some(75),
            model_context_window: None,
            elapsed_seconds: 1,
        };

        let message = goal_context_message(&goal, &usage, true);

        assert!(message.contains("automatic continuation step"));
        assert!(message.contains("25/100 tokens used"));
        assert!(message.contains(UPDATE_GOAL_TOOL_NAME));
    }

    #[test]
    fn token_total_falls_back_to_parts() {
        let usage = astrcode_extension_sdk::llm::LlmTokenUsage {
            input_tokens: Some(10),
            cached_input_tokens: Some(5),
            output_tokens: Some(7),
            reasoning_output_tokens: Some(3),
            total_tokens: None,
        };

        assert_eq!(token_total(&usage), Some(20));
    }
}

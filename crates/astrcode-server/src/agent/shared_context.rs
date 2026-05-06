//! Shared turn context — session-level identifiers, signal types, and error
//! types shared across agent sub-objects.

use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::ExtensionEvent,
    llm::LlmRole,
    tool::{ToolDefinition, ToolResult},
    types::*,
};
use astrcode_extensions::{context::ServerExtensionContext, runner::ExtensionRunner};
use tokio::sync::{mpsc, oneshot};

// ─── Constants ───────────────────────────────────────────────────────────

pub(crate) const MCP_TOOL_PREFIX: &str = "mcp__";
pub(crate) const TOOL_SEARCH_TOOL_NAME: &str = "tool_search_tool";
pub(crate) const TOOL_SEARCH_METADATA_KEY: &str = "toolSearch";

// ─── Signal ──────────────────────────────────────────────────────────────

pub(crate) enum AgentSignal {
    Event(EventPayload),
    AutoCompact {
        trigger: astrcode_core::extension::CompactTrigger,
        compaction: astrcode_context::compaction::CompactResult,
        reply: oneshot::Sender<Result<SessionId, String>>,
    },
    #[allow(dead_code)]
    BackgroundTaskCompleted {
        session_id: SessionId,
        task_id: BackgroundTaskId,
        call_id: ToolCallId,
        tool_name: String,
        result: ToolResult,
    },
}

pub(crate) fn send_event(
    event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    payload: EventPayload,
) {
    if let Some(tx) = event_tx {
        let _ = tx.send(AgentSignal::Event(payload));
    }
}

/// Emit `TurnEnd` before returning an error, preventing extensions from
/// seeing an unfinished turn.
pub(super) async fn end_turn_with_error<T, E>(
    extension_runner: &ExtensionRunner,
    ext_ctx: &ServerExtensionContext,
    error: E,
) -> Result<T, AgentError>
where
    E: Into<AgentError>,
{
    let _ = extension_runner
        .dispatch(ExtensionEvent::TurnEnd, ext_ctx)
        .await;
    Err(error.into())
}

// ─── SharedTurnContext ───────────────────────────────────────────────────

/// Session-level identifiers shared across all agent sub-objects.
#[derive(Clone)]
pub(super) struct SharedTurnContext {
    pub(super) session_id: SessionId,
    pub(super) working_dir: String,
    pub(super) model_id: String,
}

impl SharedTurnContext {
    pub(super) fn new(session_id: SessionId, working_dir: String, model_id: String) -> Self {
        Self {
            session_id,
            working_dir,
            model_id,
        }
    }

    pub(super) fn ext_ctx(&self) -> ServerExtensionContext {
        ServerExtensionContext::new(
            self.session_id.to_string(),
            self.working_dir.clone(),
            ModelSelection::simple(self.model_id.clone()),
        )
    }

    pub(super) fn ext_ctx_with_tools(&self, tools: &[ToolDefinition]) -> ServerExtensionContext {
        let mut ctx = self.ext_ctx();
        ctx.set_tools(
            tools
                .iter()
                .cloned()
                .map(|tool| (tool.name.clone(), tool))
                .collect(),
        );
        ctx
    }
}

/// Computes the retained messages by stripping the compact context prefix
/// and filtering out system messages.
pub(super) fn retained_messages_after_compaction(
    messages: &[astrcode_core::llm::LlmMessage],
    context_messages: &[astrcode_core::llm::LlmMessage],
) -> Vec<astrcode_core::llm::LlmMessage> {
    let without_session_prompt = if matches!(
        messages.first(),
        Some(message) if message.role == LlmRole::System
    ) {
        &messages[1..]
    } else {
        messages
    };
    without_session_prompt
        .strip_prefix(context_messages)
        .unwrap_or(without_session_prompt)
        .iter()
        .filter(|message| message.role != LlmRole::System)
        .cloned()
        .collect()
}

// ─── AgentError ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Tool error: {0}")]
    Tool(#[from] astrcode_core::tool::ToolError),
    #[error("Extension error: {0}")]
    Extension(#[from] astrcode_core::extension::ExtensionError),
}

impl From<astrcode_core::llm::LlmError> for AgentError {
    fn from(e: astrcode_core::llm::LlmError) -> Self {
        AgentError::Llm(e.to_string())
    }
}

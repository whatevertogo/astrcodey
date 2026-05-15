//! Shared turn context — session-level identifiers, signal types, and error
//! types shared across agent sub-objects.

use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionEvent, LifecycleContext},
    types::*,
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::sync::{mpsc, oneshot};

// ─── Signal ──────────────────────────────────────────────────────────────

pub enum AgentSignal {
    Event(EventPayload),
    AutoCompact {
        trigger: astrcode_core::extension::CompactTrigger,
        compaction: astrcode_context::compaction::CompactResult,
        reply: oneshot::Sender<Result<SessionId, String>>,
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
pub async fn end_turn_with_error_typed<T, E>(
    extension_runner: &ExtensionRunner,
    shared: &SharedTurnContext,
    error: E,
) -> Result<T, TurnError>
where
    E: Into<TurnError>,
{
    let ctx = LifecycleContext {
        session_id: shared.session_id.to_string(),
        working_dir: shared.working_dir.clone(),
        model: ModelSelection::simple(shared.model_id.clone()),
    };
    let _ = extension_runner
        .emit_lifecycle(ExtensionEvent::TurnEnd, ctx)
        .await;
    Err(error.into())
}

// ─── SharedTurnContext ───────────────────────────────────────────────────

/// Session-level identifiers shared across all agent sub-objects.
#[derive(Clone)]
pub struct SharedTurnContext {
    pub session_id: SessionId,
    pub working_dir: String,
    pub model_id: String,
    // TODO: 恢复 event_bus、session_history、system_prompt 等能力，
    // 旧 ExtensionContext trait 已在类型化注册迁移中移除，
    // 后续需要通过独立的机制（如 SharedState struct）重新暴露给 handler。
}

impl SharedTurnContext {
    pub fn new(session_id: SessionId, working_dir: String, model_id: String) -> Self {
        Self {
            session_id,
            working_dir,
            model_id,
        }
    }
}

// ─── TurnError ───────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("{0}")]
    Llm(#[from] astrcode_core::llm::LlmError),
    #[error("Tool error: {0}")]
    Tool(#[from] astrcode_core::tool::ToolError),
    #[error("Extension error: {0}")]
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("{0}")]
    Internal(String),
}

//! Turn 基础设施 — 事件总线、信号类型、共享上下文、错误类型。

use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionEvent, LifecycleContext},
    types::*,
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::sync::mpsc;

// ─── EventBus ───────────────────────────────────────────────────────────

/// 事件发射目标。
///
/// TurnRunner 每产生一个事件就调 `emit()`。
/// 实现方负责持久化和/或广播。
#[async_trait::async_trait]
pub trait EventBus: Send + Sync {
    /// 发射一个事件。实现应同时处理持久化和客户端广播。
    ///
    /// `turn_id = None` 表示会话级事件（不属于任何 turn），
    /// `turn_id = Some(..)` 表示 turn 级事件。
    /// Option 只应出现在这个边界：上层调用方（run_turn / drive_agent）
    /// 直接接收 `&TurnId`，由它们负责传 `Some(turn_id)` 进来。
    async fn emit(&self, session_id: &SessionId, turn_id: Option<&TurnId>, payload: EventPayload);
}

/// 丢弃所有事件的空实现，用于测试。
pub struct NoopEventBus;

#[async_trait::async_trait]
impl EventBus for NoopEventBus {
    async fn emit(
        &self,
        _session_id: &SessionId,
        _turn_id: Option<&TurnId>,
        _payload: EventPayload,
    ) {
    }
}

// ─── Signal ──────────────────────────────────────────────────────────────

pub enum AgentSignal {
    Event(EventPayload),
}

pub(crate) fn send_event(
    event_tx: Option<&mpsc::UnboundedSender<AgentSignal>>,
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

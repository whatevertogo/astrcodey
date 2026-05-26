//! Turn 基础设施 — 事件通道、共享上下文、错误类型。

use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExchangeSummary, ExtensionEvent, LifecycleContext, ProviderContext},
    llm::LlmMessage,
    storage::SessionReadModel,
    types::*,
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::sync::mpsc;

// ─── Turn event channel ──────────────────────────────────────────────────

/// Turn 内 live 事件发送端；新代码优先
/// [`TurnPublisher::live`](crate::turn_publish::TurnPublisher::live)。
pub type TurnEventTx = mpsc::UnboundedSender<EventPayload>;

pub(crate) fn send_event(event_tx: Option<&TurnEventTx>, payload: EventPayload) {
    if let Some(tx) = event_tx {
        let _ = tx.send(payload);
    }
}

/// StepEnd 生命周期钩子：失败只记录 warn，不中断 turn。
pub(crate) async fn on_step_end_best_effort(
    extension_runner: &ExtensionRunner,
    ctx: &LifecycleContext,
) {
    if let Err(error) = extension_runner
        .emit_lifecycle(ExtensionEvent::StepEnd, ctx.clone())
        .await
    {
        tracing::warn!(error = %error, "StepEnd lifecycle hook failed (best-effort)");
    }
}

/// Emit `TurnEnd` before returning an error, preventing extensions from
/// seeing an unfinished turn.
pub(crate) async fn end_turn_with_error_typed<T, E>(
    extension_runner: &ExtensionRunner,
    shared: &SharedTurnContext,
    error: E,
) -> Result<T, TurnError>
where
    E: Into<TurnError>,
{
    let _ = extension_runner
        .emit_lifecycle(ExtensionEvent::TurnEnd, shared.lifecycle_ctx())
        .await;
    Err(error.into())
}

// ─── SharedTurnContext ───────────────────────────────────────────────────

/// Session-level identifiers shared across all agent sub-objects.
///
/// 提供 `lifecycle_ctx` / `provider_ctx` 工厂方法，避免散落在 hook 调用点
/// 重复构造 LifecycleContext / ProviderContext。
#[derive(Clone)]
pub(crate) struct SharedTurnContext {
    pub(crate) session_id: SessionId,
    pub(crate) working_dir: String,
    pub(crate) model_id: String,
    pub(crate) session_store_dir: Option<std::path::PathBuf>,
    /// 当前 turn 的扩展事件通道（`ExtensionEventBridge` 在 `process_prompt` 期间注入）。
    pub(crate) turn_event_tx: Option<TurnEventTx>,
}

impl SharedTurnContext {
    /// 从 session 读模型构造共享上下文（不含 session_store_dir）。
    pub fn from_read_model(session_id: &SessionId, model: &SessionReadModel) -> Self {
        Self {
            session_id: session_id.clone(),
            working_dir: model.working_dir.clone(),
            model_id: model.model_id.clone(),
            session_store_dir: None,
            turn_event_tx: None,
        }
    }

    /// 构造扩展 lifecycle hook 的 ctx。
    pub fn lifecycle_ctx(&self) -> LifecycleContext {
        LifecycleContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.clone(),
            model: self.model_selection(),
            event_tx: self.turn_event_tx.clone(),
            extension_event_sink: None,
            last_exchange: None,
        }
    }

    /// 构造带当轮消息摘要的 lifecycle hook ctx（用于 TurnEnd）。
    pub fn lifecycle_ctx_with_exchange(
        &self,
        user_message: String,
        assistant_message: String,
    ) -> LifecycleContext {
        LifecycleContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.clone(),
            model: self.model_selection(),
            event_tx: self.turn_event_tx.clone(),
            extension_event_sink: None,
            last_exchange: Some(ExchangeSummary {
                user_message,
                assistant_message,
            }),
        }
    }

    /// 构造 provider hook 的 ctx，附带本次 LLM 请求的 messages。
    pub fn provider_ctx(&self, messages: Vec<LlmMessage>) -> ProviderContext {
        ProviderContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.clone(),
            model: self.model_selection(),
            messages,
            session_store_dir: self.session_store_dir.clone(),
        }
    }

    /// 构造各 tool hook ctx 共用的 `ModelSelection`。
    pub fn model_selection(&self) -> ModelSelection {
        ModelSelection::simple(self.model_id.clone())
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
    #[error("prompt is still too long after reactive compaction")]
    CompactExhausted,
    #[error("session read failed: {0}")]
    SessionReadFailed(String),
    #[error("LLM stream ended unexpectedly")]
    StreamEndedUnexpectedly,
    #[error("provider blocked request: {reason}")]
    ProviderBlocked { reason: String },
    #[error("persist tool result failed: {0}")]
    PersistToolResultFailed(String),
    #[error("tool task join failed: {0}")]
    ToolTaskJoinFailed(String),
    #[error("{0}")]
    Internal(String),
    #[error("durable event emit failed: {0}")]
    DurableEmitFailed(String),
}

/// 是否应在 turn 失败时补发 `TurnEnd`（已由 `end_turn_with_error_typed` 处理的路径除外）。
pub(crate) fn turn_error_emits_turn_end(error: &TurnError) -> bool {
    !matches!(
        error,
        TurnError::ProviderBlocked { .. }
            | TurnError::CompactExhausted
            | TurnError::Llm(_)
            | TurnError::Extension(_)
            | TurnError::Tool(_)
    )
}

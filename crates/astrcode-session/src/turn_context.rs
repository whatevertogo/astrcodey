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

/// Turn 内扩展/工具 → event bridge 的入口（unbounded，不丢事件、durable 由单 worker 保序）。
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

/// Turn 循环内的 typed early-return；`TurnEnd` 由
/// [`TurnLoop::finalize_turn_on_error`](crate::turn_runner::TurnLoop::finalize_turn_on_error)
/// 统一补发。
pub(crate) fn end_turn_with_error_typed<T, E>(error: E) -> Result<T, TurnError>
where
    E: Into<TurnError>,
{
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
    /// 当前 turn 的扩展事件通道（`ExtensionEvents` 在 `process_prompt` 期间注入）。
    pub(crate) turn_event_tx: Option<TurnEventTx>,
    pub(crate) approval_mode: astrcode_core::permission::ApprovalMode,
    pub(crate) is_child_session: bool,
    pub(crate) child_tool_policy: Option<astrcode_core::extension::ChildToolPolicy>,
    pub(crate) permission_chain: std::sync::Arc<astrcode_core::permission::PermissionChain>,
    pub(crate) approval_history: std::sync::Arc<crate::permission::ApprovalHistoryStore>,
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
            approval_mode: astrcode_core::permission::ApprovalMode::default(),
            is_child_session: model.parent_session_id.is_some(),
            child_tool_policy: None,
            permission_chain: std::sync::Arc::new(astrcode_core::permission::PermissionChain::new(
                vec![],
            )),
            approval_history: std::sync::Arc::new(
                crate::permission::ApprovalHistoryStore::default(),
            ),
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
            mid_turn_user_messages_synced: 0,
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
            mid_turn_user_messages_synced: 0,
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
    #[error("{0}")]
    Session(#[from] crate::session::SessionError),
    #[error("prompt is still too long after reactive compaction")]
    CompactExhausted,
    #[error("LLM stream ended unexpectedly")]
    StreamEndedUnexpectedly,
    #[error("turn aborted")]
    Aborted,
    #[error("provider blocked request: {reason}")]
    ProviderBlocked { reason: String },
    #[error("tool task join failed: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
    #[error("turn model cache not populated")]
    ModelCacheEmpty,
}

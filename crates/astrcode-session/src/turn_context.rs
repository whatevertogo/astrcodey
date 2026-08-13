//! Turn 基础设施 — 事件通道、共享上下文、错误类型。

use astrcode_core::{config::ModelSelection, llm::LlmMessage, types::*};
use astrcode_extension_sdk::{
    extension::{
        ExchangeSummary, ExtensionError, LifecycleEvent, LifecyclePayload, ProviderPayload,
        RuntimeHookCallContext, RuntimeLifecycleContext, RuntimeProviderContext,
    },
    runtime_ports::TurnHooks,
};
use astrcode_session_projection::SessionReadModel;
use tokio_util::sync::CancellationToken;
// ─── Turn event channel ──────────────────────────────────────────────────

/// Turn 内扩展/工具 → event bridge 的有界入口；背压显式返回，durable 由单 worker 保序。
pub type TurnEventTx = astrcode_core::event::EventSender;

/// StepEnd 生命周期钩子：失败只记录 warn，不中断 turn。
pub(crate) async fn on_step_end_best_effort(
    extension_runner: &dyn TurnHooks,
    ctx: &RuntimeLifecycleContext,
) {
    if let Err(error) = extension_runner
        .emit_lifecycle(LifecycleEvent::StepEnd, ctx.clone())
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
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) working_dir: String,
    pub(crate) model_id: String,
    pub(crate) session_store_dir: Option<std::path::PathBuf>,
    /// 当前 turn 的事件 ingress（`TurnEventBridge` 在 `process_prompt` 期间注入）。
    pub(crate) turn_event_sender: Option<crate::turn_publish::TurnEventSender>,
    pub(crate) approval_mode: astrcode_core::permission::ApprovalMode,
    pub(crate) tool_selection: Option<astrcode_core::tool::SessionToolSelection>,
    pub(crate) permission_chain: std::sync::Arc<crate::permission::PermissionChain>,
    pub(crate) approval_history: std::sync::Arc<crate::permission::ApprovalHistoryStore>,
    pub(crate) cancellation_token: CancellationToken,
}

impl SharedTurnContext {
    /// Hook / 工具侧非阻塞事件入口；turn 外为 `None`。
    pub(crate) fn turn_event_tx(&self) -> Option<TurnEventTx> {
        self.turn_event_sender
            .as_ref()
            .map(|sender| sender.event_tx())
    }

    /// 构造扩展 lifecycle hook 的 ctx。
    pub(crate) fn lifecycle_ctx(&self) -> RuntimeLifecycleContext {
        RuntimeLifecycleContext::new(self.hook_call_context(), LifecyclePayload::new(None))
    }

    /// 构造带当轮消息摘要的 lifecycle hook ctx（用于 TurnEnd）。
    pub(crate) fn lifecycle_ctx_with_exchange(
        &self,
        user_message: String,
        assistant_message: String,
    ) -> RuntimeLifecycleContext {
        RuntimeLifecycleContext::new(
            self.hook_call_context(),
            LifecyclePayload::new(Some(ExchangeSummary {
                user_message,
                assistant_message,
            })),
        )
    }

    pub(crate) fn hook_call_context(&self) -> RuntimeHookCallContext {
        let call = RuntimeHookCallContext::new(
            self.session_id.to_string(),
            self.working_dir.clone(),
            self.model_selection(),
            self.session_store_dir.clone(),
        )
        .with_event_tx(self.turn_event_tx())
        .with_cancellation(self.cancellation_token.clone());
        match &self.turn_id {
            Some(turn_id) => call.with_turn_id(turn_id.to_string()),
            None => call,
        }
    }

    /// 构造 provider hook 的 ctx，附带本次 LLM 请求的 messages。
    pub(crate) fn provider_ctx(&self, messages: Vec<LlmMessage>) -> RuntimeProviderContext {
        RuntimeProviderContext::new(self.hook_call_context(), ProviderPayload::new(messages))
    }

    /// 构造各 tool hook ctx 共用的 `ModelSelection`。
    pub(crate) fn model_selection(&self) -> ModelSelection {
        ModelSelection::simple(self.model_id.clone())
    }
}

/// 为没有活跃工具管线的 session hook 构造最小调用上下文。
pub(crate) fn hook_call_context_for_read_model(
    session_id: &SessionId,
    model: &SessionReadModel,
    session_store_dir: Option<std::path::PathBuf>,
) -> RuntimeHookCallContext {
    RuntimeHookCallContext::new(
        session_id.to_string(),
        model.identity.working_dir.clone(),
        ModelSelection::simple(model.identity.model_id.clone()),
        session_store_dir,
    )
}

// ─── TurnError ───────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("{0}")]
    Llm(#[from] astrcode_core::llm::LlmError),
    #[error("Tool error: {0}")]
    Tool(#[from] astrcode_core::tool::ToolError),
    #[error("Extension error: {0}")]
    Extension(#[from] ExtensionError),
    #[error("{0}")]
    Session(#[from] crate::SessionError),
    #[error("session projection error: {0}")]
    Projection(#[from] astrcode_session_projection::ProjectionError),
    #[error("tool approval registration error: {0}")]
    ApprovalRegistration(#[from] crate::ToolApprovalRegistrationError),
    #[error("approval history error: {0}")]
    ApprovalHistory(String),
    #[error("prompt is still too long after reactive compaction")]
    CompactExhausted,
    #[error("LLM stream ended unexpectedly")]
    StreamEndedUnexpectedly,
    #[error("turn aborted")]
    Aborted,
    #[error("input blocked by extension: {reason}")]
    InputBlocked { reason: String },
    #[error("provider blocked request: {reason}")]
    ProviderBlocked { reason: String },
    #[error("tool task join failed: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
    #[error("turn model cache not populated")]
    ModelCacheEmpty,
    #[error("turn event ingress failed: {0}")]
    EventIngress(String),
}

//! Turn 基础设施 — 信号类型、共享上下文、错误类型。

use astrcode_core::{
    capability::{CapabilityRegistry, ModelInfo, PromptMessage},
    event::EventPayload,
    extension::{ExchangeSummary, ExtensionEvent, LifecycleContext, ProviderContext},
    llm::LlmMessage,
    types::*,
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::sync::mpsc;

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
    let _ = extension_runner
        .emit_lifecycle(ExtensionEvent::TurnEnd, shared.lifecycle_ctx())
        .await;
    Err(error.into())
}

// ─── SharedTurnContext ───────────────────────────────────────────────────

/// Session-level identifiers shared across all agent sub-objects.
///
/// 提供 `lifecycle_ctx` / `provider_ctx` 工厂方法，避免散落在 hook 调用点
/// 重复构造 3-字段 LifecycleContext / 4-字段 ProviderContext。
#[derive(Clone)]
pub struct SharedTurnContext {
    pub session_id: SessionId,
    pub working_dir: String,
    pub model_id: String,
    pub session_store_dir: Option<std::path::PathBuf>,
    pub capabilities: CapabilityRegistry,
}

impl SharedTurnContext {
    /// 构造扩展 lifecycle hook 的 ctx。
    pub fn lifecycle_ctx(&self) -> LifecycleContext {
        LifecycleContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.clone(),
            model: self.model_info(),
            extension_event_sink: None,
            last_exchange: None,
            capabilities: self.capabilities.clone(),
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
            model: self.model_info(),
            extension_event_sink: None,
            last_exchange: Some(ExchangeSummary {
                user_message,
                assistant_message,
            }),
            capabilities: self.capabilities.clone(),
        }
    }

    /// 构造 provider hook 的 ctx，附带本次 LLM 请求的 messages。
    pub fn provider_ctx(&self, messages: Vec<LlmMessage>) -> ProviderContext {
        ProviderContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.clone(),
            model: ModelInfo::new(&self.model_id),
            messages: messages.iter().map(PromptMessage::from).collect(),
            capabilities: self.capabilities.clone(),
            #[allow(deprecated)]
            session_store_dir: self.session_store_dir.clone(),
        }
    }

    /// 构造各 tool hook ctx 共用的 `ModelInfo`。
    pub fn model_info(&self) -> ModelInfo {
        ModelInfo::new(&self.model_id)
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
    #[error("{0}")]
    Internal(String),
}

//! Turn 基础设施 — 信号类型、共享上下文、错误类型。

use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionEvent, LifecycleContext, ProviderContext},
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
}

impl SharedTurnContext {
    /// 构造扩展 lifecycle hook 的 ctx。
    pub fn lifecycle_ctx(&self) -> LifecycleContext {
        LifecycleContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.clone(),
            model: self.model_selection(),
            plugin_event_sink: None,
        }
    }

    /// 构造 provider hook 的 ctx，附带本次 LLM 请求的 messages。
    pub fn provider_ctx(&self, messages: Vec<LlmMessage>) -> ProviderContext {
        ProviderContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.clone(),
            model: self.model_selection(),
            messages,
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
    Internal(String),
}

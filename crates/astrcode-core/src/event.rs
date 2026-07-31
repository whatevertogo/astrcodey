//! 持久事件、实时事件及其信封类型。

mod envelope;
mod payload;

use std::sync::Arc;

pub use envelope::{
    DurableEvent, Event, EventEnvelope, EventPayload, LiveEvent, Phase, StoredEvent,
    ToolOutputStream,
};
pub use payload::{
    CompactionDetails, DurableEventPayload, ExtensionEventData, LiveEventPayload, ParentSessionRef,
    SessionStarted, TranscriptRewriteReason,
};
use serde::{Deserialize, Serialize};

/// Cloneable non-blocking event sink used by turn-scoped tools and extensions.
///
/// The closure-based boundary lets the session runtime put event and barrier commands on the
/// same FIFO without exposing its internal command type through core contracts.
#[derive(Clone)]
pub struct EventSender {
    send: Arc<dyn Fn(EventPayload) -> Result<(), EventSendError> + Send + Sync>,
}

impl EventSender {
    pub fn new(
        send: impl Fn(EventPayload) -> Result<(), EventSendError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            send: Arc::new(send),
        }
    }

    pub fn send(&self, payload: EventPayload) -> Result<(), EventSendError> {
        (self.send)(payload)
    }
}

impl From<tokio::sync::mpsc::UnboundedSender<EventPayload>> for EventSender {
    fn from(sender: tokio::sync::mpsc::UnboundedSender<EventPayload>) -> Self {
        Self::new(move |payload| sender.send(payload).map_err(|_| EventSendError))
    }
}

impl std::fmt::Debug for EventSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EventSender")
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("event receiver is closed")]
pub struct EventSendError;

/// 持久化的 system prompt 及其恢复语义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedSystemPrompt {
    pub text: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "SystemPromptSource::is_native")]
    pub source: SystemPromptSource,
}

/// system prompt 的生命周期来源。
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemPromptSource {
    #[default]
    Native,
    Inherited,
}

impl SystemPromptSource {
    pub(crate) fn is_native(&self) -> bool {
        *self == Self::Native
    }
}

#[cfg(test)]
mod tests;

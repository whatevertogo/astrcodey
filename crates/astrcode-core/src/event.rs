//! 持久事件、实时事件及其信封类型。

mod envelope;
mod payload;

pub use envelope::{
    DurableEvent, Event, EventEnvelope, EventPayload, LiveEvent, Phase, StoredEvent,
    ToolOutputStream,
};
pub use payload::{
    CompactionDetails, DurableEventPayload, ExtensionEventData, LiveEventPayload, ParentSessionRef,
    SessionStarted, TranscriptRewriteReason,
};
use serde::{Deserialize, Serialize};

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

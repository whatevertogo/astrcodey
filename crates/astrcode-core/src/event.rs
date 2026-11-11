//! 持久事件、实时事件及其信封类型。

mod envelope;
mod fingerprint;
mod ingress;
mod outcome;
mod payload;

pub use envelope::{
    DurableEvent, EVENT_FORMAT_VERSION, Event, EventEnvelope, EventPayload, LiveEvent, Phase,
    StoredEvent, ToolOutputStream,
};
pub use fingerprint::{stable_hash_hex, system_prompt_fingerprint, transcript_prefix_fingerprint};
pub use ingress::{EventDeliveryReceipt, EventPublisher, EventSendError, EventSender};
pub use outcome::{
    AgentSessionOutcome, AssistantAttemptOutcome, CompactionOutcome, StepEndReason,
    ToolCallOutcome, TurnCompletionReason, TurnEndReason,
};
pub use payload::{
    AssistantDelta, CompactionDetails, CustomEventAudience, CustomEventData, DurableEventPayload,
    LiveEventPayload, ParentSessionRef, SessionStarted, TranscriptRewriteReason,
};
use serde::{Deserialize, Serialize};

/// Provider 用量覆盖的 transcript 前缀，可随 fork 一起持久化。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextTokenAnchor {
    pub context_tokens: usize,
    pub model_context_window: usize,
    pub covered_message_count: usize,
}

/// 持久化的 system prompt 及其恢复语义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedSystemPrompt {
    pub text: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_system_prompt: Option<String>,
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

#[cfg(test)]
mod tests;

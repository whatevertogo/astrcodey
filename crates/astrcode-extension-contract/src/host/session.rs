//! Session 宿主能力线缆契约。

use serde::{Deserialize, Serialize};

use super::llm::HostLlmMessage;
use crate::session::{SessionPhaseDto, SessionToolSelectionDto};

/// Stable summary returned by the narrow session-history domain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSessionSummary {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub source_extension: Option<String>,
    pub working_dir: String,
    pub model_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub latest_cursor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionSummariesOutput {
    pub sessions: Vec<HostSessionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTranscriptMessage {
    pub message: HostLlmMessage,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTranscript {
    pub session_id: String,
    pub messages: Vec<HostSessionTranscriptMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionProviderMessagesOutput {
    pub session_id: String,
    pub messages: Vec<HostLlmMessage>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTokenUsage {
    pub total_tokens: u64,
    pub model_context_window: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTokenUsageOutput {
    pub usage: Option<HostSessionTokenUsage>,
}

/// `astrcode.process.spawn` 的线缆请求。
///
/// `stdin` 最大为 [`HOST_PROCESS_MAX_STDIN_BYTES`] 个 UTF-8 字节，`timeout_ms` 必须位于
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionInputRequest {
    pub target_session_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostSessionDeliveryOutput {
    Started { turn_id: String },
    Injected { turn_id: String },
    Queued { queue_len: usize },
}

/// Result of idempotently requesting cancellation of the active turn.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionCancelOutput {
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionExecutionView {
    pub phase: SessionPhaseDto,
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostConfigureSessionToolsRequest {
    pub session_id: String,
    pub selection: SessionToolSelectionDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostConfigureSessionToolsOutput {
    pub selection: SessionToolSelectionDto,
}

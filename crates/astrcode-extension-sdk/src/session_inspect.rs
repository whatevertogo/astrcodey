//! `session_inspect` 插件边界契约。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::{SessionLifecycleStateDto, SessionToolSelectionDto};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectListItem {
    pub session_id: String,
    pub working_dir: String,
    pub model_id: String,
    pub parent_session_id: Option<String>,
    pub source_extension: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub phase: String,
    pub latest_cursor: String,
    pub first_user_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectListOutput {
    pub sessions: Vec<SessionInspectListItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectSnapshot {
    pub session_id: String,
    pub cursor: String,
    pub working_dir: String,
    pub model_id: String,
    pub phase: String,
    pub parent_session_id: Option<String>,
    pub source_extension: Option<String>,
    pub message_count: usize,
    pub pending_tool_call_ids: Vec<String>,
    pub agent_session_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectSnapshotOutput {
    pub snapshot: SessionInspectSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectMessage {
    pub role: String,
    pub content: Vec<SessionInspectContent>,
    pub name: Option<String>,
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionInspectContent {
    Text {
        text: String,
    },
    Image {
        base64: String,
        media_type: String,
        filename: Option<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectSequencedMessage {
    pub message: SessionInspectMessage,
    pub updated_seq: u64,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectPendingApproval {
    pub prompt: String,
    pub rule_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectAgentSession {
    pub child_session_id: String,
    pub tool_call_id: Option<String>,
    pub agent_name: String,
    pub task: String,
    pub status: String,
    pub final_session_id: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub phase: Option<String>,
    pub current_tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectCompaction {
    pub trigger: String,
    pub pre_tokens: usize,
    pub post_tokens: usize,
    pub summary: String,
    pub transcript_path: Option<String>,
    pub seq: u64,
    pub source_seq: u64,
    pub strategy: String,
    pub keep_recent_turns: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectReadModel {
    pub session_id: String,
    pub messages: Vec<SessionInspectSequencedMessage>,
    pub working_dir: String,
    pub model_id: String,
    pub phase: String,
    pub system_prompt: Option<String>,
    pub extra_system_prompt: Option<String>,
    pub system_prompt_fingerprint: Option<String>,
    pub pending_tool_call_ids: Vec<String>,
    pub pending_tool_approvals: BTreeMap<String, SessionInspectPendingApproval>,
    pub created_at: String,
    pub updated_at: String,
    pub parent_session_id: Option<String>,
    pub tool_selection: Option<SessionToolSelectionDto>,
    pub source_extension: Option<String>,
    pub agent_sessions: Vec<SessionInspectAgentSession>,
    pub compactions: Vec<SessionInspectCompaction>,
    pub latest_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectReadModelOutput {
    pub read_model: SessionInspectReadModel,
}

/// `astrcode.session.history.snapshot` 的作用域受限只读响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistorySnapshotOutput {
    pub lifecycle: SessionLifecycleStateDto,
    pub read_model: SessionInspectReadModel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInspectProviderMessagesOutput {
    pub messages: Vec<SessionInspectMessage>,
}

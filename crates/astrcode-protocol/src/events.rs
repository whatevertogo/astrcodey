//! Server-to-client event types (JSON-RPC notifications and responses).

use serde::{Deserialize, Serialize};

/// Events that the server streams to connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ServerEvent {
    // Session lifecycle
    SessionCreated {
        session_id: String,
        working_dir: String,
    },
    SessionResumed {
        session_id: String,
        snapshot: SessionSnapshot,
    },
    SessionDeleted {
        session_id: String,
    },
    SessionList {
        sessions: Vec<SessionListItem>,
    },

    // Agent lifecycle
    AgentStarted,
    AgentEnded {
        reason: String,
    },

    // Turn lifecycle
    TurnStarted {
        turn_id: String,
    },
    TurnEnded {
        turn_id: String,
        finish_reason: String,
    },

    // Message streaming (Server → Client incremental updates)
    MessageStart {
        message_id: String,
        role: String,
    },
    MessageDelta {
        message_id: String,
        delta: String,
    },
    MessageEnd {
        message_id: String,
    },

    // Tool execution streaming
    ToolCallStart {
        call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolCallDelta {
        call_id: String,
        output_delta: String,
    },
    ToolCallEnd {
        call_id: String,
        result: ToolCallResultDto,
    },

    // Compaction
    CompactionStarted,
    CompactionEnded {
        pre_tokens: usize,
        post_tokens: usize,
        summary: String,
    },

    // UI request (Server → Client, requires response)
    UiRequest {
        request_id: String,
        kind: UiRequestKind,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        #[serde(default)]
        timeout_secs: u64,
    },

    // Errors
    Error {
        code: i32,
        message: String,
    },
}

/// UI request kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRequestKind {
    /// Yes/no confirmation.
    Confirm,
    /// Single option from a list.
    Select,
    /// Free-form text input.
    Input,
    /// Informational notification.
    Notify,
}

/// Result of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResultDto {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// Session list item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListItem {
    pub session_id: String,
    pub created_at: String,
    pub last_active_at: String,
    pub working_dir: String,
    pub parent_session_id: Option<String>,
}

/// Session snapshot for reconnection/recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub cursor: String,
    pub messages: Vec<MessageDto>,
    pub model_id: String,
    pub working_dir: String,
}

/// A message in the session snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDto {
    pub role: String,
    pub content: String,
}

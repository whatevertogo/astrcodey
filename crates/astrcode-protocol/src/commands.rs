//! Client-to-server command types (JSON-RPC requests).

use serde::{Deserialize, Serialize};

/// Commands that a client (frontend) can send to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ClientCommand {
    // Session management
    CreateSession {
        working_dir: String,
    },
    ResumeSession {
        session_id: String,
    },
    ForkSession {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        at_cursor: Option<String>,
    },
    DeleteSession {
        session_id: String,
    },
    ListSessions,
    SwitchSession {
        session_id: String,
    },

    // Prompting
    SubmitPrompt {
        text: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    Abort,

    // Configuration
    SetModel {
        model_id: String,
    },
    SetThinkingLevel {
        level: String,
    },
    Compact,
    SwitchMode {
        mode: String,
    },

    // State
    GetState,

    // UI response (to a server-initiated UI request)
    UiResponse {
        request_id: String,
        value: UiResponseValue,
    },
}

/// File/image attachment to a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub content: String,
    pub media_type: String,
}

/// Response to a UI request from the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UiResponseValue {
    Confirm { accepted: bool },
    Select { selected: String },
    Input { text: String },
    NotifyAck,
}

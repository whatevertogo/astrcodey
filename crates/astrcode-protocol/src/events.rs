//! Server-to-client protocol notifications.

use astrcode_core::event::Event;
use serde::{Deserialize, Serialize};

/// Notifications that the server streams to connected clients.
///
/// Runtime/session facts are carried as core `Event`s. Protocol-only
/// interactions such as session lists and UI requests stay out of the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ClientNotification {
    Event(Event),
    SessionResumed {
        session_id: String,
        snapshot: SessionSnapshot,
    },
    SessionList {
        sessions: Vec<SessionListItem>,
    },
    UiRequest {
        request_id: String,
        kind: UiRequestKind,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        #[serde(default)]
        timeout_secs: u64,
    },
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

//! Session storage traits and event types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::*;

/// A session event — the atomic unit of the event log.
///
/// Every state change is recorded as an event. The full history
/// of a session can be replayed from its events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A session was created.
    SessionStart {
        session_id: SessionId,
        timestamp: DateTime<Utc>,
        working_dir: String,
        model_id: String,
    },
    /// A user sent a prompt.
    UserMessage {
        event_id: EventId,
        turn_id: TurnId,
        timestamp: DateTime<Utc>,
        text: String,
    },
    /// The assistant generated a message.
    AssistantMessage {
        event_id: EventId,
        turn_id: TurnId,
        message_id: MessageId,
        timestamp: DateTime<Utc>,
        text: String,
    },
    /// A tool was called.
    ToolCall {
        event_id: EventId,
        turn_id: TurnId,
        call_id: ToolCallId,
        timestamp: DateTime<Utc>,
        tool_name: String,
        arguments: serde_json::Value,
    },
    /// A tool returned a result.
    ToolResult {
        event_id: EventId,
        turn_id: TurnId,
        call_id: ToolCallId,
        timestamp: DateTime<Utc>,
        content: String,
        is_error: bool,
    },
    /// The context was compacted.
    Compaction {
        event_id: EventId,
        timestamp: DateTime<Utc>,
        pre_tokens: usize,
        post_tokens: usize,
        summary: String,
    },
    /// A turn started.
    TurnStart {
        turn_id: TurnId,
        timestamp: DateTime<Utc>,
    },
    /// A turn ended.
    TurnEnd {
        turn_id: TurnId,
        timestamp: DateTime<Utc>,
        finish_reason: String,
    },
    /// The session was forked from a parent session.
    SessionFork {
        timestamp: DateTime<Utc>,
        parent_session_id: SessionId,
        fork_cursor: Cursor,
    },
    /// A custom event (for extensions).
    Custom {
        event_id: EventId,
        timestamp: DateTime<Utc>,
        name: String,
        data: serde_json::Value,
    },
}

/// Trait for session event storage.
///
/// Implementations provide persistence for the event-sourced session model.
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    /// Create a new session event log with an initial SessionStart event.
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
    ) -> Result<(), StorageError>;

    /// Append an event to the session's event log.
    async fn append_event(
        &self,
        session_id: &SessionId,
        event: SessionEvent,
    ) -> Result<(), StorageError>;

    /// Replay all events for a session from the beginning.
    async fn replay_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionEvent>, StorageError>;

    /// Replay events from a cursor position.
    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<SessionEvent>, StorageError>;

    /// Create a checkpoint snapshot at the current position.
    async fn checkpoint(&self, session_id: &SessionId, cursor: &Cursor)
        -> Result<(), StorageError>;

    /// List all session IDs.
    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError>;

    /// Open an existing session from disk, preparing it for appends.
    /// Must be called before append_event on a resumed session.
    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        // Default: replay events to verify session exists
        self.replay_events(session_id).await.map(|_| ())
    }

    /// Delete a session and all its data.
    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError>;
}

/// Session metadata for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub working_dir: String,
    pub model_id: String,
    pub parent_session_id: Option<SessionId>,
}

/// Error from storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Invalid session ID: {0}")]
    InvalidId(String),
    #[error("Lock error: {0}")]
    LockError(String),
}

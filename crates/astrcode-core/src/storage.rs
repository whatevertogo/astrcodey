//! Session storage traits.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{event::Event, types::*};

/// Trait for session event storage.
///
/// Implementations persist unified events and assign the per-session sequence
/// number when an event enters the durable JSONL log.
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    /// Create a new session event log with an initial SessionStarted event.
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
    ) -> Result<Event, StorageError>;

    /// Append an event to the session's event log.
    async fn append_event(&self, event: Event) -> Result<Event, StorageError>;

    /// Replay all events for a session from the beginning.
    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError>;

    /// Replay events from a cursor position.
    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError>;

    /// Create a checkpoint snapshot at the current position.
    async fn checkpoint(&self, session_id: &SessionId, cursor: &Cursor)
    -> Result<(), StorageError>;

    /// List all session IDs.
    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError>;

    /// Open an existing session from disk, preparing it for appends.
    ///
    /// Must be called before append_event on a resumed session.
    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
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

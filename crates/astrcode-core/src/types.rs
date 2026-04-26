//! Core shared identifier and data types.
//!
//! These types are used across all crates in the astrcode platform.

use std::path::PathBuf;

/// Unique identifier for a session.
///
/// A session is the durable event-sourced unit of work.
/// All agent interactions happen within a session.
pub type SessionId = String;

/// Unique identifier for an event in a session's event log.
pub type EventId = String;

/// Unique identifier for a turn (one user-prompt + agent response cycle).
pub type TurnId = String;

/// Unique identifier for a message (user or assistant) within a turn.
pub type MessageId = String;

/// Unique identifier for a tool call within a turn.
pub type ToolCallId = String;

/// Position cursor in the session event log.
/// Opaque to clients; server uses it for pagination and recovery.
pub type Cursor = String;

/// A project identifier, derived from the working directory path.
pub type ProjectHash = String;

/// Error type for identifier validation.
#[derive(Debug, Clone, thiserror::Error)]
pub enum IdError {
    #[error("Invalid characters in ID: {0}")]
    InvalidCharacters(String),
    #[error("Path traversal attempt in ID: {0}")]
    PathTraversal(String),
}

/// Validates a session ID for safe filesystem use.
///
/// Only allows alphanumeric chars, hyphens, underscores, and 'T'.
/// Rejects `.` and `:` to prevent path traversal.
pub fn validate_session_id(id: &str) -> Result<(), IdError> {
    if id.is_empty() {
        return Err(IdError::InvalidCharacters("empty ID".into()));
    }
    if id.contains("..") || id.contains('/') || id.contains('\\') {
        return Err(IdError::PathTraversal(id.into()));
    }
    for ch in id.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' && ch != 'T' {
            return Err(IdError::InvalidCharacters(format!(
                "character '{}' not allowed in ID",
                ch
            )));
        }
    }
    Ok(())
}

/// Generates a new unique session ID.
pub fn new_session_id() -> SessionId {
    uuid::Uuid::new_v4().to_string()
}

/// Generates a new unique event ID.
pub fn new_event_id() -> EventId {
    uuid::Uuid::new_v4().to_string()
}

/// Generates a new unique turn ID.
pub fn new_turn_id() -> TurnId {
    uuid::Uuid::new_v4().to_string()
}

/// Generates a new unique message ID.
pub fn new_message_id() -> MessageId {
    uuid::Uuid::new_v4().to_string()
}

/// Derives a project hash from a working directory path.
pub fn project_hash_from_path(path: &PathBuf) -> ProjectHash {
    use std::hash::Hasher;
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&path, &mut hasher);
    format!("{:016x}", hasher.finish())
}

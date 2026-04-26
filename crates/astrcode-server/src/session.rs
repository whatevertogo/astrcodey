//! Session management — the durable event-sourced unit of work.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

use astrcode_core::storage::SessionEvent;
use astrcode_core::types::*;

use crate::agent::Agent;
use astrcode_protocol::events::ServerEvent;

/// A session — an append-only event log with subscribers.
///
/// Sessions are the source of truth. Agents are created from
/// session events and write back to the session.
pub struct Session {
    pub id: SessionId,
    pub state: RwLock<SessionState>,
    agent_handle: Mutex<Option<AgentHandle>>,
    event_tx: broadcast::Sender<ServerEvent>,
}

/// Handle to the currently active agent in this session.
pub struct AgentHandle {
    pub agent: Agent,
}

/// In-memory state derived from session events.
pub struct SessionState {
    pub messages: Vec<astrcode_core::llm::LlmMessage>,
    pub tool_results: Vec<astrcode_core::tool::ToolResult>,
    pub model_id: String,
    pub thinking_level: String,
    pub working_dir: String,
    pub cursor: Cursor,
    pub subscriber_count: usize,
}

impl Session {
    pub fn new(id: SessionId, working_dir: String, model_id: String, capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(capacity);
        Self {
            id,
            state: RwLock::new(SessionState {
                messages: Vec::new(),
                tool_results: Vec::new(),
                model_id,
                thinking_level: "default".into(),
                working_dir,
                cursor: String::new(),
                subscriber_count: 0,
            }),
            agent_handle: Mutex::new(None),
            event_tx,
        }
    }

    /// Subscribe to session events.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.event_tx.subscribe()
    }

    /// Broadcast an event to all subscribers.
    pub fn broadcast(&self, event: ServerEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Attach an agent to this session.
    pub async fn attach_agent(&self, agent: Agent) {
        let mut handle = self.agent_handle.lock().await;
        *handle = Some(AgentHandle { agent });
    }

    /// Detach the current agent.
    pub async fn detach_agent(&self) {
        let mut handle = self.agent_handle.lock().await;
        *handle = None;
    }
}

/// Manages multiple sessions within the server.
pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new session.
    pub async fn create(
        &self,
        working_dir: &str,
        model_id: &str,
        broadcast_capacity: usize,
    ) -> SessionId {
        let id = new_session_id();
        let session = Arc::new(Session::new(
            id.clone(),
            working_dir.into(),
            model_id.into(),
            broadcast_capacity,
        ));
        let mut sessions = self.sessions.write().await;
        sessions.insert(id.clone(), session);
        id
    }

    /// Get a session by ID.
    pub async fn get(&self, id: &SessionId) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        sessions.get(id).cloned()
    }

    /// List all session IDs.
    pub async fn list(&self) -> Vec<SessionId> {
        let sessions = self.sessions.read().await;
        sessions.keys().cloned().collect()
    }

    /// Fork a session — creates a new session with shared history up to cursor.
    pub async fn fork(
        &self,
        parent_id: &SessionId,
        _at_cursor: Option<&Cursor>,
        working_dir: &str,
        model_id: &str,
        broadcast_capacity: usize,
    ) -> Result<SessionId, SessionError> {
        let _parent = self
            .get(parent_id)
            .await
            .ok_or_else(|| SessionError::NotFound(parent_id.clone()))?;

        // Create child session
        let child_id = self.create(working_dir, model_id, broadcast_capacity).await;

        // TODO: Copy parent events up to cursor into child event log
        // TODO: Emit SessionFork event in child

        Ok(child_id)
    }

    /// Delete a session.
    pub async fn delete(&self, id: &SessionId) -> Result<(), SessionError> {
        let mut sessions = self.sessions.write().await;
        sessions
            .remove(id)
            .ok_or_else(|| SessionError::NotFound(id.clone()))?;
        Ok(())
    }

    /// Switch active session (handled by client reconnecting).
    pub async fn switch(&self, _from: &SessionId, _to: &SessionId) -> Result<(), SessionError> {
        // Switching is a client-side operation
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
}

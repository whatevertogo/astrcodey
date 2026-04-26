//! Session — the durable event-sourced unit of work.
//! SessionManager — glue between memory and EventStore.
//! SessionEventReducer — maps SessionEvent → SessionState update.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use astrcode_core::llm::LlmMessage;
use astrcode_core::storage::{EventStore, SessionEvent, StorageError};
use astrcode_core::types::*;

// ─── Session ─────────────────────────────────────────────────────────────

pub struct Session {
    pub id: SessionId,
    pub state: RwLock<SessionState>,
    event_tx: broadcast::Sender<astrcode_protocol::events::ServerEvent>,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub messages: Vec<LlmMessage>,
    pub working_dir: String,
    pub model_id: String,
}

impl Session {
    pub fn new(id: SessionId, working_dir: String, model_id: String, capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(capacity);
        Self { id, state: RwLock::new(SessionState { messages: vec![], working_dir, model_id }), event_tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<astrcode_protocol::events::ServerEvent> {
        self.event_tx.subscribe()
    }
}

// ─── SessionEventReducer ─────────────────────────────────────────────────

/// Pure function: applies a SessionEvent to SessionState.
pub struct SessionEventReducer;

impl SessionEventReducer {
    pub fn reduce(event: &SessionEvent, state: &mut SessionState) {
        match event {
            SessionEvent::SessionStart { working_dir, model_id, .. } => {
                state.working_dir = working_dir.clone();
                state.model_id = model_id.clone();
            }
            SessionEvent::UserMessage { text, .. } => {
                state.messages.push(LlmMessage::user(text));
            }
            SessionEvent::AssistantMessage { text, .. } => {
                state.messages.push(LlmMessage::assistant(text));
            }
            SessionEvent::ToolCall { tool_name, arguments, .. } => {
                state.messages.push(LlmMessage {
                    role: astrcode_core::llm::LlmRole::Tool,
                    content: vec![astrcode_core::llm::LlmContent::ToolResult {
                        tool_call_id: String::new(), content: format!("call: {}({:?})", tool_name, arguments), is_error: false,
                    }],
                    name: Some(tool_name.clone()),
                });
            }
            SessionEvent::ToolResult { content, is_error, .. } => {
                state.messages.push(LlmMessage {
                    role: astrcode_core::llm::LlmRole::Tool,
                    content: vec![astrcode_core::llm::LlmContent::Text { text: content.clone() }],
                    name: None,
                });
            }
            _ => {}
        }
    }

    /// Replay a list of events to build initial SessionState.
    pub fn replay(events: &[SessionEvent]) -> SessionState {
        let mut state = SessionState { messages: vec![], working_dir: String::new(), model_id: String::new() };
        for event in events {
            Self::reduce(event, &mut state);
        }
        state
    }
}

// ─── SessionManager ──────────────────────────────────────────────────────

pub struct SessionManager {
    active: RwLock<HashMap<SessionId, Arc<Session>>>,
    store: Arc<dyn EventStore>,
}

impl SessionManager {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self { active: RwLock::new(HashMap::new()), store }
    }

    /// Create a new session and persist SessionStart.
    pub async fn create(&self, working_dir: &str, model_id: &str, capacity: usize) -> Result<SessionId, SessionError> {
        let sid = new_session_id();
        self.store.create_session(&sid, working_dir, model_id).await?;

        let session = Arc::new(Session::new(sid.clone(), working_dir.into(), model_id.into(), capacity));
        self.active.write().await.insert(sid.clone(), session);
        Ok(sid)
    }

    /// Resume a session from disk, replay events, add to active set.
    pub async fn resume(&self, session_id: &SessionId) -> Result<Arc<Session>, SessionError> {
        if let Some(s) = self.active.read().await.get(session_id) { return Ok(s.clone()); }

        self.store.open_session(session_id).await?;
        let events = self.store.replay_events(session_id).await?;
        let state = SessionEventReducer::replay(&events);

        let session = Arc::new(Session {
            id: session_id.clone(),
            state: RwLock::new(state.clone()),
            event_tx: broadcast::channel(2048).0,
        });
        self.active.write().await.insert(session_id.clone(), session.clone());
        Ok(session)
    }

    /// Append an event to disk and update in-memory state.
    pub async fn append_event(&self, session_id: &SessionId, event: SessionEvent) -> Result<(), SessionError> {
        self.store.append_event(session_id, event.clone()).await?;
        if let Some(s) = self.active.read().await.get(session_id) {
            SessionEventReducer::reduce(&event, &mut *s.state.write().await);
        }
        Ok(())
    }

    /// Get active session by ID.
    pub async fn get(&self, session_id: &SessionId) -> Option<Arc<Session>> {
        self.active.read().await.get(session_id).cloned()
    }

    /// List all sessions (from disk).
    pub async fn list(&self) -> Result<Vec<SessionId>, SessionError> {
        Ok(self.store.list_sessions().await?)
    }

    /// Delete session from memory and disk.
    pub async fn delete(&self, session_id: &SessionId) -> Result<(), SessionError> {
        self.active.write().await.remove(session_id);
        self.store.delete_session(session_id).await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}

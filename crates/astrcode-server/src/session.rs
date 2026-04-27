//! Session — the durable event-sourced unit of work.
//! SessionManager — glue between memory and EventStore.
//! EventReducer — maps Event → SessionState projection.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::{EventStore, StorageError},
    types::*,
};
use astrcode_protocol::events::ClientNotification;
use tokio::sync::{RwLock, broadcast};

// ─── Session ─────────────────────────────────────────────────────────────

pub struct Session {
    pub id: SessionId,
    pub state: RwLock<SessionState>,
    event_tx: broadcast::Sender<ClientNotification>,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub messages: Vec<LlmMessage>,
    pub working_dir: String,
    pub model_id: String,
    pub phase: Phase,
    pub pending_tool_calls: HashSet<ToolCallId>,
}

impl SessionState {
    fn new(working_dir: String, model_id: String) -> Self {
        Self {
            messages: Vec::new(),
            working_dir,
            model_id,
            phase: Phase::Idle,
            pending_tool_calls: HashSet::new(),
        }
    }
}

impl Session {
    pub fn new(id: SessionId, working_dir: String, model_id: String, capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(capacity);
        Self {
            id,
            state: RwLock::new(SessionState::new(working_dir, model_id)),
            event_tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ClientNotification> {
        self.event_tx.subscribe()
    }
}

// ─── EventReducer ────────────────────────────────────────────────────────

/// Pure projection reducer: applies an Event to SessionState.
pub struct EventReducer;

impl EventReducer {
    pub fn reduce(event: &Event, state: &mut SessionState) {
        match &event.payload {
            EventPayload::SessionStarted {
                working_dir,
                model_id,
            } => {
                state.working_dir = working_dir.clone();
                state.model_id = model_id.clone();
                state.phase = Phase::Idle;
            },
            EventPayload::SessionDeleted => {
                state.phase = Phase::Idle;
            },
            EventPayload::TurnStarted | EventPayload::UserMessage { .. } => {
                state.phase = Phase::Thinking;
                if let EventPayload::UserMessage { text, .. } = &event.payload {
                    state.messages.push(LlmMessage::user(text));
                }
            },
            EventPayload::TurnCompleted { .. } => {
                state.phase = Phase::Idle;
                state.pending_tool_calls.clear();
            },
            EventPayload::AssistantMessageStarted { .. }
            | EventPayload::AssistantTextDelta { .. }
            | EventPayload::ThinkingDelta { .. } => {
                state.phase = Phase::Streaming;
            },
            EventPayload::AssistantMessageCompleted { text, .. } => {
                state.messages.push(LlmMessage::assistant(text));
                state.phase = Phase::Idle;
            },
            EventPayload::ToolCallStarted { call_id, .. } => {
                state.pending_tool_calls.insert(call_id.clone());
                state.phase = Phase::CallingTool;
            },
            EventPayload::ToolCallArgumentsDelta { .. } | EventPayload::ToolOutputDelta { .. } => {
                state.phase = Phase::CallingTool;
            },
            EventPayload::ToolCallRequested {
                call_id,
                tool_name,
                arguments,
            } => {
                state.pending_tool_calls.insert(call_id.clone());
                state.messages.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: call_id.clone(),
                        name: tool_name.clone(),
                        arguments: arguments.clone(),
                    }],
                    name: None,
                });
                state.phase = Phase::CallingTool;
            },
            EventPayload::ToolCallCompleted {
                call_id,
                tool_name,
                result,
            } => {
                state.pending_tool_calls.remove(call_id);
                state.messages.push(LlmMessage {
                    role: LlmRole::Tool,
                    content: vec![LlmContent::ToolResult {
                        tool_call_id: call_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    }],
                    name: Some(tool_name.clone()),
                });
                state.phase = if state.pending_tool_calls.is_empty() {
                    Phase::Thinking
                } else {
                    Phase::CallingTool
                };
            },
            EventPayload::CompactionStarted => {
                state.phase = Phase::Compacting;
            },
            EventPayload::CompactionCompleted { .. } => {
                state.phase = Phase::Idle;
            },
            EventPayload::AgentRunStarted => {
                state.phase = Phase::Thinking;
            },
            EventPayload::AgentRunCompleted { .. } => {
                state.phase = Phase::Idle;
            },
            EventPayload::ErrorOccurred { .. } => {
                state.phase = Phase::Error;
            },
            EventPayload::Custom { .. } => {},
        }
    }

    /// Replay a list of events to build initial SessionState.
    pub fn replay(events: &[Event]) -> SessionState {
        let mut state = SessionState::new(String::new(), String::new());
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
        Self {
            active: RwLock::new(HashMap::new()),
            store,
        }
    }

    /// Create a new session and persist SessionStarted.
    pub async fn create(
        &self,
        working_dir: &str,
        model_id: &str,
        capacity: usize,
    ) -> Result<Event, SessionError> {
        let sid = new_session_id();
        let event = self
            .store
            .create_session(&sid, working_dir, model_id)
            .await?;

        let session = Arc::new(Session::new(
            sid.clone(),
            working_dir.into(),
            model_id.into(),
            capacity,
        ));
        self.active.write().await.insert(sid, session);
        Ok(event)
    }

    /// Resume a session from disk, replay events, add to active set.
    pub async fn resume(&self, session_id: &SessionId) -> Result<Arc<Session>, SessionError> {
        if let Some(s) = self.active.read().await.get(session_id) {
            return Ok(s.clone());
        }

        self.store.open_session(session_id).await?;
        let events = self.store.replay_events(session_id).await?;
        let state = EventReducer::replay(&events);

        let session = Arc::new(Session {
            id: session_id.clone(),
            state: RwLock::new(state),
            event_tx: broadcast::channel(2048).0,
        });
        self.active
            .write()
            .await
            .insert(session_id.clone(), session.clone());
        Ok(session)
    }

    /// Append a durable event to disk and update in-memory state.
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        let stored = self.store.append_event(event).await?;
        if let Some(session) = self.active.read().await.get(&stored.session_id).cloned() {
            EventReducer::reduce(&stored, &mut *session.state.write().await);
        }
        Ok(stored)
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

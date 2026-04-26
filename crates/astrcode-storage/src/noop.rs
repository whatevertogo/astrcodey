//! NoopEventStore — pure in-memory implementation for testing.

use std::collections::HashMap;
use tokio::sync::Mutex;

use astrcode_core::storage::{EventStore, SessionEvent, StorageError};
use astrcode_core::types::{Cursor, SessionId};

/// Pure in-memory EventStore. All operations are synchronous, no disk I/O.
pub struct NoopEventStore {
    sessions: Mutex<HashMap<SessionId, Vec<SessionEvent>>>,
}

impl NoopEventStore {
    pub fn new() -> Self { Self { sessions: Mutex::new(HashMap::new()) } }
}

#[async_trait::async_trait]
impl EventStore for NoopEventStore {
    async fn create_session(&self, session_id: &SessionId, _working_dir: &str, _model_id: &str) -> Result<(), StorageError> {
        self.sessions.lock().await.insert(session_id.clone(), vec![]);
        Ok(())
    }

    async fn append_event(&self, session_id: &SessionId, event: SessionEvent) -> Result<(), StorageError> {
        let mut map = self.sessions.lock().await;
        let events = map.get_mut(session_id).ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        events.push(event);
        Ok(())
    }

    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<SessionEvent>, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id).cloned().ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn replay_from(&self, session_id: &SessionId, _cursor: &Cursor) -> Result<Vec<SessionEvent>, StorageError> {
        self.replay_events(session_id).await
    }

    async fn checkpoint(&self, _session_id: &SessionId, _cursor: &Cursor) -> Result<(), StorageError> { Ok(()) }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        Ok(self.sessions.lock().await.keys().cloned().collect())
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.sessions.lock().await.remove(session_id);
        Ok(())
    }
}

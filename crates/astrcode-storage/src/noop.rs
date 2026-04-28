//! NoopEventStore — 纯内存实现，用于测试。
//!
//! 所有操作都在内存中完成，不涉及磁盘 I/O。
//! 适用于单元测试和集成测试中不需要持久化的场景。

use std::collections::HashMap;

use astrcode_core::{
    event::{Event, EventPayload},
    storage::{EventStore, StorageError},
    types::{Cursor, SessionId},
};
use tokio::sync::Mutex;

/// 纯内存的 EventStore 实现。所有操作同步完成，无磁盘 I/O。
///
/// 使用 `HashMap<SessionId, Vec<Event>>` 存储每个会话的事件列表，
/// 通过 `Mutex` 保证线程安全。
pub struct NoopEventStore {
    /// 会话事件映射，键为会话 ID，值为该会话的事件列表
    sessions: Mutex<HashMap<SessionId, Vec<Event>>>,
}

impl NoopEventStore {
    /// 创建新的空内存存储。
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for NoopEventStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl EventStore for NoopEventStore {
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&str>,
    ) -> Result<Event, StorageError> {
        let mut event = Event::new(
            session_id.clone(),
            None,
            EventPayload::SessionStarted {
                working_dir: working_dir.into(),
                model_id: model_id.into(),
                parent_session_id: parent_session_id.map(|s| s.to_string()),
            },
        );
        event.seq = Some(0);

        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), vec![event.clone()]);
        Ok(event)
    }

    async fn append_event(&self, mut event: Event) -> Result<Event, StorageError> {
        let mut map = self.sessions.lock().await;
        let events = map
            .get_mut(&event.session_id)
            .ok_or_else(|| StorageError::NotFound(event.session_id.clone()))?;
        event.seq = Some(events.len() as u64);
        events.push(event.clone());
        Ok(event)
    }

    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError> {
        let events = self.replay_events(session_id).await?;
        let Ok(seq) = cursor.parse::<u64>() else {
            return Ok(events);
        };
        Ok(events
            .into_iter()
            .filter(|event| event.seq.unwrap_or(0) >= seq)
            .collect())
    }

    async fn checkpoint(
        &self,
        _session_id: &SessionId,
        _cursor: &Cursor,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        Ok(self.sessions.lock().await.keys().cloned().collect())
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.sessions.lock().await.remove(session_id);
        Ok(())
    }
}

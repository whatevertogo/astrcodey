//! 会话模块 — server 侧的会话生命周期 façade。
//!
//! 持久事实和投影读模型由 storage 层拥有；server 只通过这里协调创建、
//! 恢复、追加事件和查询读模型。

mod payload;
pub(crate) mod spawner;

pub(crate) use payload::{compaction_applied_payload, compaction_completed_payload};

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::Event,
    storage::{
        CompactSnapshotInput, ConversationReadModel, EventStore, SessionReadModel, SessionSummary,
        StorageError,
    },
    types::*,
};
use astrcode_protocol::events::ClientNotification;
use tokio::sync::{RwLock, broadcast};

/// 活跃会话句柄。
///
/// 这里不保存读模型；读模型属于 storage projection。
pub struct Session {
    /// 会话唯一标识。
    pub id: SessionId,
    event_tx: broadcast::Sender<ClientNotification>,
}

impl Session {
    fn new(id: SessionId, capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(capacity);
        Self { id, event_tx }
    }

    /// 订阅此会话的事件广播。
    pub fn subscribe(&self) -> broadcast::Receiver<ClientNotification> {
        self.event_tx.subscribe()
    }
}

/// 会话管理器，协调活跃会话缓存与 storage 事件存储。
pub struct SessionManager {
    active: RwLock<HashMap<SessionId, Arc<Session>>>,
    store: Arc<dyn EventStore>,
}

impl SessionManager {
    /// 创建新的会话管理器。
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
        parent_session_id: Option<&str>,
    ) -> Result<Event, SessionError> {
        let sid = new_session_id();
        let event = self
            .store
            .create_session(&sid, working_dir, model_id, parent_session_id)
            .await?;
        self.active
            .write()
            .await
            .insert(sid.clone(), Arc::new(Session::new(sid, capacity)));
        Ok(event)
    }

    /// Resume a session from disk and add it to the active set.
    pub async fn resume(&self, session_id: &SessionId) -> Result<Arc<Session>, SessionError> {
        if let Some(session) = self.active.read().await.get(session_id) {
            return Ok(session.clone());
        }

        // open_session rebuilds the storage-owned projection. The active
        // Session handle only needs its broadcast channel.
        self.store.open_session(session_id).await?;
        let session = Arc::new(Session::new(session_id.clone(), 2048));
        self.active
            .write()
            .await
            .insert(session_id.clone(), session.clone());
        Ok(session)
    }

    /// Append a durable event to disk and update the storage-owned projection.
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        Ok(self.store.append_event(event).await?)
    }

    /// 返回会话读模型。
    pub async fn read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(session_id).await?)
    }

    /// 返回 conversation 当前全量快照读模型。
    pub async fn conversation_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<ConversationReadModel, SessionError> {
        Ok(self.store.conversation_snapshot(session_id).await?)
    }

    /// 返回最新 cursor。
    pub async fn latest_cursor(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(session_id).await?)
    }

    /// 写入 compact 前 transcript snapshot。
    pub async fn write_compact_snapshot(
        &self,
        session_id: &SessionId,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(session_id, snapshot)
            .await?)
    }

    /// Get active session by ID.
    pub async fn get(&self, session_id: &SessionId) -> Option<Arc<Session>> {
        self.active.read().await.get(session_id).cloned()
    }

    /// List all session IDs.
    pub async fn list(&self) -> Result<Vec<SessionId>, SessionError> {
        Ok(self.store.list_sessions().await?)
    }

    /// List all session summaries.
    pub async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionError> {
        Ok(self.store.list_session_summaries().await?)
    }

    /// Delete session from memory and disk.
    pub async fn delete(&self, session_id: &SessionId) -> Result<(), SessionError> {
        self.active.write().await.remove(session_id);
        self.store.delete_session(session_id).await?;
        Ok(())
    }
}

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}

//! 会话模块 — server 侧的会话生命周期 façade。
//!
//! 持久事实和投影读模型由 storage 层拥有；server 只通过这里协调创建、
//! 恢复、追加事件和查询读模型。

mod payload;
pub(crate) mod spawner;

use std::{collections::HashMap, sync::Arc};

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::{Event, EventPayload},
    storage::{
        CompactSnapshotInput, ConversationReadModel, EventStore, SessionReadModel, SessionSummary,
        StorageError, ToolResultArtifactInput, ToolResultArtifactReader, ToolResultArtifactRef,
        ToolResultArtifactSlice,
    },
    types::*,
};
use astrcode_protocol::events::ClientNotification;
pub(crate) use payload::{compact_boundary_payload, session_continued_from_compaction_payload};
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

    /// 为当前 projection cursor 写入恢复 checkpoint。
    ///
    /// 只有当传入 cursor 与当前 recovered projection cursor 匹配时，才会
    /// 生成持久化快照。这样可以避免写入过时的恢复点。
    pub async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), SessionError> {
        Ok(self.store.checkpoint(session_id, cursor).await?)
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

    /// 写入当前 session 的大工具结果 artifact。
    pub async fn write_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, SessionError> {
        Ok(self
            .store
            .write_tool_result_artifact(session_id, artifact)
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

#[async_trait::async_trait]
impl ToolResultArtifactReader for SessionManager {
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        self.store
            .read_tool_result_artifact_by_path(session_id, path, char_offset, max_chars)
            .await
    }
}

/// compact continuation child 的创建输入。
pub(crate) struct CompactContinuationCreateInput {
    pub(crate) parent_session_id: SessionId,
    pub(crate) working_dir: String,
    pub(crate) model_id: String,
}

/// 已创建但尚未追加 continuation 事件的 child session。
pub(crate) struct CompactContinuationSession {
    pub(crate) parent_session_id: SessionId,
    pub(crate) parent_cursor: Cursor,
    pub(crate) child_session_id: SessionId,
    pub(crate) child_started: Event,
}

/// compact continuation durable events 的追加输入。
pub(crate) struct CompactContinuationAppendInput {
    pub(crate) session: CompactContinuationSession,
    pub(crate) system_prompt: String,
    pub(crate) system_prompt_fingerprint: String,
    pub(crate) trigger_name: String,
    pub(crate) compaction: CompactResult,
}

/// compact continuation 写入后产生的事件。
pub(crate) struct CompactContinuationEvents {
    pub(crate) child_session_id: SessionId,
    pub(crate) appended_events: Vec<Event>,
}

/// 只负责创建 compact continuation child。
///
/// 这里不广播、不切换 active session、不发送 SessionResumed，也不追加
/// continuation events；这些副作用由
/// CommandHandler 或 ServerSessionSpawner 这样的 owner 自己决定。
pub(crate) async fn create_compact_continuation_session(
    session_manager: &SessionManager,
    input: CompactContinuationCreateInput,
) -> Result<CompactContinuationSession, String> {
    let parent_cursor = session_manager
        .latest_cursor(&input.parent_session_id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "0".into());
    let child_started = session_manager
        .create(
            &input.working_dir,
            &input.model_id,
            2048,
            Some(input.parent_session_id.as_str()),
        )
        .await
        .map_err(|e| e.to_string())?;
    let child_session_id = child_started.session_id.clone();

    Ok(CompactContinuationSession {
        parent_session_id: input.parent_session_id,
        parent_cursor,
        child_session_id,
        child_started,
    })
}

/// 只负责向已创建的 continuation child 追加 durable continuation events。
///
/// Parent 和 child 分属不同 event log，v1 不提供跨 session 事务。
/// 调用方应把这里的失败视为可重试的半持久状态：child session 可能已经
/// 存在，或者已经写入了部分 child-side setup events。
pub(crate) async fn append_compact_continuation_events(
    session_manager: &SessionManager,
    input: CompactContinuationAppendInput,
) -> Result<CompactContinuationEvents, String> {
    let child_session_id = input.session.child_session_id.clone();
    let mut appended_events = Vec::with_capacity(3);
    appended_events.push(
        session_manager
            .append_event(Event::new(
                child_session_id.clone(),
                None,
                EventPayload::SystemPromptConfigured {
                    text: input.system_prompt,
                    fingerprint: input.system_prompt_fingerprint,
                },
            ))
            .await
            .map_err(|e| e.to_string())?,
    );
    appended_events.push(
        session_manager
            .append_event(Event::new(
                input.session.parent_session_id.clone(),
                None,
                compact_boundary_payload(
                    input.trigger_name,
                    &input.compaction,
                    child_session_id.clone(),
                ),
            ))
            .await
            .map_err(|e| e.to_string())?,
    );
    appended_events.push(
        session_manager
            .append_event(Event::new(
                child_session_id.clone(),
                None,
                session_continued_from_compaction_payload(
                    input.session.parent_session_id,
                    input.session.parent_cursor,
                    &input.compaction,
                ),
            ))
            .await
            .map_err(|e| e.to_string())?,
    );

    if let Some(cursor) = session_manager
        .latest_cursor(&child_session_id)
        .await
        .map_err(|e| e.to_string())?
    {
        session_manager
            .checkpoint(&child_session_id, &cursor)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(CompactContinuationEvents {
        child_session_id,
        appended_events,
    })
}

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}

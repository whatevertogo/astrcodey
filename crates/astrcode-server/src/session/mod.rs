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
        CompactSnapshotInput, EventStore, SessionReadModel, SessionSummary, StorageError,
        ToolResultArtifactInput, ToolResultArtifactReader, ToolResultArtifactRef,
        ToolResultArtifactSlice,
    },
    types::*,
};
pub(crate) use payload::{
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
    compact_boundary_payload, session_continued_from_compaction_payload,
};
use tokio::sync::RwLock;

/// 活跃会话句柄。
///
/// 这里不保存读模型；读模型属于 storage projection。
pub struct Session {
    /// 会话唯一标识。
    pub id: SessionId,
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
        parent_session_id: Option<&SessionId>,
    ) -> Result<Event, SessionError> {
        let sid = new_session_id();
        let event = self
            .store
            .create_session(&sid, working_dir, model_id, parent_session_id)
            .await?;
        self.active
            .write()
            .await
            .insert(sid.clone(), Arc::new(Session { id: sid }));
        Ok(event)
    }

    /// Resume a session from disk and add it to the active set.
    pub async fn resume(&self, session_id: &SessionId) -> Result<Arc<Session>, SessionError> {
        if let Some(session) = self.active.read().await.get(session_id) {
            return Ok(session.clone());
        }

        // open_session rebuilds the storage-owned projection.
        self.store.open_session(session_id).await?;
        let session = Arc::new(Session {
            id: session_id.clone(),
        });
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

    /// 返回最新 cursor。
    pub async fn latest_cursor(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(session_id).await?)
    }

    /// 从指定 cursor 之后重放 durable 事件。
    pub async fn replay_after(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, SessionError> {
        Ok(self.store.replay_from(session_id, cursor).await?)
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

/// 同会话 compact 追加输入。
pub(crate) struct SameSessionCompactionInput {
    pub(crate) session_id: SessionId,
    pub(crate) system_prompt: String,
    pub(crate) system_prompt_fingerprint: String,
    pub(crate) trigger_name: String,
    pub(crate) compaction: CompactResult,
}

/// 向同一个 session 追加 compact boundary 和 continuation 事件。
///
/// 不创建 child session。两个事件都使用 `session_id` 作为所属会话。
/// Projection reducer 会在遇到 `SessionContinuedFromCompaction` 时
/// 将 messages 替换为 compacted view，旧事件在投影层面被丢弃。
pub(crate) async fn append_same_session_compaction(
    session_manager: &SessionManager,
    input: SameSessionCompactionInput,
) -> Result<Vec<Event>, String> {
    let session_id = input.session_id;

    // 在追加任何 compact 事件前捕获 cursor，作为旧 transcript 的边界。
    let parent_cursor = session_manager
        .latest_cursor(&session_id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "0".into());

    let mut appended = Vec::with_capacity(3);

    // 1. CompactBoundaryCreated — boundary seq 标记旧 transcript 截止点
    appended.push(
        session_manager
            .append_event(Event::new(
                session_id.clone(),
                None,
                compact_boundary_payload(input.trigger_name, &input.compaction, session_id.clone()),
            ))
            .await
            .map_err(|e| e.to_string())?,
    );

    // 2. SystemPromptConfigured — boundary 之后刷新 prompt
    appended.push(
        session_manager
            .append_event(Event::new(
                session_id.clone(),
                None,
                EventPayload::SystemPromptConfigured {
                    text: input.system_prompt,
                    fingerprint: input.system_prompt_fingerprint,
                },
            ))
            .await
            .map_err(|e| e.to_string())?,
    );

    // 3. SessionContinuedFromCompaction — 替换 messages 为 compacted view
    appended.push(
        session_manager
            .append_event(Event::new(
                session_id.clone(),
                None,
                session_continued_from_compaction_payload(
                    session_id.clone(),
                    parent_cursor,
                    &input.compaction,
                ),
            ))
            .await
            .map_err(|e| e.to_string())?,
    );

    if let Some(cursor) = session_manager
        .latest_cursor(&session_id)
        .await
        .map_err(|e| e.to_string())?
    {
        session_manager
            .checkpoint(&session_id, &cursor)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(appended)
}

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}

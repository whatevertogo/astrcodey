//! Session 句柄 — 带存储能力的会话操作入口。
//!
//! Session 是系统唯一的持久事实来源。所有关键状态变化以不可变事件
//! 写入持久层，任何时刻都可通过事件日志和快照重建 session 状态。
//!
//! 内部 runtime 通过此类型操作会话；插件侧通过 `AgentSessionControl` trait
//! 的受限接口操作会话。

use std::sync::Arc;

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::{Event, EventPayload},
    storage::{
        CompactSnapshotInput, EventStore, SessionReadModel, StorageError, ToolResultArtifactInput,
        ToolResultArtifactReader, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::*,
};

use crate::payload::{compact_boundary_payload, session_continued_from_compaction_payload};

/// 会话句柄 — 带存储能力的会话操作入口。
///
/// 内部 runtime 通过此类型操作会话（追加事件、查询读模型等）。
/// 插件侧通过 `AgentSessionControl` trait 的受限接口。
///
/// `Clone` 是廉价的 Arc clone，可以自由复制。
#[derive(Clone)]
pub struct Session {
    id: SessionId,
    store: Arc<dyn EventStore>,
}

impl Session {
    /// 创建新会话，持久化 `SessionStarted` 事件。
    pub async fn create(
        store: Arc<dyn EventStore>,
        working_dir: &str,
        model_id: &str,
        parent: Option<&SessionId>,
    ) -> Result<Self, SessionError> {
        let sid = new_session_id();
        store
            .create_session(&sid, working_dir, model_id, parent)
            .await?;
        Ok(Self { id: sid, store })
    }

    /// 从磁盘恢复已有会话。
    ///
    /// 幂等操作：存储层已缓存会话时不产生额外 I/O。
    pub async fn open(store: Arc<dyn EventStore>, id: SessionId) -> Result<Self, SessionError> {
        store.open_session(&id).await?;
        Ok(Self { id, store })
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// 返回底层事件存储引用（用于集合操作如 list/delete）。
    pub fn store(&self) -> &Arc<dyn EventStore> {
        &self.store
    }

    // ─── 事件操作 ──────────────────────────────────────────────────────

    /// 追加持久事件到事件日志，分配递增序号。
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        Ok(self.store.append_event(event).await?)
    }

    /// 返回会话读模型。
    pub async fn read_model(&self) -> Result<SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(&self.id).await?)
    }

    /// 返回最新 durable cursor。
    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(&self.id).await?)
    }

    /// 从指定 cursor 之后重放 durable 事件。
    #[allow(dead_code)]
    pub async fn replay_after(&self, cursor: &Cursor) -> Result<Vec<Event>, SessionError> {
        Ok(self.store.replay_from(&self.id, cursor).await?)
    }

    /// 为当前 projection cursor 写入恢复 checkpoint。
    pub async fn checkpoint(&self, cursor: &Cursor) -> Result<(), SessionError> {
        Ok(self.store.checkpoint(&self.id, cursor).await?)
    }

    // ─── Artifact 操作 ─────────────────────────────────────────────────

    /// 写入 compact 前 transcript snapshot。
    pub async fn write_compact_snapshot(
        &self,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(&self.id, snapshot)
            .await?)
    }

    /// 写入大工具结果 artifact。
    pub async fn write_tool_artifact(
        &self,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, SessionError> {
        Ok(self
            .store
            .write_tool_result_artifact(&self.id, artifact)
            .await?)
    }
}

#[async_trait::async_trait]
impl ToolResultArtifactReader for Session {
    async fn read_tool_result_artifact_by_path(
        &self,
        _session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        self.store
            .read_tool_result_artifact_by_path(&self.id, path, char_offset, max_chars)
            .await
    }
}

// ─── Same-session compaction ────────────────────────────────────────────

/// 同会话 compact 追加输入。
pub struct SameSessionCompactionInput {
    pub session_id: SessionId,
    pub system_prompt: String,
    pub system_prompt_fingerprint: String,
    pub trigger_name: String,
    pub compaction: CompactResult,
}

/// 向同一个 session 追加 compact boundary 和 continuation 事件。
///
/// 不创建 child session。两个事件都使用 `session.id()` 作为所属会话。
/// Projection reducer 会在遇到 `SessionContinuedFromCompaction` 时
/// 将 messages 替换为 compacted view，旧事件在投影层面被丢弃。
pub async fn append_same_session_compaction(
    session: &Session,
    input: SameSessionCompactionInput,
) -> Result<Vec<Event>, String> {
    let session_id = input.session_id;

    let parent_cursor = session
        .latest_cursor()
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "0".into());

    let mut appended = Vec::with_capacity(3);

    appended.push(
        session
            .append_event(Event::new(
                session_id.clone(),
                None,
                compact_boundary_payload(input.trigger_name, &input.compaction, session_id.clone()),
            ))
            .await
            .map_err(|e| e.to_string())?,
    );

    appended.push(
        session
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

    appended.push(
        session
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

    if let Some(cursor) = session.latest_cursor().await.map_err(|e| e.to_string())? {
        session
            .checkpoint(&cursor)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(appended)
}

// ─── SessionError ───────────────────────────────────────────────────────

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}

//! 会话模块 — Session 作为带存储能力的会话句柄。
//!
//! 持久事实和投影读模型由 storage 层拥有；Session 是操作入口，
//! 内部 runtime 通过它追加事件、查询读模型、写入 artifact。
//! 插件侧通过 `AgentSessionControl` trait 的受限接口操作会话。

mod payload;
pub(crate) mod spawner;

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
pub(crate) use payload::{
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
    compact_boundary_payload, session_continued_from_compaction_payload,
};

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

    // ─── 事件操作（crate 内部） ─────────────────────────────────────────

    /// 追加持久事件到事件日志，分配递增序号。
    pub(crate) async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        Ok(self.store.append_event(event).await?)
    }

    /// 返回会话读模型。
    pub async fn read_model(&self) -> Result<SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(&self.id).await?)
    }

    /// 返回最新 durable cursor。
    pub(crate) async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(&self.id).await?)
    }

    /// 从指定 cursor 之后重放 durable 事件。
    #[expect(dead_code, reason = "将由 collaboration/http 模块在后续迭代中使用")]
    pub(crate) async fn replay_after(&self, cursor: &Cursor) -> Result<Vec<Event>, SessionError> {
        Ok(self.store.replay_from(&self.id, cursor).await?)
    }

    /// 为当前 projection cursor 写入恢复 checkpoint。
    pub(crate) async fn checkpoint(&self, cursor: &Cursor) -> Result<(), SessionError> {
        Ok(self.store.checkpoint(&self.id, cursor).await?)
    }

    // ─── Artifact 操作（crate 内部） ────────────────────────────────────

    /// 写入 compact 前 transcript snapshot。
    pub(crate) async fn write_compact_snapshot(
        &self,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(&self.id, snapshot)
            .await?)
    }

    /// 写入大工具结果 artifact。
    pub(crate) async fn write_tool_artifact(
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
/// 不创建 child session。两个事件都使用 `session.id()` 作为所属会话。
/// Projection reducer 会在遇到 `SessionContinuedFromCompaction` 时
/// 将 messages 替换为 compacted view，旧事件在投影层面被丢弃。
pub(crate) async fn append_same_session_compaction(
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

    // 1. CompactBoundaryCreated
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

    // 2. SystemPromptConfigured
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

    // 3. SessionContinuedFromCompaction
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

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}

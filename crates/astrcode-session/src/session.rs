//! Session 句柄 — 带存储能力的会话操作入口。
//!
//! Session 是系统唯一的持久事实来源。所有关键状态变化以不可变事件
//! 写入持久层，任何时刻都可通过事件日志和快照重建 session 状态。
//!
//! 内部 runtime 通过此类型操作会话。

use std::sync::Arc;

use astrcode_core::{
    event::{Event, EventPayload},
    storage::{
        CompactSnapshotInput, EventStore, SessionReadModel, StorageError, ToolResultArtifactInput,
        ToolResultArtifactReader, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::*,
};

use crate::compact::{compact_boundary_payload, session_continued_from_compaction_payload};

/// 会话句柄 — 带存储能力的会话操作入口。
///
/// 内部 runtime 通过此类型操作会话（追加事件、查询读模型等）。
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

    // ─── 事件操作 ──────────────────────────────────────────────────────

    /// 追加持久事件到事件日志，分配递增序号。
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        Ok(self.store.append_event(event).await?)
    }

    /// 更新会话使用的模型标识。
    ///
    /// 仅在 model_id 与当前值不同时写入 `ModelIdChanged` 事件，避免冗余事件。
    pub async fn update_model_id(&self, model_id: &str) -> Result<Option<Event>, SessionError> {
        let current = self.read_model().await?;
        if current.model_id == model_id {
            return Ok(None);
        }
        self.append_event(Event::new(
            self.id.clone(),
            None,
            EventPayload::ModelIdChanged {
                model_id: model_id.to_string(),
            },
        ))
        .await
        .map(Some)
    }

    /// 返回会话读模型。
    pub async fn read_model(&self) -> Result<SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(&self.id).await?)
    }

    /// 返回当前 system_prompt，只读单个字段避免 clone 整个读模型。
    pub async fn current_system_prompt(&self) -> Result<Option<String>, SessionError> {
        Ok(self.store.session_system_prompt(&self.id).await?)
    }

    /// 返回最新 durable cursor。
    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(&self.id).await?)
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

// ─── Compact ────────────────────────────────────────────────────────

impl Session {
    pub async fn append_compact_boundary(
        &self,
        system_prompt: String,
        fingerprint: String,
        trigger_name: String,
        compaction: astrcode_context::compaction::CompactResult,
    ) -> Result<Vec<Event>, SessionError> {
        let cursor = self.latest_cursor().await?.unwrap_or_else(|| "0".into());
        let mut events = Vec::with_capacity(3);
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                compact_boundary_payload(trigger_name, &compaction, self.id.clone()),
            ))
            .await?,
        );
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                EventPayload::SystemPromptConfigured {
                    text: system_prompt,
                    fingerprint,
                },
            ))
            .await?,
        );
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                session_continued_from_compaction_payload(self.id.clone(), cursor, &compaction),
            ))
            .await?,
        );
        if let Some(cursor) = self.latest_cursor().await? {
            self.checkpoint(&cursor).await?;
        }
        Ok(events)
    }
} // ─── SessionError ───────────────────────────────────────────────────────

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("{0}")]
    Other(String),
}

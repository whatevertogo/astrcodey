use std::{path::PathBuf, sync::Arc};

use astrcode_core::{
    event::{DurableEvent, StoredEvent},
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId},
};
use astrcode_session_projection::{AgentSessionLinkView, SessionReadModel, SessionSummary};

use crate::{CompactSnapshotInput, StorageError, ToolResultArtifactInput, ToolResultArtifactRef};

#[async_trait::async_trait]
pub trait EventReader: Send + Sync {
    async fn replay_events(&self, session_id: &SessionId)
    -> Result<Vec<StoredEvent>, StorageError>;

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError>;

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<StoredEvent>, StorageError>;

    async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let mut events = self.replay_from(session_id, cursor).await?;
        events.truncate(max_events);
        Ok(events)
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError>;
}

#[async_trait::async_trait]
pub trait SessionReader: Send + Sync {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError>;

    async fn session_has_messages(&self, session_id: &SessionId) -> Result<bool, StorageError> {
        Ok(self.session_read_model(session_id).await?.has_messages())
    }

    async fn session_agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AgentSessionLinkView>, StorageError> {
        Ok(self
            .session_read_model(session_id)
            .await?
            .agent_sessions
            .clone())
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError>;
}

#[async_trait::async_trait]
pub trait SessionPathResolver: Send + Sync {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, StorageError>;

    async fn planned_session_store_dir(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        parent_session_id: Option<&SessionId>,
        source_extension: Option<&str>,
    ) -> Result<Option<PathBuf>, StorageError>;
}

#[async_trait::async_trait]
pub trait ToolResultArtifactStore: Send + Sync {
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError>;

    async fn write_tool_result_artifact(
        &self,
        _session_id: &SessionId,
        _artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        Err(StorageError::Unsupported(
            "tool result artifact storage is not supported".into(),
        ))
    }
}

/// Durable session event 的存储写端口。
#[async_trait::async_trait]
pub trait SessionEventJournal: Send + Sync {
    /// 原子创建 session 并提交 seq=0 的 `SessionStarted`。
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError>;

    async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError>;

    async fn sync_durable_events(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Ok(())
    }
}

/// 完整 session repository 的组合入口。
///
/// 独立调用方使用上面的 reader/journal 窄端口；生命周期、checkpoint 和辅助快照只由
/// repository 门面使用，不再为它们各建一个单实现 trait。
#[async_trait::async_trait]
pub trait SessionStore:
    EventReader + SessionEventJournal + SessionReader + SessionPathResolver + ToolResultArtifactStore
{
    async fn checkpoint(&self, session_id: &SessionId, cursor: &Cursor)
    -> Result<(), StorageError>;

    async fn open_session(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Ok(())
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError>;

    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        tracing::warn!(
            session_id = %session_id,
            "SessionStore::recycle_session fell back to delete_session; this storage implementation does not preserve recycled session data"
        );
        self.delete_session(session_id).await
    }

    async fn restore_session(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Err(StorageError::Unsupported(
            "restore_session is not supported by this storage implementation".into(),
        ))
    }

    async fn write_compact_snapshot(
        &self,
        _session_id: &SessionId,
        _snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, StorageError> {
        Ok(None)
    }
}

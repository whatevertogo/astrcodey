use std::{path::PathBuf, sync::Arc};

use astrcode_core::{
    event::{DurableEvent, StoredEvent},
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId},
};
use astrcode_session_projection::{AgentSessionLinkView, SessionReadModel, SessionSummary};

use crate::{CompactSnapshotInput, StorageError, ToolResultArtifactInput, ToolResultArtifactRef};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventConsumerState {
    pub checkpoint: Option<u64>,
    pub paused: bool,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventConsumerCheckpointReset {
    Beginning,
    StreamHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventConsumerCheckpointOutcome {
    Accepted,
    StaleRevision,
}

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

    /// Rebuild a recycled session without making it active or registering runtime state.
    async fn recycled_session_read_model(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        Err(StorageError::Unsupported(
            "reading recycled sessions is not supported by this storage implementation".into(),
        ))
    }

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

    /// Atomically append one or more consecutive events from the same session.
    ///
    /// Implementations must commit the whole batch or leave both the event log and projection
    /// unchanged. Sequence numbers are assigned in input order, and the returned batch has
    /// exactly one stored event per input event.
    async fn append_events(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError>;

    async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        self.append_events(vec![event])
            .await?
            .pop()
            .ok_or_else(crate::error::short_batch_result)
    }

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
    async fn event_consumer_state(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
    ) -> Result<EventConsumerState, StorageError>;

    /// Monotonically advances an at-least-once event consumer checkpoint.
    async fn checkpoint_event_consumer(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
    ) -> Result<EventConsumerCheckpointOutcome, StorageError>;

    async fn set_event_consumer_paused(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        paused: bool,
    ) -> Result<EventConsumerState, StorageError>;

    async fn reset_event_consumer_checkpoint(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        reset: EventConsumerCheckpointReset,
    ) -> Result<EventConsumerState, StorageError>;

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

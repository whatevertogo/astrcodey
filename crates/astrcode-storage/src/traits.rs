use std::path::PathBuf;

use astrcode_core::{
    event::Event,
    llm::LlmMessage,
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId},
};
use astrcode_session_projection::{AgentSessionLinkView, SessionReadModel, SessionSummary};

use crate::{CompactSnapshotInput, StorageError, ToolResultArtifactInput, ToolResultArtifactRef};

#[async_trait::async_trait]
pub trait EventReader: Send + Sync {
    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError>;

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError>;

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError>;

    async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<Event>, StorageError> {
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
    ) -> Result<SessionReadModel, StorageError>;

    async fn session_provider_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<LlmMessage>, StorageError> {
        Ok(self
            .session_read_model(session_id)
            .await?
            .provider_messages())
    }

    async fn session_system_prompt(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<String>, StorageError>;

    async fn session_has_messages(&self, session_id: &SessionId) -> Result<bool, StorageError> {
        Ok(self.session_read_model(session_id).await?.has_messages())
    }

    async fn session_agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AgentSessionLinkView>, StorageError> {
        Ok(self.session_read_model(session_id).await?.agent_sessions)
    }

    async fn session_visible_user_message_count(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, StorageError> {
        Ok(self
            .session_read_model(session_id)
            .await?
            .visible_user_message_count())
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError>;
}

#[async_trait::async_trait]
pub trait SessionPathResolver: Send + Sync {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
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

#[async_trait::async_trait]
pub trait EventStore: EventReader + Send + Sync {
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&SessionId>,
        tool_selection: Option<&astrcode_core::tool::SessionToolSelection>,
        source_extension: Option<&str>,
    ) -> Result<Event, StorageError>;

    async fn append_event(&self, event: Event) -> Result<Event, StorageError>;

    async fn checkpoint(&self, session_id: &SessionId, cursor: &Cursor)
    -> Result<(), StorageError>;

    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.replay_events(session_id).await.map(|_| ())
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError>;

    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        tracing::warn!(
            session_id = %session_id,
            "EventStore::recycle_session fell back to delete_session; this storage implementation does not preserve recycled session data"
        );
        self.delete_session(session_id).await
    }

    async fn restore_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let _ = session_id;
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

    async fn sync_durable_events(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Ok(())
    }
}

pub trait SessionStore:
    EventStore + SessionReader + SessionPathResolver + ToolResultArtifactStore + Send + Sync
{
}

impl<T> SessionStore for T where
    T: EventStore + SessionReader + SessionPathResolver + ToolResultArtifactStore + Send + Sync
{
}

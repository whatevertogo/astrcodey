//! Session 存储读写 — event store 边界操作。

use astrcode_core::{
    storage::{
        CompactSnapshotInput, SessionReadModel, ToolResultArtifactInput, ToolResultArtifactRef,
    },
    types::Cursor,
};

use super::{Session, SessionError};

impl Session {
    pub async fn read_model(&self) -> Result<SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(&self.id).await?)
    }

    pub async fn current_system_prompt(&self) -> Result<Option<String>, SessionError> {
        Ok(self.store.session_system_prompt(&self.id).await?)
    }

    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(&self.id).await?)
    }

    pub async fn checkpoint(&self, cursor: &Cursor) -> Result<(), SessionError> {
        Ok(self.store.checkpoint(&self.id, cursor).await?)
    }

    pub async fn write_compact_snapshot(
        &self,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(&self.id, snapshot)
            .await?)
    }

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

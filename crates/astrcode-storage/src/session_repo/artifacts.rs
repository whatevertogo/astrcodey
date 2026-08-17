//! Tool result artifact 端口实现:[`ToolResultArtifactStore`]。

use astrcode_core::{tool::ToolResultArtifactSlice, types::SessionId};

use super::FileSystemSessionRepository;
use crate::{
    StorageError, ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactStore,
    tool_artifacts::{
        read_tool_result_file, validate_tool_result_artifact_id, write_tool_result_file,
    },
};

#[async_trait::async_trait]
impl ToolResultArtifactStore for FileSystemSessionRepository {
    async fn read_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
        byte_offset: usize,
        max_bytes: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        validate_tool_result_artifact_id(artifact_id)
            .map_err(|message| StorageError::InvalidId(message.into()))?;
        let artifact_dir = tokio::fs::canonicalize(meta.canonical_dir.join("tool-results")).await?;
        let path = artifact_dir.join(artifact_id);
        let canonical_path = match tokio::fs::canonicalize(&path).await {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(session_id.clone()));
            },
            Err(error) => return Err(StorageError::Io(error)),
        };
        if !artifact_dir.starts_with(&meta.canonical_dir)
            || !canonical_path.starts_with(&artifact_dir)
        {
            return Err(StorageError::InvalidId(
                "tool result artifact resolves outside this session artifact directory".into(),
            ));
        }
        let artifact_id = artifact_id.to_owned();
        crate::durable_write::spawn_blocking_storage("tool result artifact reader", move || {
            read_tool_result_file(&canonical_path, &artifact_id, byte_offset, max_bytes)
                .map_err(StorageError::Io)
        })
        .await
    }

    async fn write_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let dir = meta.dir.join("tool-results");
        crate::durable_write::spawn_blocking_storage("tool result artifact writer", move || {
            write_tool_result_file(&dir, &artifact).map_err(StorageError::Io)
        })
        .await
    }
}

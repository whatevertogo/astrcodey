//! Test-only storage construction and fault injection.

use std::path::PathBuf;

use astrcode_core::types::SessionId;

use crate::{StorageError, session_repo::FileSystemSessionRepository};

pub fn filesystem_session_repository(projects_base: PathBuf) -> FileSystemSessionRepository {
    FileSystemSessionRepository::with_projects_base(projects_base)
}

pub async fn fail_next_durable_sync(
    repository: &FileSystemSessionRepository,
    session_id: &SessionId,
) -> Result<(), StorageError> {
    repository.fail_next_durable_sync(session_id).await
}

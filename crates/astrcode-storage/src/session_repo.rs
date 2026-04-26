//! File-system session repository implementing the EventStore trait.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use astrcode_core::storage::{EventStore, SessionEvent, SessionInfo, StorageError};
use astrcode_core::types::{validate_session_id, Cursor, ProjectHash, SessionId};
use astrcode_support::hostpaths;

use crate::event_log::EventLog;
use crate::lock::TurnLock;
use crate::snapshot::SnapshotManager;

/// File-system session repository.
///
/// Manages session event logs organized by project:
/// `~/.astrcode/projects/<project>/sessions/<session>/`
pub struct FileSystemSessionRepository {
    sessions: Arc<RwLock<HashMap<SessionId, SessionMeta>>>,
    project_hash: ProjectHash,
    base_path: PathBuf,
}

struct SessionMeta {
    log: EventLog,
    snapshot_mgr: SnapshotManager,
    lock: TurnLock,
    working_dir: String,
    model_id: String,
}

impl FileSystemSessionRepository {
    pub fn new(project_hash: ProjectHash) -> Self {
        let base_path = hostpaths::sessions_dir(&project_hash);
        std::fs::create_dir_all(&base_path).ok();
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            project_hash,
            base_path,
        }
    }

    fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.base_path.join(id)
    }

    fn event_log_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join(format!("session-{}.jsonl", id))
    }
}

#[async_trait::async_trait]
impl EventStore for FileSystemSessionRepository {
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
    ) -> Result<(), StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;

        let start_event = SessionEvent::SessionStart {
            session_id: session_id.clone(),
            timestamp: chrono::Utc::now(),
            working_dir: working_dir.into(),
            model_id: model_id.into(),
        };

        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir)?;

        let log = EventLog::create(self.event_log_path(session_id), &start_event).await?;
        let snapshot_mgr = SnapshotManager::new(dir.join("snapshots"));
        let lock = TurnLock::new(dir.join("active-turn.lock"));

        let mut sessions = self.sessions.write().await;
        sessions.insert(
            session_id.clone(),
            SessionMeta {
                log,
                snapshot_mgr,
                lock,
                working_dir: working_dir.into(),
                model_id: model_id.into(),
            },
        );
        Ok(())
    }

    async fn append_event(
        &self,
        session_id: &SessionId,
        event: SessionEvent,
    ) -> Result<(), StorageError> {
        let sessions = self.sessions.read().await;
        let meta = sessions
            .get(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        meta.log.append(&event).await
    }

    async fn replay_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionEvent>, StorageError> {
        let path = self.event_log_path(session_id);
        let log = EventLog::open(path).await?;
        log.replay_all().await
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        _cursor: &Cursor,
    ) -> Result<Vec<SessionEvent>, StorageError> {
        // TODO: Implement cursor-based incremental replay
        // For now, replay all events
        self.replay_events(session_id).await
    }

    async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), StorageError> {
        let sessions = self.sessions.read().await;
        let meta = sessions
            .get(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        // TODO: Capture actual session state snapshot
        meta.snapshot_mgr.create_snapshot(cursor).await?;
        Ok(())
    }

    /// Open a session from disk and register its SessionMeta for future appends.
    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(session_id) { return Ok(()); }

        let log = EventLog::open(self.event_log_path(session_id)).await?;
        let snapshot_mgr = SnapshotManager::new(self.session_dir(session_id).join("snapshots"));
        let lock = TurnLock::new(self.session_dir(session_id).join("active-turn.lock"));

        // Read SessionStart to get working_dir and model_id
        let events = log.replay_all().await?;
        let (working_dir, model_id) = events.first().and_then(|e| match e {
            astrcode_core::storage::SessionEvent::SessionStart { working_dir, model_id, .. } =>
                Some((working_dir.clone(), model_id.clone())),
            _ => None,
        }).unwrap_or_default();

        sessions.insert(session_id.clone(), SessionMeta { log, snapshot_mgr, lock, working_dir, model_id });
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        let sessions = self.sessions.read().await;
        Ok(sessions.keys().cloned().collect())
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;

        let mut sessions = self.sessions.write().await;
        sessions.remove(session_id);
        let dir = self.session_dir(session_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

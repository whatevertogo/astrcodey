//! File-system session repository implementing the EventStore trait.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use astrcode_core::{
    event::{Event, EventPayload},
    storage::{EventStore, StorageError},
    types::{Cursor, ProjectHash, SessionId, validate_session_id},
};
use astrcode_support::hostpaths;
use tokio::sync::RwLock;

use crate::{event_log::EventLog, lock::TurnLock, snapshot::SnapshotManager};

/// File-system session repository.
///
/// Manages session event logs organized by project:
/// `~/.astrcode/projects/<project>/sessions/<session>/`
pub struct FileSystemSessionRepository {
    sessions: Arc<RwLock<HashMap<SessionId, SessionMeta>>>,
    _project_hash: ProjectHash,
    base_path: PathBuf,
}

struct SessionMeta {
    log: EventLog,
    snapshot_mgr: SnapshotManager,
    _lock: TurnLock,
    _working_dir: String,
    _model_id: String,
}

impl FileSystemSessionRepository {
    pub fn new(project_hash: ProjectHash) -> Self {
        let base_path = hostpaths::sessions_dir(&project_hash);
        std::fs::create_dir_all(&base_path).ok();
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            _project_hash: project_hash,
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
    ) -> Result<Event, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;

        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir)?;

        let start_event = Event::new(
            session_id.clone(),
            None,
            EventPayload::SessionStarted {
                working_dir: working_dir.into(),
                model_id: model_id.into(),
            },
        );

        let (log, stored_event) =
            EventLog::create(self.event_log_path(session_id), start_event).await?;
        let snapshot_mgr = SnapshotManager::new(dir.join("snapshots"));
        let lock = TurnLock::new(dir.join("active-turn.lock"));

        let mut sessions = self.sessions.write().await;
        sessions.insert(
            session_id.clone(),
            SessionMeta {
                log,
                snapshot_mgr,
                _lock: lock,
                _working_dir: working_dir.into(),
                _model_id: model_id.into(),
            },
        );
        Ok(stored_event)
    }

    async fn append_event(&self, event: Event) -> Result<Event, StorageError> {
        let session_id = event.session_id.clone();
        if !self.sessions.read().await.contains_key(&session_id) {
            self.open_session(&session_id).await?;
        }

        let sessions = self.sessions.read().await;
        let meta = sessions
            .get(&session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        meta.log.append(event).await
    }

    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError> {
        let path = self.event_log_path(session_id);
        let log = EventLog::open(path).await?;
        log.replay_all().await
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError> {
        let events = self.replay_events(session_id).await?;
        let Ok(seq) = cursor.parse::<u64>() else {
            return Ok(events);
        };
        Ok(events
            .into_iter()
            .filter(|event| event.seq.unwrap_or(0) >= seq)
            .collect())
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
        // TODO: Capture actual session state snapshot.
        meta.snapshot_mgr.create_snapshot(cursor).await?;
        Ok(())
    }

    /// Open a session from disk and register its SessionMeta for future appends.
    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(session_id) {
            return Ok(());
        }

        let log = EventLog::open(self.event_log_path(session_id)).await?;
        let snapshot_mgr = SnapshotManager::new(self.session_dir(session_id).join("snapshots"));
        let lock = TurnLock::new(self.session_dir(session_id).join("active-turn.lock"));

        let events = log.replay_all().await?;
        let (working_dir, model_id) = events
            .first()
            .and_then(|event| match &event.payload {
                EventPayload::SessionStarted {
                    working_dir,
                    model_id,
                } => Some((working_dir.clone(), model_id.clone())),
                _ => None,
            })
            .unwrap_or_default();

        sessions.insert(
            session_id.clone(),
            SessionMeta {
                log,
                snapshot_mgr,
                _lock: lock,
                _working_dir: working_dir,
                _model_id: model_id,
            },
        );
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        let mut ids: Vec<SessionId> = self.sessions.read().await.keys().cloned().collect();
        if self.base_path.exists() {
            for entry in std::fs::read_dir(&self.base_path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let id = entry.file_name().to_string_lossy().to_string();
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        ids.sort();
        Ok(ids)
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

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::{
    DeleteProjectResult, Result, SessionId, SessionMeta, SessionTurnAcquireResult, StorageEvent,
    StoredEvent,
    store::{EventLogWriter, SessionManager, StoreResult},
};
use astrcode_host_session::ports::{EventStore, RecoveredSessionState, SessionRecoveryCheckpoint};
use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{
    batch_appender::{BatchAppender, SharedAppenderRegistry},
    checkpoint,
    event_log::EventLog,
    iterator::EventLogIterator,
    paths::resolve_existing_session_path,
    turn_lock::{try_acquire_session_turn, try_acquire_session_turn_in_projects_root},
};

/// 基于本地文件系统的会话仓储实现。
#[derive(Clone)]
pub struct FileSystemSessionRepository {
    projects_root: Option<PathBuf>,
    appenders: SharedAppenderRegistry,
}

impl FileSystemSessionRepository {
    pub fn new() -> Self {
        Self {
            projects_root: None,
            appenders: default_appender_registry(),
        }
    }

    /// 基于显式项目根目录构建仓储。
    ///
    /// server 测试需要每个 runtime 使用独立 sandbox，不能共享进程级
    /// `~/.astrcode/projects`。显式传入根目录后，整个 session 存储链路都会跟随隔离。
    pub fn new_with_projects_root(projects_root: PathBuf) -> Self {
        // 启动时清理临时文件
        Self::cleanup_temp_files_blocking(&projects_root);
        Self {
            projects_root: Some(projects_root),
            appenders: default_appender_registry(),
        }
    }

    /// 清理临时文件（同步版本）。
    fn cleanup_temp_files_blocking(projects_root: &Path) {
        if let Err(error) = EventLog::cleanup_temp_files(projects_root) {
            log::warn!("failed to cleanup temp files: {}", error);
        }
    }

    pub fn ensure_session_sync(&self, session_id: &str, working_dir: &Path) -> StoreResult<()> {
        match self.open_event_log_sync(session_id) {
            Ok(_) => Ok(()),
            Err(astrcode_core::StoreError::SessionNotFound(_)) => self
                .create_event_log_sync(session_id, working_dir)
                .map(|_| ()),
            Err(error) => Err(error),
        }
    }

    pub fn create_event_log_sync(
        &self,
        session_id: &str,
        working_dir: &Path,
    ) -> StoreResult<EventLog> {
        match &self.projects_root {
            Some(projects_root) => {
                EventLog::create_in_projects_root(projects_root, session_id, working_dir)
            },
            None => EventLog::create(session_id, working_dir),
        }
    }

    /// 原子性创建事件日志并写入第一个事件。
    ///
    /// 使用临时文件 + 原子重命名确保文件创建和首个事件写入是原子操作，
    /// 避免产生空的 session 文件。
    pub fn create_event_log_with_first_event_sync(
        &self,
        session_id: &str,
        working_dir: &Path,
        first_event: &StorageEvent,
    ) -> StoreResult<(EventLog, StoredEvent)> {
        match &self.projects_root {
            Some(projects_root) => EventLog::create_with_first_event_in_projects_root(
                projects_root,
                session_id,
                working_dir,
                first_event,
            ),
            None => EventLog::create_with_first_event(session_id, working_dir, first_event),
        }
    }

    pub fn open_event_log_sync(&self, session_id: &str) -> StoreResult<EventLog> {
        match &self.projects_root {
            Some(projects_root) => EventLog::open_in_projects_root(projects_root, session_id),
            None => EventLog::open(session_id),
        }
    }

    pub fn append_sync(&self, session_id: &str, event: &StorageEvent) -> StoreResult<StoredEvent> {
        let mut log = self.open_event_log_sync(session_id)?;
        log.append_stored(event)
    }

    pub fn recover_session_sync(&self, session_id: &str) -> StoreResult<RecoveredSessionState> {
        checkpoint::recover_session(self.projects_root.as_deref(), session_id)
    }

    pub fn checkpoint_session_sync(
        &self,
        event_log_path: &Path,
        session_id: &str,
        checkpoint: &SessionRecoveryCheckpoint,
    ) -> StoreResult<()> {
        checkpoint::persist_checkpoint(
            self.projects_root.as_deref(),
            event_log_path,
            session_id,
            checkpoint,
        )
    }

    pub fn replay_events_sync(&self, session_id: &str) -> StoreResult<Vec<StoredEvent>> {
        let path = match &self.projects_root {
            Some(projects_root) => super::paths::resolve_existing_session_path_from_projects_root(
                projects_root,
                session_id,
            )?,
            None => resolve_existing_session_path(session_id)?,
        };
        EventLogIterator::from_path(&path)?.collect()
    }

    pub fn try_acquire_turn_sync(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> StoreResult<SessionTurnAcquireResult> {
        match &self.projects_root {
            Some(projects_root) => {
                try_acquire_session_turn_in_projects_root(projects_root, session_id, turn_id)
            },
            None => try_acquire_session_turn(session_id, turn_id),
        }
    }

    pub fn list_sessions_sync(&self) -> StoreResult<Vec<String>> {
        match &self.projects_root {
            Some(projects_root) => EventLog::list_sessions_from_path(projects_root),
            None => EventLog::list_sessions(),
        }
    }

    pub fn list_session_metas_sync(&self) -> StoreResult<Vec<SessionMeta>> {
        match &self.projects_root {
            Some(projects_root) => EventLog::list_sessions_with_meta_from_path(projects_root),
            None => EventLog::list_sessions_with_meta(),
        }
    }

    pub fn delete_session_sync(&self, session_id: &str) -> StoreResult<()> {
        match &self.projects_root {
            Some(projects_root) => EventLog::delete_session_from_path(projects_root, session_id),
            None => EventLog::delete_session(session_id),
        }
    }

    pub fn delete_sessions_by_working_dir_sync(
        &self,
        working_dir: &str,
    ) -> StoreResult<DeleteProjectResult> {
        match &self.projects_root {
            Some(projects_root) => {
                EventLog::delete_sessions_by_working_dir_from_path(projects_root, working_dir)
            },
            None => EventLog::delete_sessions_by_working_dir(working_dir),
        }
    }

    pub fn last_storage_seq_sync(&self, session_id: &str) -> StoreResult<u64> {
        let path = match &self.projects_root {
            Some(projects_root) => super::paths::resolve_existing_session_path_from_projects_root(
                projects_root,
                session_id,
            )?,
            None => resolve_existing_session_path(session_id)?,
        };
        EventLog::last_storage_seq_from_path(&path)
    }

    async fn appender_for_session(&self, session_id: &str) -> Arc<BatchAppender> {
        let mut registry = self.appenders.lock().await;
        if let Some(appender) = registry.get(session_id) {
            return Arc::clone(appender);
        }
        let appender = Arc::new(BatchAppender::new(
            session_id.to_string(),
            self.projects_root.clone(),
        ));
        registry.insert(session_id.to_string(), Arc::clone(&appender));
        appender
    }
}

impl Default for FileSystemSessionRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventStore for FileSystemSessionRepository {
    async fn ensure_session(&self, session_id: &SessionId, working_dir: &Path) -> Result<()> {
        let repo = self.clone();
        let session_id = session_id.to_string();
        let working_dir = working_dir.to_path_buf();
        run_blocking("ensure storage session", move || {
            repo.ensure_session_sync(&session_id, &working_dir)
        })
        .await
    }

    async fn create_session_with_first_event(
        &self,
        session_id: &SessionId,
        working_dir: &Path,
        first_event: &StorageEvent,
    ) -> Result<StoredEvent> {
        let repo = self.clone();
        let session_id = session_id.to_string();
        let working_dir = working_dir.to_path_buf();
        let first_event = first_event.clone();
        run_blocking("create session with first event", move || {
            repo.create_event_log_with_first_event_sync(&session_id, &working_dir, &first_event)
                .map(|(_log, stored)| stored)
        })
        .await
    }

    async fn append(&self, session_id: &SessionId, event: &StorageEvent) -> Result<StoredEvent> {
        let appender = self.appender_for_session(session_id.as_str()).await;
        appender
            .append(event.clone())
            .await
            .map_err(crate::map_store_error)
    }

    async fn replay(&self, session_id: &SessionId) -> Result<Vec<StoredEvent>> {
        let repo = self.clone();
        let session_id = session_id.to_string();
        run_blocking("replay storage events", move || {
            repo.replay_events_sync(&session_id)
        })
        .await
    }

    async fn recover_session(&self, session_id: &SessionId) -> Result<RecoveredSessionState> {
        let repo = self.clone();
        let session_id = session_id.to_string();
        run_blocking("recover storage session", move || {
            repo.recover_session_sync(&session_id)
        })
        .await
    }

    async fn checkpoint_session(
        &self,
        session_id: &SessionId,
        checkpoint: &SessionRecoveryCheckpoint,
    ) -> Result<()> {
        let appender = self.appender_for_session(session_id.as_str()).await;
        let repo = self.clone();
        let session_id_string = session_id.to_string();
        appender
            .checkpoint_with_payload(checkpoint.clone(), move |event_log_path, checkpoint| {
                repo.checkpoint_session_sync(event_log_path, &session_id_string, checkpoint)
            })
            .await
            .map_err(crate::map_store_error)
    }

    async fn try_acquire_turn(
        &self,
        session_id: &SessionId,
        turn_id: &str,
    ) -> Result<SessionTurnAcquireResult> {
        let repo = self.clone();
        let session_id = session_id.to_string();
        let turn_id = turn_id.to_string();
        run_blocking("acquire session turn", move || {
            repo.try_acquire_turn_sync(&session_id, &turn_id)
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>> {
        let repo = self.clone();
        run_blocking("list storage sessions", move || repo.list_sessions_sync())
            .await
            .map(|sessions| sessions.into_iter().map(SessionId::from).collect())
    }

    async fn list_session_metas(&self) -> Result<Vec<SessionMeta>> {
        let repo = self.clone();
        run_blocking("list storage session metas", move || {
            repo.list_session_metas_sync()
        })
        .await
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<()> {
        let repo = self.clone();
        let session_id = session_id.to_string();
        run_blocking("delete storage session", move || {
            repo.delete_session_sync(&session_id)
        })
        .await
    }

    async fn delete_sessions_by_working_dir(
        &self,
        working_dir: &str,
    ) -> Result<DeleteProjectResult> {
        let repo = self.clone();
        let working_dir = working_dir.to_string();
        run_blocking("delete storage project sessions", move || {
            repo.delete_sessions_by_working_dir_sync(&working_dir)
        })
        .await
    }
}

fn default_appender_registry() -> SharedAppenderRegistry {
    Arc::new(Mutex::new(std::collections::HashMap::new()))
}

async fn run_blocking<T, F>(label: &'static str, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> StoreResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| {
            astrcode_core::AstrError::Internal(format!(
                "storage blocking task '{label}' failed: {error}"
            ))
        })?
        .map_err(crate::map_store_error)
}

impl SessionManager for FileSystemSessionRepository {
    fn create_event_log(
        &self,
        session_id: &str,
        working_dir: &Path,
    ) -> StoreResult<Box<dyn EventLogWriter>> {
        self.create_event_log_sync(session_id, working_dir)
            .map(|log| Box::new(log) as Box<dyn EventLogWriter>)
    }

    fn open_event_log(&self, session_id: &str) -> StoreResult<Box<dyn EventLogWriter>> {
        self.open_event_log_sync(session_id)
            .map(|log| Box::new(log) as Box<dyn EventLogWriter>)
    }

    fn replay_events(
        &self,
        session_id: &str,
    ) -> StoreResult<Box<dyn Iterator<Item = StoreResult<StoredEvent>> + Send>> {
        let path = match &self.projects_root {
            Some(projects_root) => super::paths::resolve_existing_session_path_from_projects_root(
                projects_root,
                session_id,
            )?,
            None => resolve_existing_session_path(session_id)?,
        };
        Ok(Box::new(EventLogIterator::from_path(&path)?))
    }

    fn try_acquire_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> StoreResult<SessionTurnAcquireResult> {
        self.try_acquire_turn_sync(session_id, turn_id)
    }

    fn last_storage_seq(&self, session_id: &str) -> StoreResult<u64> {
        self.last_storage_seq_sync(session_id)
    }

    fn list_sessions(&self) -> StoreResult<Vec<String>> {
        self.list_sessions_sync()
    }

    fn list_sessions_with_meta(&self) -> StoreResult<Vec<SessionMeta>> {
        self.list_session_metas_sync()
    }

    fn delete_session(&self, session_id: &str) -> StoreResult<()> {
        self.delete_session_sync(session_id)
    }

    fn delete_sessions_by_working_dir(
        &self,
        working_dir: &str,
    ) -> StoreResult<DeleteProjectResult> {
        self.delete_sessions_by_working_dir_sync(working_dir)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use astrcode_core::{
        AgentEventContext, LlmMessage, Phase, StorageEvent, StorageEventPayload, UserMessageOrigin,
        mode::ModeId,
    };
    use astrcode_host_session::{
        ports::{EventStore, ProjectionRegistrySnapshot, SessionRecoveryCheckpoint},
        projection::AgentState,
    };

    use super::*;
    use crate::session::paths::checkpoint_snapshot_path_from_projects_root;

    fn user_message_event(turn_id: &str, content: &str) -> StorageEvent {
        StorageEvent {
            turn_id: Some(turn_id.to_string()),
            agent: AgentEventContext::default(),
            payload: StorageEventPayload::UserMessage {
                content: content.to_string(),
                origin: UserMessageOrigin::User,
                timestamp: chrono::Utc::now(),
            },
        }
    }

    #[tokio::test]
    async fn append_batches_events_with_contiguous_storage_seq() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let repo = FileSystemSessionRepository::new_with_projects_root(temp.path().to_path_buf());
        let session_id = SessionId::from("session-batch-1".to_string());
        let working_dir = temp.path().join("work");
        std::fs::create_dir_all(&working_dir).expect("working dir should exist");
        repo.ensure_session(&session_id, &working_dir)
            .await
            .expect("session should be created");

        let started = Instant::now();
        let first_event = user_message_event("turn-1", "first");
        let second_event = user_message_event("turn-2", "second");
        let first = repo.append(&session_id, &first_event);
        let second = repo.append(&session_id, &second_event);
        let (first, second) = tokio::join!(first, second);
        let elapsed = started.elapsed();

        let first = first.expect("first append should succeed");
        let second = second.expect("second append should succeed");
        let mut seqs = vec![first.storage_seq, second.storage_seq];
        seqs.sort_unstable();

        assert_eq!(seqs, vec![1, 2]);
        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "batch append should wait for the drain window before returning"
        );
        assert_eq!(
            repo.replay_events_sync(session_id.as_str())
                .expect("replay should succeed")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn recover_session_uses_checkpoint_plus_tail_events() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let repo = FileSystemSessionRepository::new_with_projects_root(temp.path().to_path_buf());
        let session_id = SessionId::from("session-recovery-1".to_string());
        let working_dir = temp.path().join("work");
        std::fs::create_dir_all(&working_dir).expect("working dir should exist");
        repo.ensure_session(&session_id, &working_dir)
            .await
            .expect("session should be created");

        repo.append(&session_id, &user_message_event("turn-1", "first"))
            .await
            .expect("first append should succeed");
        repo.append(&session_id, &user_message_event("turn-2", "second"))
            .await
            .expect("second append should succeed");
        let tail = repo
            .append(&session_id, &user_message_event("turn-3", "tail"))
            .await
            .expect("tail append should succeed");

        repo.checkpoint_session(
            &session_id,
            &SessionRecoveryCheckpoint::new(
                AgentState {
                    session_id: session_id.to_string(),
                    working_dir: working_dir.clone(),
                    messages: vec![LlmMessage::User {
                        content: "checkpoint".to_string(),
                        origin: UserMessageOrigin::User,
                    }],
                    phase: Phase::Idle,
                    mode_id: ModeId::default(),
                    turn_count: 2,
                    last_assistant_at: None,
                },
                ProjectionRegistrySnapshot::default(),
                2,
            ),
        )
        .await
        .expect("checkpoint should succeed");

        let recovered = repo
            .recover_session(&session_id)
            .await
            .expect("recovery should succeed");

        assert_eq!(
            recovered
                .checkpoint
                .expect("checkpoint should exist")
                .checkpoint_storage_seq,
            2
        );
        assert_eq!(recovered.tail_events.len(), 1);
        assert_eq!(recovered.tail_events[0].storage_seq, tail.storage_seq);
        assert_eq!(
            repo.replay_events_sync(session_id.as_str())
                .expect("replay should succeed")
                .into_iter()
                .map(|stored| stored.storage_seq)
                .collect::<Vec<_>>(),
            vec![tail.storage_seq]
        );
    }

    #[tokio::test]
    async fn recover_session_fails_when_marker_points_to_missing_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir should exist");
        let repo = FileSystemSessionRepository::new_with_projects_root(temp.path().to_path_buf());
        let session_id = SessionId::from("session-recovery-missing-snapshot".to_string());
        let working_dir = temp.path().join("work");
        std::fs::create_dir_all(&working_dir).expect("working dir should exist");
        repo.ensure_session(&session_id, &working_dir)
            .await
            .expect("session should be created");

        repo.append(&session_id, &user_message_event("turn-1", "first"))
            .await
            .expect("append should succeed");
        repo.checkpoint_session(
            &session_id,
            &SessionRecoveryCheckpoint::new(
                AgentState {
                    session_id: session_id.to_string(),
                    working_dir: working_dir.clone(),
                    messages: vec![LlmMessage::User {
                        content: "checkpoint".to_string(),
                        origin: UserMessageOrigin::User,
                    }],
                    phase: Phase::Idle,
                    mode_id: ModeId::default(),
                    turn_count: 1,
                    last_assistant_at: None,
                },
                ProjectionRegistrySnapshot::default(),
                1,
            ),
        )
        .await
        .expect("checkpoint should succeed");

        let snapshot_path =
            checkpoint_snapshot_path_from_projects_root(temp.path(), session_id.as_str(), 1)
                .expect("snapshot path should resolve");
        std::fs::remove_file(&snapshot_path).expect("snapshot should be removable");

        let error = repo
            .recover_session(&session_id)
            .await
            .expect_err("recovery should fail when snapshot is missing");
        assert!(
            error.to_string().contains("points to missing snapshot"),
            "unexpected error: {error}"
        );
    }
}

//! 基于文件系统的会话仓库，实现 EventStore trait。
//!
//! 管理按项目组织的会话事件日志，目录结构为：
//! `~/.astrcode/projects/<project>/sessions/<session>/`

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use astrcode_core::{
    event::{Event, EventPayload},
    storage::{EventStore, StorageError},
    types::{Cursor, ProjectHash, SessionId, validate_session_id},
};
use astrcode_support::hostpaths;
use tokio::sync::RwLock;

use crate::{event_log::EventLog, snapshot::SnapshotManager};

/// 基于文件系统的会话仓库。
///
/// 管理按项目组织的会话事件日志，目录结构为：
/// `~/.astrcode/projects/<project>/sessions/<session>/`
///
/// 内存中缓存已打开的会话元数据，避免频繁的磁盘 I/O。
pub struct FileSystemSessionRepository {
    /// 已打开的会话元数据缓存，按会话 ID 索引
    sessions: Arc<RwLock<HashMap<SessionId, Arc<SessionMeta>>>>,
    /// 会话存储的基础路径
    base_path: PathBuf,
}

/// 会话的内部元数据，持有事件日志和快照管理器。
struct SessionMeta {
    /// 事件日志实例，负责追加式写入和重放
    log: Arc<EventLog>,
    /// 快照管理器，负责创建和列出恢复点
    snapshot_mgr: SnapshotManager,
}

impl FileSystemSessionRepository {
    /// 创建新的文件系统会话仓库。
    ///
    /// # 参数
    /// - `project_hash`: 项目路径的哈希值，用于确定存储目录
    pub fn new(project_hash: ProjectHash) -> Self {
        let base_path = hostpaths::sessions_dir(&project_hash);
        if let Err(e) = std::fs::create_dir_all(&base_path) {
            tracing::warn!("Failed to create sessions dir {}: {e}", base_path.display());
        }
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            base_path,
        }
    }

    /// 获取指定会话的目录路径。
    fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.base_path.join(id)
    }

    /// 获取指定会话的事件日志文件路径。
    fn event_log_path(&self, id: &SessionId) -> PathBuf {
        self.session_dir(id).join(format!("session-{}.jsonl", id))
    }

    /// 获取或打开会话元数据。
    ///
    /// 如果会话已在内存中则直接返回缓存；否则从磁盘打开事件日志，
    /// 恢复其内存中的 seq 计数器，并加入缓存。
    /// 使用双重检查锁定模式避免重复打开。
    async fn get_or_open_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionMeta>, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;

        if let Some(meta) = self.sessions.read().await.get(session_id).cloned() {
            return Ok(meta);
        }

        // Opening a log restores its in-memory next_seq from disk. That should
        // happen once per active process/session, not once per append.
        let opened = Arc::new(SessionMeta {
            log: Arc::new(EventLog::open(self.event_log_path(session_id)).await?),
            snapshot_mgr: SnapshotManager::new(self.session_dir(session_id).join("snapshots")),
        });

        let mut sessions = self.sessions.write().await;
        Ok(sessions
            .entry(session_id.clone())
            .or_insert_with(|| opened)
            .clone())
    }
}

#[async_trait::async_trait]
impl EventStore for FileSystemSessionRepository {
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&str>,
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
                parent_session_id: parent_session_id.map(|s| s.to_string()),
            },
        );

        let (log, stored_event) =
            EventLog::create(self.event_log_path(session_id), start_event).await?;

        self.sessions.write().await.insert(
            session_id.clone(),
            Arc::new(SessionMeta {
                log: Arc::new(log),
                snapshot_mgr: SnapshotManager::new(dir.join("snapshots")),
            }),
        );

        Ok(stored_event)
    }

    async fn append_event(&self, event: Event) -> Result<Event, StorageError> {
        let session_id = event.session_id.clone();
        let meta = self.get_or_open_meta(&session_id).await?;
        meta.log.append(event).await
    }

    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        meta.log.replay_all().await
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;
        let events = self.replay_events(session_id).await?;
        let Ok(seq) = cursor.parse::<u64>() else {
            return Err(StorageError::InvalidId(format!("Invalid cursor: {cursor}")));
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
        let meta = self.get_or_open_meta(session_id).await?;
        // Snapshots are recovery accelerators. The event log remains the
        // append-only source of truth, so checkpointing never participates in
        // normal append seq assignment.
        meta.snapshot_mgr.create_snapshot(cursor).await?;
        Ok(())
    }

    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let _ = self.get_or_open_meta(session_id).await?;
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

        self.sessions.write().await.remove(session_id);
        let dir = self.session_dir(session_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

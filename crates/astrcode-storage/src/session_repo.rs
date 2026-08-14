//! 基于文件系统的会话仓库。
//!
//! 管理按项目组织的会话事件日志，目录结构为：
//! `~/.astrcode/projects/<project>/sessions/<session>/`

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
};

use astrcode_core::{
    config::defaults::astrcode_dir,
    event::{DurableEvent, DurableEventPayload, StoredEvent},
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId, project_key_from_path, validate_session_id},
};
use astrcode_session_projection::{
    AgentSessionLinkView, PreparedProjectionBatch, ProjectionError, SessionReadModel,
    SessionSummary, reduce, replay,
};
use chrono::Utc;
use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, Semaphore};
use uuid::Uuid;

use crate::{
    CompactSnapshotInput, EventConsumerCheckpointOutcome, EventConsumerCheckpointReset,
    EventConsumerFailureOutcome, EventConsumerQuarantine, EventConsumerSkip, EventConsumerState,
    EventReader, SessionEventJournal, SessionPathResolver, SessionReader, SessionStore,
    StorageError, ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactStore,
    event_log::EventLog,
    snapshot::SnapshotManager,
    tool_artifacts::{
        read_tool_result_file, validate_tool_result_artifact_id, write_tool_result_file,
    },
};

fn validate_storage_session_id(id: &SessionId) -> Result<(), StorageError> {
    validate_session_id(id.as_str()).map_err(|error| StorageError::InvalidId(error.to_string()))
}

fn invalid_event(error: ProjectionError) -> StorageError {
    StorageError::InvalidEvent(error.to_string())
}

fn corrupt_projection(error: ProjectionError) -> StorageError {
    StorageError::CorruptLog(error.to_string())
}

async fn directory_exists(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}

fn parse_cursor(cursor: &Cursor) -> Result<u64, StorageError> {
    cursor
        .parse()
        .map_err(|_| StorageError::InvalidId(format!("Invalid cursor: {cursor}")))
}

fn source_extension_dir_component(source_extension: &str) -> Result<String, StorageError> {
    if source_extension.is_empty() {
        return Err(StorageError::InvalidId(
            "source_extension cannot be empty".into(),
        ));
    }

    let mut encoded = String::new();
    for byte in source_extension.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(encoded)
}

fn event_consumer_state_path(
    session_dir: &Path,
    consumer_id: &str,
) -> Result<PathBuf, StorageError> {
    if consumer_id.is_empty() {
        return Err(StorageError::InvalidId(
            "event consumer id cannot be empty".into(),
        ));
    }
    let digest = Sha256::digest(consumer_id.as_bytes());
    Ok(session_dir
        .join("event-consumers")
        .join(format!("{digest:x}.state.json")))
}

const EVENT_CONSUMER_STATE_VERSION: u8 = 3;

/// Initial reason recorded on a durability marker while fsync is in flight.
const FSYNC_CONFIRMATION_PENDING: &str = "fsync confirmation is pending";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedEventConsumerState {
    version: u8,
    cursor: Option<u64>,
    paused: bool,
    revision: u64,
    consecutive_failures: u32,
    quarantined_count: u64,
    skipped_count: u64,
    quarantined: Vec<EventConsumerQuarantine>,
    skips: Vec<EventConsumerSkip>,
}

impl TryFrom<PersistedEventConsumerState> for EventConsumerState {
    type Error = StorageError;

    fn try_from(state: PersistedEventConsumerState) -> Result<Self, Self::Error> {
        let state = Self {
            checkpoint: state.cursor,
            paused: state.paused,
            revision: state.revision,
            consecutive_failures: state.consecutive_failures,
            quarantined_count: state.quarantined_count,
            skipped_count: state.skipped_count,
            quarantined: state.quarantined,
            skips: state.skips,
        };
        state.validate_audit_bounds()?;
        Ok(state)
    }
}

impl From<&EventConsumerState> for PersistedEventConsumerState {
    fn from(state: &EventConsumerState) -> Self {
        Self {
            version: EVENT_CONSUMER_STATE_VERSION,
            cursor: state.checkpoint,
            paused: state.paused,
            revision: state.revision,
            consecutive_failures: state.consecutive_failures,
            quarantined_count: state.quarantined_count,
            skipped_count: state.skipped_count,
            quarantined: state.quarantined.clone(),
            skips: state.skips.clone(),
        }
    }
}

async fn read_event_consumer_state(path: &Path) -> Result<EventConsumerState, StorageError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EventConsumerState::default());
        },
        Err(error) => return Err(StorageError::Io(error)),
    };
    let state = serde_json::from_slice::<PersistedEventConsumerState>(&bytes).map_err(|error| {
        StorageError::CorruptLog(format!(
            "invalid event consumer state {}: {error}",
            path.display()
        ))
    })?;
    if state.version != EVENT_CONSUMER_STATE_VERSION {
        return Err(StorageError::CorruptLog(format!(
            "unsupported event consumer state version {} in {}",
            state.version,
            path.display()
        )));
    }
    state.try_into()
}

async fn write_event_consumer_state(
    path: &Path,
    state: &EventConsumerState,
) -> Result<(), StorageError> {
    state.validate_audit_bounds()?;
    let bytes = serde_json::to_vec(&PersistedEventConsumerState::from(state))?;
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || replace_durable_file(&path, &bytes))
        .await
        .map_err(|error| {
            StorageError::Io(std::io::Error::other(format!(
                "event consumer state writer stopped: {error}"
            )))
        })??;
    Ok(())
}

fn replace_durable_file(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let parent = path.parent().ok_or_else(|| {
        StorageError::InvalidId("event consumer state has no parent directory".into())
    })?;
    let parent_created = !parent.exists();
    std::fs::create_dir_all(parent)?;
    if parent_created {
        sync_directory(parent.parent())?;
    }

    let temporary = path.with_extension("json.tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        sync_directory(Some(parent))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(StorageError::Io)
}

#[cfg(unix)]
pub(crate) fn sync_directory(directory: Option<&Path>) -> std::io::Result<()> {
    if let Some(directory) = directory {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_directory: Option<&Path>) -> std::io::Result<()> {
    Ok(())
}

/// 基于文件系统的会话仓库。
///
/// 管理按项目组织的会话事件日志，目录结构为：
/// `~/.astrcode/projects/<project>/sessions/<session>/`
///
/// 内存中缓存已打开的会话元数据，避免频繁的磁盘 I/O。
pub struct FileSystemSessionRepository {
    owner: Arc<()>,
    /// 已打开的会话元数据缓存，按会话 ID 索引
    sessions: Arc<RwLock<HashMap<SessionId, Arc<SessionMeta>>>>,
    /// 所有项目目录的父目录：`~/.astrcode/projects/`
    projects_base: PathBuf,
}

static SESSION_OWNER_LEASES: OnceLock<Mutex<HashMap<PathBuf, Weak<SessionOwnerLeaseInner>>>> =
    OnceLock::new();

fn session_owner_leases() -> &'static Mutex<HashMap<PathBuf, Weak<SessionOwnerLeaseInner>>> {
    SESSION_OWNER_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 会话的内部元数据，持有事件日志和快照管理器。
struct SessionMeta {
    /// 本进程对该 session 目录的所有权租约。
    ///
    /// 当前文件系统仓库刻意采用单 repository owner 模型：同一 repository 可复用
    /// 已持有的 owner lease，其他 repository 或进程会收到明确错误。
    _owner_lease: SessionOwnerLease,
    /// 事件日志实例，负责追加式写入和重放
    log: EventLog,
    /// 快照管理器，负责创建和列出恢复点
    snapshot_mgr: SnapshotManager,
    /// 当前会话所在目录。
    dir: PathBuf,
    /// 从事件日志同步维护的 projection 实例（本进程内由 `append_events` 增量更新）。
    ///
    /// reducer 规则归 `astrcode-session-projection`；storage 只拥有这个可重建缓存的实例。
    projection: SessionProjection,
    /// 串行化 journal commit 与 projection 更新。
    ///
    /// 这是进程内 storage 一致性边界；OS owner lease 只处理跨进程目录所有权。
    commit_lane: Semaphore,
    /// 一次 fsync 返回模糊失败后的 sticky 状态。
    ///
    /// `PendingProjection` 持有已写入 EventLog 但尚未对读模型可见的精确批次；
    /// `Published` 表示普通 buffered append 早已可见，但后续 fsync 的结果不确定。
    uncertain_durability: Mutex<Option<UncertainDurability>>,
    /// 串行化 durable consumer 状态，避免慢订阅者阻塞事件日志提交。
    consumer_state_lane: Semaphore,
}

enum UncertainDurability {
    PendingProjection {
        batch: PreparedProjectionBatch,
        through_seq: u64,
        reason: String,
    },
    Published {
        through_seq: u64,
        reason: String,
    },
}

impl UncertainDurability {
    const fn through_seq(&self) -> u64 {
        match self {
            Self::PendingProjection { through_seq, .. } | Self::Published { through_seq, .. } => {
                *through_seq
            },
        }
    }

    fn reason(&self) -> &str {
        match self {
            Self::PendingProjection { reason, .. } | Self::Published { reason, .. } => reason,
        }
    }

    fn set_reason(&mut self, reason: String) {
        match self {
            Self::PendingProjection {
                reason: current, ..
            }
            | Self::Published {
                reason: current, ..
            } => *current = reason,
        }
    }

    fn error(&self, session_id: &SessionId) -> StorageError {
        StorageError::DurabilityUncertain {
            session_id: session_id.clone(),
            through_seq: self.through_seq(),
            reason: self.reason().to_owned(),
        }
    }
}

impl SessionMeta {
    async fn acquire_commit_lane(&self) -> Result<tokio::sync::SemaphorePermit<'_>, StorageError> {
        self.commit_lane
            .acquire()
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("session commit lane closed")))
    }

    async fn acquire_confirmed_commit_lane(
        &self,
        session_id: &SessionId,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, StorageError> {
        let permit = self.acquire_commit_lane().await?;
        self.ensure_no_uncertain_durability(session_id)?;
        Ok(permit)
    }

    fn ensure_no_uncertain_durability(&self, session_id: &SessionId) -> Result<(), StorageError> {
        match self.uncertain_durability.lock().as_ref() {
            Some(uncertain) => Err(uncertain.error(session_id)),
            None => Ok(()),
        }
    }
}

fn uncertain_seq_mismatch(expected: u64, actual: u64) -> StorageError {
    StorageError::InvalidEvent(format!(
        "durability confirmation expected pending seq {expected}, found {actual}"
    ))
}

fn sequence_overflow() -> StorageError {
    StorageError::CorruptLog("session event sequence overflow".into())
}

fn append_session_id(events: &[DurableEvent]) -> Result<SessionId, StorageError> {
    events
        .first()
        .map(|event| event.session_id.clone())
        .ok_or_else(|| StorageError::InvalidEvent("event batch cannot be empty".into()))
}

/// The event log must sit exactly at the batch's first seq before an append.
async fn ensure_log_positioned_for(
    meta: &SessionMeta,
    prepared: &PreparedProjectionBatch,
) -> Result<(), StorageError> {
    let expected_seq = prepared.first_seq();
    let log_next_seq = meta.log.count().await?;
    if log_next_seq != expected_seq {
        return Err(StorageError::CorruptLog(format!(
            "event log next seq {log_next_seq} does not match projection next seq {expected_seq}"
        )));
    }
    Ok(())
}

fn update_uncertain_reason(
    meta: &SessionMeta,
    session_id: &SessionId,
    expected_through_seq: u64,
    reason: String,
) -> StorageError {
    let mut state = meta.uncertain_durability.lock();
    let Some(uncertain) = state.as_mut() else {
        return StorageError::CorruptLog(
            "durability marker disappeared before fsync completed".into(),
        );
    };
    if uncertain.through_seq() != expected_through_seq {
        return uncertain_seq_mismatch(expected_through_seq, uncertain.through_seq());
    }
    uncertain.set_reason(reason);
    uncertain.error(session_id)
}

fn clear_uncertain(
    meta: &SessionMeta,
    expected_through_seq: u64,
) -> Result<UncertainDurability, StorageError> {
    let mut state = meta.uncertain_durability.lock();
    match state.take() {
        Some(uncertain) if uncertain.through_seq() == expected_through_seq => Ok(uncertain),
        Some(uncertain) => {
            let actual_through_seq = uncertain.through_seq();
            // Put the marker back: only the expected boundary may be cleared.
            *state = Some(uncertain);
            Err(uncertain_seq_mismatch(
                expected_through_seq,
                actual_through_seq,
            ))
        },
        None => Err(StorageError::CorruptLog(
            "durability marker disappeared before confirmation completed".into(),
        )),
    }
}

async fn apply_confirmed_projection(
    meta: &SessionMeta,
    expected_through_seq: u64,
) -> Result<Vec<StoredEvent>, StorageError> {
    let pending_batch = {
        let state = meta.uncertain_durability.lock();
        let Some(uncertain) = state.as_ref() else {
            return Err(StorageError::CorruptLog(
                "durability marker disappeared before projection publication".into(),
            ));
        };
        if uncertain.through_seq() != expected_through_seq {
            return Err(uncertain_seq_mismatch(
                expected_through_seq,
                uncertain.through_seq(),
            ));
        }
        match uncertain {
            UncertainDurability::PendingProjection { batch, .. } => Some(batch.clone()),
            UncertainDurability::Published { .. } => None,
        }
    };

    let Some(batch) = pending_batch else {
        clear_uncertain(meta, expected_through_seq)?;
        return Ok(Vec::new());
    };
    let log_next_seq = meta.log.count().await?;
    let expected_next_seq = expected_through_seq
        .checked_add(1)
        .ok_or_else(sequence_overflow)?;
    // A live pending marker implies its append already committed, so the log
    // must sit exactly past the batch; anything else is corruption.
    if log_next_seq != expected_next_seq {
        return Err(StorageError::CorruptLog(format!(
            "event log next seq {log_next_seq} does not match pending durability boundary \
             {expected_next_seq}"
        )));
    }

    let projection_next_seq = meta
        .projection
        .snapshot()
        .await
        .stats
        .last_seq
        .checked_add(1)
        .ok_or_else(sequence_overflow)?;
    // The commit lane blocks other appends while the marker is alive, so the
    // projection cannot have advanced past the pending batch.
    let stored = if projection_next_seq == batch.first_seq() {
        meta.projection.apply_committed(batch).await
    } else {
        return Err(StorageError::CorruptLog(format!(
            "projection next seq {projection_next_seq} does not match pending batch start {}",
            batch.first_seq()
        )));
    };
    clear_uncertain(meta, expected_through_seq)?;
    Ok(stored)
}

struct SessionProjection {
    model: RwLock<Arc<SessionReadModel>>,
}

impl SessionProjection {
    fn new(model: SessionReadModel) -> Self {
        Self {
            model: RwLock::new(Arc::new(model)),
        }
    }

    async fn snapshot(&self) -> Arc<SessionReadModel> {
        let model = self.model.read().await;
        Arc::clone(&model)
    }

    async fn prepare_batch(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<PreparedProjectionBatch, StorageError> {
        let model = self.model.read().await;
        PreparedProjectionBatch::prepare(model.as_ref(), events).map_err(|error| match error {
            ProjectionError::SequenceOverflow => StorageError::CorruptLog(error.to_string()),
            error => invalid_event(error),
        })
    }

    async fn apply_committed(&self, batch: PreparedProjectionBatch) -> Vec<StoredEvent> {
        let mut model = self.model.write().await;
        batch.apply(&mut model)
    }
}

struct SessionOwnerLease {
    _inner: Arc<SessionOwnerLeaseInner>,
}

struct SessionOwnerLeaseInner {
    path: PathBuf,
    owner: Arc<()>,
    _file: File,
}

impl SessionOwnerLease {
    async fn acquire(session_dir: &Path, owner: &Arc<()>) -> Result<Self, StorageError> {
        let session_dir = session_dir.to_path_buf();
        let owner = Arc::clone(owner);
        tokio::task::spawn_blocking(move || Self::acquire_blocking(&session_dir, &owner))
            .await
            .map_err(|error| {
                StorageError::Io(std::io::Error::other(format!(
                    "session owner lease task failed: {error}"
                )))
            })?
    }

    fn acquire_blocking(session_dir: &Path, owner: &Arc<()>) -> Result<Self, StorageError> {
        let key = std::fs::canonicalize(session_dir).map_err(StorageError::Io)?;
        let mut leases = session_owner_leases().lock();

        if let Some(inner) = leases.get(&key).and_then(Weak::upgrade) {
            if Arc::ptr_eq(&inner.owner, owner) {
                return Ok(Self { _inner: inner });
            }
            return Err(StorageError::LockError(format!(
                "session directory {} is already owned by another AstrCode repository",
                key.display()
            )));
        }

        let lock_path = key.join(".astrcode-session-owner.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(StorageError::Io)?;

        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                StorageError::LockError(format!(
                    "session directory {} is already owned by another AstrCode repository; stop \
                     that server before opening this session here",
                    key.display()
                ))
            } else {
                StorageError::Io(error)
            }
        })?;

        let inner = Arc::new(SessionOwnerLeaseInner {
            path: key.clone(),
            owner: Arc::clone(owner),
            _file: file,
        });
        leases.insert(key, Arc::downgrade(&inner));
        Ok(Self { _inner: inner })
    }
}

impl Drop for SessionOwnerLeaseInner {
    fn drop(&mut self) {
        let mut leases = session_owner_leases().lock();
        if leases
            .get(&self.path)
            .is_some_and(|lease| lease.strong_count() == 0)
        {
            leases.remove(&self.path);
        }
    }
}

impl Default for FileSystemSessionRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystemSessionRepository {
    /// 创建新的文件系统会话仓库。
    ///
    /// 会话按 `working_dir` 动态分发到对应的项目目录，不再绑定启动时的 cwd。
    pub fn new() -> Self {
        Self::with_projects_base(astrcode_dir().join("projects"))
    }

    fn with_projects_base(projects_base: PathBuf) -> Self {
        if let Err(e) = std::fs::create_dir_all(&projects_base) {
            tracing::warn!(
                "Failed to create projects dir {}: {e}",
                projects_base.display()
            );
        }
        Self {
            owner: Arc::new(()),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            projects_base,
        }
    }

    #[cfg(feature = "testing")]
    pub fn for_testing(projects_base: PathBuf) -> Self {
        Self::with_projects_base(projects_base)
    }

    #[cfg(feature = "testing")]
    pub async fn fail_next_durable_sync_for_testing(
        &self,
        session_id: &SessionId,
    ) -> Result<(), StorageError> {
        self.get_or_open_meta(session_id)
            .await?
            .log
            .fail_next_sync()
            .await
    }

    /// 根据 `working_dir` 计算新会话的存储目录。
    fn session_dir_from_working_dir(&self, working_dir: &str, id: &SessionId) -> PathBuf {
        let project_key = project_key_from_path(&PathBuf::from(working_dir));
        self.projects_base
            .join(project_key)
            .join("sessions")
            .join(id.as_str())
    }

    /// 扫描 projects_base 下所有项目目录，返回其 sessions 子目录列表。
    async fn all_session_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(&self.projects_base).await else {
            return roots;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let sessions_dir = entry.path().join("sessions");
            if directory_exists(&sessions_dir).await {
                roots.push(sessions_dir);
            }
        }
        roots
    }

    async fn all_session_roots_strict(&self) -> Result<Vec<PathBuf>, StorageError> {
        let mut entries = match tokio::fs::read_dir(&self.projects_base).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut roots = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let sessions_dir = entry.path().join("sessions");
            match tokio::fs::metadata(&sessions_dir).await {
                Ok(metadata) if metadata.is_dir() => roots.push(sessions_dir),
                Ok(_) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        Ok(roots)
    }

    /// 在所有项目目录中查找指定会话的目录。
    ///
    /// 先检查 flat 位置（根 session），再递归搜索 `subagents/` 子目录树。
    async fn find_session_dir(&self, id: &SessionId) -> Option<PathBuf> {
        if let Some(meta) = self.sessions.read().await.get(id).cloned() {
            return Some(meta.dir.clone());
        }

        for root in self.all_session_roots().await {
            let dir = root.join(id.as_str());
            if directory_exists(&dir).await {
                return Some(dir);
            }
            if let Some(found) = self.search_subagents_tree(&root, id).await {
                return Some(found);
            }
        }
        None
    }

    /// 递归搜索 `base` 下所有 session 目录的 `subagents/{extension}/` 子树。
    async fn search_subagents_tree(&self, base: &Path, id: &SessionId) -> Option<PathBuf> {
        let mut stack = vec![base.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let subagents = entry.path().join("subagents");
                if !directory_exists(&subagents).await {
                    continue;
                }
                let Ok(mut extension_entries) = tokio::fs::read_dir(&subagents).await else {
                    continue;
                };
                while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                    let extension_name = extension_entry.file_name();
                    if extension_name.to_string_lossy() == ".recycled" {
                        continue;
                    }
                    let extension_dir = extension_entry.path();
                    let candidate = extension_dir.join(id.as_str());
                    if directory_exists(&candidate).await {
                        return Some(candidate);
                    }
                    stack.push(extension_dir);
                }
            }
        }
        None
    }

    async fn existing_session_dir(&self, id: &SessionId) -> Result<PathBuf, StorageError> {
        self.find_session_dir(id)
            .await
            .ok_or_else(|| StorageError::NotFound(id.clone()))
    }

    async fn new_session_dir(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        parent_session_id: Option<&SessionId>,
        source_extension: Option<&str>,
    ) -> Result<PathBuf, StorageError> {
        match parent_session_id {
            Some(parent_id) => {
                let extension = source_extension.ok_or_else(|| {
                    StorageError::InvalidId(
                        "child session requires source_extension to choose subagents directory"
                            .into(),
                    )
                })?;
                let parent_dir = self.existing_session_dir(parent_id).await?;
                let extension_dir = source_extension_dir_component(extension)?;
                Ok(parent_dir
                    .join("subagents")
                    .join(extension_dir)
                    .join(session_id.as_str()))
            },
            None => Ok(self.session_dir_from_working_dir(working_dir, session_id)),
        }
    }

    /// 获取指定会话的事件日志文件路径。
    fn event_log_path(session_dir: &Path, id: &SessionId) -> PathBuf {
        session_dir.join(format!("session-{id}.jsonl"))
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
        validate_storage_session_id(session_id)?;

        if let Some(meta) = self.sessions.read().await.get(session_id).cloned() {
            return Ok(meta);
        }

        // 磁盘打开与 projection 恢复可能较慢；不得持 `sessions` 写锁跨越 await，
        // 否则会阻塞同实例上的 append/checkpoint，并在 Windows 上与未释放的
        // EventLog 句柄争用同一 JSONL 文件。
        let dir = self.existing_session_dir(session_id).await?;
        let owner_lease = SessionOwnerLease::acquire(&dir, &self.owner).await?;
        let log = EventLog::open(Self::event_log_path(&dir, session_id)).await?;
        let snapshot_mgr = SnapshotManager::new(dir.join("snapshots"));
        let projection = self
            .restore_projection(session_id, &log, &snapshot_mgr)
            .await?;
        let opened = Arc::new(SessionMeta {
            _owner_lease: owner_lease,
            log,
            snapshot_mgr,
            dir,
            projection: SessionProjection::new(projection),
            commit_lane: Semaphore::new(1),
            uncertain_durability: Mutex::new(None),
            consumer_state_lane: Semaphore::new(1),
        });

        let mut sessions = self.sessions.write().await;
        Ok(if let Some(meta) = sessions.get(session_id) {
            Arc::clone(meta)
        } else {
            sessions.insert(session_id.clone(), Arc::clone(&opened));
            opened
        })
    }

    async fn restore_projection(
        &self,
        session_id: &SessionId,
        log: &EventLog,
        snapshot_mgr: &SnapshotManager,
    ) -> Result<SessionReadModel, StorageError> {
        // Snapshot first because it's faster than replaying the full event log.
        if let Some(snapshot) = snapshot_mgr.latest_snapshot().await? {
            match restore_from_snapshot(session_id, log, snapshot).await {
                Ok(model) => return Ok(model),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        "Falling back to full event replay after snapshot restore failed: {error}"
                    );
                },
            }
        }

        let events = log.replay_all().await?;
        replay(session_id.clone(), &events).map_err(corrupt_projection)
    }
}

async fn restore_from_snapshot(
    expected_session_id: &SessionId,
    log: &EventLog,
    snapshot: crate::snapshot::SessionProjectionSnapshot,
) -> Result<SessionReadModel, StorageError> {
    if snapshot.model.identity.session_id != *expected_session_id {
        return Err(StorageError::CorruptLog(format!(
            "projection snapshot belongs to session {}, expected {}",
            snapshot.model.identity.session_id, expected_session_id
        )));
    }
    let latest_seq = snapshot.model.stats.last_seq;

    // `count()` returns the next seq to assign (= number of persisted events).
    let next_seq = log.count().await?;
    if latest_seq >= next_seq {
        return Err(StorageError::InvalidId(format!(
            "snapshot latest_seq {latest_seq} is outside event log (next_seq={next_seq})"
        )));
    }

    let mut model = snapshot.model;
    // Reapply only the events that occurred after the snapshot. The snapshot
    // serves as a recovery checkpoint, not as an authoritative source of truth.
    for event in log.replay_after(latest_seq).await? {
        reduce(&event, &mut model).map_err(corrupt_projection)?;
    }
    Ok(model)
}

#[async_trait::async_trait]
impl EventReader for FileSystemSessionRepository {
    async fn replay_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        meta.log.replay_all().await
    }

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let cursor = meta.projection.snapshot().await.cursor();
        Ok(Some(cursor))
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let seq = parse_cursor(cursor)?;
        meta.log.replay_after(seq).await
    }

    async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let seq = parse_cursor(cursor)?;
        meta.log.replay_after_limited(seq, max_events).await
    }

    async fn replay_from_start_limited(
        &self,
        session_id: &SessionId,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        meta.log.replay_from_start_limited(max_events).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        self.list_session_dirs().await
    }

    async fn list_all_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        Ok(self
            .list_all_session_locations()
            .await?
            .into_keys()
            .collect())
    }
}

#[async_trait::async_trait]
impl SessionReader for FileSystemSessionRepository {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        Ok(meta.projection.snapshot().await)
    }

    async fn recycled_session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        validate_storage_session_id(session_id)?;
        let recycled_dir = self
            .find_recycled_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let events =
            EventLog::replay_read_only(Self::event_log_path(&recycled_dir, session_id)).await?;
        let model = replay(session_id.clone(), &events).map_err(corrupt_projection)?;
        Ok(Arc::new(model))
    }

    async fn session_has_messages(&self, session_id: &SessionId) -> Result<bool, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let has_messages = meta.projection.snapshot().await.has_messages();
        Ok(has_messages)
    }

    async fn session_agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AgentSessionLinkView>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let agent_sessions = meta.projection.snapshot().await.agent_sessions.clone();
        Ok(agent_sessions)
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let session_ids = self.list_session_dirs().await?;
        self.summaries_for_session_ids(session_ids).await
    }

    async fn list_all_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let session_locations = self.list_all_session_locations().await?;
        let sessions = self.sessions.read().await.clone();
        let mut summaries = Vec::with_capacity(session_locations.len());

        for (session_id, session_dir) in session_locations {
            if let Some(meta) = sessions.get(&session_id) {
                summaries.push(meta.projection.snapshot().await.to_summary());
            } else if let Some(summary) =
                EventLog::read_summary(&Self::event_log_path(&session_dir, &session_id), session_id)
                    .await?
            {
                summaries.push(summary);
            }
        }

        summaries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(summaries)
    }
}

impl FileSystemSessionRepository {
    async fn summaries_for_session_ids(
        &self,
        session_ids: Vec<SessionId>,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        let sessions = self.sessions.read().await.clone();
        let mut summaries = Vec::new();

        for session_id in session_ids {
            if let Some(meta) = sessions.get(&session_id) {
                // 已打开的会话直接使用内存中的投影
                summaries.push(meta.projection.snapshot().await.to_summary());
            } else {
                // 未打开的会话从事件流构造轻量摘要，不加载完整 transcript。
                if let Some(summary) = self.read_summary_from_event_log(&session_id).await? {
                    summaries.push(summary);
                }
            }
        }

        summaries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(summaries)
    }
}

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
        let artifact_dir = meta.dir.join("tool-results");
        let session_dir = tokio::fs::canonicalize(&meta.dir).await?;
        let artifact_dir = tokio::fs::canonicalize(artifact_dir).await?;
        let path = artifact_dir.join(artifact_id);
        let canonical_path = match tokio::fs::canonicalize(&path).await {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(session_id.clone()));
            },
            Err(error) => return Err(StorageError::Io(error)),
        };
        if !artifact_dir.starts_with(&session_dir) || !canonical_path.starts_with(&artifact_dir) {
            return Err(StorageError::InvalidId(
                "tool result artifact resolves outside this session artifact directory".into(),
            ));
        }
        let artifact_id = artifact_id.to_owned();
        tokio::task::spawn_blocking(move || {
            read_tool_result_file(&canonical_path, &artifact_id, byte_offset, max_bytes)
        })
        .await
        .map_err(|error| {
            StorageError::Io(std::io::Error::other(format!(
                "tool result artifact reader stopped: {error}"
            )))
        })?
        .map_err(StorageError::Io)
    }

    async fn write_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let dir = meta.dir.join("tool-results");
        tokio::task::spawn_blocking(move || write_tool_result_file(&dir, &artifact))
            .await
            .map_err(|error| {
                StorageError::Io(std::io::Error::other(format!(
                    "tool result artifact writer stopped: {error}"
                )))
            })?
            .map_err(StorageError::Io)
    }
}

#[async_trait::async_trait]
impl SessionPathResolver for FileSystemSessionRepository {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<std::path::PathBuf>, StorageError> {
        Ok(self.find_session_dir(session_id).await)
    }

    async fn planned_session_store_dir(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        parent_session_id: Option<&SessionId>,
        source_extension: Option<&str>,
    ) -> Result<Option<PathBuf>, StorageError> {
        validate_storage_session_id(session_id)?;
        self.new_session_dir(session_id, working_dir, parent_session_id, source_extension)
            .await
            .map(Some)
    }
}

#[async_trait::async_trait]
impl SessionEventJournal for FileSystemSessionRepository {
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        validate_storage_session_id(&event.session_id)?;
        if event.turn_id.is_some() {
            return Err(StorageError::InvalidId(
                "SessionStarted must be a session-level event".into(),
            ));
        }
        let DurableEventPayload::SessionStarted(started) = &event.payload else {
            return Err(StorageError::InvalidId(
                "create_session requires SessionStarted".into(),
            ));
        };
        let session_id = event.session_id.clone();
        let parent_session_id = started.parent.as_ref().map(|parent| &parent.session_id);

        let dir = self
            .new_session_dir(
                &session_id,
                &started.working_dir,
                parent_session_id,
                started.source_extension.as_deref(),
            )
            .await?;
        tokio::fs::create_dir_all(&dir).await?;
        let owner_lease = SessionOwnerLease::acquire(&dir, &self.owner).await?;

        let (log, stored_event) =
            EventLog::create(Self::event_log_path(&dir, &session_id), event).await?;

        let projection = replay(session_id.clone(), std::slice::from_ref(&stored_event))
            .map_err(invalid_event)?;

        self.sessions.write().await.insert(
            session_id,
            Arc::new(SessionMeta {
                _owner_lease: owner_lease,
                log,
                snapshot_mgr: SnapshotManager::new(dir.join("snapshots")),
                dir,
                projection: SessionProjection::new(projection),
                commit_lane: Semaphore::new(1),
                uncertain_durability: Mutex::new(None),
                consumer_state_lane: Semaphore::new(1),
            }),
        );

        Ok(stored_event)
    }

    async fn append_events(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let session_id = append_session_id(&events)?;
        let meta = self.get_or_open_meta(&session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(&session_id).await?;
        let prepared = meta.projection.prepare_batch(events).await?;
        ensure_log_positioned_for(&meta, &prepared).await?;
        let committed = meta.log.append_prepared_batch(prepared).await?;
        let stored = meta.projection.apply_committed(committed).await;
        Ok(stored)
    }

    async fn append_events_and_sync(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let session_id = append_session_id(&events)?;
        let meta = self.get_or_open_meta(&session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(&session_id).await?;
        let prepared = meta.projection.prepare_batch(events).await?;
        ensure_log_positioned_for(&meta, &prepared).await?;

        let through_seq = prepared
            .events()
            .last()
            .map(|event| event.seq)
            .ok_or_else(crate::error::short_batch_result)?;
        *meta.uncertain_durability.lock() = Some(UncertainDurability::PendingProjection {
            batch: prepared.clone(),
            through_seq,
            reason: FSYNC_CONFIRMATION_PENDING.into(),
        });
        if let Err(error) = meta.log.append_prepared_batch(prepared).await {
            // The append failed before any bytes were committed, so the marker
            // is always safe to drop; keep the original append error.
            let _ = clear_uncertain(&meta, through_seq);
            return Err(error);
        }
        if let Err(error) = meta.log.force_sync().await {
            return Err(update_uncertain_reason(
                &meta,
                &session_id,
                through_seq,
                error.to_string(),
            ));
        }

        apply_confirmed_projection(&meta, through_seq).await
    }

    async fn sync_durable_events(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let next_seq = meta.log.count().await?;
        let through_seq = next_seq.checked_sub(1).ok_or_else(|| {
            StorageError::CorruptLog("created session has an empty event log".into())
        })?;
        *meta.uncertain_durability.lock() = Some(UncertainDurability::Published {
            through_seq,
            reason: FSYNC_CONFIRMATION_PENDING.into(),
        });

        if let Err(error) = meta.log.force_sync().await {
            return Err(update_uncertain_reason(
                &meta,
                session_id,
                through_seq,
                error.to_string(),
            ));
        }
        clear_uncertain(&meta, through_seq)?;
        Ok(())
    }

    async fn retry_uncertain_sync(
        &self,
        session_id: &SessionId,
        expected_through_seq: u64,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_commit_lane().await?;
        let pending_through_seq = meta
            .uncertain_durability
            .lock()
            .as_ref()
            .map(UncertainDurability::through_seq);
        if let Some(actual_through_seq) = pending_through_seq {
            if actual_through_seq != expected_through_seq {
                return Err(uncertain_seq_mismatch(
                    expected_through_seq,
                    actual_through_seq,
                ));
            }
        } else {
            let log_next_seq = meta.log.count().await?;
            let projection = meta.projection.snapshot().await;
            let expected_next_seq = expected_through_seq
                .checked_add(1)
                .ok_or_else(sequence_overflow)?;
            if log_next_seq == expected_next_seq
                && projection.stats.last_seq == expected_through_seq
            {
                return Ok(Vec::new());
            }
            return Err(StorageError::InvalidEvent(format!(
                "session {session_id} has no pending durability confirmation for seq \
                 {expected_through_seq}"
            )));
        }

        if let Err(error) = meta.log.force_sync().await {
            return Err(update_uncertain_reason(
                &meta,
                session_id,
                expected_through_seq,
                error.to_string(),
            ));
        }

        apply_confirmed_projection(&meta, expected_through_seq).await
    }

    async fn ensure_no_uncertain_durability(
        &self,
        session_id: &SessionId,
    ) -> Result<(), StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionStore for FileSystemSessionRepository {
    async fn event_consumer_state(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
    ) -> Result<EventConsumerState, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let path = event_consumer_state_path(&meta.dir, consumer_id)?;
        read_event_consumer_state(&path).await
    }

    async fn checkpoint_event_consumer(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
    ) -> Result<EventConsumerCheckpointOutcome, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _commit_permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let _consumer_permit = meta.consumer_state_lane.acquire().await.map_err(|_| {
            StorageError::Io(std::io::Error::other("event consumer state lane closed"))
        })?;
        let event_count = meta.log.count().await?;
        if seq >= event_count {
            return Err(StorageError::InvalidId(format!(
                "event consumer checkpoint {seq} is beyond the session event log"
            )));
        }
        let path = event_consumer_state_path(&meta.dir, consumer_id)?;
        let mut state = read_event_consumer_state(&path).await?;
        if state.revision != expected_revision {
            return Ok(EventConsumerCheckpointOutcome::StaleRevision);
        }
        if state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq) {
            return Ok(EventConsumerCheckpointOutcome::Accepted);
        }
        state.checkpoint = Some(seq);
        state.consecutive_failures = 0;
        write_event_consumer_state(&path, &state).await?;
        Ok(EventConsumerCheckpointOutcome::Accepted)
    }

    async fn record_event_consumer_failure(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
        error: &str,
        quarantine_after: u32,
    ) -> Result<EventConsumerFailureOutcome, StorageError> {
        if quarantine_after == 0 {
            return Err(StorageError::InvalidId(
                "event consumer quarantine limit must be greater than zero".into(),
            ));
        }
        let meta = self.get_or_open_meta(session_id).await?;
        let _commit_permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let _consumer_permit = meta.consumer_state_lane.acquire().await.map_err(|_| {
            StorageError::Io(std::io::Error::other("event consumer state lane closed"))
        })?;
        let event_count = meta.log.count().await?;
        if seq >= event_count {
            return Err(StorageError::InvalidId(format!(
                "event consumer failure seq {seq} is beyond the session event log"
            )));
        }
        let path = event_consumer_state_path(&meta.dir, consumer_id)?;
        let mut state = read_event_consumer_state(&path).await?;
        if state.revision != expected_revision {
            return Ok(EventConsumerFailureOutcome::StaleRevision);
        }
        if state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq) {
            return Ok(EventConsumerFailureOutcome::AlreadyConsumed);
        }
        state.consecutive_failures = state
            .consecutive_failures
            .checked_add(1)
            .ok_or_else(|| StorageError::CorruptLog("consumer failure count overflow".into()))?;
        let attempts = state.consecutive_failures;
        let outcome = if attempts >= quarantine_after {
            state.record_quarantine(seq, attempts, error)?;
            state.checkpoint = Some(seq);
            state.consecutive_failures = 0;
            EventConsumerFailureOutcome::Quarantined { attempts }
        } else {
            EventConsumerFailureOutcome::Recorded { attempts }
        };
        write_event_consumer_state(&path, &state).await?;
        Ok(outcome)
    }

    async fn set_event_consumer_paused(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        paused: bool,
    ) -> Result<EventConsumerState, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _commit_permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let _consumer_permit = meta.consumer_state_lane.acquire().await.map_err(|_| {
            StorageError::Io(std::io::Error::other("event consumer state lane closed"))
        })?;
        let path = event_consumer_state_path(&meta.dir, consumer_id)?;
        let mut state = read_event_consumer_state(&path).await?;
        if state.paused != paused {
            state.paused = paused;
            write_event_consumer_state(&path, &state).await?;
        }
        Ok(state)
    }

    async fn reset_event_consumer_checkpoint(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        reset: EventConsumerCheckpointReset,
    ) -> Result<EventConsumerState, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _commit_permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let _consumer_permit = meta.consumer_state_lane.acquire().await.map_err(|_| {
            StorageError::Io(std::io::Error::other("event consumer state lane closed"))
        })?;
        let path = event_consumer_state_path(&meta.dir, consumer_id)?;
        let mut state = read_event_consumer_state(&path).await?;
        let previous_checkpoint = state.checkpoint;
        state.checkpoint = match reset {
            EventConsumerCheckpointReset::Beginning => None,
            EventConsumerCheckpointReset::StreamHead => meta.log.count().await?.checked_sub(1),
        };
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| StorageError::CorruptLog("event consumer revision overflow".into()))?;
        state.consecutive_failures = 0;
        if reset == EventConsumerCheckpointReset::StreamHead
            && state.checkpoint != previous_checkpoint
        {
            state.record_skip(previous_checkpoint, state.checkpoint)?;
        }
        write_event_consumer_state(&path, &state).await?;
        Ok(state)
    }

    async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let model = meta.projection.snapshot().await;
        let latest_cursor = model.cursor();
        // Checkpoints are only written when the cursor matches the current
        // recovered projection state. This prevents stale or out-of-order
        // checkpoint snapshots from being persisted.
        if cursor != &latest_cursor {
            return Err(StorageError::InvalidId(format!(
                "checkpoint cursor {cursor} does not match latest cursor {latest_cursor}"
            )));
        }
        meta.snapshot_mgr.create_snapshot((*model).clone()).await?;
        Ok(())
    }

    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.ensure_no_uncertain_durability(session_id).await
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_storage_session_id(session_id)?;

        if let Some(dir) = self.find_session_dir(session_id).await {
            self.sessions
                .write()
                .await
                .retain(|_, meta| !meta.dir.starts_with(&dir));
            tokio::fs::remove_dir_all(&dir).await?;
        } else {
            self.sessions.write().await.remove(session_id);
        }
        Ok(())
    }

    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_storage_session_id(session_id)?;

        let dir = self
            .find_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;

        // 从内存缓存移除
        self.sessions
            .write()
            .await
            .retain(|_, meta| !meta.dir.starts_with(&dir));

        // 结构：subagents/{extension}/{child_id}/
        // 回收到：subagents/.recycled/{extension}/{child_id}/
        // 这样 restore 时能直接 rename 回原位。
        let extension_dir = dir.parent(); // subagents/{extension}/
        let subagents_dir = extension_dir.and_then(|p| p.parent()); // subagents/

        if let (Some(extension_dir), Some(subagents_dir)) = (extension_dir, subagents_dir)
            && subagents_dir.file_name().is_some_and(|n| n == "subagents")
        {
            let extension_name = extension_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let recycled = subagents_dir
                .join(".recycled")
                .join(extension_name.as_ref());
            tokio::fs::create_dir_all(&recycled).await?;
            let dir_name = dir
                .file_name()
                .ok_or_else(|| StorageError::InvalidId("unexpected session dir path".into()))?;
            let dest = recycled.join(dir_name);
            tokio::fs::rename(&dir, &dest).await?;
            return Ok(());
        }

        // 非子 session 或非标准目录结构，回退到删除
        tokio::fs::remove_dir_all(&dir).await?;
        Ok(())
    }

    async fn restore_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_storage_session_id(session_id)?;

        // 在所有 .recycled/{extension}/{session_id} 目录中搜索
        let recycled_path = self
            .find_recycled_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;

        // 结构：subagents/.recycled/{extension}/{session_id}
        // 还原到：subagents/{extension}/{session_id}
        let extension_dir = recycled_path
            .parent() // .recycled/{extension}/
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?;
        let extension_name = extension_dir
            .file_name()
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?
            .to_string_lossy()
            .to_string();
        let recycled_root = extension_dir
            .parent() // .recycled/
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?;
        let subagents_dir = recycled_root
            .parent() // subagents/
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?;

        let dest_parent = subagents_dir.join(&extension_name);
        tokio::fs::create_dir_all(&dest_parent).await?;
        let dest = dest_parent.join(session_id.as_str());

        tokio::fs::rename(&recycled_path, &dest).await?;

        Ok(())
    }

    async fn write_compact_snapshot(
        &self,
        session_id: &SessionId,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;

        let dir = meta.dir.join("compact-snapshots");
        tokio::fs::create_dir_all(&dir).await?;

        let created_at = Utc::now();
        let path = dir.join(format!(
            "compact-{}-{}.jsonl",
            created_at.timestamp_millis(),
            Uuid::new_v4()
        ));

        let mut lines = Vec::with_capacity(snapshot.provider_messages.len() + 1);
        lines.push(
            serde_json::json!({
                "type": "metadata",
                "session_id": session_id,
                "trigger": snapshot.trigger,
                "created_at": created_at.to_rfc3339(),
                "model_id": snapshot.model_id,
                "working_dir": snapshot.working_dir,
                "system_prompt": snapshot.system_prompt,
                "message_count": snapshot.provider_messages.len(),
            })
            .to_string(),
        );
        for (index, message) in snapshot.provider_messages.into_iter().enumerate() {
            lines.push(
                serde_json::json!({
                    "type": "message",
                    "index": index,
                    "message": message,
                })
                .to_string(),
            );
        }

        let mut content = lines.join("\n");
        content.push('\n');
        tokio::fs::write(&path, content).await?;

        Ok(Some(path.to_string_lossy().to_string()))
    }
}

/// 判断目录是否位于 subagents 子树下。
fn is_subagent_dir(dir: &Path) -> bool {
    dir.ancestors()
        .any(|a| a.file_name().is_some_and(|n| n == "subagents"))
}

/// 会话目录内由存储层维护的元数据目录，扫描会话目录时必须跳过。
fn is_session_metadata_dir(name: &str) -> bool {
    matches!(
        name,
        ".recycled" | "subagents" | "snapshots" | "compact-snapshots" | "tool-results"
    )
}

impl FileSystemSessionRepository {
    /// 在所有项目的 subagents/.recycled/{extension}/ 目录中搜索指定 session。
    async fn find_recycled_session_dir(&self, id: &SessionId) -> Option<PathBuf> {
        for root in self.all_session_roots().await {
            if let Some(found) = self.search_recycled_in_root(&root, id).await {
                return Some(found);
            }
        }
        None
    }

    /// 搜索 sessions_root 下所有 session 的 subagents/.recycled/{extension}/{id}。
    async fn search_recycled_in_root(
        &self,
        sessions_root: &Path,
        id: &SessionId,
    ) -> Option<PathBuf> {
        let mut stack = vec![sessions_root.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let name = entry.file_name();
                if is_session_metadata_dir(&name.to_string_lossy()) {
                    continue;
                }
                let session_dir = entry.path();
                let recycled_dir = session_dir.join("subagents").join(".recycled");
                if directory_exists(&recycled_dir).await
                    && let Ok(mut extension_entries) = tokio::fs::read_dir(&recycled_dir).await
                {
                    while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                        let candidate = extension_entry.path().join(id.as_str());
                        if directory_exists(&candidate).await {
                            return Some(candidate);
                        }
                    }
                }
                let subagents = session_dir.join("subagents");
                if directory_exists(&subagents).await {
                    let Ok(mut extension_entries) = tokio::fs::read_dir(&subagents).await else {
                        continue;
                    };
                    while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                        let pname = extension_entry.file_name();
                        if pname.to_string_lossy() == ".recycled" {
                            continue;
                        }
                        if extension_entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                            stack.push(extension_entry.path());
                        }
                    }
                }
            }
        }
        None
    }

    /// 仅扫描磁盘上的会话目录名，不打开任何文件。
    async fn list_session_dirs(&self) -> Result<Vec<SessionId>, StorageError> {
        let mut ids: BTreeSet<SessionId> = self
            .sessions
            .read()
            .await
            .iter()
            .filter(|(_, meta)| !is_subagent_dir(&meta.dir))
            .map(|(id, _)| id.clone())
            .collect();
        for base_path in self.all_session_roots().await {
            self.collect_session_ids_from_dir(&base_path, &mut ids)
                .await;
        }
        Ok(ids.into_iter().collect())
    }

    /// Scans active root and nested subagent session directories without opening logs.
    async fn list_all_session_locations(
        &self,
    ) -> Result<BTreeMap<SessionId, PathBuf>, StorageError> {
        let mut locations: BTreeMap<SessionId, PathBuf> = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(id, meta)| (id.clone(), meta.dir.clone()))
            .collect();
        for sessions_root in self.all_session_roots_strict().await? {
            let mut containers = vec![sessions_root];
            while let Some(container) = containers.pop() {
                let mut entries = match tokio::fs::read_dir(container).await {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                while let Some(entry) = entries.next_entry().await? {
                    if !entry.file_type().await?.is_dir() {
                        continue;
                    }
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if is_session_metadata_dir(&name) {
                        continue;
                    }
                    let session_id = SessionId::from(name.into_owned());
                    let session_dir = entry.path();
                    match tokio::fs::metadata(Self::event_log_path(&session_dir, &session_id)).await
                    {
                        Ok(metadata) if metadata.is_file() => {},
                        Ok(_) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    }
                    locations.entry(session_id).or_insert(session_dir.clone());
                    let subagents = session_dir.join("subagents");
                    let mut extensions = match tokio::fs::read_dir(subagents).await {
                        Ok(entries) => entries,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    };
                    while let Some(extension) = extensions.next_entry().await? {
                        if extension.file_name().to_string_lossy() != ".recycled"
                            && extension.file_type().await?.is_dir()
                        {
                            containers.push(extension.path());
                        }
                    }
                }
            }
        }
        Ok(locations)
    }

    /// 收集 `base` 下一层会话目录名（不含 `subagents/` 等元数据目录）。
    ///
    /// 子 agent 会话存放在 `subagents/<id>/`，由 `find_session_dir` 按需解析，
    /// 不出现在 `list_sessions` 结果中。
    async fn collect_session_ids_from_dir(&self, base: &Path, ids: &mut BTreeSet<SessionId>) {
        let Ok(mut entries) = tokio::fs::read_dir(base).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if is_session_metadata_dir(&name_str) {
                continue;
            }
            let id = SessionId::from(name_str.to_string());
            ids.insert(id);
        }
    }

    /// 从事件日志投影轻量级 SessionSummary，不构造完整 transcript。
    async fn read_summary_from_event_log(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, StorageError> {
        let Some(dir) = self.find_session_dir(session_id).await else {
            return Ok(None);
        };
        let log_path = Self::event_log_path(&dir, session_id);
        EventLog::read_summary(&log_path, session_id.clone()).await
    }
}

#[cfg(test)]
mod tests;

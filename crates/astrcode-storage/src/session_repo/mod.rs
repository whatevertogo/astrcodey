//! 基于文件系统的会话仓库。
//!
//! 管理按项目组织的会话事件日志，目录结构为：
//! `~/.astrcode/projects/<project>/sessions/<session>/`
//!
//! 模块划分:
//! - `projection`:事件日志同步维护的读模型缓存
//! - `owner_lease`:跨进程会话目录所有权租约
//! - `durability`:fsync 结果不确定时的 sticky 状态机
//! - `consumer_state`:durable event consumer 状态的读写
//! - `dir_scan`:项目/会话目录扫描与路径布局
//! - `reader` / `journal` / `store` / `artifacts`:各 trait 端口实现

mod artifacts;
mod consumer_state;
mod dir_scan;
mod durability;
mod journal;
mod owner_lease;
mod projection;
mod reader;
mod store;
#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::{
    config::defaults::astrcode_dir,
    event::{DurableEvent, StoredEvent},
    types::{Cursor, SessionId, validate_session_id},
};
use astrcode_session_projection::{ProjectionError, SessionReadModel, reduce, replay};
use parking_lot::Mutex;
use tokio::sync::{RwLock, Semaphore};

use self::{
    durability::UncertainDurability, owner_lease::SessionOwnerLease, projection::SessionProjection,
};
use crate::{
    StorageError,
    event_log::EventLog,
    snapshot::{SessionProjectionSnapshot, SnapshotManager},
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

fn parse_cursor(cursor: &Cursor) -> Result<u64, StorageError> {
    cursor
        .parse()
        .map_err(|_| StorageError::InvalidId(format!("Invalid cursor: {cursor}")))
}

fn append_session_id(events: &[DurableEvent]) -> Result<SessionId, StorageError> {
    events
        .first()
        .map(|event| event.session_id.clone())
        .ok_or_else(|| StorageError::InvalidEvent("event batch cannot be empty".into()))
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
    sessions: RwLock<HashMap<SessionId, Arc<SessionMeta>>>,
    /// 冷打开按会话串行:磁盘打开与 projection 恢复太贵,并发准入时不得重复执行。
    /// 条目按本会话数有界,不主动回收。
    open_lanes: Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
    /// 所有项目目录的父目录：`~/.astrcode/projects/`
    projects_base: PathBuf,
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
    /// `dir` 的 canonical 形式。会话目录在 meta 存活期内不移动,打开时解析一次,
    /// 供 tool result artifact 读路径的前缀校验复用。
    canonical_dir: PathBuf,
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

impl SessionMeta {
    async fn new(
        dir: PathBuf,
        owner_lease: SessionOwnerLease,
        log: EventLog,
        snapshot_mgr: SnapshotManager,
        projection: SessionReadModel,
    ) -> Result<Self, StorageError> {
        let canonical_dir = tokio::fs::canonicalize(&dir).await?;
        Ok(Self {
            _owner_lease: owner_lease,
            log,
            snapshot_mgr,
            dir,
            canonical_dir,
            projection: SessionProjection::new(projection),
            commit_lane: Semaphore::new(1),
            uncertain_durability: Mutex::new(None),
            consumer_state_lane: Semaphore::new(1),
        })
    }

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

    pub(crate) fn with_projects_base(projects_base: PathBuf) -> Self {
        if let Err(e) = std::fs::create_dir_all(&projects_base) {
            tracing::warn!(
                "Failed to create projects dir {}: {e}",
                projects_base.display()
            );
        }
        Self {
            owner: Arc::new(()),
            sessions: RwLock::new(HashMap::new()),
            open_lanes: Mutex::new(HashMap::new()),
            projects_base,
        }
    }

    #[cfg(feature = "testing")]
    pub(crate) async fn fail_next_durable_sync(
        &self,
        session_id: &SessionId,
    ) -> Result<(), StorageError> {
        self.get_or_open_meta(session_id)
            .await?
            .log
            .fail_next_sync()
            .await
    }

    /// 获取指定会话的事件日志文件路径。
    fn event_log_path(session_dir: &Path, id: &SessionId) -> PathBuf {
        session_dir.join(format!("session-{id}.jsonl"))
    }

    /// 获取或打开会话元数据。
    ///
    /// 如果会话已在内存中则直接返回缓存；否则从磁盘打开事件日志，
    /// 恢复其内存中的 seq 计数器，并加入缓存。
    async fn get_or_open_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionMeta>, StorageError> {
        validate_storage_session_id(session_id)?;

        if let Some(meta) = self.sessions.read().await.get(session_id).cloned() {
            return Ok(meta);
        }

        let lane = self
            .open_lanes
            .lock()
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _permit = lane.lock().await;

        // 拿到 lane 后再查一次:前一个 opener 可能已经完成了插入。
        if let Some(meta) = self.sessions.read().await.get(session_id).cloned() {
            return Ok(meta);
        }

        // 磁盘打开与 projection 恢复可能较慢；不得持 `sessions` 写锁跨越 await，
        // 否则会阻塞同实例上的 append/checkpoint，并在 Windows 上与未释放的
        // EventLog 句柄争用同一 JSONL 文件。
        let dir = self.existing_session_dir(session_id).await?;
        let owner_lease = SessionOwnerLease::acquire(&dir, &self.owner).await?;
        let (log, events) = EventLog::open(Self::event_log_path(&dir, session_id)).await?;
        let snapshot_mgr = SnapshotManager::new(dir.join("snapshots"));
        let projection = Self::restore_projection(session_id, &snapshot_mgr, &events).await?;
        let opened =
            Arc::new(SessionMeta::new(dir, owner_lease, log, snapshot_mgr, projection).await?);

        let mut sessions = self.sessions.write().await;
        Ok(if let Some(meta) = sessions.get(session_id) {
            Arc::clone(meta)
        } else {
            sessions.insert(session_id.clone(), Arc::clone(&opened));
            opened
        })
    }

    /// 从冷打开时已验证的事件恢复 projection;快照命中时只在内存里补尾部。
    async fn restore_projection(
        session_id: &SessionId,
        snapshot_mgr: &SnapshotManager,
        events: &[StoredEvent],
    ) -> Result<SessionReadModel, StorageError> {
        // Snapshot first because it's faster than replaying the full event log.
        if let Some(snapshot) = snapshot_mgr.latest_snapshot().await? {
            match restore_from_snapshot(session_id, events, snapshot) {
                Ok(model) => return Ok(model),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        "Falling back to full event replay after snapshot restore failed: {error}"
                    );
                },
            }
        }

        replay(session_id.clone(), events).map_err(corrupt_projection)
    }
}

fn restore_from_snapshot(
    expected_session_id: &SessionId,
    events: &[StoredEvent],
    snapshot: SessionProjectionSnapshot,
) -> Result<SessionReadModel, StorageError> {
    if snapshot.model.identity.session_id != *expected_session_id {
        return Err(StorageError::CorruptLog(format!(
            "projection snapshot belongs to session {}, expected {}",
            snapshot.model.identity.session_id, expected_session_id
        )));
    }
    let latest_seq = snapshot.model.stats.last_seq;

    // 事件流经过打开时的连续 seq 校验,事件数即下一条待分配的 seq。
    let next_seq = events.len() as u64;
    if latest_seq >= next_seq {
        return Err(StorageError::InvalidId(format!(
            "snapshot latest_seq {latest_seq} is outside event log (next_seq={next_seq})"
        )));
    }

    let mut model = snapshot.model;
    // Reapply only the events that occurred after the snapshot. The snapshot
    // serves as a recovery checkpoint, not as an authoritative source of truth.
    for event in events.iter().filter(|event| event.seq > latest_seq) {
        reduce(event, &mut model).map_err(corrupt_projection)?;
    }
    Ok(model)
}

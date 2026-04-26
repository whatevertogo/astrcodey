//! # 会话轮次文件锁
//!
//! 通过操作系统级文件锁（`fs2::FileExt`）实现会话写入互斥，防止多进程
//! 同时向同一会话的 JSONL 文件追加事件，保证 `storage_seq` 的单调性和独占分配。
//!
//! ## 锁机制
//!
//! - **锁文件**：`active-turn.lock` — 通过 `try_lock_exclusive()` 获取独占锁
//! - **元数据文件**：`active-turn.json` —
//!   存储当前持有者信息（`turn_id`、`owner_pid`、`acquired_at`）
//! - **锁与元数据分离**：锁文件保证互斥，元数据文件供竞争者读取当前状态
//!
//! ## 获取流程
//!
//! 1. 打开锁文件并尝试 `try_lock_exclusive()`
//! 2. 如果成功：写入元数据文件，返回 `Acquired(lease)`
//! 3. 如果锁被占用（contended）：读取元数据文件，返回 `Busy(payload)`
//! 4. 如果元数据文件尚不可读（持有者还未写入）：短暂重试（8 次 × 5ms），
//!    重试期间如果锁已释放则直接接管
//!
//! ## 释放流程
//!
//! `FileSessionTurnLease` 的 `Drop` 实现中：
//! 1. 释放文件锁（`file.unlock()`）
//! 2. 删除元数据文件（忽略 NotFound 错误，因为可能已被新持有者覆盖）
//!
//! ## 竞态处理
//!
//! 锁状态检查和元数据读取不是原子操作，存在以下竞态：
//! - 读取元数据时锁仍被占用 → 返回 Busy
//! - 读取元数据后锁已释放 → 重试获取，成功则接管
//! - 元数据文件不存在（持有者还未写入） → 短暂重试，超时后如果锁仍占用则返回 Busy

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use astrcode_core::store::{SessionTurnAcquireResult, SessionTurnBusy, SessionTurnLease};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::paths::{
    session_turn_lock_path, session_turn_lock_path_from_projects_root, session_turn_metadata_path,
    session_turn_metadata_path_from_projects_root,
};
use crate::{Result, io_error, parse_error};

/// 活跃轮次锁的元数据载荷。
///
/// 序列化到 `active-turn.json` 文件中，供竞争者读取以判断当前会话状态。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveTurnLockPayload {
    /// 当前轮次 ID。
    turn_id: String,
    /// 持有锁的进程 ID。
    owner_pid: u32,
    /// 获取锁的时间戳。
    acquired_at: chrono::DateTime<Utc>,
}

/// 读取元数据文件的重试次数。
const LOCK_PAYLOAD_RETRY_ATTEMPTS: usize = 8;
/// 每次重试之间的等待时间。
const LOCK_PAYLOAD_RETRY_DELAY: Duration = Duration::from_millis(5);

/// 尝试获取会话轮次锁。
///
/// 如果锁空闲则获取并返回 `Acquired(lease)`；
/// 如果锁被占用则读取元数据并返回 `Busy(payload)`；
/// 如果元数据暂不可读则短暂重试，重试期间锁释放则直接接管。
pub(super) fn try_acquire_session_turn(
    session_id: &str,
    turn_id: &str,
) -> Result<SessionTurnAcquireResult> {
    let path = session_turn_lock_path(session_id)?;
    let metadata_path = session_turn_metadata_path(session_id)?;
    try_acquire_session_turn_at_paths(path, metadata_path, turn_id)
}

pub(super) fn try_acquire_session_turn_in_projects_root(
    projects_root: &Path,
    session_id: &str,
    turn_id: &str,
) -> Result<SessionTurnAcquireResult> {
    let path = session_turn_lock_path_from_projects_root(projects_root, session_id)?;
    let metadata_path = session_turn_metadata_path_from_projects_root(projects_root, session_id)?;
    try_acquire_session_turn_at_paths(path, metadata_path, turn_id)
}

fn try_acquire_session_turn_at_paths(
    path: PathBuf,
    metadata_path: PathBuf,
    turn_id: &str,
) -> Result<SessionTurnAcquireResult> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| {
            io_error(
                format!("failed to open session turn lock: {}", path.display()),
                error,
            )
        })?;

    match file.try_lock_exclusive() {
        Ok(()) => acquire_turn_lease(file, path, metadata_path, turn_id),
        Err(error) if is_lock_contended(&error) => {
            read_busy_payload_or_retry(file, path, metadata_path, turn_id)
        },
        Err(error) => Err(io_error(
            format!("failed to acquire session turn lock: {}", path.display()),
            error,
        )),
    }
}

/// 获取锁成功后写入元数据并返回租约。
///
/// 将当前进程信息和时间戳写入 `active-turn.json`，
/// 供后续竞争者读取以判断会话状态。
fn acquire_turn_lease(
    file: File,
    path: PathBuf,
    metadata_path: PathBuf,
    turn_id: &str,
) -> Result<SessionTurnAcquireResult> {
    let payload = ActiveTurnLockPayload {
        turn_id: turn_id.to_string(),
        owner_pid: std::process::id(),
        acquired_at: Utc::now(),
    };
    write_lock_payload(&metadata_path, &payload)?;
    Ok(SessionTurnAcquireResult::Acquired(Box::new(
        FileSessionTurnLease {
            file,
            path,
            metadata_path,
        },
    )))
}

/// 锁被占用时的处理逻辑。
///
/// 先尝试读取元数据文件获取当前持有者信息，如果元数据暂不可读（持有者
/// 还未刷盘），则短暂重试。重试期间如果锁已释放则直接接管。
fn read_busy_payload_or_retry(
    file: File,
    path: PathBuf,
    metadata_path: PathBuf,
    requested_turn_id: &str,
) -> Result<SessionTurnAcquireResult> {
    let mut last_retryable_error = None;

    for attempt in 0..LOCK_PAYLOAD_RETRY_ATTEMPTS {
        match read_lock_payload(&metadata_path) {
            Ok(payload) => match file.try_lock_exclusive() {
                Ok(()) => {
                    // contended 结果和 metadata 读取不是原子操作；如果对方已经在两者之间
                    // 释放了锁，就直接接管，避免把一把已空闲的 session 误判成 Busy。
                    return acquire_turn_lease(file, path, metadata_path, requested_turn_id);
                },
                Err(lock_error) if is_lock_contended(&lock_error) => {
                    return Ok(SessionTurnAcquireResult::Busy(session_turn_busy(payload)));
                },
                Err(lock_error) => {
                    return Err(io_error(
                        format!(
                            "failed to confirm busy session turn lock state: {}",
                            path.display()
                        ),
                        lock_error,
                    ));
                },
            },
            Err(error) if should_retry_busy_payload_error(&error) => {
                last_retryable_error = Some(error);

                // metadata 与文件锁分离存储：拿到 contended 结果后，锁持有者可能还在
                // 把 payload 刷盘，或者刚完成 unlock。这里短暂重试并趁锁释放时直接接管，
                // 可以把瞬时竞态收敛成正常 acquire，而不是向上层抛 500。
                match file.try_lock_exclusive() {
                    Ok(()) => {
                        return acquire_turn_lease(file, path, metadata_path, requested_turn_id);
                    },
                    Err(lock_error) if is_lock_contended(&lock_error) => {},
                    Err(lock_error) => {
                        return Err(io_error(
                            format!(
                                "failed to retry session turn lock acquisition: {}",
                                path.display()
                            ),
                            lock_error,
                        ));
                    },
                }

                if attempt + 1 < LOCK_PAYLOAD_RETRY_ATTEMPTS {
                    std::thread::sleep(LOCK_PAYLOAD_RETRY_DELAY);
                }
            },
            Err(error) => return Err(error),
        }
    }

    Err(last_retryable_error.unwrap_or_else(|| {
        io_error(
            format!(
                "busy session turn metadata never became readable: {}",
                metadata_path.display()
            ),
            std::io::Error::other("session turn payload was unavailable while lock stayed busy"),
        )
    }))
}

/// 将锁元数据转换为 `SessionTurnBusy` 响应。
fn session_turn_busy(payload: ActiveTurnLockPayload) -> SessionTurnBusy {
    SessionTurnBusy {
        turn_id: payload.turn_id,
        owner_pid: payload.owner_pid,
        acquired_at: payload.acquired_at,
    }
}

/// 判断 IO 错误是否为锁竞争错误。
///
/// 不同平台的锁竞争错误码可能不同，此函数通过比较 `kind` 和 `raw_os_error`
/// 来跨平台正确识别竞争状态。
fn is_lock_contended(error: &std::io::Error) -> bool {
    error.kind() == fs2::lock_contended_error().kind()
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// 判断是否应该重试读取忙载荷。
///
/// 当元数据文件尚未创建（NotFound）或解析失败（Parse）时，
/// 说明持有者可能还在写入过程中，可以短暂重试。
fn should_retry_busy_payload_error(error: &astrcode_core::StoreError) -> bool {
    match error {
        astrcode_core::StoreError::Io { source, .. } => {
            source.kind() == std::io::ErrorKind::NotFound
        },
        astrcode_core::StoreError::Parse { .. } => true,
        _ => false,
    }
}

fn write_lock_payload(path: &Path, payload: &ActiveTurnLockPayload) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(payload)
        .map_err(|error| parse_error("failed to serialize active turn lock payload", error))?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| {
            io_error(
                format!("failed to open session turn metadata: {}", path.display()),
                error,
            )
        })?;
    file.set_len(0).map_err(|error| {
        io_error(
            format!(
                "failed to truncate session turn metadata: {}",
                path.display()
            ),
            error,
        )
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        io_error(
            format!("failed to seek session turn metadata: {}", path.display()),
            error,
        )
    })?;
    file.write_all(&bytes).map_err(|error| {
        io_error(
            format!("failed to write session turn metadata: {}", path.display()),
            error,
        )
    })?;
    file.flush().map_err(|error| {
        io_error(
            format!("failed to flush session turn metadata: {}", path.display()),
            error,
        )
    })?;
    file.sync_all().map_err(|error| {
        io_error(
            format!("failed to sync session turn metadata: {}", path.display()),
            error,
        )
    })?;
    Ok(())
}

fn read_lock_payload(path: &Path) -> Result<ActiveTurnLockPayload> {
    let mut file = OpenOptions::new().read(true).open(path).map_err(|error| {
        io_error(
            format!(
                "failed to open busy session turn metadata: {}",
                path.display()
            ),
            error,
        )
    })?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).map_err(|error| {
        io_error(
            format!(
                "failed to read busy session turn metadata: {}",
                path.display()
            ),
            error,
        )
    })?;
    serde_json::from_str(&raw).map_err(|error| {
        parse_error(
            format!(
                "failed to parse busy session turn lock payload '{}'",
                path.display()
            ),
            error,
        )
    })
}

struct FileSessionTurnLease {
    file: File,
    path: PathBuf,
    metadata_path: PathBuf,
}

impl Drop for FileSessionTurnLease {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            log::warn!(
                "failed to unlock session turn lock '{}': {}",
                self.path.display(),
                error
            );
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.metadata_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "failed to remove session turn metadata '{}': {}",
                    self.metadata_path.display(),
                    error
                );
            }
        }
    }
}

impl SessionTurnLease for FileSessionTurnLease {}

#[cfg(test)]
mod tests {
    use std::thread;

    use astrcode_core::test_support::TestEnvGuard;
    use fs2::FileExt;

    use super::{super::event_log::EventLog, *};

    fn create_session_fixture(session_id: &str) -> tempfile::TempDir {
        let working_dir = tempfile::tempdir().expect("tempdir should be created");
        EventLog::create(session_id, working_dir.path()).expect("event log should be created");
        working_dir
    }

    #[test]
    fn second_acquire_reads_busy_payload_when_metadata_is_ready() {
        let _guard = TestEnvGuard::new();
        let _working_dir = create_session_fixture("lock-busy-ready");

        let first = try_acquire_session_turn("lock-busy-ready", "turn-1")
            .expect("first acquire should succeed");
        let busy = try_acquire_session_turn("lock-busy-ready", "turn-2")
            .expect("second acquire should return busy");

        match busy {
            SessionTurnAcquireResult::Busy(active_turn) => {
                assert_eq!(active_turn.turn_id, "turn-1");
            },
            SessionTurnAcquireResult::Acquired(_) => {
                panic!("second acquire must not take the lock while the first lease lives")
            },
        }

        drop(first);
    }

    #[test]
    fn missing_busy_metadata_retries_until_the_lock_is_released() {
        let _guard = TestEnvGuard::new();
        let _working_dir = create_session_fixture("lock-metadata-retry");

        let lock_path =
            session_turn_lock_path("lock-metadata-retry").expect("lock path should resolve");
        let metadata_path = session_turn_metadata_path("lock-metadata-retry")
            .expect("metadata path should resolve");
        let holder = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("lock file should open");
        holder
            .try_lock_exclusive()
            .expect("holder should acquire lock");

        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(15));
            holder.unlock().expect("holder should unlock cleanly");
        });

        let result = try_acquire_session_turn("lock-metadata-retry", "turn-2")
            .expect("retry path should recover instead of failing");
        releaser.join().expect("releaser should finish");

        let lease = match result {
            SessionTurnAcquireResult::Acquired(lease) => lease,
            SessionTurnAcquireResult::Busy(_) => {
                panic!("retry path should acquire once the stale lock is gone")
            },
        };

        let payload =
            read_lock_payload(&metadata_path).expect("recovered lease should rewrite payload");
        assert_eq!(payload.turn_id, "turn-2");

        drop(lease);
        assert!(
            !metadata_path.exists(),
            "dropping a healthy lease should clean up its metadata file"
        );
    }

    #[test]
    fn invalid_busy_metadata_retries_until_a_new_owner_can_rewrite_it() {
        let _guard = TestEnvGuard::new();
        let _working_dir = create_session_fixture("lock-corrupt-metadata");

        let lock_path =
            session_turn_lock_path("lock-corrupt-metadata").expect("lock path should resolve");
        let metadata_path = session_turn_metadata_path("lock-corrupt-metadata")
            .expect("metadata path should resolve");
        let holder = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("lock file should open");
        holder
            .try_lock_exclusive()
            .expect("holder should acquire lock");
        std::fs::write(&metadata_path, "{").expect("corrupt metadata should be written");

        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(15));
            holder.unlock().expect("holder should unlock cleanly");
        });

        let result = try_acquire_session_turn("lock-corrupt-metadata", "turn-3")
            .expect("corrupt busy metadata should be recoverable once the lock is free");
        releaser.join().expect("releaser should finish");

        let lease = match result {
            SessionTurnAcquireResult::Acquired(lease) => lease,
            SessionTurnAcquireResult::Busy(_) => {
                panic!("corrupt metadata should not leave the session permanently busy")
            },
        };

        let payload =
            read_lock_payload(&metadata_path).expect("new owner should replace corrupt metadata");
        assert_eq!(payload.turn_id, "turn-3");

        drop(lease);
    }
}

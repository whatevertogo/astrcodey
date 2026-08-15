//! 会话目录的跨进程所有权租约。
//!
//! 进程内通过全局注册表按 canonical 路径去重;跨进程通过
//! `.astrcode-session-owner.lock` 文件上的 OS 排他锁互斥。

use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
};

use fs2::FileExt;
use parking_lot::Mutex;

use crate::StorageError;

static SESSION_OWNER_LEASES: OnceLock<Mutex<HashMap<PathBuf, Weak<SessionOwnerLeaseInner>>>> =
    OnceLock::new();

fn session_owner_leases() -> &'static Mutex<HashMap<PathBuf, Weak<SessionOwnerLeaseInner>>> {
    SESSION_OWNER_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) struct SessionOwnerLease {
    _inner: Arc<SessionOwnerLeaseInner>,
}

struct SessionOwnerLeaseInner {
    path: PathBuf,
    owner: Arc<()>,
    _file: File,
}

impl SessionOwnerLease {
    pub(super) async fn acquire(session_dir: &Path, owner: &Arc<()>) -> Result<Self, StorageError> {
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

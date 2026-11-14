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
    file: File,
}

impl SessionOwnerLease {
    pub(super) async fn acquire(session_dir: &Path, owner: &Arc<()>) -> Result<Self, StorageError> {
        let session_dir = session_dir.to_path_buf();
        let owner = Arc::clone(owner);
        crate::durable_write::spawn_blocking_storage("session owner lease", move || {
            Self::acquire_blocking(&session_dir, &owner)
        })
        .await
    }

    fn acquire_blocking(session_dir: &Path, owner: &Arc<()>) -> Result<Self, StorageError> {
        let key = std::fs::canonicalize(session_dir).map_err(StorageError::Io)?;
        let mut leases = session_owner_leases().lock();

        if let Some(inner) = leases.get(&key).and_then(Weak::upgrade) {
            // The upgraded reference can become the last one, and Drop locks the registry.
            drop(leases);
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
            file,
        });
        leases.insert(key, Arc::downgrade(&inner));
        Ok(Self { _inner: inner })
    }
}

impl Drop for SessionOwnerLeaseInner {
    fn drop(&mut self) {
        let mut leases = session_owner_leases().lock();
        // An inherited or duplicated descriptor can outlive this File.
        let unlock = FileExt::unlock(&self.file);
        if leases
            .get(&self.path)
            .is_some_and(|lease| lease.strong_count() == 0)
        {
            leases.remove(&self.path);
        }
        drop(leases);
        if let Err(error) = unlock {
            tracing::warn!(path = %self.path.display(), %error, "failed to unlock session owner lease");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn releasing_owner_drops_lock_even_with_a_duplicate_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let lease = SessionOwnerLease::acquire_blocking(dir.path(), &Arc::new(())).unwrap();
        let duplicate = lease._inner.file.try_clone().unwrap();
        drop(lease);

        let _next_lease = SessionOwnerLease::acquire_blocking(dir.path(), &Arc::new(())).unwrap();
        drop(duplicate);
    }
}

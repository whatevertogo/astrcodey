//! Turn-level file locking for session concurrency control.

use std::path::PathBuf;

/// A file-based lock for session turns.
///
/// Only one turn can execute per session at a time.
pub struct TurnLock {
    path: PathBuf,
}

impl TurnLock {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Acquire the turn lock (blocking until available).
    pub async fn acquire(&self) -> Result<TurnLockGuard, std::io::Error> {
        // Simple file-based lock: create the lock file
        // TODO: Use fs2 for proper OS-level file locking
        while self.path.exists() {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        std::fs::write(&self.path, &[])?;
        Ok(TurnLockGuard {
            path: self.path.clone(),
        })
    }
}

pub struct TurnLockGuard {
    path: PathBuf,
}

impl Drop for TurnLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

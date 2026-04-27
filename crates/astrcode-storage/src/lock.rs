//! Turn-level file locking for session concurrency control.

use std::path::PathBuf;

/// A file-based lock for session turns.
///
/// Only one turn can execute per session at a time.
/// Uses atomic file creation (`File::create_new`) to prevent TOCTOU races.
pub struct TurnLock {
    path: PathBuf,
}

impl TurnLock {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Acquire the turn lock (blocking until available).
    pub async fn acquire(&self) -> Result<TurnLockGuard, std::io::Error> {
        loop {
            match std::fs::File::create_new(&self.path) {
                Ok(_) => {
                    return Ok(TurnLockGuard {
                        path: self.path.clone(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

pub struct TurnLockGuard {
    path: PathBuf,
}

impl Drop for TurnLockGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!(
                "Failed to remove turn lock file {}: {e}",
                self.path.display()
            );
        }
    }
}

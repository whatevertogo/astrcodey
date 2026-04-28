//! 回合级别的文件锁，用于会话并发控制。
//!
//! 确保每个会话同一时间只有一个回合在执行。
//! 使用原子文件创建（`File::create_new`）防止 TOCTOU 竞态条件。

use std::{path::PathBuf, time::Duration};

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

    /// Acquire the turn lock, waiting until the current owner releases it.
    pub async fn acquire(&self) -> Result<TurnLockGuard, std::io::Error> {
        self.acquire_inner(None).await
    }

    /// Acquire with a custom timeout.
    ///
    /// This never removes an existing lock file. A long-running turn is still
    /// the active owner, so callers that need crash recovery must use a
    /// separate ownership/heartbeat mechanism before breaking the lock.
    pub async fn acquire_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<TurnLockGuard, std::io::Error> {
        self.acquire_inner(Some(timeout)).await
    }

    async fn acquire_inner(
        &self,
        timeout: Option<Duration>,
    ) -> Result<TurnLockGuard, std::io::Error> {
        let start = tokio::time::Instant::now();
        loop {
            match std::fs::File::create_new(&self.path) {
                Ok(_) => {
                    return Ok(TurnLockGuard {
                        path: self.path.clone(),
                    });
                },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if timeout.is_some_and(|timeout| start.elapsed() > timeout) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "Timed out waiting for turn lock after {:?}: {}",
                                timeout.unwrap(),
                                self.path.display()
                            ),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                },
                Err(e) => return Err(e),
            }
        }
    }
}

/// 回合锁的守卫，释放时自动删除锁文件（RAII 模式）。
pub struct TurnLockGuard {
    /// 锁文件路径
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timed_out_acquire_does_not_break_existing_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("turn.lock");
        let lock = TurnLock::new(path.clone());
        let _guard = lock.acquire().await.unwrap();

        let error = match lock.acquire_with_timeout(Duration::from_millis(10)).await {
            Ok(_) => panic!("second acquire should time out while the first guard is held"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(path.exists());
    }
}

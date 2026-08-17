//! 落盘写原语:临时文件 + fsync + 原子 rename + 目录 fsync。
//!
//! 供需要崩溃一致性的持久化写共用(consumer state、config store)。

use std::{io::Write as _, path::Path};

use crate::StorageError;

/// 在阻塞线程池运行存储 IO，join 失败时映射为 [`StorageError::Io`]。
///
/// `noun` 用于 join 错误文案，标识被阻塞的存储操作。
pub(crate) async fn spawn_blocking_storage<F, T>(noun: &str, f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|error| {
        StorageError::Io(std::io::Error::other(format!(
            "{noun} blocking task failed: {error}"
        )))
    })?
}

/// 以「写临时文件 → fsync → rename → fsync 目录」的顺序替换 `path` 的内容。
pub(crate) fn replace_durable_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_created = !parent.exists();
    std::fs::create_dir_all(parent)?;
    if parent_created {
        sync_directory(parent.parent())?;
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(".astrcode-durable-")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_directory(Some(parent))
}

#[cfg(unix)]
pub(crate) fn sync_directory(directory: Option<&Path>) -> std::io::Result<()> {
    if let Some(directory) = directory {
        std::fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_directory: Option<&Path>) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn concurrent_replacements_use_independent_temporary_files() {
        const WRITERS: usize = 16;

        let directory = tempfile::tempdir().unwrap();
        let path = Arc::new(directory.path().join("config.toml"));
        let barrier = Arc::new(Barrier::new(WRITERS));
        let writes = (0..WRITERS)
            .map(|writer| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let content = format!("writer = {writer}\n").repeat(4_096);
                    barrier.wait();
                    replace_durable_file(&path, content.as_bytes()).map(|()| content)
                })
            })
            .collect::<Vec<_>>();

        let completed = writes
            .into_iter()
            .map(|write| write.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        let persisted = std::fs::read_to_string(path.as_ref()).unwrap();

        assert!(completed.contains(&persisted));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

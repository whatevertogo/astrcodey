//! 落盘写原语:临时文件 + fsync + 原子 rename + 目录 fsync。
//!
//! 供需要崩溃一致性的持久化写共用(consumer state、config store)。

use std::{
    fs::File,
    io::Write as _,
    path::{Path, PathBuf},
};

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

    let temporary = temporary_sibling(path);
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
    result
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
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

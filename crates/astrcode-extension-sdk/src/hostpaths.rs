//! Host path operations exposed to extensions.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write as _},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub use astrcode_core::config::defaults::{astrcode_dir, user_home_dir};

/// Resolve `path` against `base_dir` unless it is already absolute.
pub fn resolve_path(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

/// Atomically write `content` to `path`.
///
/// The bytes go to a unique sibling temporary file first, which is flushed and
/// synced before being renamed over `path`. Parent directories are created as needed.
pub fn write_file_atomic(path: &Path, content: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let (mut temporary, temporary_path) = create_atomic_write_file(parent)?;
    let write_result = temporary
        .write_all(content.as_bytes())
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.sync_all());
    drop(temporary);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary_path, path) {
        let _ = std::fs::remove_file(temporary_path);
        return Err(error);
    }
    Ok(())
}

static NEXT_ATOMIC_WRITE_ID: AtomicU64 = AtomicU64::new(0);

fn create_atomic_write_file(parent: &Path) -> io::Result<(File, PathBuf)> {
    for _ in 0..100 {
        let id = NEXT_ATOMIC_WRITE_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".astrcode-write-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate a unique atomic-write temporary file",
    ))
}

/// 读取 JSON 状态文件；文件不存在时返回 `Ok(None)`。解析失败以 io::Error 返回。
pub fn read_json_state<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content)
            .map(Some)
            .map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 以 pretty JSON 原子写入状态文件（内部使用 [`write_file_atomic`]）。
pub fn write_json_state<T: serde::Serialize>(path: &Path, state: &T) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    write_file_atomic(path, &json)
}

/// Return whether `candidate` stays inside `root`.
///
/// Existing paths are canonicalized so symlink escapes are rejected. For a path
/// that does not exist yet, its nearest existing ancestor is checked instead.
pub fn is_path_within(candidate: &Path, root: &Path) -> bool {
    let Some(root) = root.canonicalize().ok() else {
        return normalize_path(candidate).starts_with(normalize_path(root));
    };

    if let Ok(candidate) = candidate.canonicalize() {
        return candidate.starts_with(root);
    }

    nearest_existing_ancestor(candidate)
        .and_then(|ancestor| ancestor.canonicalize().ok())
        .is_some_and(|ancestor| ancestor.starts_with(root))
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            },
            Component::CurDir => {},
            other => normalized.push(other),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_atomic_replaces_the_target_and_leaves_no_temporary_file() {
        let root = std::env::temp_dir().join(format!(
            "astrcode-sdk-hostpaths-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("nested").join("state.json");

        write_file_atomic(&path, "old").unwrap();
        write_file_atomic(&path, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_paths_and_rejects_lexical_escape() {
        let root = Path::new("workspace");

        assert_eq!(
            resolve_path(root, Path::new("src/main.rs")),
            PathBuf::from("workspace/src/main.rs")
        );
        assert!(is_path_within(Path::new("workspace/src/main.rs"), root));
        assert!(!is_path_within(Path::new("workspace/../outside"), root));
    }
}

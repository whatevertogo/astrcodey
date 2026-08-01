//! Host path operations exposed to extensions.

use std::path::{Component, Path, PathBuf};

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
/// The bytes go to a sibling `<file name>.tmp` file first, which is then
/// renamed over `path`, so a crash mid-write cannot leave a truncated target.
/// Parent directories are created as needed.
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    std::fs::write(PathBuf::from(&tmp), content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
    fn write_file_atomic_creates_parents_and_leaves_no_tmp() {
        let root = std::env::temp_dir().join(format!(
            "astrcode-sdk-hostpaths-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("nested").join("state.json");

        write_file_atomic(&path, "{}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert!(!root.join("nested").join("state.json.tmp").exists());
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

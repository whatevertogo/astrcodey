//! Host path resolution for astrcode directories.
//!
//! Resolves paths for config, sessions, projects, and runtime data.

use std::path::{Path, PathBuf};

/// Resolve the user's home directory.
///
/// Checks in order: ASTRCODE_TEST_HOME, ASTRCODE_HOME_DIR, dirs::home_dir().
pub fn resolve_home_dir() -> PathBuf {
    if let Ok(test_home) = std::env::var("ASTRCODE_TEST_HOME") {
        if !test_home.is_empty() {
            return PathBuf::from(test_home);
        }
    }
    if let Ok(astrcode_home) = std::env::var("ASTRCODE_HOME_DIR") {
        if !astrcode_home.is_empty() {
            return PathBuf::from(astrcode_home);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Get the astrcode base directory: `~/.astrcode/`.
pub fn astrcode_dir() -> PathBuf {
    resolve_home_dir().join(".astrcode")
}

/// Get the projects directory: `~/.astrcode/projects/`.
pub fn projects_dir() -> PathBuf {
    astrcode_dir().join("projects")
}

/// Get the project-specific directory: `~/.astrcode/projects/<project_hash>/`.
pub fn project_dir(project_hash: &str) -> PathBuf {
    projects_dir().join(project_hash)
}

/// Get the sessions directory for a project: `~/.astrcode/projects/<hash>/sessions/`.
pub fn sessions_dir(project_hash: &str) -> PathBuf {
    project_dir(project_hash).join("sessions")
}

/// Get the runtime directory: `~/.astrcode/runtime/`.
pub fn runtime_dir() -> PathBuf {
    astrcode_dir().join("runtime")
}

/// Get the global extensions directory: `~/.astrcode/extensions/`.
pub fn extensions_dir() -> PathBuf {
    astrcode_dir().join("extensions")
}

/// Get the project-level extensions directory: `<workspace>/.astrcode/extensions/`.
pub fn project_extensions_dir(workspace: &str) -> PathBuf {
    PathBuf::from(workspace)
        .join(".astrcode")
        .join("extensions")
}

/// Ensure a directory exists, creating parents as needed.
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Resolve a path that may be relative against a working directory.
///
/// If `raw` is absolute, returns it unchanged. Otherwise joins it with `cwd`.
pub fn resolve_path(cwd: &Path, raw: &Path) -> PathBuf {
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_astrcode_dir() {
        let dir = astrcode_dir();
        assert!(dir.ends_with(".astrcode"));
    }

    #[test]
    fn test_resolve_home_with_test_env() {
        std::env::set_var("ASTRCODE_TEST_HOME", "/tmp/test-astrcode");
        let home = resolve_home_dir();
        assert_eq!(home, PathBuf::from("/tmp/test-astrcode"));
        std::env::remove_var("ASTRCODE_TEST_HOME");
    }
}

//! 宿主路径解析。
//!
//! 解析 astrcode 各类目录路径，包括配置目录、会话目录、项目目录和运行时数据目录。
//! 同时提供路径安全检查，防止路径遍历攻击。

use std::path::{Component, Path, PathBuf};

use astrcode_core::types::project_key_from_path;

/// 工作区路径解析失败（路径穿越、绝对路径、非法组件等）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePathError {
    pub code: &'static str,
    pub message: String,
}

impl WorkspacePathError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 将相对路径 `rel` 解析到已规范化的工作区根 `root` 之下。
///
/// 拒绝：`../`、绝对路径、Windows 前缀路径、NUL、根组件与符号链接逃逸（经 canonicalize +
/// `starts_with`）。
pub fn resolve_under_workspace_root(
    root: impl AsRef<Path>,
    rel: &str,
) -> Result<PathBuf, WorkspacePathError> {
    if rel.is_empty() {
        return Err(WorkspacePathError::new("invalid_input", "empty path"));
    }
    if rel.contains('\0') {
        return Err(WorkspacePathError::new(
            "invalid_input",
            "path contains NUL",
        ));
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(WorkspacePathError::new(
            "invalid_input",
            "absolute paths are not allowed",
        ));
    }
    for component in rel_path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(WorkspacePathError::new(
                    "invalid_input",
                    "absolute path components are not allowed",
                ));
            },
            Component::ParentDir => {
                return Err(WorkspacePathError::new(
                    "permission_denied",
                    "path escapes workspace root",
                ));
            },
            Component::Normal(name) => {
                if name == ".." {
                    return Err(WorkspacePathError::new(
                        "permission_denied",
                        "path escapes workspace root",
                    ));
                }
            },
            Component::CurDir => {},
        }
    }

    let root = root
        .as_ref()
        .canonicalize()
        .map_err(|e| WorkspacePathError::new("io_error", e.to_string()))?;

    let mut joined = root.clone();
    for component in rel_path.components() {
        if let Component::Normal(name) = component {
            joined.push(name);
        }
    }

    let canonical = joined
        .canonicalize()
        .map_err(|e| WorkspacePathError::new("io_error", e.to_string()))?;
    if !is_path_within(&canonical, &root) {
        return Err(WorkspacePathError::new(
            "permission_denied",
            "path outside working directory",
        ));
    }
    Ok(canonical)
}

/// 解析用户主目录。
///
/// 按以下优先级查找：
/// 1. `ASTRCODE_TEST_HOME` 环境变量（用于测试）
/// 2. `ASTRCODE_HOME_DIR` 环境变量（用户自定义主目录）
/// 3. `dirs::home_dir()`（系统默认主目录）
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

/// 获取 astrcode 基础目录：`~/.astrcode/`。
pub fn astrcode_dir() -> PathBuf {
    resolve_home_dir().join(".astrcode")
}

/// 获取项目总目录：`~/.astrcode/projects/`。
pub fn projects_dir() -> PathBuf {
    astrcode_dir().join("projects")
}

/// 获取特定项目的目录：`~/.astrcode/projects/<project_key>/`。
pub fn project_dir(project_key: &str) -> PathBuf {
    projects_dir().join(project_key)
}

/// 获取某项目下的会话目录：`~/.astrcode/projects/<project_key>/sessions/`。
pub fn sessions_dir(project_key: &str) -> PathBuf {
    project_dir(project_key).join("sessions")
}

/// 获取某个会话目录：`~/.astrcode/projects/<project_key>/sessions/<session>/`。
pub fn session_dir(project_key: &str, session_id: &str) -> PathBuf {
    sessions_dir(project_key).join(session_id)
}

/// 根据真实项目路径获取当前可读 project key 的会话目录。
pub fn sessions_dir_for_project_path(project_path: &Path) -> PathBuf {
    sessions_dir(&project_key_from_path(project_path))
}

/// 获取运行时目录：`~/.astrcode/runtime/`。
pub fn runtime_dir() -> PathBuf {
    astrcode_dir().join("runtime")
}

/// 获取全局扩展目录：`~/.astrcode/extensions/`。
pub fn extensions_dir() -> PathBuf {
    astrcode_dir().join("extensions")
}

/// 获取日志目录：`~/.astrcode/logs/`。
pub fn logs_dir() -> PathBuf {
    astrcode_dir().join("logs")
}

/// 获取测试专用目录。
///
/// 该目录位于系统临时目录下，调用方负责在测试前后清理。
pub fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join("astrcode-tests").join(name)
}

/// 获取项目级扩展目录：`<workspace>/.astrcode/extensions/`。
pub fn project_extensions_dir(workspace: &str) -> PathBuf {
    PathBuf::from(workspace)
        .join(".astrcode")
        .join("extensions")
}

/// 获取插件专属数据目录：`~/.astrcode/extension_data/<extension_id>/`。
pub fn extensions_data_dir(extension_id: &str) -> PathBuf {
    astrcode_dir()
        .join("extensions_data_dir")
        .join(extension_id)
}

/// 确保目录存在，如不存在则递归创建（包含父目录）。
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// 将可能是相对路径的 `raw` 相对于 `cwd` 解析为绝对路径。
///
/// 如果 `raw` 已经是绝对路径，则原样返回；否则将其与 `cwd` 拼接。
pub fn resolve_path(cwd: &Path, raw: &Path) -> PathBuf {
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    }
}

/// 检查解析后的路径是否位于 `base` 目录内，防止路径遍历攻击。
///
/// 优先使用 `canonicalize` 获取真实路径进行比较；如果路径尚不存在，
/// 则回退到最近存在的祖先目录进行规范化比较。
pub fn is_path_within(resolved: &Path, base: &Path) -> bool {
    // 先尝试规范化 base 目录
    let Some(base) = base.canonicalize().ok() else {
        // base 不存在时，回退到纯路径规范化比较
        return normalize_path(resolved).starts_with(normalize_path(base));
    };

    // 再尝试规范化目标路径
    if let Ok(resolved) = resolved.canonicalize() {
        return resolved.starts_with(base);
    }

    // 目标路径不存在时，查找最近存在的祖先目录进行比较
    let Some(existing_parent) = nearest_existing_ancestor(resolved) else {
        return false;
    };
    existing_parent
        .canonicalize()
        .map(|parent| parent.starts_with(base))
        .unwrap_or(false)
}

/// 从给定路径向上查找最近存在的祖先目录。
fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    let mut current = Some(path);
    while let Some(path) = current {
        if path.exists() {
            return Some(path);
        }
        current = path.parent();
    }
    None
}

/// 规范化路径，消除 `.` 和 `..` 组件。
///
/// 纯字符串级别的路径简化，不访问文件系统。
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            },
            std::path::Component::CurDir => {},
            other => normalized.push(other),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;

    #[test]
    fn test_astrcode_dir() {
        let dir = astrcode_dir();
        assert!(dir.ends_with(".astrcode"));
    }

    #[test]
    fn project_path_sessions_use_readable_project_key() {
        let path = sessions_dir_for_project_path(Path::new(r"D:\work\astrcode"));

        assert!(path.ends_with(Path::new("D-work-astrcode").join("sessions")));
    }

    #[test]
    fn test_resolve_home_with_test_env() {
        let _guard = test_env_lock().lock().unwrap();
        std::env::set_var("ASTRCODE_TEST_HOME", "/tmp/test-astrcode");
        let home = resolve_home_dir();
        assert_eq!(home, PathBuf::from("/tmp/test-astrcode"));
        std::env::remove_var("ASTRCODE_TEST_HOME");
    }

    fn test_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn resolve_under_workspace_root_rejects_parent_segments() {
        let root = std::env::temp_dir().join("astrcode-path-test-root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("safe")).unwrap();
        let err = resolve_under_workspace_root(&root, "../outside.txt").unwrap_err();
        assert_eq!(err.code, "permission_denied");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_under_workspace_root_rejects_absolute_rel() {
        let root = std::env::temp_dir().join("astrcode-path-test-abs");
        std::fs::create_dir_all(&root).unwrap();
        let err = resolve_under_workspace_root(&root, "/etc/passwd").unwrap_err();
        assert_eq!(err.code, "invalid_input");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_under_workspace_root_allows_nested_file() {
        let root = std::env::temp_dir().join("astrcode-path-test-nested");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a").join("b.txt"), "ok").unwrap();
        let resolved = resolve_under_workspace_root(&root, "a/b.txt").unwrap();
        assert!(resolved.ends_with("b.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

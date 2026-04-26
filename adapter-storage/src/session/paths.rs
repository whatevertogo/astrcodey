//! # 会话路径解析
//!
//! 负责将 `session_id` 和 `working_dir` 映射到 `~/.astrcode/projects/<project>/sessions/`
//! 下的具体文件路径。
//!
//! ## 路径约定
//!
//! ```text
//! ~/.astrcode/
//! └── projects/
//!     └── <project-bucket>/          # 由 working_dir 哈希或名称映射
//!         └── sessions/
//!             └── <session-id>/
//!                 ├── session-<session-id>.jsonl   # 事件日志
//!                 ├── active-turn.lock             # 文件锁
//!                 └── active-turn.json             # 锁元数据
//! ```
//!
//! ## 安全设计
//!
//! - 所有 `session_id` 必须通过 `validated_session_id()` 校验，防止路径穿越攻击
//! - 仅允许字母数字、`-`、`_`、`T` 字符，不允许 `:`（Windows 文件名非法）和 `.`（路径穿越）
//! - `canonical_session_id()` 统一处理带/不带 `session-` 前缀的 ID，避免调用方各自处理

use std::{
    fs,
    path::{Path, PathBuf},
};

use astrcode_core::store::StoreError;
use astrcode_support::hostpaths::{project_dir, project_dir_name, projects_dir};

use crate::{Result, internal_io_error, io_error};

/// 会话目录下的 sessions 子目录名称。
const SESSIONS_DIR_NAME: &str = "sessions";

/// 获取 Astrcode 项目根目录（`~/.astrcode/projects`）。
pub(crate) fn projects_root_dir() -> Result<PathBuf> {
    projects_dir().map_err(|error| {
        internal_io_error(format!(
            "failed to resolve Astrcode projects directory: {error}"
        ))
    })
}

/// 获取指定工作目录对应的项目 sessions 目录。
///
/// 通过 `project_dir(working_dir)` 将工作目录映射到项目分桶，
/// 再拼接 `sessions` 子目录。
pub(crate) fn project_sessions_dir(working_dir: &Path) -> Result<PathBuf> {
    Ok(project_dir(working_dir)
        .map_err(|error| {
            internal_io_error(format!(
                "failed to resolve project directory for '{}': {error}",
                working_dir.display()
            ))
        })?
        .join(SESSIONS_DIR_NAME))
}

/// 从项目根目录和工作目录计算 sessions 目录路径。
///
/// 与 `project_sessions_dir` 的区别在于此函数不依赖全局配置，
/// 而是直接基于给定的 `projects_root` 计算，用于测试和跨根目录操作。
pub(crate) fn project_sessions_dir_from_root(projects_root: &Path, working_dir: &Path) -> PathBuf {
    projects_root
        .join(project_dir_name(working_dir))
        .join(SESSIONS_DIR_NAME)
}

/// 计算指定会话的目录路径（不含文件名）。
///
/// 路径格式：`<project_sessions_dir>/<session_id>/`
pub(crate) fn session_dir(session_id: &str, working_dir: &Path) -> Result<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    Ok(project_sessions_dir(working_dir)?.join(&session_id))
}

/// 从显式项目根目录计算会话目录路径。
///
/// server 测试通过显式 bootstrap sandbox 隔离存储根目录时，需要避免回落到
/// 全局 `~/.astrcode/projects`，因此这里提供不依赖全局 home 的变体。
pub(crate) fn session_dir_from_projects_root(
    projects_root: &Path,
    session_id: &str,
    working_dir: &Path,
) -> Result<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    Ok(project_sessions_dir_from_root(projects_root, working_dir).join(&session_id))
}

/// 计算指定会话的 JSONL 文件完整路径。
///
/// 路径格式：`<project_sessions_dir>/<session_id>/session-<session_id>.jsonl`
pub(crate) fn session_path(session_id: &str, working_dir: &Path) -> Result<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    Ok(session_dir(&session_id, working_dir)?.join(session_file_name(&session_id)))
}

/// 从显式项目根目录计算会话 JSONL 路径。
pub(crate) fn session_path_from_projects_root(
    projects_root: &Path,
    session_id: &str,
    working_dir: &Path,
) -> Result<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    Ok(
        session_dir_from_projects_root(projects_root, &session_id, working_dir)?
            .join(session_file_name(&session_id)),
    )
}

/// 查找已存在的会话文件路径。
///
/// 遍历所有项目的 sessions 目录，返回第一个匹配的文件路径。
/// 如果未找到，返回 `SessionNotFound` 错误并附带期望的路径模式。
///
/// 此函数用于 `open()` 场景：调用者只知道 `session_id`，
/// 不知道它属于哪个项目，需要跨项目查找。
pub(crate) fn resolve_existing_session_path(session_id: &str) -> Result<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    let projects_root = projects_root_dir()?;
    resolve_existing_session_path_from_projects_root(&projects_root, &session_id)
}

/// 从显式项目根目录查找已存在的会话文件路径。
pub(crate) fn resolve_existing_session_path_from_projects_root(
    projects_root: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    let session_id = validated_session_id(session_id)?;
    let candidate_name = session_file_name(&session_id);

    for sessions_dir in session_storage_dirs(projects_root)? {
        let candidate = sessions_dir.join(&session_id).join(&candidate_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(StoreError::SessionNotFound(
        projects_root
            .join("<project>")
            .join(SESSIONS_DIR_NAME)
            .join(&session_id)
            .join(candidate_name)
            .display()
            .to_string(),
    ))
}

/// 查找已存在的会话文件所在目录。
///
/// 基于 `resolve_existing_session_path` 的结果取其父目录，
/// 用于定位锁文件和元数据文件的位置。
pub(crate) fn resolve_existing_session_dir(session_id: &str) -> Result<PathBuf> {
    let path = resolve_existing_session_path(session_id)?;
    path.parent().map(Path::to_path_buf).ok_or_else(|| {
        internal_io_error(format!(
            "session file '{}' has no parent directory",
            path.display()
        ))
    })
}

/// 从显式项目根目录查找已存在的会话目录。
pub(crate) fn resolve_existing_session_dir_from_projects_root(
    projects_root: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    let path = resolve_existing_session_path_from_projects_root(projects_root, session_id)?;
    path.parent().map(Path::to_path_buf).ok_or_else(|| {
        internal_io_error(format!(
            "session file '{}' has no parent directory",
            path.display()
        ))
    })
}

/// 获取会话文件锁的路径。
///
/// 锁文件用于 `try_acquire_session_turn` 的独占文件锁，
/// 防止多进程同时写入同一会话。
pub(crate) fn session_turn_lock_path(session_id: &str) -> Result<PathBuf> {
    Ok(resolve_existing_session_dir(session_id)?.join("active-turn.lock"))
}

/// 从显式项目根目录获取会话文件锁路径。
pub(crate) fn session_turn_lock_path_from_projects_root(
    projects_root: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(
        resolve_existing_session_dir_from_projects_root(projects_root, session_id)?
            .join("active-turn.lock"),
    )
}

/// 获取会话锁元数据文件的路径。
///
/// 元数据文件包含当前锁持有者的 `turn_id`、`owner_pid`、`acquired_at`，
/// 供竞争者读取以判断当前会话状态。
pub(crate) fn session_turn_metadata_path(session_id: &str) -> Result<PathBuf> {
    Ok(resolve_existing_session_dir(session_id)?.join("active-turn.json"))
}

/// 从显式项目根目录获取会话锁元数据文件路径。
pub(crate) fn session_turn_metadata_path_from_projects_root(
    projects_root: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(
        resolve_existing_session_dir_from_projects_root(projects_root, session_id)?
            .join("active-turn.json"),
    )
}

pub(crate) fn snapshots_dir(session_id: &str) -> Result<PathBuf> {
    Ok(resolve_existing_session_dir(session_id)?.join("snapshots"))
}

pub(crate) fn snapshots_dir_from_projects_root(
    projects_root: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(
        resolve_existing_session_dir_from_projects_root(projects_root, session_id)?
            .join("snapshots"),
    )
}

pub(crate) fn checkpoint_snapshot_path(
    session_id: &str,
    checkpoint_storage_seq: u64,
) -> Result<PathBuf> {
    Ok(snapshots_dir(session_id)?.join(format!("checkpoint-{checkpoint_storage_seq}.json")))
}

pub(crate) fn checkpoint_snapshot_path_from_projects_root(
    projects_root: &Path,
    session_id: &str,
    checkpoint_storage_seq: u64,
) -> Result<PathBuf> {
    Ok(snapshots_dir_from_projects_root(projects_root, session_id)?
        .join(format!("checkpoint-{checkpoint_storage_seq}.json")))
}

pub(crate) fn latest_checkpoint_marker_path(session_id: &str) -> Result<PathBuf> {
    Ok(snapshots_dir(session_id)?.join("latest-checkpoint.json"))
}

pub(crate) fn latest_checkpoint_marker_path_from_projects_root(
    projects_root: &Path,
    session_id: &str,
) -> Result<PathBuf> {
    Ok(snapshots_dir_from_projects_root(projects_root, session_id)?.join("latest-checkpoint.json"))
}

/// 枚举所有项目下的 sessions 目录。
///
/// 遍历 `projects_root` 下的每个子目录，查找其 `sessions` 子目录，
/// 用于跨项目列出所有会话。
pub(crate) fn session_storage_dirs(projects_root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    if projects_root.exists() {
        for entry in fs::read_dir(projects_root).map_err(|error| {
            io_error(
                format!(
                    "failed to read projects directory: {}",
                    projects_root.display()
                ),
                error,
            )
        })? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }

            let sessions_dir = entry.path().join(SESSIONS_DIR_NAME);
            if sessions_dir.is_dir() {
                dirs.push(sessions_dir);
            }
        }
    }
    Ok(dirs)
}

/// 根据会话 ID 生成 JSONL 文件名。
///
/// 文件名格式：`session-<session_id>.jsonl`
pub(crate) fn session_file_name(session_id: &str) -> String {
    format!("session-{session_id}.jsonl")
}

/// 宽容归一化：接受带 "session-" 前缀和不带前缀的 ID。
/// 设计意图：API 调用者可能传入 "session-xxx" 或 "xxx" 两种格式，
/// 此函数统一剥离前缀，避免调用方各自处理前缀逻辑。
pub(crate) fn canonical_session_id(session_id: &str) -> &str {
    session_id.strip_prefix("session-").unwrap_or(session_id)
}

/// 验证会话 ID 只含安全字符。显式允许 'T' 是因为 ID 中嵌入了类 ISO-8601
/// 时间戳（如 "2026-03-08T10-00-00"），'T' 是日期与时间的分隔符。
/// 不允许 ':' 是因为冒号在 Windows 文件名中非法（session ID 直接用于文件名）。
pub(crate) fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == 'T')
}

pub(crate) fn validated_session_id(session_id: &str) -> Result<String> {
    let canonical = canonical_session_id(session_id);
    if !is_valid_session_id(canonical) {
        return Err(StoreError::InvalidSessionId(session_id.to_string()));
    }
    Ok(canonical.to_string())
}

#[cfg(test)]
mod tests {
    use astrcode_core::test_support::TestEnvGuard;

    use super::*;

    #[test]
    fn session_path_rejects_invalid_session_id() {
        let err = session_path("../../etc/passwd", Path::new(r"D:\project"))
            .expect_err("invalid id should fail");
        assert!(err.to_string().contains("invalid session id"));
    }

    #[test]
    fn session_path_uses_project_session_directory() {
        let guard = TestEnvGuard::new();
        let working_dir = Path::new(r"D:\project1");

        let path =
            session_path("2026-04-02T10-00-00-aaaaaaaa", working_dir).expect("path should resolve");

        assert!(path.starts_with(guard.home_dir().join(".astrcode").join("projects")));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("session-2026-04-02T10-00-00-aaaaaaaa.jsonl")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("2026-04-02T10-00-00-aaaaaaaa")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.parent())
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("sessions")
        );
    }

    #[test]
    fn session_storage_dirs_lists_project_session_folders_only() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        fs::create_dir_all(temp.path().join("project-a").join("sessions"))
            .expect("sessions dir should exist");
        fs::create_dir_all(temp.path().join("project-b").join("sessions"))
            .expect("sessions dir should exist");
        fs::create_dir_all(temp.path().join("project-c").join("notes"))
            .expect("non-session dir should exist");

        let dirs = session_storage_dirs(temp.path()).expect("project session dirs should resolve");

        assert_eq!(dirs.len(), 2);
        assert!(dirs.iter().all(|dir| dir.ends_with("sessions")));
    }
}

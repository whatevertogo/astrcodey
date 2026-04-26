//! # 会话查询与管理
//!
//! 提供会话列表、元数据提取、删除、以及尾部值扫描等功能。
//! 所有方法作为 `EventLog` 的 `impl` 块定义，与事件日志操作共享同一类型。
//!
//! ## 核心功能
//!
//! - **会话列表**：`list_sessions()` / `list_sessions_with_meta()` 扫描所有项目的 sessions
//!   目录，返回会话 ID 列表或包含标题、时间、阶段的完整元数据
//! - **会话删除**：`delete_session()` 删除单个会话文件并清理空目录；
//!   `delete_sessions_by_working_dir()` 批量删除指定项目的所有会话
//! - **尾部扫描**：`read_tail_value()` 使用指数窗口从文件尾部向前扫描， 高效提取最后一个时间戳或
//!   Phase，避免全量加载大文件
//! - **头部元数据**：`read_session_head_meta()` 从 JSONL 首行提取 `SessionStart`
//!   事件中的创建时间、工作目录、父会话等信息
//!
//! ## 尾部扫描算法
//!
//! `read_tail_value()` 使用指数退避窗口（4096 → 8192 → 16384 → ...）从文件末尾
//! 向前扫描，每次扩大窗口直到找到目标值或覆盖整个文件。这样对于常见的短会话
//! 只需读取少量字节，而对于长会话也能在有限次迭代内完成。
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use astrcode_core::{
    DeleteProjectResult, Phase, SessionMeta, StorageEvent, StorageEventPayload, StoredEvent,
    normalize_recovered_phase,
};
use chrono::{DateTime, Utc};

use super::{
    event_log::EventLog,
    paths::{
        canonical_session_id, is_valid_session_id, project_sessions_dir_from_root,
        projects_root_dir, session_file_name, session_storage_dirs, validated_session_id,
    },
};
use crate::{Result, internal_io_error, io_error, parse_error};
/// 从 session JSONL 首行提取的元信息。
///
/// 通过解析 `SessionStart` 事件获取会话的创建时间、工作目录、标题、
/// 以及父会话信息（用于会话分支/派生场景）。
struct SessionHeadMeta {
    /// 会话创建时间，来自 `SessionStart.timestamp`。
    created_at: DateTime<Utc>,
    /// 会话关联的工作目录。
    working_dir: String,
    /// 会话标题，从首个 `UserMessage` 内容提取（最多 20 字符）。
    title: String,
    /// 父会话 ID，如果此会话是从另一个会话派生的。
    parent_session_id: Option<String>,
    /// 父会话中的事件序号，标记派生起点。
    parent_storage_seq: Option<u64>,
}

impl EventLog {
    /// 列出所有会话 ID。
    ///
    /// 扫描全局项目根目录下的所有 sessions 目录，返回去重排序后的会话 ID 列表。
    pub fn list_sessions() -> Result<Vec<String>> {
        let projects_root = projects_root_dir()?;
        Self::list_sessions_from_path(&projects_root)
    }

    /// 列出所有会话及其完整元数据。
    ///
    /// 除会话 ID 外，还包含标题、创建/更新时间、工作目录、当前 Phase 等信息，
    /// 用于前端会话列表展示。结果按 `updated_at` 降序排列。
    pub fn list_sessions_with_meta() -> Result<Vec<SessionMeta>> {
        let projects_root = projects_root_dir()?;
        Self::list_sessions_with_meta_from_path(&projects_root)
    }

    /// 删除指定会话。
    ///
    /// 删除 JSONL 文件后，如果会话目录为空则一并删除。
    pub fn delete_session(session_id: &str) -> Result<()> {
        let projects_root = projects_root_dir()?;
        Self::delete_session_from_path(&projects_root, session_id)
    }

    /// 删除指定工作目录对应项目的所有会话。
    ///
    /// 返回成功删除的会话数和失败的会话 ID 列表。
    pub fn delete_sessions_by_working_dir(working_dir: &str) -> Result<DeleteProjectResult> {
        let projects_root = projects_root_dir()?;
        Self::delete_sessions_by_working_dir_from_path(&projects_root, working_dir)
    }

    /// 从指定项目根路径列出所有会话 ID。
    ///
    /// 内部方法，支持传入自定义 `projects_root`，用于测试和跨根目录操作。
    pub(crate) fn list_sessions_from_path(projects_root: &Path) -> Result<Vec<String>> {
        if !projects_root.exists() {
            return Ok(Vec::new());
        }

        let mut ids = BTreeSet::new();
        for path in Self::session_files_under_projects_root(projects_root)? {
            if let Some(id) = session_id_from_path(&path) {
                ids.insert(id);
            }
        }

        Ok(ids.into_iter().collect())
    }

    /// 从指定项目根路径列出所有会话及其元数据。
    ///
    /// 对每个会话文件：
    /// 1. 读取头部元数据（`SessionStart` 事件）
    /// 2. 从尾部扫描最后更新时间（避免全量加载）
    /// 3. 从尾部扫描最后 Phase
    ///
    /// 不可读的会话文件会被跳过并记录警告日志。
    pub(crate) fn list_sessions_with_meta_from_path(
        projects_root: &Path,
    ) -> Result<Vec<SessionMeta>> {
        if !projects_root.exists() {
            return Ok(Vec::new());
        }

        let mut metas = Vec::new();
        for path in Self::session_files_under_projects_root(projects_root)? {
            let Some(id) = session_id_from_path(&path) else {
                continue;
            };

            let head_meta = match Self::read_session_head_meta(&path) {
                Ok(meta) => meta,
                Err(error) => {
                    log::warn!(
                        "skipping unreadable session file '{}': {}",
                        path.display(),
                        error
                    );
                    continue;
                },
            };
            let updated_at = Self::read_last_timestamp(&path).unwrap_or(head_meta.created_at);
            let phase = Self::read_last_phase(&path).unwrap_or(Phase::Idle);
            metas.push(SessionMeta {
                session_id: canonical_session_id(&id).to_string(),
                working_dir: head_meta.working_dir.clone(),
                display_name: session_display_name(&head_meta.working_dir),
                title: head_meta.title,
                created_at: head_meta.created_at,
                updated_at,
                parent_session_id: head_meta.parent_session_id,
                parent_storage_seq: head_meta.parent_storage_seq,
                phase,
            });
        }

        metas.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.session_id.cmp(&a.session_id))
        });

        Ok(metas)
    }

    /// 从指定项目根路径删除单个会话。
    ///
    /// 删除 JSONL 文件后清理空目录。
    pub(crate) fn delete_session_from_path(projects_root: &Path, session_id: &str) -> Result<()> {
        let target = Self::resolve_existing_session_path_from_root(projects_root, session_id)?;
        let session_dir = target.parent().map(|p| p.to_path_buf());

        // 删除 JSONL 文件（主操作，失败直接返回错误）
        fs::remove_file(&target).map_err(|error| {
            io_error(
                format!("failed to delete session file: {}", target.display()),
                error,
            )
        })?;

        // 清理整个会话目录（包含 tool-results/、tool-state/ 等子目录）
        if let Some(dir) = &session_dir {
            if let Err(error) = fs::remove_dir_all(dir) {
                // 目录清理失败不阻止主操作，但记录诊断日志
                log::warn!(
                    "session jsonl deleted but directory cleanup failed for '{}': {error}",
                    dir.display()
                );
            }
        }
        Ok(())
    }

    /// 删除指定工作目录对应项目的所有会话。
    ///
    /// 逐个删除会话文件，删除成功后清理空目录。
    /// 失败的会话 ID 会被收集到 `failed_session_ids` 中返回给调用者。
    pub(crate) fn delete_sessions_by_working_dir_from_path(
        projects_root: &Path,
        working_dir: &str,
    ) -> Result<DeleteProjectResult> {
        let sessions_dir = project_sessions_dir_from_root(projects_root, Path::new(working_dir));
        if !sessions_dir.exists() {
            return Ok(DeleteProjectResult {
                success_count: 0,
                failed_session_ids: Vec::new(),
            });
        }

        let mut success_count = 0usize;
        let mut failed_session_ids = Vec::new();
        for entry in fs::read_dir(&sessions_dir).map_err(|error| {
            io_error(
                format!(
                    "failed to read project sessions directory: {}",
                    sessions_dir.display()
                ),
                error,
            )
        })? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }

            let session_dir = entry.path();
            let path = match session_file_path_in_dir(&session_dir) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let Some(session_id) = session_id_from_path(&path) else {
                continue;
            };

            match fs::remove_file(&path) {
                Ok(()) => {
                    // 清理整个会话目录（包含 tool-results/、tool-state/ 等子目录）
                    if let Err(error) = fs::remove_dir_all(&session_dir) {
                        log::warn!(
                            "session jsonl deleted but directory cleanup failed for '{}': {error}",
                            session_dir.display()
                        );
                    }
                    success_count += 1;
                },
                Err(_) => failed_session_ids.push(session_id),
            }
        }

        Ok(DeleteProjectResult {
            success_count,
            failed_session_ids,
        })
    }

    /// 枚举指定项目根路径下的所有会话文件。
    ///
    /// 遍历所有项目的 sessions 目录，收集每个会话子目录下的 JSONL 文件路径。
    fn session_files_under_projects_root(projects_root: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for sessions_dir in session_storage_dirs(projects_root)? {
            for entry in fs::read_dir(&sessions_dir).map_err(|error| {
                io_error(
                    format!(
                        "failed to read sessions directory: {}",
                        sessions_dir.display()
                    ),
                    error,
                )
            })? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }

                if let Ok(path) = session_file_path_in_dir(&entry.path()) {
                    if path.is_file() {
                        files.push(path);
                    }
                }
            }
        }
        Ok(files)
    }

    /// 从指定项目根路径查找已存在的会话文件。
    ///
    /// 遍历所有 sessions 目录，返回第一个匹配的文件路径。
    fn resolve_existing_session_path_from_root(
        projects_root: &Path,
        session_id: &str,
    ) -> Result<PathBuf> {
        let session_id = validated_session_id(session_id)?;

        for sessions_dir in session_storage_dirs(projects_root)? {
            let candidate = sessions_dir
                .join(&session_id)
                .join(session_file_name(&session_id));
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        Err(astrcode_core::store::StoreError::SessionNotFound(
            projects_root
                .join("<project>")
                .join("sessions")
                .join(&session_id)
                .join(session_file_name(&session_id))
                .display()
                .to_string(),
        ))
    }

    /// 从会话文件头部读取元数据。
    ///
    /// 逐行解析 JSONL 文件，从 `SessionStart` 事件提取创建时间、工作目录、
    /// 父会话信息，从首个 `UserMessage` 事件提取标题（截断至 20 字符）。
    /// 找到所需信息后提前停止读取，避免扫描整个文件。
    fn read_session_head_meta(path: &Path) -> Result<SessionHeadMeta> {
        let file = File::open(path).map_err(|error| {
            io_error(
                format!("failed to open session file: {}", path.display()),
                error,
            )
        })?;
        let reader = BufReader::new(file);

        let mut created_at = None;
        let mut working_dir = None;
        let mut title = None;
        let mut parent_session_id = None;
        let mut parent_storage_seq = None;

        for (i, line) in reader.lines().enumerate() {
            let line =
                line.map_err(|error| io_error("failed to read line from session file", error))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let event = serde_json::from_str::<StoredEvent>(trimmed).map_err(|error| {
                parse_error(
                    format!(
                        "failed to parse head event at {}:{}: {}",
                        path.display(),
                        i + 1,
                        trimmed
                    ),
                    error,
                )
            })?;
            event.event.validate().map_err(|error| {
                internal_io_error(format!(
                    "invalid head event at {}:{}: {}",
                    path.display(),
                    i + 1,
                    error
                ))
            })?;
            let event = event.event;

            match event.payload {
                StorageEventPayload::SessionStart {
                    timestamp,
                    working_dir: session_working_dir,
                    parent_session_id: source_session_id,
                    parent_storage_seq: source_storage_seq,
                    ..
                } if created_at.is_none() => {
                    created_at = Some(timestamp);
                    working_dir = Some(session_working_dir);
                    parent_session_id = source_session_id;
                    parent_storage_seq = source_storage_seq;
                },
                StorageEventPayload::SessionStart { .. } => {},
                StorageEventPayload::UserMessage { content, .. } if title.is_none() => {
                    title = Some(title_from_user_message(&content));
                },
                _ => {},
            }

            if created_at.is_some() && title.is_some() {
                break;
            }
        }

        let created_at = created_at.ok_or_else(|| {
            internal_io_error(format!(
                "session file missing sessionStart: {}",
                path.display()
            ))
        })?;
        let working_dir = working_dir.unwrap_or_default();
        let title = title.unwrap_or_else(|| "新会话".to_string());
        Ok(SessionHeadMeta {
            created_at,
            working_dir,
            title,
            parent_session_id,
            parent_storage_seq,
        })
    }

    /// 从会话文件尾部读取最后一个时间戳。
    ///
    /// 使用 `read_tail_value` 的指数窗口扫描，高效定位最后一个带时间戳的事件。
    fn read_last_timestamp(path: &Path) -> Result<DateTime<Utc>> {
        Self::read_tail_value(path, timestamp_of_event)?.ok_or_else(|| {
            internal_io_error(format!(
                "unable to resolve tail timestamp from session file: {}",
                path.display()
            ))
        })
    }

    /// 从会话文件尾部读取最后一个事件的 Phase。
    ///
    /// Phase 用于标识会话当前状态（Idle、CallingTool、WaitingForUser 等），
    /// 前端据此决定 UI 展示方式。
    fn read_last_phase(path: &Path) -> Result<Phase> {
        Ok(Self::read_tail_value(path, |event| {
            Some(astrcode_core::phase_of_storage_event(event))
        })?
        .map(normalize_recovered_phase)
        .unwrap_or(Phase::Idle))
    }

    /// 从会话文件尾部扫描，查找满足 mapper 条件的最后一个值。
    ///
    /// 使用指数窗口扫描（4096 → 8192 → 16384 → ...），是因为 UI 列表只关心
    /// "最近更新时间/阶段"，没必要为了一个尾部值把整个 JSONL 全量读回内存。
    /// 对于大多数会话，4096 字节的初始窗口就足够覆盖尾部事件。
    fn read_tail_value<T, F>(path: &Path, mut mapper: F) -> Result<Option<T>>
    where
        F: FnMut(&StorageEvent) -> Option<T>,
    {
        let file = File::open(path).map_err(|error| {
            io_error(
                format!("failed to open session file: {}", path.display()),
                error,
            )
        })?;
        let mut reader = BufReader::new(file);
        let len = reader
            .get_ref()
            .metadata()
            .map_err(|error| {
                io_error(
                    format!("failed to stat session file: {}", path.display()),
                    error,
                )
            })?
            .len();

        if len == 0 {
            return Err(internal_io_error(format!(
                "empty session file: {}",
                path.display()
            )));
        }

        let mut window: u64 = 4096;
        loop {
            let start = len.saturating_sub(window);
            reader.seek(SeekFrom::Start(start))?;

            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;

            let slice = if start > 0 {
                if let Some(pos) = bytes.iter().position(|b| *b == b'\n') {
                    &bytes[pos + 1..]
                } else if window >= len {
                    bytes.as_slice()
                } else {
                    window = (window * 2).min(len);
                    continue;
                }
            } else {
                bytes.as_slice()
            };

            let text = String::from_utf8_lossy(slice);
            for line in text.lines().rev() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let event = serde_json::from_str::<StoredEvent>(trimmed).map_err(|error| {
                    parse_error(
                        format!(
                            "failed to parse tail event at {}: {}",
                            path.display(),
                            trimmed
                        ),
                        error,
                    )
                })?;
                event.event.validate().map_err(|error| {
                    internal_io_error(format!(
                        "invalid tail event at {}: {}",
                        path.display(),
                        error
                    ))
                })?;
                let event = event.event;
                if let Some(value) = mapper(&event) {
                    return Ok(Some(value));
                }
            }

            if start == 0 || window >= len {
                break;
            }
            window = (window * 2).min(len);
        }

        Ok(None)
    }
}

/// 从文件路径中提取会话 ID。
///
/// 期望路径格式为 `.../sessions/<session-id>/session-<session-id>.jsonl`，
/// 从文件名剥离 `session-` 前缀和 `.jsonl` 后缀后校验合法性。
fn session_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    let id = name
        .strip_prefix("session-")
        .and_then(|value| value.strip_suffix(".jsonl"))?;
    if is_valid_session_id(id) {
        Some(id.to_string())
    } else {
        None
    }
}

/// 根据会话目录路径推导对应的 JSONL 文件路径。
///
/// 目录名即为 session_id，拼接标准文件名即可得到完整路径。
fn session_file_path_in_dir(session_dir: &Path) -> Result<PathBuf> {
    let dir_name = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            internal_io_error(format!(
                "session directory missing valid name: {}",
                session_dir.display()
            ))
        })?;
    let session_id = validated_session_id(dir_name)?;
    Ok(session_dir.join(session_file_name(&session_id)))
}

/// 从事件中提取时间戳。
///
/// 仅对携带时间戳的事件类型返回 `Some`，用于 `read_last_timestamp` 的 mapper。
fn timestamp_of_event(event: &StorageEvent) -> Option<DateTime<Utc>> {
    match &event.payload {
        StorageEventPayload::SessionStart { timestamp, .. } => Some(*timestamp),
        StorageEventPayload::UserMessage { timestamp, .. } => Some(*timestamp),
        StorageEventPayload::AssistantFinal { timestamp, .. } => timestamp.as_ref().cloned(),
        StorageEventPayload::TurnDone { timestamp, .. } => Some(*timestamp),
        StorageEventPayload::Error { timestamp, .. } => timestamp.as_ref().cloned(),
        _ => None,
    }
}

/// 从工作目录字符串提取显示名称。
///
/// 取路径的最后一段（目录名），用于前端会话列表的项目列展示。
fn session_display_name(working_dir: &str) -> String {
    let normalized = working_dir.trim_end_matches(['/', '\\']);
    normalized
        .rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or("默认项目")
        .to_string()
}

/// 从用户消息内容中提取会话标题。
///
/// 截取前 20 个字符并去除首尾空白，作为会话的简短标题。
/// 如果内容为空则返回默认标题 "新会话"。
fn title_from_user_message(content: &str) -> String {
    let title: String = content.chars().take(20).collect();
    let title = title.trim();
    if title.is_empty() {
        "新会话".to_string()
    } else {
        title.to_string()
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{StorageEventPayload, StoredEvent};
    use astrcode_support::hostpaths::project_dir_name;
    use chrono::TimeZone;

    use super::*;

    fn write_stored_events(path: &Path, events: &[StoredEvent]) {
        let payload = events
            .iter()
            .map(|event| serde_json::to_string(event).expect("event should serialize"))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("session parent dir should exist");
        }
        fs::write(path, format!("{payload}\n")).expect("log should be written");
    }

    fn project_sessions_dir(root: &Path, working_dir: &str) -> PathBuf {
        root.join(project_dir_name(Path::new(working_dir)))
            .join("sessions")
    }

    fn session_dir(root: &Path, working_dir: &str, session_id: &str) -> PathBuf {
        project_sessions_dir(root, working_dir).join(session_id)
    }

    #[test]
    fn read_last_timestamp_uses_error_event_timestamp() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let path = temp.path().join("session-test.jsonl");
        let created_at = Utc
            .with_ymd_and_hms(2026, 3, 18, 8, 0, 0)
            .single()
            .expect("timestamp should be valid");
        let failed_at = Utc
            .with_ymd_and_hms(2026, 3, 18, 8, 5, 0)
            .single()
            .expect("timestamp should be valid");
        let lines = [
            serde_json::to_string(&StoredEvent {
                storage_seq: 1,
                event: StorageEvent {
                    turn_id: None,
                    agent: astrcode_core::AgentEventContext::default(),
                    payload: StorageEventPayload::SessionStart {
                        session_id: "session-1".to_string(),
                        timestamp: created_at,
                        working_dir: "/tmp/project".to_string(),
                        parent_session_id: None,
                        parent_storage_seq: None,
                    },
                },
            })
            .expect("session start should serialize"),
            serde_json::to_string(&StoredEvent {
                storage_seq: 2,
                event: StorageEvent {
                    turn_id: Some("turn-1".to_string()),
                    agent: astrcode_core::AgentEventContext::default(),
                    payload: StorageEventPayload::Error {
                        message: "boom".to_string(),
                        timestamp: Some(failed_at),
                    },
                },
            })
            .expect("error event should serialize"),
        ];
        fs::write(&path, format!("{}\n{}\n", lines[0], lines[1])).expect("log should be written");

        let updated_at = EventLog::read_last_timestamp(&path).expect("timestamp should resolve");

        assert_eq!(updated_at, failed_at);
    }

    #[test]
    fn read_last_phase_normalizes_stale_transient_phase_to_interrupted() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let path = temp.path().join("session-test.jsonl");
        let started_at = Utc
            .with_ymd_and_hms(2026, 3, 18, 8, 0, 0)
            .single()
            .expect("timestamp should be valid");
        let lines = [
            serde_json::to_string(&StoredEvent {
                storage_seq: 1,
                event: StorageEvent {
                    turn_id: None,
                    agent: astrcode_core::AgentEventContext::default(),
                    payload: StorageEventPayload::SessionStart {
                        session_id: "session-1".to_string(),
                        timestamp: started_at,
                        working_dir: "/tmp/project".to_string(),
                        parent_session_id: None,
                        parent_storage_seq: None,
                    },
                },
            })
            .expect("session start should serialize"),
            serde_json::to_string(&StoredEvent {
                storage_seq: 2,
                event: StorageEvent {
                    turn_id: Some("turn-1".to_string()),
                    agent: astrcode_core::AgentEventContext::default(),
                    payload: StorageEventPayload::UserMessage {
                        content: "still running?".to_string(),
                        origin: astrcode_core::UserMessageOrigin::User,
                        timestamp: started_at,
                    },
                },
            })
            .expect("user message should serialize"),
        ];
        fs::write(&path, format!("{}\n{}\n", lines[0], lines[1])).expect("log should be written");

        let phase = EventLog::read_last_phase(&path).expect("phase should resolve");

        assert_eq!(phase, Phase::Interrupted);
    }

    #[test]
    fn list_sessions_returns_sorted_ids_across_projects() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let alpha_dir = tmp.path().join("alpha").join("sessions");
        let beta_dir = tmp.path().join("beta").join("sessions");
        fs::create_dir_all(&alpha_dir).expect("alpha sessions dir should exist");
        fs::create_dir_all(&beta_dir).expect("beta sessions dir should exist");

        let ids = [
            ("alpha", "2026-03-01T10-00-00-aaaaaaaa"),
            ("beta", "2026-03-02T12-30-00-bbbbbbbb"),
            ("alpha", "2026-03-01T09-00-00-cccccccc"),
        ];
        for (project, id) in ids {
            let dir = if project == "alpha" {
                &alpha_dir
            } else {
                &beta_dir
            };
            let session_dir = dir.join(id);
            fs::create_dir_all(&session_dir).expect("session dir should exist");
            File::create(session_dir.join(format!("session-{id}.jsonl")))
                .expect("session file should exist");
        }

        File::create(alpha_dir.join("other-file.txt")).expect("non-session file should exist");
        let invalid_dir = beta_dir.join("evil..id");
        fs::create_dir_all(&invalid_dir).expect("invalid dir should exist");
        File::create(invalid_dir.join("session-evil..id.jsonl"))
            .expect("invalid session file should exist");

        let found = EventLog::list_sessions_from_path(tmp.path()).expect("sessions should list");

        assert_eq!(
            found,
            vec![
                "2026-03-01T09-00-00-cccccccc".to_string(),
                "2026-03-01T10-00-00-aaaaaaaa".to_string(),
                "2026-03-02T12-30-00-bbbbbbbb".to_string(),
            ]
        );
    }

    #[test]
    fn delete_session_from_path_succeeds_and_removes_empty_session_directory() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let sessions_dir = tmp.path().join("alpha").join("sessions");
        fs::create_dir_all(&sessions_dir).expect("sessions dir should exist");
        let id = "2026-03-08T10-00-00-aaaaaaaa";
        let session_dir = sessions_dir.join(id);
        fs::create_dir_all(&session_dir).expect("session dir should exist");
        let path = session_dir.join(format!("session-{id}.jsonl"));
        File::create(&path).expect("session file should exist");

        EventLog::delete_session_from_path(tmp.path(), id).expect("delete should succeed");

        assert!(!path.exists());
        assert!(!session_dir.exists());
    }

    #[test]
    fn delete_sessions_by_working_dir_deletes_target_project_directory_only() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let working_dir = r"D:\repo\alpha";
        let other_working_dir = r"D:\repo\beta";
        let id_a = "2026-03-08T10-00-00-aaaaaaaa";
        let id_b = "2026-03-08T11-00-00-bbbbbbbb";
        let id_other = "2026-03-08T12-00-00-cccccccc";
        let timestamp = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("timestamp should be valid");
        write_stored_events(
            &session_dir(tmp.path(), working_dir, id_a).join(format!("session-{id_a}.jsonl")),
            &[StoredEvent {
                storage_seq: 1,
                event: StorageEvent {
                    turn_id: None,
                    agent: astrcode_core::AgentEventContext::default(),
                    payload: StorageEventPayload::SessionStart {
                        session_id: id_a.to_string(),
                        timestamp,
                        working_dir: working_dir.to_string(),
                        parent_session_id: None,
                        parent_storage_seq: None,
                    },
                },
            }],
        );
        write_stored_events(
            &session_dir(tmp.path(), working_dir, id_b).join(format!("session-{id_b}.jsonl")),
            &[StoredEvent {
                storage_seq: 1,
                event: StorageEvent {
                    turn_id: None,
                    agent: astrcode_core::AgentEventContext::default(),
                    payload: StorageEventPayload::SessionStart {
                        session_id: id_b.to_string(),
                        timestamp,
                        working_dir: working_dir.to_string(),
                        parent_session_id: None,
                        parent_storage_seq: None,
                    },
                },
            }],
        );
        write_stored_events(
            &session_dir(tmp.path(), other_working_dir, id_other)
                .join(format!("session-{id_other}.jsonl")),
            &[StoredEvent {
                storage_seq: 1,
                event: StorageEvent {
                    turn_id: None,
                    agent: astrcode_core::AgentEventContext::default(),
                    payload: StorageEventPayload::SessionStart {
                        session_id: id_other.to_string(),
                        timestamp,
                        working_dir: other_working_dir.to_string(),
                        parent_session_id: None,
                        parent_storage_seq: None,
                    },
                },
            }],
        );

        let result = EventLog::delete_sessions_by_working_dir_from_path(tmp.path(), working_dir)
            .expect("project delete should succeed");

        assert_eq!(result.success_count, 2);
        assert!(result.failed_session_ids.is_empty());
        assert!(!session_dir(tmp.path(), working_dir, id_a).exists());
        assert!(!session_dir(tmp.path(), working_dir, id_b).exists());
        assert!(session_dir(tmp.path(), other_working_dir, id_other).exists());
    }
}

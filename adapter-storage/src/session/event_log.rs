//! # 事件日志实现
//!
//! 提供 `EventLog` 结构体，负责 JSONL 会话文件的创建、打开、追加写入与回放。
//!
//! ## 设计要点
//!
//! - **Append-only 模型**：每个事件以 `StoredEvent { storage_seq, event }` 格式追加写入，
//!   `storage_seq` 单调递增且由 writer 独占分配，保证事件全局有序。
//! - **同步刷盘**：每次 `append_stored` 后执行 `flush` + `sync_all`，确保数据持久化到磁盘，
//!   避免进程崩溃导致事件丢失。
//! - **Drop 安全**：`Drop` 实现中再次 flush 和 sync，防止遗漏未刷盘的数据。
//! - **尾部扫描优化**：`last_storage_seq_from_path` 对大文件只读取尾部 64KB， 避免全量加载整个
//!   JSONL 文件。

use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
};

use astrcode_core::{StorageEvent, StoredEvent, store::EventLogWriter};

use super::{
    iterator::EventLogIterator,
    paths::{
        resolve_existing_session_path, resolve_existing_session_path_from_projects_root,
        session_path, session_path_from_projects_root,
    },
};
use crate::Result;

/// 文件系统 JSONL 事件日志 writer。
///
/// 封装了对会话 JSONL 文件的写入操作，维护 `next_storage_seq` 以保证
/// 每个事件的 `storage_seq` 单调递增。每次追加写入后自动 flush 并 sync 到磁盘。
///
/// ## 生命周期
///
/// 通过 `Drop` 实现确保未刷盘的数据在对象销毁时写入磁盘。
pub struct EventLog {
    /// 会话 JSONL 文件的完整路径。
    path: PathBuf,
    /// 缓冲写入器，减少系统调用次数。
    writer: BufWriter<File>,
    /// 下一个事件的 storage_seq，从 1 开始单调递增。
    next_storage_seq: u64,
}

impl Drop for EventLog {
    fn drop(&mut self) {
        if let Err(error) = self.writer.flush() {
            log::warn!(
                "failed to flush event log '{}' on drop: {}",
                self.path.display(),
                error
            );
            return;
        }

        if let Err(error) = self.writer.get_ref().sync_all() {
            log::warn!(
                "failed to sync event log '{}' on drop: {}",
                self.path.display(),
                error
            );
        }
    }
}

impl EventLog {
    /// 清理指定目录中的临时文件。
    ///
    /// 扫描 sessions 目录，删除所有 .tmp 文件。这些文件是由于
    /// session 创建过程中断或崩溃而留下的。
    ///
    /// 此方法是幂等的，可以安全地多次调用。
    pub fn cleanup_temp_files(projects_root: &Path) -> Result<()> {
        let sessions_root = projects_root.join("sessions");
        if !sessions_root.exists() {
            return Ok(());
        }

        let projects = fs::read_dir(&sessions_root).map_err(|e| {
            crate::io_error(
                format!("failed to read sessions root: {}", sessions_root.display()),
                e,
            )
        })?;

        for project_entry in projects.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }

            let sessions_dir = project_path.join("sessions");
            if !sessions_dir.exists() {
                continue;
            }

            // 扫描每个 session 目录
            if let Ok(session_entries) = fs::read_dir(&sessions_dir) {
                for session_entry in session_entries.flatten() {
                    let entry_path = session_entry.path();

                    // 删除 .tmp 文件
                    if entry_path.extension().and_then(|s| s.to_str()) == Some("tmp") {
                        log::debug!("cleaning up temp session file: {}", entry_path.display());
                        let _ = fs::remove_file(&entry_path);
                    }

                    // 删除 session 目录中的 .tmp 文件
                    if entry_path.is_dir() {
                        if let Ok(temp_files) = fs::read_dir(&entry_path) {
                            for temp_file in temp_files.flatten() {
                                let temp_path = temp_file.path();
                                if temp_path.extension().and_then(|s| s.to_str()) == Some("tmp") {
                                    log::debug!(
                                        "cleaning up temp file in session directory: {}",
                                        temp_path.display()
                                    );
                                    let _ = fs::remove_file(&temp_path);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// 仅用于测试：在指定路径创建事件日志。
    ///
    /// 绕过正常路径解析逻辑，直接在给定路径创建文件，
    /// 以便测试可以精确控制文件位置。
    #[cfg(test)]
    pub fn create_at_path(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                crate::io_error(
                    format!("failed to create directory: {}", parent.display()),
                    e,
                )
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                crate::io_error(
                    format!("failed to create session file: {}", path.display()),
                    e,
                )
            })?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            next_storage_seq: 1,
        })
    }

    /// 创建新的事件日志。
    ///
    /// 根据 `session_id` 和 `working_dir` 解析出完整的 JSONL 文件路径，
    /// 使用 `create_new(true)` 确保文件不存在，避免覆盖已有会话。
    ///
    /// ## 参数约束
    ///
    /// - `session_id` 必须符合格式要求（仅含字母数字、`-`、`_`、`T`）
    /// - `working_dir` 必须可映射到确定的项目分桶目录
    /// - 文件必须不存在（`create_new` 保证）
    ///
    /// ## 注意
    ///
    /// 此方法创建空文件，需要立即调用 `append_stored` 写入 `SessionStart` 事件。
    /// 如果需要原子性创建（文件创建 + 首个事件写入），请使用
    /// `create_with_first_event` 或 `create_with_first_event_in_projects_root`。
    pub fn create(session_id: &str, working_dir: &Path) -> Result<Self> {
        let path = session_path(session_id, working_dir)?;
        Self::create_at_resolved_path(path)
    }

    /// 在显式项目根目录下创建新的事件日志。
    ///
    /// ## 注意
    ///
    /// 此方法创建空文件，需要立即调用 `append_stored` 写入 `SessionStart` 事件。
    /// 如果需要原子性创建（文件创建 + 首个事件写入），请使用
    /// `create_with_first_event` 或 `create_with_first_event_in_projects_root`。
    pub fn create_in_projects_root(
        projects_root: &Path,
        session_id: &str,
        working_dir: &Path,
    ) -> Result<Self> {
        let path = session_path_from_projects_root(projects_root, session_id, working_dir)?;
        Self::create_at_resolved_path(path)
    }

    /// 原子性创建事件日志并写入第一个事件。
    ///
    /// 使用临时文件 + 原子重命名确保：
    /// 1. 临时文件中写入完整的首个事件（通常是 SessionStart）
    /// 2. 通过原子重命名操作将临时文件移动到最终位置
    /// 3. 即使在写入过程中崩溃，也只会留下临时文件，不会产生空文件
    ///
    /// ## 参数约束
    ///
    /// - `session_id` 必须符合格式要求（仅含字母数字、`-`、`_`、`T`）
    /// - `working_dir` 必须可映射到确定的项目分桶目录
    /// - 文件必须不存在（`create_new` 保证）
    ///
    /// ## 返回
    ///
    /// 返回包含已写入首个事件的 `EventLog` 实例，以及写入的 `StoredEvent`。
    pub fn create_with_first_event(
        session_id: &str,
        working_dir: &Path,
        first_event: &StorageEvent,
    ) -> Result<(Self, StoredEvent)> {
        let path = session_path(session_id, working_dir)?;
        Self::create_with_first_event_at_path(path, first_event)
    }

    /// 在显式项目根目录下原子性创建事件日志并写入第一个事件。
    ///
    /// 参见 `create_with_first_event` 的文档。
    pub fn create_with_first_event_in_projects_root(
        projects_root: &Path,
        session_id: &str,
        working_dir: &Path,
        first_event: &StorageEvent,
    ) -> Result<(Self, StoredEvent)> {
        let path = session_path_from_projects_root(projects_root, session_id, working_dir)?;
        Self::create_with_first_event_at_path(path, first_event)
    }

    /// 在指定路径原子性创建事件日志并写入第一个事件（内部实现）。
    fn create_with_first_event_at_path(
        path: PathBuf,
        first_event: &StorageEvent,
    ) -> Result<(Self, StoredEvent)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                crate::io_error(
                    format!("failed to create sessions directory: {}", parent.display()),
                    e,
                )
            })?;
        }

        // 创建临时文件路径
        let temp_path = path.with_extension("tmp");

        // 在临时文件中写入首个事件
        let temp_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|e| {
                crate::io_error(
                    format!(
                        "failed to create temp session file: {}",
                        temp_path.display()
                    ),
                    e,
                )
            })?;

        let mut temp_writer = BufWriter::new(temp_file);
        let stored = StoredEvent {
            storage_seq: 1,
            event: first_event.clone(),
        };

        serde_json::to_writer(&mut temp_writer, &stored)
            .map_err(|e| crate::parse_error("failed to serialize first event", e))?;
        writeln!(temp_writer).map_err(|e| crate::io_error("failed to write newline", e))?;
        temp_writer
            .flush()
            .map_err(|e| crate::io_error("failed to flush temp session file", e))?;

        // into_inner() 可能失败如果缓冲区在写入时出错
        let temp_file = temp_writer.into_inner().map_err(|e| {
            crate::io_error(
                format!(
                    "failed to extract temp file from writer: {}",
                    temp_path.display()
                ),
                e.into_error(),
            )
        })?;
        temp_file
            .sync_all()
            .map_err(|e| crate::io_error("failed to sync temp session file", e))?;

        // 原子重命名到最终位置
        atomic_rename(&temp_path, &path)?;

        // 打开已创建的文件继续写入
        let file = OpenOptions::new().append(true).open(&path).map_err(|e| {
            crate::io_error(
                format!("failed to open created session file: {}", path.display()),
                e,
            )
        })?;

        Ok((
            Self {
                path,
                writer: BufWriter::new(file),
                next_storage_seq: 2,
            },
            stored,
        ))
    }

    fn create_at_resolved_path(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            // 每个 session 单独目录，后续才能安全地给该 session 增加附件或索引文件。
            fs::create_dir_all(parent).map_err(|e| {
                crate::io_error(
                    format!("failed to create sessions directory: {}", parent.display()),
                    e,
                )
            })?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                crate::io_error(
                    format!("failed to create session file: {}", path.display()),
                    e,
                )
            })?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            next_storage_seq: 1,
        })
    }

    /// 打开现有的事件日志。
    ///
    /// 通过扫描所有项目的 sessions 目录查找匹配的 session 文件，
    /// 并从文件尾部推断下一个 `storage_seq`，确保续写时序列号连续。
    pub fn open(session_id: &str) -> Result<Self> {
        let path = resolve_existing_session_path(session_id)?;
        Self::open_at_resolved_path(path)
    }

    /// 从显式项目根目录下打开现有事件日志。
    pub fn open_in_projects_root(projects_root: &Path, session_id: &str) -> Result<Self> {
        let path = resolve_existing_session_path_from_projects_root(projects_root, session_id)?;
        Self::open_at_resolved_path(path)
    }

    fn open_at_resolved_path(path: PathBuf) -> Result<Self> {
        let next_storage_seq = Self::last_storage_seq_from_path(&path)?.saturating_add(1);
        let file = OpenOptions::new().append(true).open(&path).map_err(|e| {
            crate::io_error(
                format!("failed to open session file: {}", path.display()),
                e,
            )
        })?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            next_storage_seq,
        })
    }

    /// 返回事件日志文件的完整路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一个存储事件到 JSONL 文件。
    ///
    /// 将 `StorageEvent` 包装为 `StoredEvent`（附带 `storage_seq`），
    /// 序列化为 JSON 行写入文件，然后立即 flush 并 sync 到磁盘。
    /// 返回包含已分配 `storage_seq` 的 `StoredEvent`。
    pub fn append_stored(&mut self, event: &StorageEvent) -> Result<StoredEvent> {
        Ok(self
            .append_batch(std::slice::from_ref(event))?
            .into_iter()
            .next()
            .expect("single append should always produce one stored event"))
    }

    pub fn append_batch(&mut self, events: &[StorageEvent]) -> Result<Vec<StoredEvent>> {
        let mut stored_events = Vec::with_capacity(events.len());
        for event in events {
            let stored = StoredEvent {
                storage_seq: self.next_storage_seq,
                event: event.clone(),
            };
            serde_json::to_writer(&mut self.writer, &stored)
                .map_err(|e| crate::parse_error("failed to serialize StoredEvent", e))?;
            writeln!(self.writer).map_err(|e| crate::io_error("failed to write newline", e))?;
            self.next_storage_seq = self.next_storage_seq.saturating_add(1);
            stored_events.push(stored);
        }
        self.flush_and_sync()?;
        Ok(stored_events)
    }

    pub(crate) fn flush_and_sync(&mut self) -> Result<()> {
        self.writer
            .flush()
            .map_err(|e| crate::io_error("failed to flush event log", e))?;
        self.writer
            .get_ref()
            .sync_all()
            .map_err(|e| crate::io_error("failed to sync event log", e))?;
        Ok(())
    }

    /// 回放指定路径的会话文件中的所有事件。
    ///
    /// 通过 [`EventLogIterator`] 逐行读取并调用回调函数，用于
    /// 会话重建或事件流订阅场景。
    pub fn replay_to<F>(path: &Path, mut callback: F) -> Result<()>
    where
        F: FnMut(&StoredEvent) -> Result<()>,
    {
        for event_result in EventLogIterator::from_path(path)? {
            callback(&event_result?)?;
        }
        Ok(())
    }

    /// 从会话文件尾部扫描最后一个 `storage_seq`。
    ///
    /// 对于小文件（≤64KB）全量扫描；对于大文件只读取尾部 64KB，
    /// 从后往前查找第一个包含 `storage_seq` 的 JSON 行。
    /// 如果尾部扫描未命中（例如截断点恰好在关键行上），则回退到全量扫描。
    ///
    /// 此方法用于 `open()` 时确定下一个 `storage_seq`，保证续写时序列号连续。
    pub fn last_storage_seq_from_path(path: &Path) -> Result<u64> {
        let file_size = std::fs::metadata(path)
            .map_err(|e| Self::enhance_metadata_error(path, e))?
            .len();

        if file_size == 0 {
            return Ok(0);
        }

        const TAIL_THRESHOLD: u64 = 64 * 1024;
        if file_size <= TAIL_THRESHOLD {
            return Self::scan_full_file_for_last_seq(path);
        }

        let offset = file_size - TAIL_THRESHOLD;
        let mut file = File::open(path).map_err(|e| Self::enhance_open_error(path, e))?;
        let started_mid_line = if offset == 0 {
            false
        } else {
            file.seek(std::io::SeekFrom::Start(offset - 1))
                .map_err(|e| crate::io_error("failed to seek in session file", e))?;
            let mut previous = [0u8; 1];
            file.read_exact(&mut previous)
                .map_err(|e| crate::io_error("failed to inspect session file tail", e))?;
            previous[0] != b'\n'
        };
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(|e| crate::io_error("failed to seek in session file", e))?;

        let mut tail_bytes = Vec::new();
        file.read_to_end(&mut tail_bytes)
            .map_err(|e| Self::enhance_read_error(path, e))?;

        if started_mid_line {
            let Some(position) = tail_bytes.iter().position(|byte| *byte == b'\n') else {
                return Self::scan_full_file_for_last_seq(path);
            };
            tail_bytes = tail_bytes[position + 1..].to_vec();
        }

        for line in tail_bytes.rsplit(|byte| *byte == b'\n') {
            let trimmed = trim_ascii_whitespace(line);
            if trimmed.is_empty() {
                continue;
            }
            if let Some(seq) = (|| {
                let v = match serde_json::from_slice::<serde_json::Value>(trimmed) {
                    Ok(v) => v,
                    Err(err) => {
                        log::warn!("failed to parse event line while scanning tail: {err}");
                        return None;
                    },
                };
                v.get("storage_seq").and_then(|s| s.as_u64())
            })() {
                return Ok(seq);
            }
        }

        Self::scan_full_file_for_last_seq(path)
    }

    /// 全量扫描文件，返回最后一个事件的 storage_seq。
    fn scan_full_file_for_last_seq(path: &Path) -> Result<u64> {
        let mut last_seq: Option<u64> = None;
        for event_result in EventLogIterator::from_path(path)? {
            let event = event_result?;
            last_seq = Some(event.storage_seq);
        }
        Ok(last_seq.unwrap_or(0))
    }

    /// 增强 metadata 读取错误的提示信息。
    ///
    /// 根据错误类型提供更具体的诊断信息，帮助用户定位问题。
    fn enhance_metadata_error(path: &Path, e: std::io::Error) -> crate::StoreError {
        use std::io::ErrorKind;

        let hint = match e.kind() {
            ErrorKind::PermissionDenied => format!(
                "permission denied: cannot access session file '{}'. Check if the file is owned \
                 by another user or has restrictive permissions.",
                path.display()
            ),
            ErrorKind::NotFound => format!(
                "session file '{}' not found. The session may have been deleted or moved.",
                path.display()
            ),
            _ => format!(
                "failed to read metadata for session file '{}'",
                path.display()
            ),
        };
        crate::io_error(hint, e)
    }

    /// 增强文件打开错误的提示信息。
    ///
    /// 针对常见的打开失败原因（权限、锁定等）提供具体诊断。
    fn enhance_open_error(path: &Path, e: std::io::Error) -> crate::StoreError {
        use std::io::ErrorKind;

        let hint = match e.kind() {
            ErrorKind::PermissionDenied => format!(
                "permission denied: cannot open session file '{}'. Check file permissions or if \
                 another process has locked it.",
                path.display()
            ),
            ErrorKind::NotFound => format!(
                "session file '{}' not found. The session may have been deleted.",
                path.display()
            ),
            _ => format!("failed to open session file '{}'", path.display()),
        };
        crate::io_error(hint, e)
    }

    /// 增强文件读取错误的提示信息。
    ///
    /// 特别处理非 UTF-8 数据的情况，这是会话文件损坏的常见原因。
    fn enhance_read_error(path: &Path, e: std::io::Error) -> crate::StoreError {
        use std::io::ErrorKind;

        let hint = match e.kind() {
            ErrorKind::InvalidData => format!(
                "session file '{}' contains invalid UTF-8 data. The file may be corrupted or \
                 truncated. Consider deleting this session to recover.",
                path.display()
            ),
            ErrorKind::PermissionDenied => format!(
                "permission denied while reading session file '{}'.",
                path.display()
            ),
            ErrorKind::UnexpectedEof => format!(
                "unexpected end of session file '{}'. The file may be truncated or still being \
                 written.",
                path.display()
            ),
            _ => format!(
                "failed to read session file '{}' (I/O error: {})",
                path.display(),
                e
            ),
        };
        crate::io_error(hint, e)
    }
}

fn trim_ascii_whitespace(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &line[start..end]
}

/// 原子性重命名文件。
///
/// 在 Unix 上，`fs::rename` 是原子操作。
/// 在 Windows 上，同卷 rename 是原子的；跨卷需要特殊处理（但 session 创建通常不会跨卷）。
fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|e| {
        crate::io_error(
            format!(
                "failed to atomically rename '{}' to '{}': {}",
                from.display(),
                to.display(),
                e
            ),
            e,
        )
    })
}

impl EventLogWriter for EventLog {
    fn append(&mut self, event: &StorageEvent) -> Result<StoredEvent> {
        self.append_stored(event)
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{AgentEventContext, StorageEvent, StorageEventPayload};
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn last_storage_seq_tail_scan_skips_partial_first_line() {
        let temp_dir = tempfile::tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("session-test-session.jsonl");
        let mut log = EventLog::create_at_path(path.clone()).expect("event log");

        for index in 0..3 {
            log.append_stored(&StorageEvent {
                turn_id: Some(format!("turn-{index}")),
                agent: AgentEventContext::default(),
                payload: StorageEventPayload::AssistantFinal {
                    content: "x".repeat(40_000),
                    reasoning_content: None,
                    reasoning_signature: None,
                    step_index: None,
                    timestamp: Some(Utc::now()),
                },
            })
            .expect("append should succeed");
        }

        assert_eq!(
            EventLog::last_storage_seq_from_path(&path).expect("tail scan should succeed"),
            3
        );
    }

    #[test]
    fn last_storage_seq_tail_scan_handles_multibyte_cutoff() {
        const TAIL_THRESHOLD: usize = 64 * 1024;
        let temp_dir = tempfile::tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("session-test-session.jsonl");
        let mut log = EventLog::create_at_path(path.clone()).expect("event log");

        let mut matched = false;
        for index in 0..16 {
            log.append_stored(&StorageEvent {
                turn_id: Some(format!("turn-{index}")),
                agent: AgentEventContext::default(),
                payload: StorageEventPayload::AssistantFinal {
                    content: "你".repeat(30_000),
                    reasoning_content: None,
                    reasoning_signature: None,
                    step_index: None,
                    timestamp: Some(Utc::now()),
                },
            })
            .expect("append should succeed");

            let bytes = std::fs::read(&path).expect("session file should be readable");
            if bytes.len() <= TAIL_THRESHOLD {
                continue;
            }

            let offset = bytes.len() - TAIL_THRESHOLD;
            // UTF-8 continuation byte（10xxxxxx）意味着 tail 读取将从多字节字符中间开始。
            if (bytes[offset] & 0b1100_0000) == 0b1000_0000 && bytes[offset - 1] != b'\n' {
                matched = true;
                assert_eq!(
                    EventLog::last_storage_seq_from_path(&path)
                        .expect("tail scan should succeed even on multibyte cutoff"),
                    (index + 1) as u64
                );
                break;
            }
        }

        assert!(
            matched,
            "test setup failed to produce a multibyte tail cutoff scenario"
        );
    }

    #[test]
    fn create_with_first_event_produces_valid_event_log() {
        let temp_dir = tempfile::tempdir().expect("tempdir should be created");
        let path = temp_dir
            .path()
            .join("sessions")
            .join("test-session")
            .join("session-test.jsonl");

        let created_at = Utc
            .with_ymd_and_hms(2026, 4, 25, 15, 40, 54)
            .single()
            .expect("timestamp should be valid");

        let first_event = StorageEvent {
            turn_id: None,
            agent: AgentEventContext::default(),
            payload: StorageEventPayload::SessionStart {
                session_id: "test-session".to_string(),
                timestamp: created_at,
                working_dir: "/tmp/project".to_string(),
                parent_session_id: None,
                parent_storage_seq: None,
            },
        };

        let (log, stored) = EventLog::create_with_first_event_at_path(path.clone(), &first_event)
            .expect("create with first event should succeed");

        // 验证返回的 StoredEvent
        assert_eq!(stored.storage_seq, 1);
        // 验证事件类型正确
        match &stored.event.payload {
            StorageEventPayload::SessionStart { session_id, .. } => {
                assert_eq!(session_id, "test-session");
            },
            _ => panic!("unexpected event type"),
        }

        // 验证文件已创建
        assert!(path.exists(), "session file should exist");

        // 验证 EventLog 可以打开并继续写入
        assert_eq!(log.next_storage_seq, 2, "next storage_seq should be 2");

        // 验证没有临时文件残留
        let temp_path = path.with_extension("tmp");
        assert!(!temp_path.exists(), "temp file should be cleaned up");

        // 验证文件可以被正常回放
        let replayed: Vec<_> = EventLogIterator::from_path(&path)
            .expect("should replay events")
            .collect::<Result<Vec<_>>>()
            .expect("should collect events");
        assert_eq!(replayed.len(), 1, "should have one event");
        assert_eq!(replayed[0].storage_seq, 1);
    }

    #[test]
    fn create_with_first_event_handles_existing_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir should be created");
        let path = temp_dir
            .path()
            .join("sessions")
            .join("test-session-replace")
            .join("session-test.jsonl");
        let temp_path = path.with_extension("tmp");

        // 创建一个预先存在的临时文件（模拟之前失败的创建）
        fs::create_dir_all(path.parent().unwrap()).expect("parent dir should exist");
        fs::write(&temp_path, "partial content").expect("temp file should be created");

        // 验证临时文件确实被创建了
        assert!(temp_path.exists(), "temp file should exist");

        // 创建需要先删除临时文件
        let _ = fs::remove_file(&temp_path);

        let created_at = Utc
            .with_ymd_and_hms(2026, 4, 25, 15, 40, 54)
            .single()
            .expect("timestamp should be valid");

        let first_event = StorageEvent {
            turn_id: None,
            agent: AgentEventContext::default(),
            payload: StorageEventPayload::SessionStart {
                session_id: "test-session-replace".to_string(),
                timestamp: created_at,
                working_dir: "/tmp/project".to_string(),
                parent_session_id: None,
                parent_storage_seq: None,
            },
        };

        let _ = EventLog::create_with_first_event_at_path(path.clone(), &first_event)
            .expect("create with first event should succeed");

        // 验证最终文件存在
        assert!(path.exists(), "session file should exist");

        // 验证临时文件被清理
        assert!(!temp_path.exists(), "temp file should be cleaned up");
    }
}

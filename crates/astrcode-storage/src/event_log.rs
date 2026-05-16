//! 追加式 JSONL 事件日志，用于会话持久化。
//!
//! 每个会话对应一个事件日志文件，事件以换行分隔的扁平 JSON 对象写入，
//! 写入后不可修改。存储层在追加时分配单调递增的 `seq` 序号。

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use astrcode_core::{
    event::{Event, EventPayload},
    storage::StorageError,
};

/// An append-only JSONL event log.
///
/// Each session has one event log file. Events are written as newline-delimited
/// flat JSON objects and never modified. Storage assigns `seq` at append time.
///
/// 内部持有 BufWriter<File> 避免每次 append 都重开文件，大幅减少系统调用开销。
pub struct EventLog {
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
    next_seq: Mutex<u64>,
    sync_pending: AtomicBool,
}

impl Drop for EventLog {
    fn drop(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            if let Err(e) = writer.flush() {
                tracing::warn!(
                    "Failed to flush event log '{}' on drop: {e}",
                    self.path.display()
                );
                return;
            }
            if let Err(e) = writer.get_ref().sync_all() {
                tracing::warn!(
                    "Failed to sync event log '{}' on drop: {e}",
                    self.path.display()
                );
            }
        }
    }
}

impl EventLog {
    /// Create a new event log file with an initial event.
    pub async fn create(
        path: PathBuf,
        initial_event: Event,
    ) -> Result<(Self, Event), StorageError> {
        let mut event = initial_event;
        event.seq = Some(0);

        let file = File::create(&path)?;
        let mut writer = BufWriter::new(file);
        let line = serde_json::to_string(&event)?;
        writeln!(writer, "{}", line)?;
        Self::flush_and_sync_writer(&mut writer, &path)?;
        Ok((
            Self {
                path,
                writer: Mutex::new(writer),
                next_seq: Mutex::new(1),
                sync_pending: AtomicBool::new(false),
            },
            event,
        ))
    }

    /// Open an existing event log.
    pub async fn open(path: PathBuf) -> Result<Self, StorageError> {
        if !path.exists() {
            return Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("Event log not found: {}", path.display()),
            )
            .into());
        }
        let next_seq = last_seq_from_path(&path)?.saturating_add(1);
        // 以 append 模式打开文件，持有句柄供后续写入
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            writer: Mutex::new(BufWriter::new(file)),
            next_seq: Mutex::new(next_seq),
            sync_pending: AtomicBool::new(false),
        })
    }

    /// Append a durable event to the log and return it with its assigned seq.
    ///
    /// Writes to the OS page cache immediately (process-crash-safe) but defers
    /// `sync_all()` until [`force_sync`] is called, typically at turn boundaries.
    pub async fn append(&self, mut event: Event) -> Result<Event, StorageError> {
        let mut next_seq = self
            .next_seq
            .lock()
            .map_err(|_| StorageError::LockError("event log sequence lock poisoned".into()))?;
        let seq = *next_seq;
        event.seq = Some(seq);

        let mut writer = self
            .writer
            .lock()
            .map_err(|_| StorageError::LockError("event log writer lock poisoned".into()))?;
        let line = serde_json::to_string(&event)?;
        writeln!(writer, "{}", line)?;
        writer.flush().map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_flush_error(&self.path, e),
            ))
        })?;
        self.sync_pending.store(true, Ordering::Release);
        *next_seq += 1;
        Ok(event)
    }

    /// Replay all events from the beginning.
    pub async fn replay_all(&self) -> Result<Vec<Event>, StorageError> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)?;
            events.push(event);
        }
        Ok(events)
    }

    /// Replay events whose assigned seq is greater than `seq`.
    ///
    /// This is used when recovering from a snapshot: only the events that
    /// occurred after the snapshot point need to be replayed, not the whole log.
    pub async fn replay_after(&self, seq: u64) -> Result<Vec<Event>, StorageError> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)?;
            if event.seq.is_some_and(|event_seq| event_seq > seq) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Count total events.
    pub async fn count(&self) -> Result<usize, StorageError> {
        let next_seq = self
            .next_seq
            .lock()
            .map_err(|_| StorageError::LockError("event log sequence lock poisoned".into()))?;
        Ok(*next_seq as usize)
    }

    /// Force-fsync the event log if there are pending writes.
    ///
    /// Called at turn boundaries to ensure all events written since the last
    /// sync are durable (power-loss-safe). No-op if nothing is pending.
    pub fn force_sync(&self) -> Result<(), StorageError> {
        if !self.sync_pending.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| StorageError::LockError("event log writer lock poisoned".into()))?;
        writer.flush().map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_flush_error(&self.path, e),
            ))
        })?;
        writer.get_ref().sync_all().map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_sync_error(&self.path, e),
            ))
        })?;
        self.sync_pending.store(false, Ordering::Release);
        Ok(())
    }

    /// Get the file path.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Read only the first event from the log file.
    ///
    /// This is significantly faster than `replay_all()` for large logs
    /// because it stops after reading the first non-empty line.
    /// Useful for extracting session metadata (SessionStarted event)
    /// without replaying the entire history.
    pub async fn read_first_event(path: &PathBuf) -> Result<Option<Event>, StorageError> {
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line)?;
            return Ok(Some(event));
        }
        Ok(None)
    }

    /// Read the first event, last event, and first user message from the log
    /// in a single pass. Returns `(first, last, first_user_message)`.
    pub async fn read_first_and_last(
        path: &PathBuf,
    ) -> Result<(Option<Event>, Option<Event>, Option<String>), StorageError> {
        if !path.exists() {
            return Ok((None, None, None));
        }
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut first: Option<Event> = None;
        let mut last: Option<Event> = None;
        let mut first_user: Option<String> = None;
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Event>(&line) {
                if first.is_none() {
                    first = Some(event.clone());
                }
                if first_user.is_none() {
                    if let EventPayload::UserMessage { text, .. } = &event.payload {
                        first_user = Some(text.clone());
                    }
                }
                last = Some(event);
            }
        }
        Ok((first, last, first_user))
    }

    fn flush_and_sync_writer(
        writer: &mut BufWriter<File>,
        path: &Path,
    ) -> Result<(), StorageError> {
        writer.flush().map_err(|e| {
            StorageError::Io(std::io::Error::new(e.kind(), enhance_flush_error(path, e)))
        })?;
        writer.get_ref().sync_all().map_err(|e| {
            StorageError::Io(std::io::Error::new(e.kind(), enhance_sync_error(path, e)))
        })
    }
}

/// 从 JSONL 文件尾部扫描最后一个事件的 seq。
///
/// 对于小文件（≤64KB）全量扫描；对于大文件只读取尾部 64KB，
/// 从后往前查找最后一个包含有效 `seq` 的事件行。
fn last_seq_from_path(path: &Path) -> Result<u64, StorageError> {
    let file_size = fs::metadata(path)
        .map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_metadata_error(path, e),
            ))
        })?
        .len();

    if file_size == 0 {
        return Ok(0);
    }

    const TAIL_THRESHOLD: u64 = 64 * 1024;
    if file_size <= TAIL_THRESHOLD {
        return scan_full_file_for_last_seq(path);
    }

    let offset = file_size - TAIL_THRESHOLD;
    let mut file = File::open(path).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(path, e)))
    })?;

    // Check if the tail starts mid-line by examining the byte before offset.
    let started_mid_line = if offset == 0 {
        false
    } else {
        file.seek(std::io::SeekFrom::Start(offset - 1))
            .map_err(StorageError::Io)?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous).map_err(StorageError::Io)?;
        previous[0] != b'\n'
    };
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(StorageError::Io)?;

    let mut tail_bytes = Vec::new();
    file.read_to_end(&mut tail_bytes).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_read_error(path, e)))
    })?;

    // Skip the first partial line if we landed mid-line.
    if started_mid_line {
        let Some(position) = tail_bytes.iter().position(|b| *b == b'\n') else {
            return scan_full_file_for_last_seq(path);
        };
        tail_bytes = tail_bytes[position + 1..].to_vec();
    }

    // Walk backwards through lines looking for the last valid seq.
    for line in tail_bytes.rsplit(|b| *b == b'\n') {
        let trimmed = trim_ascii_whitespace(line);
        if trimmed.is_empty() {
            continue;
        }
        if let Some(seq) = parse_seq_from_line(trimmed) {
            return Ok(seq);
        }
    }

    scan_full_file_for_last_seq(path)
}

fn scan_full_file_for_last_seq(path: &Path) -> Result<u64, StorageError> {
    let mut last_seq: Option<u64> = None;
    let iterator = EventLogIterator::new(&path.to_path_buf())?;
    for event_result in iterator {
        let (_line_number, event) = event_result?;
        last_seq = event.seq;
    }
    Ok(last_seq.unwrap_or(0))
}

fn parse_seq_from_line(line: &[u8]) -> Option<u64> {
    let v = serde_json::from_slice::<serde_json::Value>(line).ok()?;
    v.get("seq")?.as_u64()
}

fn trim_ascii_whitespace(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &line[start..end]
}

fn enhance_open_error(path: &Path, e: std::io::Error) -> String {
    match e.kind() {
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
    }
}

fn enhance_read_error(path: &Path, e: std::io::Error) -> String {
    match e.kind() {
        ErrorKind::InvalidData => format!(
            "session file '{}' contains invalid UTF-8 data. The file may be corrupted. Consider \
             deleting this session.",
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
    }
}

fn enhance_flush_error(path: &Path, e: std::io::Error) -> String {
    format!("failed to flush event log '{}': {}", path.display(), e)
}

fn enhance_sync_error(path: &Path, e: std::io::Error) -> String {
    format!(
        "failed to sync event log '{}' to disk: {}",
        path.display(),
        e
    )
}

fn enhance_metadata_error(path: &Path, e: std::io::Error) -> String {
    match e.kind() {
        ErrorKind::PermissionDenied => format!(
            "permission denied: cannot access session file '{}'. Check file permissions.",
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
    }
}

/// 事件日志的流式迭代器，逐行读取并解析事件。
pub struct EventLogIterator {
    reader: BufReader<File>,
    /// 当前读取的行号（从 1 开始，含空行），用于错误定位。
    line_number: usize,
    /// 文件路径，用于错误消息上下文。
    path: PathBuf,
}

impl EventLogIterator {
    /// 从指定路径创建事件日志迭代器。
    pub fn new(path: &PathBuf) -> Result<Self, StorageError> {
        let file = File::open(path).map_err(|e| {
            StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(path, e)))
        })?;
        Ok(Self {
            reader: BufReader::new(file),
            line_number: 0,
            path: path.clone(),
        })
    }
}

impl Iterator for EventLogIterator {
    /// 返回 (行号, 事件) 元组，跳过空行。
    type Item = Result<(usize, Event), StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {
                    self.line_number += 1;
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let event = match serde_json::from_str::<Event>(trimmed) {
                        Ok(event) => event,
                        Err(e) => {
                            let preview = if trimmed.len() > 100 {
                                format!("{}...", &trimmed[..100])
                            } else {
                                trimmed.to_string()
                            };
                            let context = format!(
                                "failed to parse event at {}:{} (content: '{}'). The session file \
                                 may be corrupted. Original error: {e}",
                                self.path.display(),
                                self.line_number,
                                preview,
                            );
                            return Some(Err(StorageError::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                context,
                            ))));
                        },
                    };
                    if let Err(e) = validate_event(&event, self.line_number, &self.path) {
                        return Some(Err(e));
                    }
                    return Some(Ok((self.line_number, event)));
                },
                Err(e) => {
                    return Some(Err(StorageError::Io(std::io::Error::new(
                        e.kind(),
                        enhance_read_error(&self.path, e),
                    ))));
                },
            }
        }
    }
}

fn validate_event(event: &Event, line_number: usize, path: &Path) -> Result<(), StorageError> {
    if event.session_id.as_str().is_empty() {
        return Err(StorageError::InvalidId(format!(
            "event at {}:{} has empty session_id",
            path.display(),
            line_number,
        )));
    }
    if event.timestamp.timestamp() == 0 {
        tracing::warn!(
            "Event at {}:{} has epoch-zero timestamp; may indicate corruption",
            path.display(),
            line_number,
        );
    }
    Ok(())
}

/// 批量追加器，用于提高写入效率。
///
/// 缓冲追加请求，在可配置的时间窗口内批量刷盘。
/// 适用于高频事件写入场景，减少磁盘 I/O 次数。
///
/// # 所有权
///
/// `BatchAppender` 通过值获取 `EventLog` 的所有权。
/// 创建后不得再通过原始引用调用 `EventLog::append()`，
/// 否则会导致序列号冲突和数据损坏。
/// 使用 [`BatchAppender::into_inner`] 可以回收底层日志。
pub struct BatchAppender {
    /// 底层事件日志
    log: EventLog,
    /// 待刷盘的事件缓冲区
    buffer: Vec<Event>,
    /// 刷盘时间窗口（毫秒）
    flush_window_ms: u64,
}

impl BatchAppender {
    /// 创建新的批量追加器。
    ///
    /// 接管 `log` 的所有权，调用方不应再持有或使用该 `EventLog`。
    ///
    /// # 参数
    /// - `log`: 底层事件日志（所有权转移）
    /// - `flush_window_ms`: 刷盘时间窗口（毫秒）
    pub fn new(log: EventLog, flush_window_ms: u64) -> Self {
        Self {
            log,
            buffer: Vec::new(),
            flush_window_ms,
        }
    }

    /// 将事件推入缓冲区，等待后续刷盘。
    pub fn push(&mut self, event: Event) -> Result<(), StorageError> {
        self.buffer.push(event);
        Ok(())
    }

    /// 将缓冲区中的所有事件批量写入日志文件并刷盘。
    ///
    /// 返回本次刷盘的事件数量。刷盘成功后才更新 seq 和清空缓冲区，
    /// 避免部分写入导致事件丢失。
    pub fn flush(&mut self) -> Result<usize, StorageError> {
        if self.buffer.is_empty() {
            return Ok(0);
        }

        let count = self.buffer.len();
        let mut next_seq = self
            .log
            .next_seq
            .lock()
            .map_err(|_| StorageError::LockError("event log sequence lock poisoned".into()))?;
        let mut writer = self
            .log
            .writer
            .lock()
            .map_err(|_| StorageError::LockError("event log writer lock poisoned".into()))?;
        let mut seq = *next_seq;
        for event in &mut self.buffer {
            event.seq = Some(seq);
            seq += 1;
            let line = serde_json::to_string(event)?;
            writeln!(writer, "{}", line)?;
        }
        writer.flush().map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_flush_error(&self.log.path, e),
            ))
        })?;
        writer.get_ref().sync_all().map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_sync_error(&self.log.path, e),
            ))
        })?;
        // 先刷盘成功再更新 seq 和清空 buffer，避免部分写入后 seq 已前进导致事件丢失
        *next_seq = seq;
        self.buffer.clear();
        Ok(count)
    }

    /// 获取配置的刷盘时间窗口（毫秒）。
    pub fn flush_window_ms(&self) -> u64 {
        self.flush_window_ms
    }

    /// 回收底层 `EventLog`，丢弃未刷盘的缓冲区事件。
    ///
    /// 返回前会尝试刷盘。如果刷盘失败，仍然返回 `EventLog`
    /// （未刷盘的事件会丢失）。
    pub fn into_inner(mut self) -> EventLog {
        if let Err(error) = self.flush() {
            tracing::warn!(
                path = %self.log.path.display(),
                %error,
                "BatchAppender::into_inner: flush failed, buffered events may be lost"
            );
        }
        self.log
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::event::EventPayload;
    use tempfile::tempdir;

    use super::*;

    fn make_start_event(id: &str) -> Event {
        Event::new(
            id.into(),
            None,
            EventPayload::SessionStarted {
                working_dir: "/tmp".into(),
                model_id: "test-model".into(),
                parent_session_id: None,
            },
        )
    }

    #[tokio::test]
    async fn create_append_and_replay_assigns_stable_seq() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let (log, start) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();

        assert_eq!(start.seq, Some(0));

        let appended = log
            .append(Event::new(
                "s1".into(),
                Some("turn-1".into()),
                EventPayload::TurnStarted,
            ))
            .await
            .unwrap();

        assert_eq!(appended.seq, Some(1));
        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, Some(0));
        assert_eq!(events[1].seq, Some(1));
    }

    #[tokio::test]
    async fn open_continues_seq_from_existing_log() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let (log, _) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();
        log.append(Event::new(
            "s1".into(),
            Some("turn-1".into()),
            EventPayload::TurnStarted,
        ))
        .await
        .unwrap();

        let reopened = EventLog::open(path).await.unwrap();
        let appended = reopened
            .append(Event::new(
                "s1".into(),
                Some("turn-1".into()),
                EventPayload::TurnCompleted {
                    finish_reason: "stop".into(),
                },
            ))
            .await
            .unwrap();

        assert_eq!(appended.seq, Some(2));
        assert_eq!(reopened.count().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn event_log_only_receives_durable_events_from_callers() {
        assert!(
            !EventPayload::AssistantTextDelta {
                message_id: "m1".into(),
                delta: "partial".into(),
            }
            .is_durable()
        );
        assert!(
            EventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            }
            .is_durable()
        );
    }

    #[tokio::test]
    async fn batch_appender_push_and_flush_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("batch.jsonl");
        let (log, start) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();

        assert_eq!(start.seq, Some(0));

        let mut appender = BatchAppender::new(log, 100);
        appender
            .push(Event::new(
                "s1".into(),
                Some("turn-1".into()),
                EventPayload::TurnStarted,
            ))
            .unwrap();
        appender
            .push(Event::new(
                "s1".into(),
                Some("turn-1".into()),
                EventPayload::TurnCompleted {
                    finish_reason: "stop".into(),
                },
            ))
            .unwrap();

        assert_eq!(appender.flush().unwrap(), 2);

        let log = appender.into_inner();
        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, Some(0)); // start
        assert_eq!(events[1].seq, Some(1)); // batch event 1
        assert_eq!(events[2].seq, Some(2)); // batch event 2
    }

    #[tokio::test]
    async fn batch_appender_flush_empty_is_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        let (log, start) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();

        let mut appender = BatchAppender::new(log, 100);
        assert_eq!(appender.flush().unwrap(), 0);

        let log = appender.into_inner();
        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, Some(start.seq.unwrap()));
    }

    #[tokio::test]
    async fn batch_appender_into_inner_flushes_remaining() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("drop.jsonl");
        let (log, _) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();

        let mut appender = BatchAppender::new(log, 100);
        appender
            .push(Event::new(
                "s1".into(),
                Some("turn-1".into()),
                EventPayload::TurnStarted,
            ))
            .unwrap();
        // Don't call flush — into_inner should flush for us.
        let log = appender.into_inner();

        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].seq, Some(1));
    }
}

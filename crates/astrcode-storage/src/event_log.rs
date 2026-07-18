//! 追加式 JSONL 事件日志，用于会话持久化。
//!
//! 每个会话对应一个事件日志文件，事件以换行分隔的 JSON 对象写入，
//! 写入后不可修改。存储层在追加时分配单调递增的 `seq` 序号。

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, ErrorKind, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use astrcode_core::{
    event::{Event, EventPayload},
    storage::StorageError,
};
use tokio::sync::{mpsc, oneshot};

/// `(first_event, last_event, first_user_message)` from a single log scan.
pub type EventLogEnds = (Option<Event>, Option<Event>, Option<String>);

async fn run_blocking_io<F, T>(f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        StorageError::Io(std::io::Error::other(format!(
            "event log blocking task failed: {e}"
        )))
    })?
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

fn parse_event_line(path: &Path, line_number: usize, line: &str) -> Result<Event, StorageError> {
    let trimmed = line.trim();
    let event = match serde_json::from_str::<Event>(trimmed) {
        Ok(event) => event,
        Err(e) => {
            let preview = if trimmed.len() > 100 {
                let end = trimmed.floor_char_boundary(100);
                format!("{}...", &trimmed[..end])
            } else {
                trimmed.to_string()
            };
            let context = format!(
                "failed to parse event at {}:{} (content: '{}'). The session file may be \
                 corrupted. Original error: {e}",
                path.display(),
                line_number,
                preview,
            );
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                context,
            )));
        },
    };
    validate_event(&event, line_number, path)?;
    Ok(event)
}

fn replay_events_at_path(
    path: &Path,
    after_seq: Option<u64>,
    max_events: Option<usize>,
) -> Result<Vec<Event>, StorageError> {
    if max_events == Some(0) {
        return Ok(Vec::new());
    }
    let file = File::open(path).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(path, e)))
    })?;
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut line_number = 0usize;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).map_err(|e| {
            StorageError::Io(std::io::Error::new(e.kind(), enhance_read_error(path, e)))
        })?;
        if bytes_read == 0 {
            break;
        }
        if !line.ends_with('\n') {
            tracing::warn!(
                path = %path.display(),
                discarded_bytes = bytes_read,
                "ignored incomplete trailing event log record while replaying"
            );
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        line_number += 1;
        let event = parse_event_line(path, line_number, &line)?;
        if after_seq.is_none_or(|seq| event.seq.is_some_and(|event_seq| event_seq > seq)) {
            events.push(event);
            if max_events.is_some_and(|limit| events.len() >= limit) {
                break;
            }
        }
    }
    Ok(events)
}

fn read_first_event_at_path(path: &Path) -> Result<Option<Event>, StorageError> {
    if !path.exists() {
        return Ok(None);
    }
    let file = File::open(path).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(path, e)))
    })?;
    let mut reader = BufReader::new(file);
    let mut line_number = 0usize;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).map_err(|e| {
            StorageError::Io(std::io::Error::new(e.kind(), enhance_read_error(path, e)))
        })?;
        if bytes_read == 0 {
            return Ok(None);
        }
        if !line.ends_with('\n') {
            return Ok(None);
        }
        if line.trim().is_empty() {
            continue;
        }
        line_number += 1;
        return Ok(Some(parse_event_line(path, line_number, &line)?));
    }
}

fn read_first_and_last_at_path(path: &Path) -> Result<EventLogEnds, StorageError> {
    if !path.exists() {
        return Ok((None, None, None));
    }
    let file = File::open(path).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(path, e)))
    })?;
    let mut reader = BufReader::new(file);
    let mut first: Option<Event> = None;
    let mut last: Option<Event> = None;
    let mut first_user: Option<String> = None;
    let mut line_number = 0usize;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).map_err(|e| {
            StorageError::Io(std::io::Error::new(e.kind(), enhance_read_error(path, e)))
        })?;
        if bytes_read == 0 {
            break;
        }
        if !line.ends_with('\n') {
            tracing::warn!(
                path = %path.display(),
                discarded_bytes = bytes_read,
                "ignored incomplete trailing event log record while reading summary"
            );
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        line_number += 1;
        let event = parse_event_line(path, line_number, &line)?;
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
    Ok((first, last, first_user))
}

// ── Write-side commands ───────────────────────────────────────────────────────

const CHANNEL_CAPACITY: usize = 1024;

enum WriteCommand {
    Append {
        event: Box<Event>,
        done: oneshot::Sender<Result<Event, StorageError>>,
    },
    AppendBatch {
        events: Vec<Event>,
        done: oneshot::Sender<Result<Vec<Event>, StorageError>>,
    },
    FlushSync {
        done: oneshot::Sender<Result<(), StorageError>>,
    },
    Shutdown,
}

struct WriterState {
    writer: File,
    next_seq: u64,
    committed_len: u64,
    path: PathBuf,
    dirty: bool,
    poisoned: Option<String>,
}

impl WriterState {
    fn open_append(path: PathBuf, next_seq: u64) -> Result<Self, StorageError> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(&path, e)))
            })?;
        let committed_len = file.metadata().map_err(StorageError::Io)?.len();
        Ok(Self {
            writer: file,
            next_seq,
            committed_len,
            path,
            dirty: false,
            poisoned: None,
        })
    }

    fn append_one(&mut self, mut event: Box<Event>) -> Result<Event, StorageError> {
        event.seq = Some(self.next_seq);
        let mut encoded = serde_json::to_vec(&*event)?;
        encoded.push(b'\n');
        self.write_committed_record(&encoded)?;
        self.next_seq += 1;
        self.dirty = true;
        Ok(*event)
    }

    fn append_batch(&mut self, events: &mut [Event]) -> Result<(), StorageError> {
        let mut next_seq = self.next_seq;
        let mut encoded = Vec::new();
        for event in events.iter_mut() {
            event.seq = Some(next_seq);
            next_seq += 1;
            serde_json::to_writer(&mut encoded, event)?;
            encoded.push(b'\n');
        }
        self.write_committed_record(&encoded)?;
        self.next_seq = next_seq;
        self.dirty = true;
        Ok(())
    }

    fn write_committed_record(&mut self, encoded: &[u8]) -> Result<(), StorageError> {
        if let Some(reason) = &self.poisoned {
            return Err(StorageError::Io(std::io::Error::other(format!(
                "event log writer is unavailable after failed recovery: {reason}"
            ))));
        }

        let committed_len = self.committed_len;
        if let Err(write_error) = self
            .writer
            .write_all(encoded)
            .and_then(|_| self.writer.flush())
        {
            if let Err(rollback_error) = self.rollback_partial_write(committed_len) {
                let reason = format!(
                    "write failed: {write_error}; rollback to {committed_len} bytes failed: \
                     {rollback_error}"
                );
                self.poisoned = Some(reason.clone());
                return Err(StorageError::Io(std::io::Error::new(
                    write_error.kind(),
                    reason,
                )));
            }
            return Err(StorageError::Io(std::io::Error::new(
                write_error.kind(),
                format!(
                    "failed to append event log '{}'; partial write was rolled back: {write_error}",
                    self.path.display()
                ),
            )));
        }
        self.committed_len = self.committed_len.saturating_add(encoded.len() as u64);
        Ok(())
    }

    fn rollback_partial_write(&mut self, committed_len: u64) -> std::io::Result<()> {
        self.writer.set_len(committed_len)?;
        self.writer.seek(std::io::SeekFrom::Start(committed_len))?;
        self.committed_len = committed_len;
        Ok(())
    }

    fn flush_and_sync(&mut self) -> Result<(), StorageError> {
        if !self.dirty {
            return Ok(());
        }
        self.writer.flush().map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_flush_error(&self.path, e),
            ))
        })?;
        self.writer.sync_all().map_err(|e| {
            StorageError::Io(std::io::Error::new(
                e.kind(),
                enhance_sync_error(&self.path, e),
            ))
        })?;
        self.dirty = false;
        Ok(())
    }
}

fn write_loop(
    mut rx: mpsc::Receiver<WriteCommand>,
    mut state: WriterState,
    next_seq: Arc<AtomicU64>,
) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCommand::Append { event, done } => {
                let result = state.append_one(event);
                if result.is_ok() {
                    next_seq.store(state.next_seq, Ordering::Release);
                }
                let _ = done.send(result);
            },
            WriteCommand::AppendBatch { mut events, done } => {
                let result = state.append_batch(&mut events);
                if result.is_ok() {
                    next_seq.store(state.next_seq, Ordering::Release);
                }
                let _ = done.send(result.map(|_| events));
            },
            WriteCommand::FlushSync { done } => {
                let _ = done.send(state.flush_and_sync());
            },
            WriteCommand::Shutdown => break,
        }
    }

    if let Err(e) = state.flush_and_sync() {
        tracing::warn!(
            path = %state.path.display(),
            error = %e,
            "failed to flush event log on writer thread shutdown"
        );
    }
}

// ── EventLog ──────────────────────────────────────────────────────────────────

fn create_at_path(
    path: PathBuf,
    mut initial_event: Event,
) -> Result<(WriterState, Event), StorageError> {
    initial_event.seq = Some(0);
    let file = File::create(&path).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(&path, e)))
    })?;
    let mut writer = file;
    let mut encoded = serde_json::to_vec(&initial_event)?;
    encoded.push(b'\n');
    writer.write_all(&encoded)?;
    writer.flush().map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_flush_error(&path, e)))
    })?;
    writer.sync_all().map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_sync_error(&path, e)))
    })?;
    Ok((
        WriterState {
            writer,
            next_seq: 1,
            committed_len: encoded.len() as u64,
            path,
            dirty: false,
            poisoned: None,
        },
        initial_event,
    ))
}

fn open_at_path(path: PathBuf) -> Result<WriterState, StorageError> {
    if !path.exists() {
        return Err(std::io::Error::new(
            ErrorKind::NotFound,
            format!("Event log not found: {}", path.display()),
        )
        .into());
    }
    recover_incomplete_tail(&path)?;
    let next_seq = last_seq_from_path(&path)?.saturating_add(1);
    WriterState::open_append(path, next_seq)
}

/// Treat the terminating newline as the commit marker for a JSONL record.
/// A process crash may leave only the final record incomplete; corruption in
/// any earlier committed line is still rejected by normal replay validation.
fn recover_incomplete_tail(path: &Path) -> Result<(), StorageError> {
    let file_len = fs::metadata(path).map_err(StorageError::Io)?.len();
    if file_len == 0 {
        return Ok(());
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(StorageError::Io)?;
    file.seek(std::io::SeekFrom::End(-1))
        .map_err(StorageError::Io)?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).map_err(StorageError::Io)?;
    if last[0] == b'\n' {
        return Ok(());
    }

    const SCAN_CHUNK_SIZE: u64 = 8 * 1024;
    let mut end = file_len;
    let mut chunk = vec![0u8; SCAN_CHUNK_SIZE as usize];
    while end > 0 {
        let start = end.saturating_sub(SCAN_CHUNK_SIZE);
        let len = (end - start) as usize;
        file.seek(std::io::SeekFrom::Start(start))
            .map_err(StorageError::Io)?;
        file.read_exact(&mut chunk[..len])
            .map_err(StorageError::Io)?;
        if let Some(index) = chunk[..len].iter().rposition(|byte| *byte == b'\n') {
            let committed_len = start + index as u64 + 1;
            file.set_len(committed_len).map_err(StorageError::Io)?;
            file.sync_all().map_err(StorageError::Io)?;
            tracing::warn!(
                path = %path.display(),
                discarded_bytes = file_len - committed_len,
                "discarded incomplete trailing event log record"
            );
            return Ok(());
        }
        end = start;
    }

    Err(StorageError::Io(std::io::Error::new(
        ErrorKind::InvalidData,
        format!(
            "event log '{}' has no committed newline-terminated record",
            path.display()
        ),
    )))
}

/// An append-only JSONL event log backed by a dedicated writer thread.
///
/// Each session has one event log file. Events are written as newline-delimited
/// JSON objects and never modified. Storage assigns `seq` at append time.
///
/// # Architecture
///
/// ```text
/// EventLog
///   ├── tx (bounded channel, 1024 capacity)
///   │     └── write_loop (spawn_blocking)
///   │           ├── File (pre-encoded atomic batches)
///   │           └── dirty tracking (deferred fsync)
///   └── next_seq (AtomicU64, lock-free count)
/// ```
pub struct EventLog {
    path: PathBuf,
    tx: mpsc::Sender<WriteCommand>,
    next_seq: Arc<AtomicU64>,
}

impl Drop for EventLog {
    fn drop(&mut self) {
        let _ = self.tx.try_send(WriteCommand::Shutdown);
    }
}

impl EventLog {
    /// Create a new event log file with an initial event.
    pub async fn create(
        path: PathBuf,
        initial_event: Event,
    ) -> Result<(Self, Event), StorageError> {
        let (state, stored_event) =
            run_blocking_io(move || create_at_path(path, initial_event)).await?;
        Ok((Self::from_writer_state(state), stored_event))
    }

    /// Open an existing event log.
    pub async fn open(path: PathBuf) -> Result<Self, StorageError> {
        let state = run_blocking_io(move || open_at_path(path)).await?;
        Ok(Self::from_writer_state(state))
    }

    fn from_writer_state(state: WriterState) -> Self {
        let path = state.path.clone();
        let next_seq = Arc::new(AtomicU64::new(state.next_seq));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let next_seq_clone = Arc::clone(&next_seq);
        let panic_path = state.path.clone();
        tokio::task::spawn_blocking(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_loop(rx, state, next_seq_clone);
            }));
            if let Err(e) = result {
                let msg: String = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic payload".to_string());
                tracing::error!(
                    path = %panic_path.display(),
                    panic = %msg,
                    "event log writer thread panicked; pending writes may be lost"
                );
            }
        });
        Self { path, tx, next_seq }
    }

    /// Append a durable event to the log and return it with its assigned seq.
    ///
    /// Sends the event to a dedicated writer thread via a bounded channel.
    /// The writer thread assigns `seq`, serializes, and writes the line —
    /// no mutex contention on the write path.
    /// Writes to the OS page cache immediately; call [`force_sync`] for fsync.
    pub async fn append(&self, event: Event) -> Result<Event, StorageError> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Append {
                event: Box::new(event),
                done,
            })
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer closed")))?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer dropped")))?
    }

    /// Append multiple events in a single writer-thread command.
    ///
    /// The writer thread assigns sequential `seq` numbers, serializes,
    /// and writes the pre-encoded batch with one file write/flush transaction.
    pub async fn append_batch(&self, events: Vec<Event>) -> Result<Vec<Event>, StorageError> {
        if events.is_empty() {
            return Ok(events);
        }
        let (done, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::AppendBatch { events, done })
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer closed")))?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer dropped")))?
    }

    /// Replay all events from the beginning.
    pub async fn replay_all(&self) -> Result<Vec<Event>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, None, None)).await
    }

    /// Replay events whose assigned seq is greater than `seq`.
    ///
    /// This is used when recovering from a snapshot: only the events that
    /// occurred after the snapshot point need to be replayed, not the whole log.
    pub async fn replay_after(&self, seq: u64) -> Result<Vec<Event>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, Some(seq), None)).await
    }

    /// Replay at most `max_events` events after `seq`, stopping the file scan
    /// once the limit is reached.
    pub async fn replay_after_limited(
        &self,
        seq: u64,
        max_events: usize,
    ) -> Result<Vec<Event>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, Some(seq), Some(max_events))).await
    }

    /// Count total events (lock-free read of the writer thread's seq counter).
    pub async fn count(&self) -> Result<usize, StorageError> {
        Ok(self.next_seq.load(Ordering::Acquire) as usize)
    }

    /// Force-fsync the event log if there are pending writes.
    ///
    /// Called at turn boundaries to ensure all events written since the last
    /// sync are durable (power-loss-safe). No-op if nothing is pending.
    pub async fn force_sync(&self) -> Result<(), StorageError> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::FlushSync { done })
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer closed")))?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer dropped")))?
    }

    /// Get the file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read only the first event from the log file.
    ///
    /// This is significantly faster than `replay_all()` for large logs
    /// because it stops after reading the first non-empty line.
    /// Useful for extracting session metadata (SessionStarted event)
    /// without replaying the entire history.
    pub async fn read_first_event(path: &Path) -> Result<Option<Event>, StorageError> {
        let path = path.to_path_buf();
        run_blocking_io(move || read_first_event_at_path(&path)).await
    }

    /// Read the first event, last event, and first user message from the log
    /// in a single pass. Returns `(first, last, first_user_message)`.
    pub async fn read_first_and_last(path: &Path) -> Result<EventLogEnds, StorageError> {
        let path = path.to_path_buf();
        run_blocking_io(move || read_first_and_last_at_path(&path)).await
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
                    let event = match parse_event_line(&self.path, self.line_number, trimmed) {
                        Ok(event) => event,
                        Err(error) => return Some(Err(error)),
                    };
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
                tool_policy: None,
                source_extension: None,
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

    #[test]
    fn partial_write_rollback_repositions_new_file_writer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollback.jsonl");
        let mut writer = WriterState {
            writer: File::create(&path).unwrap(),
            next_seq: 1,
            committed_len: 9,
            path: path.clone(),
            dirty: false,
            poisoned: None,
        };
        writer.writer.write_all(b"committedpartial").unwrap();

        writer.rollback_partial_write(9).unwrap();
        writer.writer.write_all(b"next").unwrap();
        writer.writer.flush().unwrap();

        assert_eq!(std::fs::read(path).unwrap(), b"committednext");
    }

    #[tokio::test]
    async fn event_log_writes_nested_payload_format() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested.jsonl");
        let (_log, _start) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        let first_line = content.lines().next().unwrap();
        let value: serde_json::Value = serde_json::from_str(first_line).unwrap();
        assert_eq!(value["session_id"], "s1");
        assert_eq!(value["payload"]["type"], "session_started");
        assert!(value.get("type").is_none());
        assert!(value.get("working_dir").is_none());
    }

    #[test]
    fn iterator_rejects_legacy_flat_event_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("flat.jsonl");
        std::fs::write(
            &path,
            r#"{"seq":0,"id":"event-1","session_id":"s1","timestamp":"2026-01-01T00:00:00Z","type":"turn_started"}"#,
        )
        .unwrap();

        let mut iter = EventLogIterator::new(&path.to_path_buf()).unwrap();
        let err = iter.next().unwrap().unwrap_err();
        assert!(matches!(
            err,
            StorageError::Io(io) if io.kind() == std::io::ErrorKind::InvalidData
        ));
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
    async fn open_discards_only_an_incomplete_trailing_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("interrupted-tail.jsonl");
        let mut start = make_start_event("s1");
        start.seq = Some(0);
        let committed = serde_json::to_string(&start).unwrap();
        std::fs::write(&path, format!("{committed}\n{{\"seq\":1")).unwrap();

        let log = EventLog::open(path.clone()).await.unwrap();
        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, Some(0));

        let appended = log
            .append(Event::new(
                "s1".into(),
                Some("turn-1".into()),
                EventPayload::TurnStarted,
            ))
            .await
            .unwrap();
        assert_eq!(appended.seq, Some(1));

        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[tokio::test]
    async fn append_batch_writes_multiple_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("batch.jsonl");
        let (log, start) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();

        assert_eq!(start.seq, Some(0));

        let stored = log
            .append_batch(vec![
                Event::new(
                    "s1".into(),
                    Some("turn-1".into()),
                    EventPayload::TurnStarted,
                ),
                Event::new(
                    "s1".into(),
                    Some("turn-1".into()),
                    EventPayload::TurnCompleted {
                        finish_reason: "stop".into(),
                    },
                ),
            ])
            .await
            .unwrap();

        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].seq, Some(1));
        assert_eq!(stored[1].seq, Some(2));

        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, Some(0));
        assert_eq!(events[1].seq, Some(1));
        assert_eq!(events[2].seq, Some(2));
    }

    #[tokio::test]
    async fn append_batch_empty_is_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        let (log, start) = EventLog::create(path.clone(), make_start_event("s1"))
            .await
            .unwrap();

        let stored = log.append_batch(vec![]).await.unwrap();
        assert!(stored.is_empty());

        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, Some(start.seq.unwrap()));
    }

    #[tokio::test]
    async fn drop_flushes_pending_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("drop.jsonl");
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
        // append() already flushed to OS page cache; data is readable before Drop.
        drop(log);

        let reopened = EventLog::open(path).await.unwrap();
        let events = reopened.replay_all().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].seq, Some(1));
    }

    #[tokio::test]
    async fn read_first_and_last_rejects_malformed_jsonl_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt-summary.jsonl");
        let valid = serde_json::to_string(&make_start_event("s1")).unwrap();
        std::fs::write(&path, format!("{valid}\nnot-json\n")).unwrap();
        let err = EventLog::read_first_and_last(&path).await.unwrap_err();
        assert!(matches!(
            err,
            StorageError::Io(io) if io.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[tokio::test]
    async fn readers_ignore_uncommitted_trailing_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial-summary.jsonl");
        let mut start = make_start_event("s1");
        start.seq = Some(0);
        let committed = serde_json::to_string(&start).unwrap();
        std::fs::write(&path, format!("{committed}\n\n{{\"seq\":1")).unwrap();

        let (first, last, _) = EventLog::read_first_and_last(&path).await.unwrap();
        assert_eq!(first.and_then(|event| event.seq), Some(0));
        assert_eq!(last.and_then(|event| event.seq), Some(0));

        let replayed = replay_events_at_path(&path, None, None).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].seq, Some(0));
    }

    #[test]
    fn iterator_rejects_malformed_jsonl_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.jsonl");
        let valid = serde_json::to_string(&make_start_event("s1")).unwrap();
        std::fs::write(&path, format!("{valid}\nnot-json\n")).unwrap();
        let mut iter = EventLogIterator::new(&path.to_path_buf()).unwrap();
        assert!(iter.next().unwrap().is_ok());
        let err = iter.next().unwrap().unwrap_err();
        assert!(matches!(
            err,
            StorageError::Io(io) if io.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn iterator_malformed_line_table() {
        let cases = [
            "{",
            "{\"session_id\":",
            "[]",
            "null",
            "{\"not\":\"an_event\"}",
        ];
        for (idx, line) in cases.iter().enumerate() {
            let dir = tempdir().unwrap();
            let path = dir.path().join(format!("bad-{idx}.jsonl"));
            std::fs::write(&path, format!("{line}\n")).unwrap();
            let mut iter = EventLogIterator::new(&path.to_path_buf()).unwrap();
            let err = iter.next().unwrap().unwrap_err();
            assert!(
                matches!(err, StorageError::Io(io) if io.kind() == std::io::ErrorKind::InvalidData),
                "case {idx} should be InvalidData"
            );
        }
    }
}

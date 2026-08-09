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

use astrcode_core::event::{DurableEvent, DurableEventPayload, StoredEvent};
use astrcode_session_projection::{SessionSummary, SessionSummaryProjection};
use tokio::sync::{mpsc, oneshot};

use crate::StorageError;

/// `(first_event, last_event)` from a single log scan.
type EventLogEnds = (Option<StoredEvent>, Option<StoredEvent>);

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

fn validate_event(
    event: &StoredEvent,
    line_number: usize,
    path: &Path,
) -> Result<(), StorageError> {
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

#[derive(Default)]
struct EventStreamValidator {
    session_id: Option<astrcode_core::types::SessionId>,
    next_seq: u64,
}

impl EventStreamValidator {
    fn observe(
        &mut self,
        event: &StoredEvent,
        line_number: usize,
        path: &Path,
    ) -> Result<(), StorageError> {
        if event.seq != self.next_seq {
            return Err(corrupt_log(
                path,
                line_number,
                format!("expected seq {}, got {}", self.next_seq, event.seq),
            ));
        }

        match &self.session_id {
            None => {
                if event.turn_id.is_some() {
                    return Err(corrupt_log(
                        path,
                        line_number,
                        "SessionStarted must be a session-level event",
                    ));
                }
                if !matches!(event.payload, DurableEventPayload::SessionStarted(_)) {
                    return Err(corrupt_log(
                        path,
                        line_number,
                        "first event must be SessionStarted",
                    ));
                }
                self.session_id = Some(event.session_id.clone());
            },
            Some(session_id) => {
                if &event.session_id != session_id {
                    return Err(corrupt_log(
                        path,
                        line_number,
                        format!(
                            "event belongs to session {}, expected {}",
                            event.session_id, session_id
                        ),
                    ));
                }
                if matches!(event.payload, DurableEventPayload::SessionStarted(_)) {
                    return Err(corrupt_log(
                        path,
                        line_number,
                        "SessionStarted may only appear at seq 0",
                    ));
                }
            },
        }

        self.next_seq += 1;
        Ok(())
    }

    fn finish(&self, path: &Path) -> Result<(), StorageError> {
        if self.session_id.is_none() {
            return Err(StorageError::CorruptLog(format!(
                "{} is empty",
                path.display()
            )));
        }
        Ok(())
    }
}

fn corrupt_log(path: &Path, line_number: usize, message: impl Into<String>) -> StorageError {
    StorageError::CorruptLog(format!(
        "{}:{}: {}",
        path.display(),
        line_number,
        message.into()
    ))
}

fn parse_event_line(
    path: &Path,
    line_number: usize,
    line: &str,
) -> Result<StoredEvent, StorageError> {
    let trimmed = line.trim();
    let event = serde_json::from_str::<StoredEvent>(trimmed).map_err(|error| {
        let preview = if trimmed.len() > 100 {
            let end = trimmed.floor_char_boundary(100);
            format!("{}...", &trimmed[..end])
        } else {
            trimmed.to_string()
        };
        StorageError::CorruptLog(format!(
            "failed to parse event at {}:{} (content: '{}'): {}",
            path.display(),
            line_number,
            preview,
            error
        ))
    })?;
    validate_event(&event, line_number, path)?;
    Ok(event)
}

fn scan_events_at_path(
    path: &Path,
    mut visit: impl FnMut(StoredEvent) -> Result<bool, StorageError>,
) -> Result<(), StorageError> {
    let file = File::open(path).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(path, e)))
    })?;
    let mut reader = BufReader::new(file);
    let mut validator = EventStreamValidator::default();
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
                "ignored incomplete trailing event log record while scanning"
            );
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        line_number += 1;
        let event = parse_event_line(path, line_number, &line)?;
        validator.observe(&event, line_number, path)?;
        if !visit(event)? {
            return Ok(());
        }
    }
    validator.finish(path)?;
    Ok(())
}

fn replay_events_at_path(
    path: &Path,
    after_seq: Option<u64>,
    max_events: Option<usize>,
) -> Result<Vec<StoredEvent>, StorageError> {
    if max_events == Some(0) {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    scan_events_at_path(path, |event| {
        if after_seq.is_none_or(|seq| event.seq > seq) {
            events.push(event);
        }
        Ok(!max_events.is_some_and(|limit| events.len() >= limit))
    })?;
    Ok(events)
}

fn read_first_and_last_at_path(path: &Path) -> Result<EventLogEnds, StorageError> {
    if !path.exists() {
        return Ok((None, None));
    }
    let mut first: Option<StoredEvent> = None;
    let mut last: Option<StoredEvent> = None;
    scan_events_at_path(path, |event| {
        if first.is_none() {
            first = Some(event.clone());
        }
        last = Some(event);
        Ok(true)
    })?;
    Ok((first, last))
}

fn read_summary_at_path(
    path: &Path,
    session_id: astrcode_core::types::SessionId,
) -> Result<Option<SessionSummary>, StorageError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut projection = SessionSummaryProjection::new(session_id);
    scan_events_at_path(path, |event| {
        projection
            .apply(&event)
            .map_err(|error| StorageError::CorruptLog(format!("{}: {error}", path.display())))?;
        Ok(true)
    })?;
    projection
        .snapshot()
        .map(Some)
        .map_err(|error| StorageError::CorruptLog(format!("{}: {error}", path.display())))
}

// ── Write-side commands ───────────────────────────────────────────────────────

const CHANNEL_CAPACITY: usize = 1024;

enum WriteCommand {
    AppendBatch {
        events: Vec<DurableEvent>,
        done: oneshot::Sender<Result<Vec<StoredEvent>, StorageError>>,
    },
    FlushSync {
        done: oneshot::Sender<Result<(), StorageError>>,
    },
    Shutdown,
}

struct WriterState {
    writer: File,
    session_id: astrcode_core::types::SessionId,
    next_seq: u64,
    committed_len: u64,
    path: PathBuf,
    dirty: bool,
    poisoned: Option<String>,
}

impl WriterState {
    fn open_append(
        path: PathBuf,
        session_id: astrcode_core::types::SessionId,
        next_seq: u64,
    ) -> Result<Self, StorageError> {
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
            session_id,
            next_seq,
            committed_len,
            path,
            dirty: false,
            poisoned: None,
        })
    }

    fn append_batch(
        &mut self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        if events.is_empty() {
            return Err(StorageError::InvalidEvent(
                "event log batch cannot be empty".into(),
            ));
        }

        let mut stored_events = Vec::with_capacity(events.len());
        let mut encoded = Vec::new();
        let mut next_seq = self.next_seq;
        for event in events {
            if event.session_id != self.session_id {
                return Err(StorageError::InvalidEvent(format!(
                    "cannot append event for session {} to log for {}",
                    event.session_id, self.session_id
                )));
            }
            if matches!(event.payload, DurableEventPayload::SessionStarted(_)) {
                return Err(StorageError::InvalidEvent(
                    "SessionStarted may only be written while creating a log".into(),
                ));
            }
            let stored = StoredEvent::new(next_seq, event);
            serde_json::to_writer(&mut encoded, &stored)?;
            encoded.push(b'\n');
            stored_events.push(stored);
            next_seq = next_seq.checked_add(1).ok_or_else(|| {
                StorageError::CorruptLog("session event sequence overflow".into())
            })?;
        }

        self.write_committed_record(&encoded)?;
        self.next_seq = next_seq;
        self.dirty = true;
        Ok(stored_events)
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
            WriteCommand::AppendBatch { events, done } => {
                let result = state.append_batch(events);
                if result.is_ok() {
                    next_seq.store(state.next_seq, Ordering::Release);
                }
                let _ = done.send(result);
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
    initial_event: DurableEvent,
) -> Result<(WriterState, StoredEvent), StorageError> {
    if initial_event.turn_id.is_some()
        || !matches!(
            initial_event.payload,
            DurableEventPayload::SessionStarted(_)
        )
    {
        return Err(StorageError::InvalidEvent(
            "event log creation requires a session-level SessionStarted event".into(),
        ));
    }
    let session_id = initial_event.session_id.clone();
    let stored_event = StoredEvent::new(0, initial_event);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(&path, e)))
        })?;
    let mut writer = file;
    let mut encoded = serde_json::to_vec(&stored_event)?;
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
            session_id,
            next_seq: 1,
            committed_len: encoded.len() as u64,
            path,
            dirty: false,
            poisoned: None,
        },
        stored_event,
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
    let (first, last) = read_first_and_last_at_path(&path)?;
    let first = first
        .as_ref()
        .ok_or_else(|| StorageError::CorruptLog(format!("{} is empty", path.display())))?;
    let next_seq = last
        .and_then(|event| event.seq.checked_add(1))
        .ok_or_else(|| StorageError::CorruptLog("session event sequence overflow".into()))?;
    WriterState::open_append(path, first.session_id.clone(), next_seq)
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
        initial_event: DurableEvent,
    ) -> Result<(Self, StoredEvent), StorageError> {
        let (state, stored_event) =
            run_blocking_io(move || create_at_path(path, initial_event)).await?;
        Ok((Self::from_writer_state(state), stored_event))
    }

    /// Open an existing event log.
    pub async fn open(path: PathBuf) -> Result<Self, StorageError> {
        let state = run_blocking_io(move || open_at_path(path)).await?;
        Ok(Self::from_writer_state(state))
    }

    pub(crate) async fn replay_read_only(path: PathBuf) -> Result<Vec<StoredEvent>, StorageError> {
        run_blocking_io(move || replay_events_at_path(&path, None, None)).await
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
    pub async fn append(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        self.append_batch(vec![event])
            .await?
            .pop()
            .ok_or_else(crate::error::short_batch_result)
    }

    /// Append a prevalidated batch as one recoverable file write.
    pub async fn append_batch(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::AppendBatch { events, done })
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer closed")))?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer dropped")))?
    }

    /// Replay all events from the beginning.
    pub async fn replay_all(&self) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, None, None)).await
    }

    /// Replay events whose assigned seq is greater than `seq`.
    ///
    /// This is used when recovering from a snapshot: only the events that
    /// occurred after the snapshot point need to be replayed, not the whole log.
    pub async fn replay_after(&self, seq: u64) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, Some(seq), None)).await
    }

    /// Replay at most `max_events` events after `seq`, stopping the file scan
    /// once the limit is reached.
    pub(crate) async fn replay_after_limited(
        &self,
        seq: u64,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, Some(seq), Some(max_events))).await
    }

    /// Count total events (lock-free read of the writer thread's seq counter).
    pub(crate) async fn count(&self) -> Result<usize, StorageError> {
        Ok(self.next_seq.load(Ordering::Acquire) as usize)
    }

    /// Force-fsync the event log if there are pending writes.
    ///
    /// Called at turn boundaries to ensure all events written since the last
    /// sync are durable (power-loss-safe). No-op if nothing is pending.
    pub(crate) async fn force_sync(&self) -> Result<(), StorageError> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::FlushSync { done })
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer closed")))?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer dropped")))?
    }

    /// Project a session-list summary directly from an event log.
    pub(crate) async fn read_summary(
        path: &Path,
        session_id: astrcode_core::types::SessionId,
    ) -> Result<Option<SessionSummary>, StorageError> {
        let path = path.to_path_buf();
        run_blocking_io(move || read_summary_at_path(&path, session_id)).await
    }
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

#[cfg(test)]
mod tests;

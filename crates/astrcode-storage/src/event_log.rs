//! 追加式 JSONL 事件日志，用于会话持久化。
//!
//! 每个会话对应一个事件日志文件，事件以换行分隔的 JSON 对象写入，
//! 写入后不可修改。存储层在追加时分配单调递增的 `seq` 序号。

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use astrcode_core::event::{DurableEvent, DurableEventPayload, StoredEvent};
use astrcode_session_projection::{
    PreparedProjectionBatch, SessionSummary, SessionSummaryProjection,
};
use tokio::sync::{mpsc, oneshot};

use crate::StorageError;

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
            format!("{}...", crate::traits::truncate_utf8(trimmed, 100))
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
    max_bytes: Option<u64>,
    mut visit: impl FnMut(StoredEvent) -> Result<bool, StorageError>,
) -> Result<(), StorageError> {
    let file = File::open(path).map_err(|e| {
        StorageError::Io(std::io::Error::new(e.kind(), enhance_open_error(path, e)))
    })?;
    let mut reader = BufReader::new(file.take(max_bytes.unwrap_or(u64::MAX)));
    let mut validator = EventStreamValidator::default();
    let mut line_number = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
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
    max_bytes: Option<u64>,
    after_seq: Option<u64>,
    max_events: Option<usize>,
) -> Result<Vec<StoredEvent>, StorageError> {
    if max_events == Some(0) {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    scan_events_at_path(path, max_bytes, |event| {
        if after_seq.is_none_or(|seq| event.seq > seq) {
            events.push(event);
        }
        Ok(!max_events.is_some_and(|limit| events.len() >= limit))
    })?;
    Ok(events)
}

fn replay_events_before_at_path(
    path: &Path,
    before_seq: Option<u64>,
    max_events: usize,
) -> Result<Vec<StoredEvent>, StorageError> {
    if max_events == 0 {
        return Ok(Vec::new());
    }
    const REVERSE_SCAN_CHUNK_BYTES: u64 = 64 * 1024;

    let mut file = File::open(path).map_err(|error| {
        StorageError::Io(std::io::Error::new(
            error.kind(),
            enhance_open_error(path, error),
        ))
    })?;
    let file_len = file.metadata().map_err(StorageError::Io)?.len();
    let Some(mut position) = last_committed_offset(&mut file, file_len)? else {
        return Ok(Vec::new());
    };
    let mut leading_fragment = Vec::new();
    let mut events = Vec::with_capacity(max_events);
    let mut newer_seq = None;
    let mut session_id = None;

    while position > 0 && events.len() < max_events {
        let start = position.saturating_sub(REVERSE_SCAN_CHUNK_BYTES);
        let mut chunk = vec![0; (position - start) as usize];
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut chunk))
            .map_err(StorageError::Io)?;
        chunk.extend_from_slice(&leading_fragment);

        let (complete, next_fragment) = if start == 0 {
            (chunk.as_slice(), Vec::new())
        } else if let Some(first_newline) = chunk.iter().position(|byte| *byte == b'\n') {
            (&chunk[first_newline + 1..], chunk[..first_newline].to_vec())
        } else {
            (&[][..], chunk)
        };

        for line in complete.rsplit(|byte| *byte == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let line = std::str::from_utf8(line).map_err(|error| {
                StorageError::CorruptLog(format!(
                    "event at {} is not valid UTF-8: {error}",
                    path.display()
                ))
            })?;
            let event = parse_event_line(path, 0, line)?;
            if let Some(expected_newer) = newer_seq
                && event.seq.checked_add(1) != Some(expected_newer)
            {
                return Err(corrupt_log(
                    path,
                    0,
                    format!(
                        "expected reverse seq {}, got {}",
                        expected_newer.saturating_sub(1),
                        event.seq
                    ),
                ));
            }
            if let Some(expected_session) = &session_id {
                if &event.session_id != expected_session {
                    return Err(corrupt_log(
                        path,
                        0,
                        format!(
                            "event belongs to session {}, expected {}",
                            event.session_id, expected_session
                        ),
                    ));
                }
            } else {
                session_id = Some(event.session_id.clone());
            }
            newer_seq = Some(event.seq);

            if before_seq.is_none_or(|before| event.seq < before) {
                events.push(event);
                if events.len() == max_events {
                    break;
                }
            }
        }
        leading_fragment = next_fragment;
        position = start;
    }

    events.reverse();
    Ok(events)
}

fn last_committed_offset(file: &mut File, file_len: u64) -> Result<Option<u64>, StorageError> {
    const TAIL_SCAN_BYTES: u64 = 8 * 1024;
    let mut position = file_len;
    while position > 0 {
        let start = position.saturating_sub(TAIL_SCAN_BYTES);
        let mut chunk = vec![0; (position - start) as usize];
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut chunk))
            .map_err(StorageError::Io)?;
        if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
            return Ok(Some(start + newline as u64 + 1));
        }
        position = start;
    }
    Ok(None)
}

fn read_summary_at_path(
    path: &Path,
    session_id: astrcode_core::types::SessionId,
    max_bytes: Option<u64>,
) -> Result<Option<SessionSummary>, StorageError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut projection = SessionSummaryProjection::new(session_id);
    scan_events_at_path(path, max_bytes, |event| {
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

#[cfg(test)]
static FAIL_NEXT_OPEN_SYNC_PATHS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::OnceLock::new();

enum WriteCommand {
    #[cfg(test)]
    AppendBatch {
        events: Vec<DurableEvent>,
        done: oneshot::Sender<Result<Vec<StoredEvent>, StorageError>>,
    },
    AppendPreparedBatch {
        batch: PreparedProjectionBatch,
        done: oneshot::Sender<Result<PreparedProjectionBatch, StorageError>>,
    },
    FlushSync {
        done: oneshot::Sender<Result<(), StorageError>>,
    },
    #[cfg(any(test, feature = "testing"))]
    FailNextSync {
        done: oneshot::Sender<()>,
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
    #[cfg(any(test, feature = "testing"))]
    fail_next_sync: bool,
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
            #[cfg(any(test, feature = "testing"))]
            fail_next_sync: false,
        })
    }

    #[cfg(test)]
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
        let mut next_seq = self.next_seq;
        for event in events {
            stored_events.push(StoredEvent::new(next_seq, event));
            next_seq = next_seq.checked_add(1).ok_or_else(|| {
                StorageError::CorruptLog("session event sequence overflow".into())
            })?;
        }

        self.append_stored_batch(&stored_events)?;
        Ok(stored_events)
    }

    fn append_stored_batch(&mut self, events: &[StoredEvent]) -> Result<(), StorageError> {
        if events.is_empty() {
            return Err(StorageError::InvalidEvent(
                "event log batch cannot be empty".into(),
            ));
        }

        let mut encoded = Vec::new();
        let mut next_seq = self.next_seq;
        for event in events {
            if event.seq != next_seq {
                return Err(StorageError::InvalidEvent(format!(
                    "event log expected seq {next_seq}, got {}",
                    event.seq
                )));
            }
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
            serde_json::to_writer(&mut encoded, event)?;
            encoded.push(b'\n');
            next_seq = next_seq.checked_add(1).ok_or_else(|| {
                StorageError::CorruptLog("session event sequence overflow".into())
            })?;
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
        #[cfg(any(test, feature = "testing"))]
        if std::mem::take(&mut self.fail_next_sync) {
            return Err(StorageError::Io(std::io::Error::other(
                "injected fsync failure",
            )));
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

fn apply_write_command(cmd: WriteCommand, state: &mut WriterState, next_seq: &AtomicU64) -> bool {
    match cmd {
        #[cfg(test)]
        WriteCommand::AppendBatch { events, done } => {
            let result = state.append_batch(events);
            if result.is_ok() {
                next_seq.store(state.next_seq, Ordering::Release);
            }
            let _ = done.send(result);
        },
        WriteCommand::AppendPreparedBatch { batch, done } => {
            let result = state.append_stored_batch(batch.events()).map(|()| batch);
            if result.is_ok() {
                next_seq.store(state.next_seq, Ordering::Release);
            }
            let _ = done.send(result);
        },
        WriteCommand::FlushSync { done } => {
            let _ = done.send(state.flush_and_sync());
        },
        #[cfg(any(test, feature = "testing"))]
        WriteCommand::FailNextSync { done } => {
            state.fail_next_sync = true;
            let _ = done.send(());
        },
        WriteCommand::Shutdown => return true,
    }
    false
}

async fn write_loop(
    mut rx: mpsc::Receiver<WriteCommand>,
    mut state: WriterState,
    next_seq: Arc<AtomicU64>,
) {
    while let Some(cmd) = rx.recv().await {
        let path = state.path.clone();
        let next_seq = Arc::clone(&next_seq);
        let operation = tokio::task::spawn_blocking(move || {
            let shutdown = apply_write_command(cmd, &mut state, &next_seq);
            (state, shutdown)
        });
        match operation.await {
            Ok((next_state, shutdown)) => {
                state = next_state;
                if shutdown {
                    break;
                }
            },
            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    %error,
                    "event log writer task failed; pending writes may be lost"
                );
                return;
            },
        }
    }

    let path = state.path.clone();
    match tokio::task::spawn_blocking(move || state.flush_and_sync()).await {
        Ok(Ok(())) => {},
        Ok(Err(error)) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to flush event log on writer task shutdown"
            );
        },
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "event log shutdown task failed"
            );
        },
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
            #[cfg(any(test, feature = "testing"))]
            fail_next_sync: false,
        },
        stored_event,
    ))
}

fn open_at_path(path: PathBuf) -> Result<(WriterState, Vec<StoredEvent>), StorageError> {
    if !path.exists() {
        return Err(std::io::Error::new(
            ErrorKind::NotFound,
            format!("Event log not found: {}", path.display()),
        )
        .into());
    }
    recover_incomplete_tail(&path)?;
    let confirmed_len = sync_existing_log(&path)?;
    // 冷打开只扫一遍文件:校验事件流的同时把事件交给调用方做 projection 恢复,
    // 避免 open 与 replay 各自全量读一次。
    let events = replay_events_at_path(&path, Some(confirmed_len), None, None)?;
    let first = events
        .first()
        .ok_or_else(|| StorageError::CorruptLog(format!("{} is empty", path.display())))?;
    let next_seq = events
        .last()
        .and_then(|event| event.seq.checked_add(1))
        .ok_or_else(|| StorageError::CorruptLog("session event sequence overflow".into()))?;
    let state = WriterState::open_append(path, first.session_id.clone(), next_seq)?;
    Ok((state, events))
}

/// A previous process may have observed an ambiguous fsync result. Existing records are not
/// eligible for replay until the file has been durably confirmed in this process.
fn sync_existing_log(path: &Path) -> Result<u64, StorageError> {
    #[cfg(test)]
    if take_fail_next_open_sync(path) {
        return Err(StorageError::Io(std::io::Error::other(
            "injected fsync failure",
        )));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            StorageError::Io(std::io::Error::new(
                error.kind(),
                enhance_open_error(path, error),
            ))
        })?;
    let confirmed_len = file.metadata().map_err(StorageError::Io)?.len();
    file.sync_all().map_err(|error| {
        StorageError::Io(std::io::Error::new(
            error.kind(),
            enhance_sync_error(path, error),
        ))
    })?;
    Ok(confirmed_len)
}

#[cfg(test)]
fn take_fail_next_open_sync(path: &Path) -> bool {
    FAIL_NEXT_OPEN_SYNC_PATHS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(path)
}

#[cfg(test)]
fn fail_next_open_sync(path: PathBuf) {
    FAIL_NEXT_OPEN_SYNC_PATHS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path);
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

/// An append-only JSONL event log backed by an asynchronous writer actor.
///
/// Each session has one event log file. Events are written as newline-delimited
/// JSON objects and never modified. Storage assigns `seq` at append time.
///
/// # Architecture
///
/// ```text
/// EventLog
///   ├── tx (bounded channel, 1024 capacity)
///   │     └── write_loop (idle without occupying a thread)
///   │           └── shared blocking pool (one operation at a time)
///   │                 ├── File (pre-encoded atomic batches)
///   │                 └── dirty tracking (deferred fsync)
///   └── next_seq (AtomicU64, lock-free count)
/// ```
pub(crate) struct EventLog {
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
    pub(crate) async fn create(
        path: PathBuf,
        initial_event: DurableEvent,
    ) -> Result<(Self, StoredEvent), StorageError> {
        let (state, stored_event) =
            run_blocking_io(move || create_at_path(path, initial_event)).await?;
        Ok((Self::from_writer_state(state), stored_event))
    }

    /// Open an existing event log, returning the events validated during the scan.
    ///
    /// 调用方(冷打开的 projection 恢复)直接使用这批事件,不再二次读盘。
    pub(crate) async fn open(path: PathBuf) -> Result<(Self, Vec<StoredEvent>), StorageError> {
        let (state, events) = run_blocking_io(move || open_at_path(path)).await?;
        Ok((Self::from_writer_state(state), events))
    }

    pub(crate) async fn replay_read_only(path: PathBuf) -> Result<Vec<StoredEvent>, StorageError> {
        run_blocking_io(move || {
            let confirmed_len = sync_existing_log(&path)?;
            replay_events_at_path(&path, Some(confirmed_len), None, None)
        })
        .await
    }

    fn from_writer_state(state: WriterState) -> Self {
        let path = state.path.clone();
        let next_seq = Arc::new(AtomicU64::new(state.next_seq));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let next_seq_clone = Arc::clone(&next_seq);
        tokio::spawn(write_loop(rx, state, next_seq_clone));
        Self { path, tx, next_seq }
    }

    /// Append a durable event to the log and return it with its assigned seq.
    ///
    /// Sends the event to the per-log writer actor via a bounded channel.
    /// The actor assigns `seq`, serializes, and writes the line on the shared blocking pool —
    /// no mutex contention on the write path.
    /// Writes to the OS page cache immediately; call [`force_sync`] for fsync.
    #[cfg(test)]
    async fn append(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        self.append_batch(vec![event])
            .await?
            .pop()
            .ok_or_else(crate::error::short_batch_result)
    }

    /// Append a prevalidated batch as one recoverable file write.
    #[cfg(test)]
    async fn append_batch(
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

    /// Append a projection-validated batch without rebuilding or cloning its event payloads.
    pub(crate) async fn append_prepared_batch(
        &self,
        batch: PreparedProjectionBatch,
    ) -> Result<PreparedProjectionBatch, StorageError> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::AppendPreparedBatch { batch, done })
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer closed")))?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer dropped")))?
    }

    /// Replay all events from the beginning.
    pub(crate) async fn replay_all(&self) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, None, None, None)).await
    }

    /// Replay at most `max_events` events from the beginning of the log.
    pub(crate) async fn replay_from_start_limited(
        &self,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, None, None, Some(max_events))).await
    }

    /// Replay the newest events before an optional exclusive sequence cursor.
    pub(crate) async fn replay_before_limited(
        &self,
        before_seq: Option<u64>,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_before_at_path(&path, before_seq, max_events)).await
    }

    /// Replay events whose assigned seq is greater than `seq`.
    ///
    /// This is used when recovering from a snapshot: only the events that
    /// occurred after the snapshot point need to be replayed, not the whole log.
    pub(crate) async fn replay_after(&self, seq: u64) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, None, Some(seq), None)).await
    }

    /// Replay at most `max_events` events after `seq`, stopping the file scan
    /// once the limit is reached.
    pub(crate) async fn replay_after_limited(
        &self,
        seq: u64,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let path = self.path.clone();
        run_blocking_io(move || replay_events_at_path(&path, None, Some(seq), Some(max_events)))
            .await
    }

    /// Count total events (lock-free read of the writer thread's seq counter).
    pub(crate) fn count(&self) -> u64 {
        self.next_seq.load(Ordering::Acquire)
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

    #[cfg(any(test, feature = "testing"))]
    pub(crate) async fn fail_next_sync(&self) -> Result<(), StorageError> {
        let (done, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::FailNextSync { done })
            .await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer closed")))?;
        rx.await
            .map_err(|_| StorageError::Io(std::io::Error::other("event log writer dropped")))
    }

    #[cfg(test)]
    pub(crate) fn fail_next_open_sync_for_testing(path: PathBuf) {
        fail_next_open_sync(path);
    }

    /// Project a session-list summary directly from an event log.
    pub(crate) async fn read_summary(
        path: &Path,
        session_id: astrcode_core::types::SessionId,
    ) -> Result<Option<SessionSummary>, StorageError> {
        let path = path.to_path_buf();
        run_blocking_io(move || {
            let confirmed_len = sync_existing_log(&path)?;
            read_summary_at_path(&path, session_id, Some(confirmed_len))
        })
        .await
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

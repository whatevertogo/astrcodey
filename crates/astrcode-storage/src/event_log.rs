//! Append-only JSONL event log for session persistence.

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::Mutex,
};

use astrcode_core::{event::Event, storage::StorageError};

/// An append-only JSONL event log.
///
/// Each session has one event log file. Events are written as newline-delimited
/// flat JSON objects and never modified. Storage assigns `seq` at append time.
pub struct EventLog {
    path: PathBuf,
    next_seq: Mutex<u64>,
}

impl EventLog {
    /// Create a new event log file with an initial event.
    pub async fn create(
        path: PathBuf,
        initial_event: Event,
    ) -> Result<(Self, Event), StorageError> {
        let mut event = initial_event;
        event.seq = Some(0);

        let mut file = std::fs::File::create(&path)?;
        let line = serde_json::to_string(&event)?;
        writeln!(file, "{}", line)?;
        Ok((
            Self {
                path,
                next_seq: Mutex::new(1),
            },
            event,
        ))
    }

    /// Open an existing event log.
    pub async fn open(path: PathBuf) -> Result<Self, StorageError> {
        if !path.exists() {
            return Err(StorageError::NotFound(format!(
                "Event log not found: {}",
                path.display()
            )));
        }
        let next_seq = count_lines(&path)? as u64;
        Ok(Self {
            path,
            next_seq: Mutex::new(next_seq),
        })
    }

    /// Append a durable event to the log and return it with its assigned seq.
    pub async fn append(&self, mut event: Event) -> Result<Event, StorageError> {
        let mut next_seq = self
            .next_seq
            .lock()
            .map_err(|_| StorageError::LockError("event log sequence lock poisoned".into()))?;
        let seq = *next_seq;
        event.seq = Some(seq);

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(&event)?;
        writeln!(file, "{}", line)?;
        *next_seq += 1;
        Ok(event)
    }

    /// Replay all events from the beginning.
    pub async fn replay_all(&self) -> Result<Vec<Event>, StorageError> {
        let file = std::fs::File::open(&self.path)?;
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

    /// Count total events.
    pub async fn count(&self) -> Result<usize, StorageError> {
        let next_seq = self
            .next_seq
            .lock()
            .map_err(|_| StorageError::LockError("event log sequence lock poisoned".into()))?;
        Ok(*next_seq as usize)
    }

    /// Get the file path.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

fn count_lines(path: &PathBuf) -> Result<usize, StorageError> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    Ok(reader.lines().count())
}

/// Streaming iterator over event log lines.
pub struct EventLogIterator {
    reader: BufReader<std::fs::File>,
    line_number: usize,
}

impl EventLogIterator {
    pub fn new(path: &PathBuf) -> Result<Self, StorageError> {
        let file = std::fs::File::open(path)?;
        Ok(Self {
            reader: BufReader::new(file),
            line_number: 0,
        })
    }
}

impl Iterator for EventLogIterator {
    type Item = Result<(usize, Event), StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => {
                self.line_number += 1;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return self.next();
                }
                match serde_json::from_str::<Event>(trimmed) {
                    Ok(event) => Some(Ok((self.line_number, event))),
                    Err(e) => Some(Err(StorageError::Serialization(e))),
                }
            },
            Err(e) => Some(Err(StorageError::Io(e))),
        }
    }
}

/// Batch appender for write efficiency.
///
/// Buffers append requests and flushes them in batches within a configurable
/// time window. The window is currently consumed by higher-level schedulers.
pub struct BatchAppender {
    log: EventLog,
    buffer: Vec<Event>,
    flush_window_ms: u64,
}

impl BatchAppender {
    pub fn new(log: EventLog, flush_window_ms: u64) -> Self {
        Self {
            log,
            buffer: Vec::new(),
            flush_window_ms,
        }
    }

    pub async fn push(&mut self, event: Event) -> Result<(), StorageError> {
        self.buffer.push(event);
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<usize, StorageError> {
        if self.buffer.is_empty() {
            return Ok(0);
        }

        let count = self.buffer.len();
        let mut next_seq = self
            .log
            .next_seq
            .lock()
            .map_err(|_| StorageError::LockError("event log sequence lock poisoned".into()))?;
        let mut seq = *next_seq;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log.path())?;
        for event in &mut self.buffer {
            event.seq = Some(seq);
            seq += 1;
            let line = serde_json::to_string(event)?;
            writeln!(file, "{}", line)?;
        }
        *next_seq = seq;
        self.buffer.clear();
        Ok(count)
    }

    pub fn flush_window_ms(&self) -> u64 {
        self.flush_window_ms
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
}

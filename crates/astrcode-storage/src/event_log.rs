//! Append-only JSONL event log for session persistence.

use astrcode_core::storage::{SessionEvent, StorageError};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// An append-only JSONL event log.
///
/// Each session has one event log file. Events are written as newline-delimited
/// JSON objects and never modified. Recovery replays from the beginning or
/// from a snapshot cursor.
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    /// Create a new event log file with an initial event.
    pub async fn create(path: PathBuf, initial_event: &SessionEvent) -> Result<Self, StorageError> {
        let mut file = std::fs::File::create(&path)?;
        let line = serde_json::to_string(initial_event)?;
        writeln!(file, "{}", line)?;
        Ok(Self { path })
    }

    /// Open an existing event log.
    pub async fn open(path: PathBuf) -> Result<Self, StorageError> {
        if !path.exists() {
            return Err(StorageError::NotFound(format!(
                "Event log not found: {}",
                path.display()
            )));
        }
        Ok(Self { path })
    }

    /// Append an event to the log (atomic single-line write).
    pub async fn append(&self, event: &SessionEvent) -> Result<(), StorageError> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(event)?;
        writeln!(file, "{}", line)?;
        Ok(())
    }

    /// Replay all events from the beginning.
    pub async fn replay_all(&self) -> Result<Vec<SessionEvent>, StorageError> {
        let file = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let event: SessionEvent = serde_json::from_str(&line)?;
            events.push(event);
        }
        Ok(events)
    }

    /// Count total events.
    pub async fn count(&self) -> Result<usize, StorageError> {
        let file = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        Ok(reader.lines().count())
    }

    /// Get the file path.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
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
    type Item = Result<(usize, SessionEvent), StorageError>;

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
                match serde_json::from_str::<SessionEvent>(trimmed) {
                    Ok(event) => Some(Ok((self.line_number, event))),
                    Err(e) => Some(Err(StorageError::Serialization(e))),
                }
            }
            Err(e) => Some(Err(StorageError::Io(e))),
        }
    }
}

/// Batch appender for write efficiency.
///
/// Buffers concurrent append requests and flushes them in batches
/// within a configurable time window.
pub struct BatchAppender {
    log: EventLog,
    buffer: Vec<SessionEvent>,
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

    pub async fn push(&mut self, event: SessionEvent) -> Result<(), StorageError> {
        self.buffer.push(event);
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<usize, StorageError> {
        if self.buffer.is_empty() {
            return Ok(0);
        }
        let count = self.buffer.len();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log.path())?;
        for event in &self.buffer {
            let line = serde_json::to_string(event)?;
            writeln!(file, "{}", line)?;
        }
        self.buffer.clear();
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrcode_core::storage::SessionEvent;
    use chrono::Utc;
    use tempfile::tempdir;

    fn make_start_event(id: &str) -> SessionEvent {
        SessionEvent::SessionStart {
            session_id: id.into(),
            timestamp: Utc::now(),
            working_dir: "/tmp".into(),
            model_id: "test-model".into(),
        }
    }

    #[tokio::test]
    async fn test_create_and_replay() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let log = EventLog::create(path.clone(), &make_start_event("s1"))
            .await
            .unwrap();
        log.append(&make_start_event("s2")).await.unwrap();

        let events = log.replay_all().await.unwrap();
        assert_eq!(events.len(), 2);
    }
}

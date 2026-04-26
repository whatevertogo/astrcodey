# session-storage Specification

## Purpose
TBD - created by archiving change astrcode-v2-architecture. Update Purpose after archive.
## Requirements
### Requirement: JSONL append-only event log
Session events SHALL be stored as JSONL files: one JSON object per line, appended atomically.
The event log file SHALL be created with the session and never modified (append-only).
Each line SHALL be a complete, valid JSON object (no multi-line events).

#### Scenario: Atomic append
- **WHEN** two concurrent tasks append events to the same session
- **THEN** each event is written as a complete line
- **THEN** no interleaving occurs within a single line (file lock or append queue)

### Requirement: Snapshot + tail recovery
The storage layer SHALL periodically create full state snapshots at a given event offset.
Recovery SHALL load the most recent snapshot, then replay all events after the snapshot's offset.
Snapshots older than N versions SHALL be pruned (configurable, default keep last 3).

#### Scenario: Recover from snapshot at offset 50
- **WHEN** session has snapshots at offsets 30 and 50, and 63 events total
- **THEN** recovery loads snapshot at offset 50
- **THEN** replays events 51-63 to reach current state

### Requirement: File-based turn lock
The storage layer SHALL use OS-level file locks (`fs2`) to prevent concurrent turn execution within the same session.
The lock file SHALL be created per-session at `sessions/<id>/active-turn.lock`.

#### Scenario: Lock prevents concurrent turns
- **WHEN** task-A acquires the turn lock for session-X
- **THEN** task-B attempting to acquire the same lock blocks until task-A releases it
- **THEN** task-B proceeds after task-A completes its turn

### Requirement: Batch appender for write efficiency
The storage layer SHALL buffer concurrent event appends via a `BatchAppender` with a configurable flush window (default 50ms).
Within the window, multiple append requests SHALL be merged into a single file write.

#### Scenario: Multiple events batched
- **WHEN** 10 events are appended within a 50ms window
- **THEN** all 10 events are written in a single `write()` call
- **THEN** each event is a separate JSONL line

### Requirement: Multi-session file layout
Session files SHALL be organized as:
```
~/.astrcode/projects/<project_hash>/sessions/<session_id>/
  ├── session-<session_id>.jsonl    # Append-only event log
  ├── active-turn.lock              # Turn-level file lock
  ├── active-turn.json              # Current turn metadata
  └── snapshots/                    # Periodic state snapshots
      ├── snapshot-000050.json
      └── snapshot-000100.json
```

#### Scenario: New session creates directory structure
- **WHEN** a session is created with id "abc123"
- **THEN** the directory `~/.astrcode/projects/<project>/sessions/abc123/` is created
- **THEN** session-abc123.jsonl is created with the SessionStart event

### Requirement: Session ID validation
Session IDs SHALL be validated: only alphanumeric characters, hyphens, underscores, and the letter 'T' allowed.
Characters `.` and `:` SHALL be rejected to prevent path traversal.

#### Scenario: Invalid session ID rejected
- **WHEN** storage receives a request with session_id="../etc/passwd"
- **THEN** the request is rejected with a validation error
- **THEN** no file system operation is performed

### Requirement: Config storage
The storage layer SHALL provide configuration persistence via atomic file writes (write to temp + rename).
Config SHALL be stored as JSON at `~/.astrcode/config.json`.
Project-level overrides SHALL be stored at `<workspace>/.astrcode/config.json`.

#### Scenario: Atomic config save
- **WHEN** user updates a setting
- **THEN** the new config is written to a temp file first
- **THEN** the temp file is renamed to config.json (atomic on most filesystems)
- **THEN** if the rename fails, the original config is untouched


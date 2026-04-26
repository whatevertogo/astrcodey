## ADDED Requirements

### Requirement: Session is append-only event log
Session SHALL be the single source of truth, implemented as an append-only event log.
All state changes (user messages, LLM responses, tool calls, compaction) SHALL be recorded as immutable events.
Session state SHALL be reconstructable from the event log at any time.

#### Scenario: Events are appended atomically
- **WHEN** a turn produces a new event (e.g., user message, assistant response, tool result)
- **THEN** the event is written atomically as a single JSONL line to the session event log

#### Scenario: Session state is rebuilt from events
- **WHEN** server resumes a session that has 100 events in its log
- **THEN** the session replays all 100 events and builds memory state identical to pre-shutdown state

### Requirement: Agent is ephemeral, born from Session
Agent SHALL be a transient processor with no persistent state of its own.
Agent SHALL be created by reading session events and rebuilding context.
After processing a turn, Agent SHALL append new events to the session and then may be discarded.

#### Scenario: Agent created on-demand
- **WHEN** user submits a prompt to a session with no active agent
- **THEN** the server creates an Agent by replaying the session's events into Agent context
- **THEN** the Agent processes the turn and appends new events to the session

#### Scenario: Agent can be recreated after crash
- **WHEN** the server process crashes mid-turn and restarts
- **THEN** a new Agent is created from the session's persisted events
- **THEN** the session is in the same state as before the crash (last completed event)

### Requirement: SessionStart event initiates every session
A `SessionStart` event SHALL be the first event in every session's event log.
This event SHALL contain session metadata: session_id, created_at, working_directory, and initial model configuration.

#### Scenario: New session writes SessionStart
- **WHEN** a client requests session creation with working_dir="/home/user/project"
- **THEN** the session event log is created with a SessionStart event as its first entry
- **THEN** the SessionStart event includes session_id, timestamp, and working_directory

### Requirement: Session tree supports fork, branch, and switch
Session SHALL support fork: creating a new independent session that shares history up to a cursor point.
Session SHALL support branch tracking: each fork records its parent session ID.
Session SHALL support switch: disconnecting from one session and connecting to another.

#### Scenario: Fork creates independent branch
- **WHEN** user forks session-A at cursor position 42
- **THEN** a new session-B is created with events 1-42 copied from session-A
- **THEN** subsequent events in session-A and session-B are independent

#### Scenario: Session list shows parent relationships
- **WHEN** client requests session list
- **THEN** each session entry includes its parent_session_id (null for root sessions)

### Requirement: Session snapshot enables fast recovery
Session SHALL periodically create snapshots: a summary of session state at a specific event offset.
Recovery SHALL load the latest snapshot then replay only events after the snapshot's offset.

#### Scenario: Recovery from snapshot
- **WHEN** session has snapshot at event #80 and current log has 95 events
- **THEN** recovery loads snapshot #80 and replays events 81-95
- **THEN** recovery is faster than replaying all 95 events

### Requirement: Multi-session concurrent access
Server SHALL support multiple sessions active concurrently.
Each session's turn processing SHALL be serial (one turn at a time per session).
Different sessions SHALL be able to process turns concurrently in separate tokio tasks.

#### Scenario: Two sessions process turns concurrently
- **WHEN** user-A sends prompt to session-A and user-B sends prompt to session-B at the same time
- **THEN** both prompts are processed concurrently
- **THEN** session-A's event log only contains session-A events
- **THEN** session-B's event log only contains session-B events

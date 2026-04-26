## ADDED Requirements

### Requirement: Server manages session lifecycle
The server SHALL provide a `SessionManager` that handles create, resume, fork, switch, delete, and list operations on sessions.
Sessions SHALL be persisted via `EventStore` trait (backed by `astrcode-storage`).

#### Scenario: Create new session
- **WHEN** server receives `CreateSession { working_dir }` command
- **THEN** a new session is created with a unique session_id
- **THEN** a SessionStart event is written as the first entry
- **THEN** server returns `SessionCreated { session_id }` event

#### Scenario: Resume existing session
- **WHEN** server receives `ResumeSession { session_id }` command
- **THEN** the session's event log is loaded
- **THEN** state is rebuilt from the latest snapshot + tail events
- **THEN** server returns `SessionResumed { session_id, snapshot }` event

#### Scenario: List all sessions
- **WHEN** server receives `ListSessions` command
- **THEN** returns a list of all sessions with metadata (id, created_at, working_dir, last_active_at, parent_session_id)

### Requirement: Server orchestrates Agent execution
Server SHALL create Agent instances on-demand when a prompt is submitted.
Server SHALL feed prompt through: extension hooks → prompt composition → LLM call → tool execution → event emission.
Server SHALL broadcast all produced events to connected frontends.

#### Scenario: Full turn processing pipeline
- **WHEN** user submits prompt "fix the bug in main.rs"
- **THEN** UserPromptSubmit hooks fire
- **THEN** prompt composer assembles the full prompt context
- **THEN** LLM is called with the assembled prompt
- **THEN** LLM response stream is emitted as MessageDelta events
- **THEN** if LLM requests tool calls, each tool goes through BeforeToolCall → execute → AfterToolCall
- **THEN** tool results are emitted as ToolCallStart/ToolCallDelta/ToolCallEnd events

### Requirement: Server handles multiple transports
Server SHALL accept connections via a `ServerTransport` trait.
Server SHALL provide a `StdioTransport` implementation for JSON-RPC over stdin/stdout.
The transport trait SHALL be designed to allow future WebSocket transport implementation.

#### Scenario: Stdio transport reads commands
- **WHEN** a client writes a JSON-RPC command line to stdin
- **THEN** the server parses and dispatches the command
- **THEN** the response or error is written to stdout

#### Scenario: Transport trait is extensible
- **WHEN** a new `WebSocketTransport` struct implements `ServerTransport`
- **THEN** the server can accept WebSocket connections with no changes to session/agent logic

### Requirement: Server supports multi-session concurrency
Server SHALL allow multiple sessions to be active simultaneously.
Each session SHALL own an independent event log and state.
Server SHALL spawn a tokio task per session for turn processing.

#### Scenario: Three clients each have their own session
- **WHEN** three separate TUI instances connect to the same server
- **THEN** each TUI creates/resumes its own session
- **THEN** all three can process prompts independently

### Requirement: Graceful shutdown
Server SHALL handle shutdown signal (SIGTERM/Ctrl+C) gracefully.
Active sessions SHALL flush pending events to disk before exit.
Server SHALL emit `SessionShutdown` event for each active session.

#### Scenario: Shutdown during active turn
- **WHEN** server receives Ctrl+C while an agent is processing a turn
- **THEN** the current event being written completes atomically
- **THEN** all event logs are flushed to disk
- **THEN** server exits with code 0

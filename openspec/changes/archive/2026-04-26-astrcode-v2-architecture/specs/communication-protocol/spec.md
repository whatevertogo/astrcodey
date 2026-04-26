## ADDED Requirements

### Requirement: JSON-RPC 2.0 over stdio
The communication protocol SHALL be JSON-RPC 2.0 transported over stdin/stdout.
Each message SHALL be a single JSON object followed by a newline (`\n`) delimiter.
Stderr SHALL be reserved for server diagnostics and MUST NOT contain protocol messages.

#### Scenario: Client sends a command
- **WHEN** client writes `{"jsonrpc":"2.0","id":1,"method":"submit_prompt","params":{"text":"hello"}}\n` to stdin
- **THEN** server reads and parses the command
- **THEN** server writes response events to stdout as JSONL

#### Scenario: Server streams events
- **WHEN** server processes a prompt and generates 5 events
- **THEN** each event is written as a separate JSONL line to stdout
- **THEN** the final response includes the same `id` as the request for correlation

### Requirement: ClientCommand types
Protocol SHALL define a `ClientCommand` enum with at minimum: create_session, resume_session, fork_session, delete_session, list_sessions, submit_prompt, abort, set_model, set_thinking_level, compact, switch_mode, get_state, ui_response.
Each command variant SHALL carry typed parameters.

#### Scenario: Submit prompt with attachments
- **WHEN** client sends submit_prompt with text and an image attachment
- **THEN** the server includes the image in the LLM request's message content

#### Scenario: Abort during generation
- **WHEN** client sends abort command while LLM is streaming
- **THEN** the LLM stream is cancelled
- **THEN** partial results up to the abort point are preserved in the session

### Requirement: ServerEvent types
Protocol SHALL define a `ServerEvent` enum with at minimum: session_created, session_resumed, session_deleted, session_list, agent_started, agent_ended, turn_started, turn_ended, message_start, message_delta, message_end, tool_call_start, tool_call_delta, tool_call_end, compaction_started, compaction_ended, ui_request, error.
Each event variant SHALL carry typed payload data.

#### Scenario: Message streaming
- **WHEN** LLM generates "Hello, I can help you with that"
- **THEN** server emits message_start (message_id, role: assistant)
- **THEN** server emits message_delta (message_id, delta: "Hello, I can help")
- **THEN** server emits message_delta (message_id, delta: " you with that")
- **THEN** server emits message_end (message_id)

### Requirement: UI request sub-protocol
When the server needs user interaction (confirm, select, input, notify), it SHALL emit a `ui_request` event.
The client SHALL respond with a `ui_response` command referencing the request_id.
UI requests SHALL support timeout and abort.

#### Scenario: Server asks for confirmation
- **WHEN** agent's tool call requires user approval
- **THEN** server emits ui_request { request_id, kind: "confirm", message: "Execute rm -rf node_modules?" }
- **THEN** client displays the confirmation dialog
- **THEN** client sends ui_response { request_id, value: { accepted: true } }
- **THEN** server proceeds with the tool call

#### Scenario: UI request timeout
- **WHEN** server emits a ui_request with timeout=60s
- **THEN** if client does not respond within 60s, the request is cancelled
- **THEN** server returns a default response (reject for confirm, empty for input)

### Requirement: Protocol version negotiation
The protocol SHALL support version negotiation during initial handshake.
If client and server versions are incompatible, the handshake SHALL fail with a clear error message.

#### Scenario: Compatible versions
- **WHEN** client sends initialize with protocol_version=1
- **THEN** server responds with accepted_version=1 and server capabilities

#### Scenario: Incompatible versions
- **WHEN** client sends initialize with protocol_version=999
- **THEN** server responds with error { code: -32600, message: "Unsupported protocol version 999. Server supports [1]" }

### Requirement: Error codes
Protocol SHALL use standard JSON-RPC error codes (-32700 parse error, -32600 invalid request, -32601 method not found, -32602 invalid params, -32603 internal error).
Protocol SHALL define application-level error codes for domain errors (session not found, tool execution failed, LLM provider error, etc.).

#### Scenario: Session not found
- **WHEN** client sends resume_session with a non-existent session_id
- **THEN** server responds with error { code: 40401, message: "Session not found: abc123" }

## ADDED Requirements

### Requirement: Agent is created from session events
The agent loop SHALL create an Agent instance by replaying a session's event log.
Agent SHALL have no persistent state of its own — all state is derived from session events.
After processing a turn, Agent SHALL append new events to the session and may be discarded.

#### Scenario: Agent built from session replay
- **WHEN** user submits a prompt to session with 50 prior events
- **THEN** agent replays all 50 events to build message history, tool results, and context
- **THEN** agent is ready to process the new prompt

#### Scenario: Agent recreated after server restart
- **WHEN** server restarts and client resumes session
- **THEN** a new Agent is created from the persisted session events
- **THEN** the agent's state is identical to pre-restart state

### Requirement: Turn processing pipeline
Each turn SHALL flow through: receive prompt → fire UserPromptSubmit hooks → assemble prompt context → call LLM → stream response → execute tool calls → fire TurnEnd hooks → append events.
The pipeline SHALL be observable via events emitted at each stage.

#### Scenario: Full turn with no tool calls
- **WHEN** user submits "What is Rust?" and LLM responds with text only
- **THEN** UserPromptSubmit hooks fire
- **THEN** prompt composer assembles context from contributors + extensions
- **THEN** LLM generates text response streamed as MessageDelta events
- **THEN** no tool calls are requested
- **THEN** TurnEnd hooks fire
- **THEN** user message + assistant response events are appended to session

#### Scenario: Turn with tool calls
- **WHEN** user submits "read main.rs" and LLM responds with a readFile tool call
- **THEN** LLM response is streamed to completion
- **THEN** the readFile tool call is parsed
- **THEN** BeforeToolCall hooks fire → tool executes → AfterToolCall hooks fire
- **THEN** tool result is appended to session
- **THEN** LLM is called again with the tool result in context
- **THEN** final response is streamed and turn completes

### Requirement: Tool execution orchestration
Agent loop SHALL orchestrate tool execution: parse tool calls from LLM response, fire BeforeToolCall hooks, execute tool, fire AfterToolCall hooks, append result.
Blocking hooks SHALL be able to prevent tool execution.
Sequential tools SHALL execute one at a time; parallel tools SHALL execute concurrently.

#### Scenario: Parallel tool execution
- **WHEN** LLM requests 3 readFile calls and 1 grep call
- **THEN** readFile calls (parallel mode) execute concurrently
- **THEN** grep call (may be sequential) waits for parallel tools
- **THEN** results are emitted in LLM-requested order

#### Scenario: Blocking hook prevents tool
- **WHEN** LLM requests shell "rm -rf /"
- **THEN** BeforeToolCall Blocking hook returns Block
- **THEN** tool is not executed
- **THEN** LLM receives error: "Tool execution blocked: Dangerous command"

### Requirement: LLM interaction
Agent loop SHALL call the LLM provider with assembled messages and available tool definitions.
The response SHALL be streamed as events.
On tool calls, agent loop SHALL loop back to LLM with tool results appended.
The loop SHALL continue until LLM produces a final response (no more tool calls) or max continuation attempts reached.

#### Scenario: Multi-turn LLM loop
- **WHEN** LLM calls readFile, then grep on the file content, then responds
- **THEN** iteration 1: LLM calls readFile → tool executes → result appended
- **THEN** iteration 2: LLM calls grep → tool executes → result appended
- **THEN** iteration 3: LLM produces final text response → turn ends

#### Scenario: Max continuation attempts reached
- **WHEN** LLM calls tools 4 times in a row (max_output_continuation_attempts=3)
- **THEN** on the 4th tool call request, agent stops the loop
- **THEN** an error event is emitted: "Max continuation attempts (3) exceeded"

### Requirement: Turn abortion
Agent loop SHALL support aborting a turn mid-processing.
On abort, the LLM stream SHALL be cancelled, running tools SHALL be terminated.
Partial results up to the abort point SHALL be preserved in the session.

#### Scenario: User aborts during LLM streaming
- **WHEN** LLM is mid-stream and user sends abort command
- **THEN** the HTTP stream to LLM provider is cancelled
- **THEN** partial text generated so far is preserved in session
- **THEN** agent becomes ready for next prompt

### Requirement: Event emission during turn
Agent loop SHALL emit events for every significant action: TurnStarted, MessageStart/Delta/End, ToolCallStart/Delta/End, TurnEnded.
Events SHALL be appended to session event log AND broadcast to connected frontends.

#### Scenario: Full event stream for a turn
- **WHEN** a turn processes "explain main.rs"
- **THEN** events emitted in order: TurnStarted → MessageStart → MessageDelta* → MessageEnd → (ToolCallStart → ToolCallDelta* → ToolCallEnd)* → TurnEnded
- **THEN** all events are persisted in session JSONL
- **THEN** all events are broadcast to subscribers

### Requirement: Error recovery within turn
Agent loop SHALL handle LLM errors with retry (per ai crate config).
Agent loop SHALL handle tool execution errors by reporting them to LLM.
After max_consecutive_failures consecutive errors, the turn SHALL abort.

#### Scenario: Tool execution fails
- **WHEN** readFile fails with "file not found"
- **THEN** the error is formatted as a tool result with is_error=true
- **THEN** the LLM receives the error and can try a different approach
- **THEN** this does NOT count toward max_consecutive_failures (tool errors are expected)

#### Scenario: Consecutive LLM failures
- **WHEN** LLM provider returns 503 three times in a row
- **THEN** max_consecutive_failures (3) is reached
- **THEN** turn aborts with error: "Too many consecutive failures"

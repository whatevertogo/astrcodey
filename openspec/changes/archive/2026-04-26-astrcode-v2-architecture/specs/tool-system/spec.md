## ADDED Requirements

### Requirement: Tool trait for all tools
All tools SHALL implement the `Tool` trait: `definition()` returning ToolDefinition, `execute()` performing the action and returning ToolResult.
Tools SHALL be registered in a `ToolRegistry` that the Agent queries for available tools.

#### Scenario: Tool execution
- **WHEN** Agent calls tool registry.execute("readFile", {"path": "/src/main.rs"})
- **THEN** the ReadFile tool reads the file content
- **THEN** returns ToolResult with the file content and metadata

### Requirement: Event emission around tool execution
Before a tool executes, a `ToolCallStart` event SHALL be emitted.
During execution with streaming output, `ToolCallDelta` events SHALL be emitted.
After execution completes, a `ToolCallEnd` event SHALL be emitted with the final result.

#### Scenario: Shell tool streaming
- **WHEN** shell tool executes `cargo build` and produces 100 lines of output
- **THEN** ToolCallStart is emitted with tool_name="shell" and arguments
- **THEN** multiple ToolCallDelta events stream the build output
- **THEN** ToolCallEnd contains the exit code and final output

### Requirement: Extension hooks for tool execution
BeforeToolCall and AfterToolCall extension hooks SHALL fire for every tool execution.
Blocking hooks SHALL be able to prevent tool execution.
AfterToolCall hooks SHALL be able to modify tool results.

#### Scenario: Hook blocks dangerous file write
- **WHEN** agent calls writeFile with path="/etc/passwd"
- **THEN** a BeforeToolCall hook returns Block { reason: "System file modification denied" }
- **THEN** the writeFile tool is not called
- **THEN** the LLM receives a tool error: "Blocked: System file modification denied"

### Requirement: Built-in file tools
The system SHALL provide 6 built-in file tools: readFile, writeFile, editFile, applyPatch, findFiles, grep.
readFile SHALL detect binary/image/PDF files and handle them appropriately.
writeFile SHALL only create/overwrite text files.
editFile SHALL use unique string replacement for precise edits.

#### Scenario: readFile on image
- **WHEN** readFile is called on "screenshot.png"
- **THEN** the file is detected as an image (PNG magic bytes)
- **THEN** the image content is base64-encoded and returned as an image block

#### Scenario: editFile unique string match
- **WHEN** editFile is called with old_string="fn foo()" and new_string="fn bar()"
- **THEN** the file is searched for exactly one occurrence of old_string
- **THEN** if found exactly once, the replacement is made
- **THEN** if found 0 or >1 times, an error is returned

### Requirement: Shell execution tool
The system SHALL provide a shell tool for executing shell commands.
Shell execution SHALL capture and stream stdout and stderr.
Shell execution SHALL enforce a configurable timeout (default 120 seconds).

#### Scenario: Shell command succeeds
- **WHEN** shell executes "git status"
- **THEN** stdout is captured and streamed to the client
- **THEN** stderr is captured and streamed to the client
- **THEN** ToolCallEnd includes exit_code: 0

#### Scenario: Shell command timeout
- **WHEN** shell executes a command that runs longer than the timeout
- **THEN** the process is killed
- **THEN** ToolCallEnd includes exit_code: null and timed_out: true

### Requirement: Agent collaboration tools
The system SHALL provide 4 agent collaboration tools: spawn, send, observe, close.
spawn SHALL create a child agent with isolated context.
send SHALL pass messages bidirectionally between parent and child agents.
observe SHALL provide a read-only snapshot of a child agent's state.
close SHALL terminate a child agent and cascade to its descendants.

#### Scenario: Spawn child agent for sub-task
- **WHEN** agent calls spawn with task="Review the authentication module"
- **THEN** a child agent is created with its own session
- **THEN** the child agent processes the review task independently
- **THEN** results are available via observe and send

### Requirement: Tool result persistence
Large tool results SHALL be persisted to disk when they exceed an inline threshold.
The in-place result SHALL be replaced with a reference (file path + byte count).
On re-read (e.g., after compaction), persisted results SHALL be re-hydrated.

#### Scenario: Large file read is persisted
- **WHEN** readFile returns 50KB of content (threshold: 10KB)
- **THEN** the content is written to a tool results file on disk
- **THEN** the ToolResult visible to LLM contains a reference: "Result persisted to tool_results/abc123.txt (50KB)"

### Requirement: Per-tool execution mode
Each tool SHALL declare its preferred execution mode: sequential or parallel.
Sequential tools SHALL be executed one at a time.
Parallel tools SHALL be preflighted sequentially but executed concurrently.

#### Scenario: Parallel tool execution
- **WHEN** LLM requests 3 readFile calls and 1 shell call
- **THEN** all 3 readFile calls (parallel mode) execute concurrently
- **THEN** the shell call (sequential mode) waits for them to complete
- **THEN** results are emitted in the order the LLM requested them

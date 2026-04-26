## ADDED Requirements

### Requirement: Extension lifecycle event subscription
Extensions SHALL subscribe to one or more lifecycle events from the 12 core events.
Events SHALL include: SessionStart, SessionBeforeFork, SessionBeforeCompact, SessionShutdown, AgentStart, AgentEnd, TurnStart, TurnEnd, BeforeToolCall, AfterToolCall, MessageDelta, UserPromptSubmit.
Each subscription SHALL declare a `HookMode`: Blocking, NonBlocking, or Advisory.

#### Scenario: Extension subscribes to tool call events
- **WHEN** an extension declares subscription to BeforeToolCall and AfterToolCall with HookMode::Blocking
- **THEN** before any tool executes, the extension's handler is called synchronously
- **THEN** the extension can return Block to prevent tool execution

#### Scenario: NonBlocking extension does not delay execution
- **WHEN** an extension subscribes to TurnEnd with HookMode::NonBlocking
- **THEN** the extension's handler is spawned as a fire-and-forget task
- **THEN** turn completion is not delayed by the extension

### Requirement: Blocking hooks can prevent execution
A hook with HookMode::Blocking SHALL return `HookEffect::Allow` to permit the operation or `HookEffect::Block { reason }` to prevent it.
Blocked tool calls SHALL produce an error event visible to the LLM.
Blocking hooks SHALL have a configurable timeout (default 30 seconds).

#### Scenario: Security hook blocks dangerous command
- **WHEN** an agent calls shell tool with "rm -rf /"
- **THEN** the BeforeToolCall hook returns Block { reason: "Dangerous recursive delete" }
- **THEN** the tool is not executed
- **THEN** the LLM receives a tool result with the block reason as error

#### Scenario: Blocking hook timeout
- **WHEN** a Blocking hook handler takes longer than the timeout (30s)
- **THEN** the hook is cancelled
- **THEN** server proceeds according to configured timeout_policy (Allow or Abort)

### Requirement: AfterToolCall hooks can modify results
A hook with HookMode::Blocking or HookMode::NonBlocking subscribed to AfterToolCall SHALL be able to return `HookEffect::Modify { patches }`.
Modified results SHALL replace the original tool output before it is sent to the LLM.

#### Scenario: Post-process tool output
- **WHEN** a tool returns 5000 lines of log output
- **THEN** an AfterToolCall hook returns Modify that truncates the output to 500 lines
- **THEN** the LLM receives the truncated 500 lines

### Requirement: Global + session-level extension loading
Server SHALL load global extensions from `~/.astrcode/extensions/` at startup.
Server SHALL load project-level extensions from `<workspace>/.astrcode/extensions/` when a session is created.
Both levels SHALL be merged into the active extension set for each session.

#### Scenario: Project extension overrides global behavior
- **WHEN** a global extension handles BeforeToolCall for shell commands
- **THEN** a project-level extension also handles BeforeToolCall for shell commands
- **THEN** both handlers are called; project-level handler runs first

### Requirement: Extensions can register custom tools
Extensions SHALL be able to register custom Tool implementations via `register_tools()`.
Custom tools SHALL be included in the LLM's tool list alongside built-in tools.
Custom tool execution SHALL go through the same BeforeToolCall/AfterToolCall hook pipeline.

#### Scenario: Extension registers a database query tool
- **WHEN** an extension registers a "query_db" tool
- **THEN** the tool definition appears in the prompt's available tools section
- **THEN** when the LLM calls query_db, the extension's tool handler executes

### Requirement: Extensions can register slash commands
Extensions SHALL be able to register slash commands that users can invoke from the frontend.
Commands SHALL include name, description, argument schema, and handler function.

#### Scenario: Extension adds /deploy command
- **WHEN** an extension registers a "/deploy" command
- **THEN** the user can type "/deploy staging" in the frontend
- **THEN** the extension's command handler is invoked with args=["staging"]

### Requirement: Extensions can provide context
Extensions SHALL be able to register context providers that inject text into the system prompt.
Context text SHALL be injected at a specified priority layer (Stable/SemiStable/Inherited/Dynamic).

#### Scenario: Extension injects project conventions
- **WHEN** an extension registers a context provider that returns "Use tabs, not spaces"
- **THEN** the text is included in the system prompt's Inherited layer
- **THEN** it survives across turns with the same cache TTL as other Inherited blocks

# extension-system Specification

## Purpose

The extension system provides astrcode's in-process extension boundary for lifecycle hooks, dynamic tools, slash command metadata, and context contributions. Native extensions are trusted host code and are loaded only when explicitly enabled.

## Requirements

### Requirement: Extension lifecycle event subscription

Extensions SHALL subscribe to one or more lifecycle events from the current core event set: `SessionStart`, `SessionShutdown`, `TurnStart`, `TurnEnd`, `PreToolUse`, `PostToolUse`, `BeforeProviderRequest`, `AfterProviderResponse`, and `UserPromptSubmit`.
Each subscription SHALL declare a `HookMode`: `Blocking`, `NonBlocking`, or `Advisory`.

#### Scenario: Extension subscribes to tool call events

- **WHEN** an extension declares subscription to `PreToolUse` and `PostToolUse` with `HookMode::Blocking`
- **THEN** before any tool executes, the extension's `PreToolUse` handler is called synchronously
- **THEN** after the tool executes, the extension's `PostToolUse` handler is called synchronously
- **THEN** the extension can block execution, modify the input before execution, or modify the result after execution

#### Scenario: NonBlocking extension does not delay execution

- **WHEN** an extension subscribes to `TurnEnd` with `HookMode::NonBlocking`
- **THEN** the extension's handler is spawned as a fire-and-forget task
- **THEN** turn completion is not delayed by the extension

### Requirement: Blocking hooks can prevent execution

A hook with `HookMode::Blocking` SHALL return `HookEffect::Allow` to permit the operation or `HookEffect::Block { reason }` to prevent it.
Blocked tool calls SHALL produce an error result visible to the LLM.
Blocking hooks SHALL have a configurable timeout, defaulting to 30 seconds.

#### Scenario: Security hook blocks dangerous command

- **WHEN** an agent calls shell tool with `rm -rf /`
- **THEN** the `PreToolUse` hook returns `Block { reason: "Dangerous recursive delete" }`
- **THEN** the tool is not executed
- **THEN** the LLM receives a tool result with the block reason as error

### Requirement: Tool hooks can modify input and results

A Blocking `PreToolUse` hook MAY return `HookEffect::ModifiedInput { tool_input }`.
A Blocking `PostToolUse` hook MAY return `HookEffect::ModifiedResult { content }`.
When multiple Blocking hooks return modifications, the last applicable modification in dispatch order SHALL win. The first Blocking hook that returns `Block` SHALL stop the dispatch.

#### Scenario: Pre-process tool input

- **WHEN** a tool call contains shorthand arguments
- **THEN** a `PreToolUse` hook can return normalized JSON input
- **THEN** the tool executes with the normalized input

#### Scenario: Post-process tool output

- **WHEN** a tool returns 5000 lines of log output
- **THEN** a `PostToolUse` hook can return truncated content
- **THEN** the LLM receives the truncated content

### Requirement: Global + project-level extension loading

Server SHALL load global extensions from `~/.astrcode/extensions/` and project-level extensions from `<workspace>/.astrcode/extensions/` during bootstrap for the active workspace.
Both levels SHALL be merged into the active extension set. Project-level extensions SHALL run before global extensions.
Native extensions SHALL require `ASTRCODE_ENABLE_NATIVE_EXTENSIONS=1` because they execute trusted in-process code.

#### Scenario: Project extension overrides global behavior

- **WHEN** a global extension handles `PreToolUse` for shell commands
- **THEN** a project-level extension also handles `PreToolUse` for shell commands
- **THEN** both handlers are called, with the project-level handler first

### Requirement: Extensions can register executable custom tools

Extensions SHALL be able to register custom tool definitions and execution handlers.
Custom tools SHALL be included in the LLM's tool list alongside built-in tools.
Custom tool execution SHALL go through the same `PreToolUse` / execution / `PostToolUse` pipeline as built-in tools.
Dynamic extension tools SHALL be applied through the capability router without removing stable built-in tools.

#### Scenario: Extension registers a database query tool

- **WHEN** an extension registers a `query_db` tool definition and handler
- **THEN** the tool definition appears in the prompt's available tools section
- **THEN** when the LLM calls `query_db`, the extension's tool handler executes

### Requirement: Extensions can register slash commands

Extensions SHALL be able to register slash command metadata that users can invoke from the frontend.
Commands SHALL include name, description, and argument schema. Command execution MAY be implemented by the owning extension boundary.

#### Scenario: Extension adds /deploy command

- **WHEN** an extension registers a `/deploy` command
- **THEN** the frontend can surface `/deploy` as an available command

### Requirement: Extensions can provide context

Extensions SHALL be able to provide context blocks for prompt assembly.
Context text SHALL be injected at the appropriate prompt layer by the host composer or extension integration point.

#### Scenario: Extension injects project conventions

- **WHEN** an extension contributes project convention text
- **THEN** the text is included in the system prompt assembly

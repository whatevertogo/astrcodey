# Tool Loading Boundary

This note defines the intended boundary for tools, extensions, and future SDK
registration.

## Position

Tools are a core runtime capability. Extensions are one source of tools, not the
owner of the tool system.

Every callable tool, regardless of origin, must enter the same session tool
registry and use the same execution path:

```text
tool source
  -> ToolDefinition + Tool implementation
  -> session ToolRegistry snapshot
  -> PreToolUse hooks
  -> tool execution
  -> PostToolUse hooks
  -> event stream + persisted ToolResult
  -> model-visible tool result
```

This keeps observability, policy checks, ordering, streaming, result pruning, and
session persistence consistent. There should not be a second path where an SDK,
plugin, or MCP integration calls runtime capabilities outside the normal tool
pipeline.

## Tool Sources

The registry should treat these as different origins feeding the same interface:

| Source | Meaning | Examples |
|--------|---------|----------|
| `builtin` | Minimal tools that are always available for the coding runtime | file read/write/edit, search, patch, shell |
| `bundled` | First-party tool packs shipped with the server but not fundamental to the `Tool` trait | agent delegation, task tracking |
| `extension` | User or project extensions loaded through the extension runtime | custom project tools, policy-aware tools |
| `sdk` | Tools registered by a future SDK without requiring a full plugin lifecycle | app-specific tool packs |
| `mcp` | Future external MCP tools adapted into the same local contract | remote/server-provided tools |

The distinction is provenance and lifecycle, not execution semantics.

## Current Mapping

The current code already mostly follows this direction:

- `astrcode-core` owns `Tool`, `ToolDefinition`, `ToolResult`, and
  `ToolExecutionContext`.
- `astrcode-tools` owns the built-in tool implementations and `ToolRegistry`.
- `astrcode-server` builds a session-level tool registry snapshot during session
  creation or resume.
- `astrcode-extensions` adapts extension-provided tool definitions and handlers
  into normal `Tool` trait objects.
- `astrcode-extension-agent-tools` and `astrcode-extension-task-tools` are
  statically linked first-party tool packs. They currently implement the
  `Extension` trait, but operationally they are bundled tool sources.

This means the immediate architecture does not need to move all tools out of the
extension path. It needs one clear merge point owned by the server.

## Boundary Rules

1. `Tool` remains the runtime execution interface.
2. `ToolRegistry` remains the single lookup and execution gateway for model
   tool calls.
3. Extensions may provide tools, but tool execution remains owned by the agent
   loop and registry.
4. Built-in and bundled tools should not depend on dynamic plugin loading.
5. SDK tool registration should not require implementing the full extension
   lifecycle unless the SDK user needs hooks, prompt contributions, commands, or
   other extension behavior.
6. MCP tools, when added, should be adapted into `Tool` rather than called as a
   side channel.
7. Tool origin should be metadata for policy, diagnostics, UI, and precedence;
   it should not create a parallel execution stack.

## Implemented Shape

### 1. Tool origin is explicit

`ToolDefinition` uses a protocol-visible origin enum:

```rust
pub enum ToolOrigin {
    Builtin,
    Bundled,
    Extension,
    Sdk,
    Mcp,
}
```

There is no legacy `is_builtin` flag. Code that needs built-in checks should
compare `definition.origin == ToolOrigin::Builtin`.

### 2. Keep source merging explicit

There is no public `ToolSource` abstraction yet. The server owns registry
snapshot construction directly:

```text
builtin_tools(working_dir, timeout)
extension_runner.collect_tool_adapters(working_dir)
  -> merge into ToolRegistry snapshot
```

Bundled agent/task tools currently enter through the extension runner and carry
`ToolOrigin::Bundled`. Future SDK or MCP support should first be added at this
server merge point; introduce a source trait only after repeated implementations
make that abstraction pay for itself.

### 3. Rename or document bundled tool packs

`astrcode-extension-agent-tools` and `astrcode-extension-task-tools` are not
normal user plugins. They are first-party bundled tool packs that happen to use
the `Extension` trait because they also contribute prompt content or reuse the
extension adapter.

Either rename later:

- `astrcode-agent-tools`
- `astrcode-task-tools`

Or keep the names and document them as bundled extensions. Avoid treating them as
proof that all tools must be plugins.

### 4. Split SDK registration into two levels

Future SDK should expose:

- tool-only SDK: register `ToolDefinition` + handler
- full extension SDK: register hooks, prompt contributions, commands, and tools

Most integrations only need the first level. Forcing them through the full
extension lifecycle would couple simple tools to plugin loading, hook ordering,
and manifest concerns.

### 5. Remove or wire half-finished dynamic registration paths

There are registration methods for tools on extension contexts and runtime
state. If they are not wired into session registry snapshot creation, they should
be removed or completed. Half-available registration APIs make it unclear which
tool source owns the final list.

## Precedence

Use explicit precedence when merging sources into a session snapshot.

Recommended default:

```text
project extension / project SDK
user extension / user SDK
bundled first-party tools
builtin core tools
```

Higher-precedence tools may override lower-precedence tools, but overrides should
emit diagnostics. Built-in safety-critical tools should be protected later by a
policy layer if overriding them becomes risky.

## What Not To Do

- Do not make plugin loading the only way to add tools.
- Do not let SDK tools bypass `ToolRegistry`.
- Do not add tool execution directly to the LLM provider layer.
- Do not make bundled agent/task tools look like third-party dynamic plugins in
  documentation or UI.
- Do not create DTOs for internal tool registration unless the data crosses a
  process, protocol, persistence, or plugin boundary.

## Decision

Adopt a tool-first model:

```text
Tool system = core runtime capability
Extension system = lifecycle and customization capability
Tool sources = built-in, bundled, extension, SDK, MCP adapters
Tool execution = one registry-owned path
```

This preserves the current good part of the implementation while leaving a clean
path for SDK and MCP support.

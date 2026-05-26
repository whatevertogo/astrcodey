# AstrCode

**English | [中文](README_CN.md)**

**BE PI OR BETTER THAN PI**  
*Inspired by Claude Code, Codex, OpenCode, and Pi — but built as a Rust-native*

| Interface | Preview |
|-----------|---------|
| **CLI (TUI)** | <img width="1210" height="924" alt="astrcode TUI screenshot" src="https://github.com/user-attachments/assets/55259723-9bd7-4a1a-a74e-1e799ece2eed" /> |
| **Web / Desktop** | <img width="1252" height="960" alt="astrcode web frontend screenshot" src="https://github.com/user-attachments/assets/af918c12-6fb7-4d72-b9ea-64133a2e2729" /> |

A Rust-built AI coding agent platform.

AstrCode is a full-stack AI coding assistant built from scratch in ~67.6k lines of Rust across 21 crates under `crates/` (plus a Tauri desktop shell), and a React + TypeScript web frontend (~6.3k lines). It features an agent loop with tool execution, a streaming SSE-based multi-provider LLM layer (Anthropic, OpenAI, Google GenAI), an SDK-based extension/hook system with sandboxed WASM extensions, background pre-warm, health checks, and a startup event channel, a persistent MCP process pool (reusing long-lived connections across turns), context window management with auto-compaction, an eval framework for automated benchmarking, and multiple interfaces: a terminal TUI, Web frontend, Tauri desktop app, HTTP/SSE API, and ACP (Agent Client Protocol) adapter.

## Table of Contents

- [Installation](#installation)
- [Configuration (Recommended Before First Run)](#configuration-recommended-before-first-run)
- [Quick Start](#quick-start)
- [Architecture](#architecture)
- [Crates](#crates)
- [Key Design Decisions](#key-design-decisions)
- [Running Modes](#running-modes)
- [Further Reading](#further-reading)
- [Distribution](#distribution)
- [Acknowledgments](#acknowledgments)
- [License](#license)

## Installation

### NPM Package

```bash
npm i @whatevertogo/astrcode
```

The `@whatevertogo/astrcode` npm package provides pre-built binaries for Linux, macOS, and Windows (x64 + arm64). After installation, the `astrcode` command will be available globally.

**Package**: [`@whatevertogo/astrcode`](https://www.npmjs.com/package/@whatevertogo/astrcode)

### Build from Source

See [Quick Start](#quick-start) below for building from source.

## Configuration (Recommended Before First Run)

AstrCode requires LLM provider and API key configuration to function properly. It is recommended to complete the following configuration before the first run.

### Configuration File Locations

| File | Path | Purpose |
|---|---|---|
| Main config | `~/.astrcode/config.json` | LLM providers, models, runtime parameters |
| Project config | `<workspace>/.astrcode/config.json` | Project-level overrides (optional) |
| Global MCP | `~/.astrcode/mcp.json` | MCP server configuration |
| Project MCP | `<workspace>/.astrcode/mcp.json` | Project-level MCP configuration (optional) |

### LLM Provider Configuration

Example `~/.astrcode/config.json`:

```json
{
  "version": "1",
  "activeProfile": "anthropic",
  "activeModel": "claude-sonnet-4-6",
  "activeSmallProfile": "anthropic",
  "activeSmallModel": "claude-haiku-4-5-20251001",
  "profiles": [
    {
      "name": "anthropic",
      "providerKind": "anthropic",
      "apiKey": "env:ANTHROPIC_API_KEY",
      "models": [
        { "id": "claude-sonnet-4-6", "maxTokens": 16384, "contextLimit": 200000 }
      ]
    },
    {
      "name": "openai",
      "providerKind": "openai",
      "apiKey": "env:OPENAI_API_KEY",
      "apiMode": "chat_completions",
      "models": [
        { "id": "gpt-4.1", "maxTokens": 16384, "contextLimit": 128000 }
      ]
    },
    {
      "name": "deepseek",
      "providerKind": "openai",
      "baseUrl": "https://api.deepseek.com",
      "apiKey": "env:DEEPSEEK_API_KEY",
      "apiMode": "chat_completions",
      "models": [
        { "id": "deepseek-chat", "maxTokens": 16384, "contextLimit": 128000 }
      ]
    }
  ]
}
```

**API Key Note**: Use `"apiKey": "env:VARIABLE_NAME"` to reference environment variables instead of writing keys directly in the configuration file.

Set the corresponding environment variables beforehand:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export OPENAI_API_KEY="sk-..."
export DEEPSEEK_API_KEY="sk-..."
```

### MCP Server Configuration

`~/.astrcode/mcp.json` registers external MCP tool servers. Both stdio (subprocess) and HTTP transports are supported:

**Stdio example:**

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/dir"],
      "env": {}
    }
  }
}
```

**HTTP example:**

```json
{
  "mcpServers": {
    "web-reader": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Field descriptions:

| Field | Required | Description |
|---|---|---|
| `command` | Yes (stdio) | Command to start the MCP server |
| `args` | No | Command line arguments array |
| `env` | No | Environment variables to pass to the process |
| `cwd` | No | Working directory (validated to be within workspace in project-level config) |
| `type` | No | Transport type: `"stdio"` (default) or `"http"` |
| `url` | Yes (http) | MCP server HTTP endpoint |
| `headers` | No | Custom HTTP headers for the MCP endpoint |

MCP servers start at extension initialization and persist across turns via a long-lived process pool. Global servers (`~/.astrcode/mcp.json`) are pre-warmed at startup; project-level servers (`<workspace>/.astrcode/mcp.json`) are pre-warmed in the background when a session is created or resumed. The first turn only blocks if the background pre-warm has not yet completed.

### Extension Configuration

Extensions can be enabled or disabled via `~/.astrcode/config.json`. By default, all extensions are enabled except `memory`, which is disabled by default.

```json
{
  "version": "1",
  "extensionStates": {
    "astrcode.memory": true
  }
}
```

To enable the memory extension, add `"astrcode.memory": true` to `extensionStates`.

### Built-in Extensions

First-party extensions are wired through [`astrcode-bundled-extensions`](crates/astrcode-bundled-extensions). Authors of new extensions should depend on [`astrcode-extension-sdk`](crates/astrcode-extension-sdk) rather than internal host crates.

| Extension | Crate | Description |
|---|---|---|
| **Mode** | `astrcode-extension-mode` | Agent running mode switching (Code / Plan), with Exit Gate, plan artifact persistence, keybinding & status item registration |
| **Skill** | `astrcode-extension-skill` | Slash-command skill discovery and dispatch |
| **MCP** | `astrcode-extension-mcp` | MCP protocol client with persistent process pool, background pre-warm, inflight merge |
| **Todo Tool** | `astrcode-extension-todo-tool` | Progress tracking todo list tool |
| **Agent Tools** | `astrcode-extension-agent-tools` | Sub-agent delegation, agent discovery |
| **Memory** | `astrcode-extension-memory` | Project-scoped markdown memory storage (disabled by default) |

## Quick Start

```bash
# 1. Build backend
cargo build

# 2. Create config directory and config file
mkdir -p ~/.astrcode
cat > ~/.astrcode/config.json << 'EOF'
{
  "version": "1",
  "activeProfile": "openai",
  "activeModel": "gpt-4o",
  "activeSmallProfile": "openai",
  "activeSmallModel": "gpt-4o-mini",
  "profiles": [
    {
      "name": "openai",
      "providerKind": "openai",
      "baseUrl": "https://api.openai.com/v1",
      "apiKey": "env:OPENAI_API_KEY",
      "models": [
        {
          "id": "gpt-4o",
          "maxTokens": 128000,
          "contextLimit": 128000,
          "reasoning": false
        },
        {
          "id": "gpt-4o-mini",
          "maxTokens": 128000,
          "contextLimit": 128000,
          "reasoning": false
        }
      ],
      "apiMode": "chat_completions"
    }
  ]
}
EOF

# 3. Set API key environment variable
export OPENAI_API_KEY="your-api-key-here"

# 4. Run interactive terminal UI
cargo run -- tui

# Headless single-shot execution
cargo run -- exec "explain the agent loop architecture"

# HTTP/SSE server
cargo run -- server

# Web frontend (dev server)
cd frontend && npm ci && npm run dev

# Tauri desktop app (dev mode)
cd frontend && npm ci && npm run tauri:dev

# Eval framework (requires dev-mode feature)
cargo run --features dev-mode -- eval
```

## Configuration

AstrCode uses a JSON-based configuration system stored in `~/.astrcode/config.json`. The configuration supports multiple LLM providers, model selection, runtime behavior tuning, and project-level overrides.

**Key configuration features:**
- Multi-provider support (Anthropic, OpenAI, Google GenAI)
- Separate small LLM configuration for extensions (e.g., memory extraction)
- Project-level config overrides via `.astrcode/config.json`
- Environment variable substitution for API keys (`env:VAR_NAME`)
- Runtime behavior tuning (timeouts, retries, compaction, agent limits)
- Compact circuit breaker and optional predictive compact
- WASM extension sandbox limits (fuel, memory)

For detailed configuration documentation, see [Configuration Guide](docs/configuration.md).

## Architecture

```
          ┌──────────┐  ┌──────────────────────┐  ┌───────────┐
          │   TUI    │  │ Web / Tauri Frontend  │  │ ACP Client│
          │ (ratatui)│  │ React 19 + TypeScript │  │  (stdio)  │
          └────┬─────┘  └────────┬─────────────┘  └─────┬─────┘
               │                  │ SSE / JSON-RPC       │ ACP JSON-RPC
               │    stdio         │                      │ over stdio
               └────────┬────────┘──────────────────────┘
                   ┌─────┴──────┐
                   │astrcode-cli│  TUI / exec / server launcher
                   └─────┬──────┘
                         │
                   ┌─────┴──────┐
                   │astrcode-   │  Session management, JSON-RPC + HTTP handler
                   │server      │  ACP adapter, transport, concurrency control
                   └─────┬──────┘
                         │
                   ┌─────┴───────┐
                   │astrcode-    │  Agent loop core: turn runner, tool pipeline
                   │session      │  LLM stream, context compaction orchestration
                   └─────┬───────┘
             ┌───────────┼───────────┐
             │           │           │
    ┌────────┴───┐ ┌─────┴─────┐ ┌───┴──────────┐
    │ astrcode-ai│ │astrcode-  │ │ astrcode-    │
    │            │ │extensions │ │ tools        │
    │ Anthropic  │ │Hook system│ │File/shell/   │
    │ OpenAI     │ │Ext SDK    │ │task tools    │
    │ Google     │ │WASM ext   │ │              │
    │ SSE+retry  │ │           │ │              │
    └────────┬───┘ └─────┬─────┘ └──────────────┘
             │           │
   ┌─────────┴──┐  ┌────┴─────────────────────────┐
   │astrcode-   │  │ Extension layer             │
   │ context    │  │ bundled-extensions          │
   │ Token budget│  │ sdk · mode · skill · todo  │
   │ Auto-compact│  │ agent-tools · mcp · memory │
   └────────────┘  │ + disk WASM (s5r)          │
                   └────────────────────────────┘
        ┌─────────────────────────────────────┐
        │              Shared layer            │
        │ core · protocol · storage · support  │
        │ log · client                         │
        └─────────────────────────────────────┘
```

## Crates

The Cargo workspace under [`crates/`](crates/) contains **21 crates**, plus [`src-tauri/`](src-tauri/) as the desktop shell (**22 workspace members** total). Crates are grouped by architectural layer (details in [Project Structure](docs/Project-Structure.md)).

### Layer 0: Foundation

| Crate | Lines | Description |
|---|---|---|
| [`astrcode-core`](crates/astrcode-core) | 5.3k | Shared domain types, traits, config system, extension contracts, prompt composition |
| [`astrcode-support`](crates/astrcode-support) | 1.0k | Host utilities: path resolution, shell detection, tool result persistence |
| [`astrcode-log`](crates/astrcode-log) | 308 | File rotation, stderr output, env-filter logging |

### Layer 1: Domain Services

| Crate | Lines | Description |
|---|---|---|
| [`astrcode-ai`](crates/astrcode-ai) | 3.5k | Multi-provider LLM layer (Anthropic, OpenAI-compatible, Google GenAI), SSE streaming, retry |
| [`astrcode-tools`](crates/astrcode-tools) | 5.1k | Built-in tools: read, write, edit, patch, find, grep, shell, terminal, task |
| [`astrcode-storage`](crates/astrcode-storage) | 3.8k | JSONL event log, snapshots, config persistence, file locking |
| [`astrcode-context`](crates/astrcode-context) | 3.6k | Token estimation, context window budgeting, auto-compact, prompt engine |
| [`astrcode-session`](crates/astrcode-session) | 8.0k | Agent loop: turn runner, tool pipeline, LLM stream, compact orchestration, runtime services |
| [`astrcode-extensions`](crates/astrcode-extensions) | 5.1k | Extension lifecycle, hook dispatch, capability gating, WASM runtime (wasmtime + s5r) |

### Layer 2: Extensions

| Crate | Lines | Description |
|---|---|---|
| [`astrcode-extension-sdk`](crates/astrcode-extension-sdk) | 642 | Stable extension authoring API, capability declarations, s5r wire types, manifest helpers |
| [`astrcode-bundled-extensions`](crates/astrcode-bundled-extensions) | 88 | Composition root that registers all first-party extension crates |
| [`astrcode-extension-mode`](crates/astrcode-extension-mode) | 978 | Code / Plan mode switching, exit gate, plan artifact, keybindings & status bar |
| [`astrcode-extension-skill`](crates/astrcode-extension-skill) | 852 | Slash-command skill discovery and Skill tool dispatch |
| [`astrcode-extension-todo-tool`](crates/astrcode-extension-todo-tool) | 786 | Progress-tracking todo list tool |
| [`astrcode-extension-agent-tools`](crates/astrcode-extension-agent-tools) | 658 | Sub-agent delegation, agent discovery (Claude Code compatible) |
| [`astrcode-extension-mcp`](crates/astrcode-extension-mcp) | 2.7k | MCP client: stdio/HTTP transports, persistent process pool, pre-warm, health checks |
| [`astrcode-extension-memory`](crates/astrcode-extension-memory) | 1.6k | Project-scoped markdown memory (disabled by default) |

### Layer 3: Server & Protocol

| Crate | Lines | Description |
|---|---|---|
| [`astrcode-protocol`](crates/astrcode-protocol) | 1.3k | JSON-RPC 2.0 wire types, commands, events, HTTP/UI DTOs |
| [`astrcode-server`](crates/astrcode-server) | 12.2k | Session manager, JSON-RPC/HTTP/ACP handlers, transport, HTTP projection & SSE |

### Layer 4: Clients

| Crate | Lines | Description |
|---|---|---|
| [`astrcode-client`](crates/astrcode-client) | 617 | Typed JSON-RPC client, transport abstraction, stream subscription |
| [`astrcode-cli`](crates/astrcode-cli) | 7.7k | CLI entry: TUI (ratatui), headless exec, server launcher |

### Eval

| Crate | Lines | Description |
|---|---|---|
| [`astrcode-eval`](crates/astrcode-eval) | 1.0k | Benchmark runner: HTTP server control, event-log metrics, structured reports |

### Desktop Shell

| Component | Lines | Description |
|---|---|---|
| [`src-tauri/`](src-tauri) | ~690 | Tauri v2 shell: sidecar management, single-instance coordination, native dialogs |

**Totals:** ~67.6k lines of Rust (21 crates + Tauri), **261** `.rs` files; ~6.3k lines of TypeScript in `frontend/` (~**74k** lines overall).

### Frontend & Desktop App

| Component | Lines | Description |
|---|---|---|
| `frontend/` (React + TS) | ~6.3k | Web frontend — chat view, sidebar, session management, SSE streaming, status bar |
| `src-tauri/` (Tauri v2) | ~670 | Desktop app shell — sidecar management, single-instance coordination, native dialogs |

The web frontend (`frontend/`) is a React 19 + TypeScript + Tailwind CSS v4 + Vite single-page application. It connects to the `astrcode-server` backend via SSE for real-time streaming and JSON-RPC for commands. The frontend supports running standalone in the browser (`npm run dev`) or packaged as a Tauri desktop app (`npm run tauri:dev`).

The Tauri desktop app (`src-tauri/`) wraps the web frontend in a native window and manages the `astrcode-server` as a sidecar process — automatically launching it on startup, discovering a free port, and bridging the connection. It also provides single-instance coordination (file-lock + TCP activation) and native file dialogs via `tauri-plugin-dialog`.

## Key Design Decisions

### Agent Loop

The agent loop (`astrcode-session`) follows a phased pipeline pattern:

1. **Prepare context** — token budget check, auto-compact if needed
2. **Build provider request** — hook dispatch, message assembly, collect tools (MCP tools served from pre-warmed cache; deferred tools activated via `tool_search_tool`)
3. **Stream LLM response** — SSE parsing, UTF-8 safe decoding, event accumulation
4. **Execute tools** — parallel batch execution with pre/post hooks, result persistence
5. **Loop or return** — tool calls loop back; text-only responses terminate

The agent supports running mode switching (Code / Plan). Plan mode restricts tools to read-only and plan management, enforces an exit gate (self-review checklist + required heading validation), and persists the plan artifact to `<session>/plan/plan.md`. Mode instructions are injected via `BeforeProviderRequest`, preserving the system prompt KV cache.

The `ToolPipeline` struct owns tool preprocessing, parallel scheduling, and result persistence. The `SharedTurnContext` struct carries session-level identifiers. `consume_llm_stream` returns a `StreamOutcome` enum (`Complete` | `ToolCalls`) that makes the loop body read as a linear sequence of named phases.

### LLM Provider Layer

`astrcode-ai` supports multiple providers — Anthropic (native Messages API), OpenAI-compatible (Chat Completions + Responses API), and Google GenAI. Key components:

- **`Utf8StreamDecoder`** — handles multi-byte UTF-8 boundaries and bad-byte recovery across TCP chunks
- **`SseLineReader`** — generic SSE line buffering (reusable across all providers)
- **`RetryPolicy`** — exponential backoff with jitter for 429/5xx errors

### Context Window Management

When conversation history approaches 83.5% of the model's context limit, `astrcode-context` triggers automatic compaction:

1. LLM-backed compaction (model generates a structured 9-section summary) runs by default for both auto and manual compact
2. On LLM failure (network error, parse error, timeout), the system falls back to deterministic rule-based summarization
3. A compact circuit breaker temporarily skips auto-compact after consecutive LLM failures, with configurable cooldown
4. Optional predictive compact estimates turn token growth and compacts before the context window is exceeded
5. Compact results are persisted with CAS conflict detection; concurrent writes fail safely instead of corrupting history
6. Compact transcripts are persisted as snapshots for debugging
7. Post-compact context restoration re-reads recent files and preserves agent/skill/tool state
8. **Incremental compact** — when a summary already exists, new compaction merges new information rather than rewriting from scratch

### Tool Execution

Tools run in parallel batches (up to 5 concurrent). The pipeline:

1. **Prepare** — parse JSON args (with repair for malformed LLM output), check visibility, dispatch `PreToolUse` hooks
2. **Execute** — parallel batch via `JoinSet`, sequential tools flush the batch first
3. **Commit** — dispatch `PostToolUse` / `PostToolUseFailure` hooks, persist large results, enforce message budget, emit events

Large tool results are automatically persisted to disk and replaced with preview summaries to stay within the message character budget. Each tool declares an `ExecutionMode`: read-only tools (find/grep/read) are marked Parallel, writing tools (edit/write/shell) are marked Sequential.

### Extension System

The extension system (`astrcode-extensions`) is a core architectural pillar, not an afterthought:

- **Extension trait** — each extension declares hook subscriptions, contributes tools and slash commands, handles lifecycle events
- **Extension SDK** — bundled extensions and extension authors depend on `astrcode-extension-sdk` rather than coupling to host-internal `astrcode-core`
- **Capability declarations** — bundled extensions use `Extension::capabilities()`; WASM extensions declare `requested_capabilities` during the s5r handshake; the runtime authorizes `astrcode.*` invokes via `HostRouter`
- **Namespaced session state** — session-scoped extension state is stored under `<session>/extension_data/<extension-id>/`, keeping the session root owned by the host
- **Hook modes** — `Blocking` (can modify input/output), `NonBlocking` (fire-and-forget), `Advisory` (observe-only)
- **Keybinding registration** — extensions register keyboard shortcuts (e.g. `Shift+Tab` for mode toggle) via `Registrar::keybinding()`
- **Status bar items** — extensions contribute status bar entries (e.g. current mode indicator) with runtime updates via `StatusItemUpdate` notifications
- **WASM extension runtime** — wasmtime sandbox + **s5r symmetric peer** (`peer_exchange`, `handler.invoke`, capability-scoped `astrcode.*` host invokes, multi-step **continuations**); disk extensions require `protocol.s5r: "1.0"` in `extension.json`. See [docs/extension-system.md](docs/extension-system.md)
- **Extension runtime** — session spawning with depth limits, tool registration queue, priority-based dispatch
- **Lifecycle hooks** — `SessionStart` / `SessionResume` / `SessionShutdown`, `TurnStart` / `TurnEnd` / `TurnAborted`, `PreToolUse` / `PostToolUse` / `PostToolUseFailure`, `BeforeProviderRequest` / `AfterProviderResponse`, `PreCompact` / `PostCompact`, `PromptBuild`, `UserPromptSubmit`
- **Extension runtime APIs** — `Extension::start()` (receives `ExtensionCtx` with `startup_working_dir`, `event_sink`, and capability-scoped host services), `Extension::stop()` (with `StopReason`), `Extension::health()` (health probe), `Extension::on_config_changed()` (hot config reload)
- **Active health checks** — `ExtensionRunner::check_health()` provides an on-demand sampling API; polling strategy is decided by the host
- **Startup event channel** — `bind_startup_event_channel()` binds a process-level event channel so extensions can emit custom events during `start()`

### ACP Adapter

The ACP adapter (`astrcode-server::acp`) bridges the standard Agent Client Protocol to astrcode's internal command/broadcast architecture:

- Stdio JSON-RPC server implementing Initialize / NewSession / Prompt / Cancel
- Real-time event streaming via broadcast channel to ACP `SessionNotification`
- Deterministic event flushing with completion oneshot for turn lifecycle
- Designed for IDE extensions and editor integrations

### Event-Sourcing Architecture

AstrCode follows a session-first event-sourcing pattern:

- **EventLog is the single source of truth** — all state changes are immutable, append-only events
- **Session is a projection** — reconstructed by replaying from the event log; fork = replay from a specific sequence number
- **Agent is stateless** — `TurnRunner` is discarded after each turn; state lives in the event log
- **Recovery is replay** — if the agent crashes, the session is intact; simply re-project from the event log

### Prompt Engineering

System prompt assembly follows a pipeline pattern:

```
Identity → System → Task Guidelines → Communication → Environment
→ User Rules → Project Rules → Tool Summary → Extension → Additional
```

Stable sections (Identity, System, Task Guidelines) come first to leverage prompt cache prefix matching. Users can customize via `~/.astrcode/IDENTITY.md` (identity override) and project-level `AGENTS.md` (project rules, searched upward from working directory).

## Running Modes

| Mode | Command | Description |
|---|---|---|
| **TUI** | `cargo run -- tui` | Interactive terminal UI with message history, tool display, slash commands, status bar |
| **Exec** | `cargo run -- exec "prompt"` | Headless single-shot execution, supports `--jsonl`|
| **Server** | `cargo run -- server [--addr 0.0.0.0:3847]` | HTTP/SSE server with JSON-RPC, session management, real-time event streaming |
| **ACP** | `cargo run -- acp` | ACP stdio adapter for IDE/editor integration |
| **Eval** | `cargo run --features dev-mode -- eval` | Run evaluation benchmarks (requires `dev-mode` feature) |
| **Web** | `cd frontend && npm run dev` | Browser-based chat interface connected to the server via SSE |
| **Desktop** | `cd frontend && npm run tauri:dev` | Tauri desktop app (auto-launches server as sidecar) |

### TUI Reference

**Keyboard Shortcuts:**

| Key | Action |
|---|---|
| `Enter` | Submit prompt / accept slash command selection |
| `Shift+Enter` / `Alt+Enter` | Insert newline |
| `Esc` | Close slash palette / stop streaming turn |
| `Tab` | Complete slash command selection |
| `Shift+Tab` | Trigger extension-registered keybinding |
| `Ctrl+A` / `Ctrl+E` | Move to start / end of line |
| `Ctrl+U` / `Ctrl+K` | Delete before / after cursor |
| `Ctrl+W` | Delete previous word |
| `Ctrl+C` | Quit (with confirmation) |

**Slash Commands:**

| Command | Description |
|---|---|
| `/new` | Create a fresh session |
| `/resume <id>` or `/r <id>` | Resume a previous session |
| `/sessions` or `/ls` | Open session picker |
| `/compact` | Compact the current session context |
| `/help` or `/?` | Show command help |
| `/quit` or `/q` | Exit astrcode |

Extensions can register additional slash commands and keybindings at runtime.

## Further Reading

| Document | Description |
|---|---|
| [Project Structure](docs/Project-Structure.md) | Workspace layout, crate layers, HTTP/frontend/Tauri modules |
| [Design Overview](docs/AstrCode-Design.md) | Event-sourcing, compact, tools, extensions, eval |
| [Configuration Guide](docs/configuration.md) | Full `config.json` reference |
| [Extension System](docs/extension-system.md) | Built-in vs WASM extensions, s5r protocol, host capabilities |
| [Session-First](docs/session-first.md) | Event log lifecycle and replay model |

## Distribution

Pre-built binaries are available for Linux, macOS, and Windows (x86_64 + aarch64) via GitHub Releases on every version tag. A weekly automated release pipeline publishes patch bumps every Monday.

**NPM Package**: [`@whatevertogo/astrcode`](https://www.npmjs.com/package/@whatevertogo/astrcode)

## Acknowledgments

This project drew inspiration and design patterns from several open-source projects:

- **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** — tool execution pipeline, system prompt design
- **[OpenCode](https://github.com/anomalyco/opencode)** — the frontend-backend separation (HTTP/SSE + JSON-RPC) references OpenCode's architecture.
- **[Codex CLI](https://github.com/openai/codex)** — TUI layout and terminal UI design borrow from Codex's approach to rendering agent interactions in the terminal.

## License

AGPL-3.0

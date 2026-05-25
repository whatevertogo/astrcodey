# AstrCode

**BE PI OR BETTER THAN PI**  
*Inspired by Claude Code, Codex, OpenCode, and Pi — but built as a Rust-native*

| Interface | Preview |
|-----------|---------|
| **CLI (TUI)** | <img width="1210" height="924" alt="astrcode TUI screenshot" src="https://github.com/user-attachments/assets/55259723-9bd7-4a1a-a74e-1e799ece2eed" /> |
| **Web / Desktop** | <img width="1252" height="960" alt="astrcode web frontend screenshot" src="https://github.com/user-attachments/assets/af918c12-6fb7-4d72-b9ea-64133a2e2729" /> |

A Rust-built AI coding agent platform.

AstrCode is a full-stack AI coding assistant built from scratch in ~55k lines of Rust across 21 crates, plus a React + TypeScript web frontend (~4.8k lines). It features an agent loop with tool execution, a streaming SSE-based multi-provider LLM layer (Anthropic, OpenAI, Google GenAI), an extension/hook system (with native extension loading via FFI and WASM extension support), context window management with auto-compaction, an eval framework for automated benchmarking, and multiple interfaces: a terminal UI (TUI), a web frontend, a Tauri desktop app, an HTTP/SSE API, and an ACP (Agent Client Protocol) adapter.

## Table of Contents

- [Installation](#installation)
- [Configuration (Recommended Before First Run)](#configuration-recommended-before-first-run)
- [Quick Start](#quick-start)
- [Architecture](#architecture)
- [Crates](#crates)
- [Key Design Decisions](#key-design-decisions)
- [Running Modes](#running-modes)
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
      "apiMode": "chatCompletions",
      "models": [
        { "id": "gpt-4.1", "maxTokens": 16384, "contextLimit": 128000 }
      ]
    },
    {
      "name": "deepseek",
      "providerKind": "openai",
      "baseUrl": "https://api.deepseek.com",
      "apiKey": "env:DEEPSEEK_API_KEY",
      "apiMode": "chatCompletions",
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

`~/.astrcode/mcp.json` is used to register external MCP tool servers:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/dir"],
      "env": {}
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_..." }
    }
  }
}
```

Field descriptions:

| Field | Required | Description |
|---|---|---|
| `command` | Yes | Command to start the MCP server |
| `args` | No | Command line arguments array |
| `env` | No | Environment variables to pass to the process |
| `cwd` | No | Working directory (validated to be within workspace in project-level config) |

Project-level MCP configuration (`<workspace>/.astrcode/mcp.json`) overrides global configuration, but requires an environment variable to enable:

```bash
export ASTRCODE_ENABLE_PROJECT_MCP=1
```

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
      "apiKey": "${OPENAI_API_KEY}",
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
cd frontend && npm install && npm run dev

# Tauri desktop app (dev mode)
cd frontend && npm install && npm run tauri:dev

# Eval framework (requires dev-mode feature)
cargo run --features dev-mode -- eval
```

## Configuration

AstrCode uses a JSON-based configuration system stored in `~/.astrcode/config.json`. The configuration supports multiple LLM providers, model selection, runtime behavior tuning, and project-level overrides.

**Key configuration features:**
- Multi-provider support (Anthropic, OpenAI, Google GenAI)
- Separate small LLM configuration for extensions (e.g., memory extraction)
- Project-level config overrides via `.astrcode/config.json`
- Environment variable substitution for API keys
- Runtime behavior tuning (timeouts, retries, compaction, agent limits)

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
    │ OpenAI     │ │Native FFI │ │task tools    │
    │ Google     │ │WASM ext   │ │              │
    │ SSE+retry  │ │           │ │              │
    └────────┬───┘ └─────┬─────┘ └──────────────┘
             │           │
   ┌─────────┴──┐  ┌────┴────────────┐
   │astrcode-   │  │ Extension crates │
   │ context    │  │ ├ mcp            │
   │ Token budget│  │ ├ skill          │
   │ Auto-compact│  │ ├ todo-tool      │
   └────────────┘  │ ├ mode           │
                   │ ├ agent-tools    │
                   │ └ memory         │
                   └─────────────────┘
        ┌─────────────────────────────┐
        │        Shared layer         │
        │ core · protocol · storage   │
        │ support · log               │
        └─────────────────────────────┘
```

## Crates

| Crate | Lines | Description |
|---|---|---|
| `astrcode-server` | 9.5k | Session management, JSON-RPC/HTTP/ACP handlers, transport, concurrency control |
| `astrcode-cli` | 8.0k | Terminal UI (ratatui), headless exec, server launcher |
| `astrcode-session` | 5.2k | Agent loop core: turn runner, tool pipeline, LLM stream consumption, compact orchestration |
| `astrcode-core` | 4.9k | Shared types, traits, config system, error types, prompt composition, extension contracts |
| `astrcode-tools` | 4.6k | Built-in tools: read, write, edit, patch, find, grep, shell, terminal, task |
| `astrcode-storage` | 3.7k | JSONL event log, session snapshots, config persistence, file locking |
| `astrcode-ai` | 3.6k | Multi-provider LLM layer (Anthropic, OpenAI, Google GenAI), SSE streaming, retry |
| `astrcode-context` | 3.5k | Token estimation, context window budgeting, auto-compact, prompt engine |
| `astrcode-extensions` | 2.8k | Extension lifecycle, hook dispatch, native FFI loading, WASM extension runtime |
| `astrcode-extension-mcp` | ~2.4k | MCP protocol client — persistent process pool, background pre-warm, inflight merge, health check |
| `astrcode-protocol` | 1.2k | JSON-RPC 2.0 wire types, commands, events, HTTP DTOs |
| `astrcode-extension-mode` | 1.2k | Agent running mode switching (Code / Plan), plan artifact, exit gate, keybinding & status item registration |
| `astrcode-eval` | 1.1k | Eval framework — HTTP server control, event log metrics, structured reporting |
| `astrcode-extension-skill` | 949 | Slash-command skill discovery and dispatch |
| `astrcode-extension-todo-tool` | 733 | Progress tracking todo list tool |
| `astrcode-extension-agent-tools` | 704 | Sub-agent delegation, agent discovery (Claude Code compatible format) |
| `astrcode-extension-memory` | ~1.8k  | Project-scoped markdown memory storage |
| `astrcode-support` | 682 | Path resolution, shell detection, text processing |
| `astrcode-client` | 521 | Typed JSON-RPC client, transport, stream subscription |
| `astrcode-log` | 353 | File rotation, stderr output, env-filter logging |
| `astrcode-bundled-extensions` | 39 | Composition root for optional extension crates |

**Total: ~57k lines across 20 Rust crates + Tauri shell, 203 source files.**

### Frontend & Desktop App

| Component | Lines | Description |
|---|---|---|
| `frontend/` (React + TS) | ~4.8k | Web frontend — chat view, sidebar, session management, SSE streaming, status bar |
| `src-tauri/` (Tauri v2) | ~670 | Desktop app shell — sidecar management, single-instance coordination, native dialogs |

The web frontend (`frontend/`) is a React 19 + TypeScript + Tailwind CSS v4 + Vite single-page application. It connects to the `astrcode-server` backend via SSE for real-time streaming and JSON-RPC for commands. The frontend supports running standalone in the browser (`npm run dev`) or packaged as a Tauri desktop app (`npm run tauri:dev`).

The Tauri desktop app (`src-tauri/`) wraps the web frontend in a native window and manages the `astrcode-server` as a sidecar process — automatically launching it on startup, discovering a free port, and bridging the connection. It also provides single-instance coordination (file-lock + TCP activation) and native file dialogs via `tauri-plugin-dialog`.

## Key Design Decisions

### Agent Loop

The agent loop (`astrcode-session`) follows a phased pipeline pattern:

1. **Prepare context** — token budget check, auto-compact if needed
2. **Build provider request** — hook dispatch, message assembly, MCP tool discovery
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
3. Compact transcripts are persisted as snapshots for debugging
4. Post-compact context restoration re-reads recent files and preserves agent/skill/tool state
5. **Incremental compact** — when a summary already exists, new compaction merges new information rather than rewriting from scratch

### Tool Execution

Tools run in parallel batches (up to 5 concurrent). The pipeline:

1. **Prepare** — parse JSON args (with repair for malformed LLM output), check visibility, dispatch `PreToolUse` hooks
2. **Execute** — parallel batch via `JoinSet`, sequential tools flush the batch first
3. **Commit** — dispatch `PostToolUse` hooks, persist large results, enforce message budget, emit events

Large tool results are automatically persisted to disk and replaced with preview summaries to stay within the message character budget. Each tool declares an `ExecutionMode`: read-only tools (find/grep/read) are marked Parallel, writing tools (edit/write/shell) are marked Sequential.

### Extension System

The extension system (`astrcode-extensions`) is a core architectural pillar, not an afterthought:

- **Extension trait** — each extension declares hook subscriptions, contributes tools and slash commands, handles lifecycle events
- **Hook modes** — `Blocking` (can modify input/output), `NonBlocking` (fire-and-forget), `Advisory` (observe-only)
- **Keybinding registration** — extensions register keyboard shortcuts (e.g. `Shift+Tab` for mode toggle) via `Registrar::keybinding()`
- **Status bar items** — extensions contribute status bar entries (e.g. current mode indicator) with runtime updates via `StatusItemUpdate` notifications
- **Native extension loading** — disk-loaded `.dll`/`.so` extensions via `libloading` + FFI, supporting global (`~/.astrcode/extensions/`) and project-level (`.astrcode/extensions/`) directories
- **WASM extension runtime** — wasmtime-based sandboxed extension execution with a host-guest protocol for tool registration and event handling
- **Extension runtime** — session spawning with depth limits, tool registration queue, priority-based dispatch
- **Lifecycle hooks** — `SessionStart` / `SessionResume` / `SessionShutdown`, `TurnStart` / `TurnEnd` / `TurnAborted`, `PreToolUse` / `PostToolUse` / `PostToolUseFailure`, `BeforeProviderRequest` / `AfterProviderResponse`, `PreCompact` / `PostCompact`, `PromptBuild`, `UserPromptSubmit`
- **Extension runtime APIs** — `Extension::start()` (receives `ExtensionCtx` with `startup_working_dir` and `event_sink`), `Extension::stop()` (with `StopReason`), `Extension::health()` (health probe), `Extension::on_config_changed()` (hot config reload)
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

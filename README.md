# AstrCode

A Rust-built AI coding agent platform.

AstrCode is a full-stack AI coding assistant built from scratch in ~49k lines of Rust across 18 crates, plus a React + TypeScript web frontend (~2.8k lines). It features an agent loop with tool execution, a streaming SSE-based LLM provider layer, a plugin/hook extension system (with native extension loading via FFI and WASM extension support), context window management with auto-compaction, and multiple interfaces: a terminal UI, a web frontend, a Tauri desktop app, an HTTP/SSE API, and an ACP (Agent Client Protocol) adapter.

> **Why?** I wanted to understand how an AI coding agent works at every layer — from SSE stream parsing to context window compaction — so I built one. The architecture draws on engineering practices from several coding agents, but all code is original.

## Quick Start

```bash
# Nightly Rust required
rustup toolchain install nightly

# Build backend
cargo build

# Interactive terminal UI
cargo run -- tui

# Headless single-shot execution
cargo run -- exec "explain the agent loop architecture"

# HTTP/SSE server
cargo run --bin astrcode-server

# Web frontend (dev server)
cd frontend && npm install && npm run dev

# Tauri desktop app (dev mode)
cd frontend && npm install && npm run tauri:dev
```

## Architecture

```
          ┌──────────┐  ┌──────────────────┐  ┌───────────┐
          │   TUI    │  │ Web / Tauri Frontend│  │ ACP Client│
          │ (ratatui)│  │ React + TypeScript │  │  (stdio)  │
          └────┬─────┘  └────────┬──────────┘  └─────┬─────┘
               │                  │ SSE / JSON-RPC     │ ACP JSON-RPC
               │    stdio         │                    │ over stdio
               └────────┬────────┘────────────────────┘
                   ┌─────┴──────┐
                   │astrcode-cli │  TUI / exec / server launcher
                   └─────┬──────┘
                         │
                   ┌─────┴──────┐
                   │astrcode-   │  Agent loop, session manager, JSON-RPC + HTTP handler
                   │server      │  ACP adapter, transport, concurrency control
                   └─────┬──────┘
             ┌───────────┼───────────┐
             │           │           │
    ┌────────┴───┐ ┌─────┴─────┐ ┌───┴──────────┐
    │ astrcode-ai│ │astrcode-  │ │ astrcode-    │
    │            │ │extensions │ │ tools        │
    │ LLM provider│ │Hook system│ │File/shell/   │
    │ SSE+retry  │ │Native FFI │ │task tools    │
    └────────┬───┘ │WASM ext   │ └──────────────┘
             │     └─────┬─────┘
   ┌─────────┴──┐  ┌──────┴──────────┐
   │astrcode-   │  │ Extension crates │
   │ context    │  │ ├ mcp            │
   │ Token budget│  │ ├ skill         │
   │ Auto-compact│  │ ├ todo-tool     │
   └────────────┘  │ ├ mode          │
                   │ └ agent-tools   │
                   └─────────────────┘
        ┌─────────────────────────────┐
        │        Shared layer         │
        │ core · protocol · storage   │
        │ support · log · session     │
        └─────────────────────────────┘
```

## Crates

| Crate | Lines | Description |
|---|---|---|
| `astrcode-server` | 9.1k | Agent loop, session management, JSON-RPC/HTTP/ACP handlers, transport, concurrency control |
| `astrcode-cli` | 6.7k | Terminal UI (ratatui), headless exec, server launcher |
| `astrcode-session` | 3.9k | Session runtime: session handle, turn execution, event bus |
| `astrcode-tools` | 4.5k | Built-in tools: read, write, edit, patch, find, grep, shell, task |
| `astrcode-core` | 4.3k | Shared types, traits, config system, error types, prompt composition |
| `astrcode-ai` | 3.5k | OpenAI-compatible provider (Chat Completions + Responses API), SSE streaming, retry |
| `astrcode-context` | 3.3k | Token estimation, context window budgeting, auto-compact, prompt engine |
| `astrcode-storage` | 3.0k | JSONL event log, session snapshots, config persistence, file locking |
| `astrcode-extensions` | 2.4k | Extension lifecycle, hook dispatch, native extension loading (FFI), WASM extension runtime |
| `astrcode-extension-mcp` | 1.9k | MCP protocol client via stdio, tool discovery |
| `astrcode-protocol` | 1.1k | JSON-RPC 2.0 wire types, commands, events, HTTP DTOs |
| `astrcode-extension-mode` | 1.1k | Agent running mode switching (Code / Plan), plan artifact, exit gate |
| `astrcode-extension-skill` | 950 | Slash-command skill discovery and dispatch |
| `astrcode-support` | 929 | Path resolution, shell detection, tool result persistence |
| `astrcode-extension-agent-tools` | 914 | Sub-agent delegation (Agent tool) |
| `astrcode-extension-todo-tool` | 734 | Progress tracking todo list tool |
| `astrcode-client` | 521 | Typed JSON-RPC client, transport, stream subscription |
| `astrcode-log` | 353 | File rotation, stderr output, env-filter logging |

**Total: ~49k lines across 18 Rust crates, 153 source files.**

### Frontend & Desktop App

| Component | Lines | Description |
|---|---|---|
| `frontend/` (React + TS) | ~2.8k | Web frontend — chat view, sidebar, session management, SSE streaming |
| `src-tauri/` (Tauri v2) | ~670 | Desktop app shell — sidecar management, native dialogs, auto port binding |

The web frontend (`frontend/`) is a React 19 + TypeScript + Tailwind CSS v4 + Vite 8 single-page application. It connects to the `astrcode-server` backend via SSE for real-time streaming and JSON-RPC for commands. The frontend supports running standalone in the browser (`npm run dev`) or packaged as a Tauri desktop app (`npm run tauri dev`).

The Tauri desktop app (`src-tauri/`) wraps the web frontend in a native window and manages the `astrcode-server` as a sidecar process — automatically launching it on startup, discovering a free port, and bridging the connection. It also provides native file dialogs via `tauri-plugin-dialog`.

## Key Design Decisions

### Agent Loop

The agent loop (`astrcode-server/src/agent/`) follows a phased pipeline pattern:

1. **Prepare context** — token budget check, auto-compact if needed
2. **Build provider request** — hook dispatch, message assembly, MCP tool discovery
3. **Stream LLM response** — SSE parsing, UTF-8 safe decoding, event accumulation
4. **Execute tools** — parallel batch execution with pre/post hooks, result persistence
5. **Loop or return** — tool calls loop back; text-only responses terminate

The agent supports running mode switching (Code / Plan). Plan mode restricts tools to read-only and plan management, enforces an exit gate (self-review checklist + required heading validation), and persists the plan artifact to `<session>/plan/plan.md`. Mode instructions are injected via `BeforeProviderRequest`, preserving the system prompt KV cache.

The `ToolPipeline` struct owns tool preprocessing, parallel scheduling, and result persistence. The `SharedTurnContext` struct carries session-level identifiers. `consume_llm_stream` returns a `StreamOutcome` enum (`Complete` | `ToolCalls`) that makes the loop body read as a linear sequence of named phases.

### LLM Provider Layer

`astrcode-ai` supports both OpenAI Chat Completions and Responses API modes. Key components:

- **`Utf8StreamDecoder`** — handles multi-byte UTF-8 boundaries and bad-byte recovery across TCP chunks
- **`SseLineReader`** — generic SSE line buffering (reusable for any future provider)
- **`LlmAccumulator`** — OpenAI-specific event accumulation (tool call tracking, content delta merging)
- **`RetryPolicy`** — exponential backoff with jitter for 429/5xx errors

### Context Window Management

When conversation history approaches 83.5% of the model's context limit, `astrcode-context` triggers automatic compaction:

1. Deterministic compaction (rule-based summarization) runs by default
2. Provider-backed compaction (LLM generates summary) is attempted when available
3. Compact transcripts are persisted as snapshots for debugging
4. Consecutive provider failures fall back to deterministic mode

### Tool Execution

Tools run in parallel batches (up to 5 concurrent). The pipeline:

1. **Prepare** — parse JSON args (with repair for malformed LLM output), check visibility, dispatch `PreToolUse` hooks
2. **Execute** — parallel batch via `JoinSet`, sequential tools flush the batch first
3. **Commit** — dispatch `PostToolUse` hooks, persist large results, enforce message budget, emit events

Large tool results are automatically persisted to disk and replaced with preview summaries to stay within the message character budget.

### Extension System

The extension system (`astrcode-extensions`) is a core architectural pillar, not an afterthought:

- **Extension trait** — each extension declares hook subscriptions, contributes tools and slash commands, handles lifecycle events
- **Hook modes** — `Blocking` (can modify input/output), `NonBlocking` (fire-and-forget), `Advisory` (observe-only)
- **Native extension loading** — disk-loaded `.dll`/`.so` extensions via `libloading` + FFI, supporting global (`~/.astrcode/extensions/`) and project-level (`.astrcode/extensions/`) directories
- **WASM extension runtime** — wasmtime-based sandboxed extension execution with a host-guest protocol for tool registration and event handling
- **Extension runtime** — session spawning with depth limits, tool registration queue, priority-based dispatch

### ACP Adapter

The ACP adapter (`astrcode-server::acp`) bridges the standard Agent Client Protocol to astrcode's internal command/broadcast architecture:

- Stdio JSON-RPC server implementing Initialize / NewSession / Prompt / Cancel
- Real-time event streaming via broadcast channel to ACP `SessionNotification`
- Deterministic event flushing with completion oneshot for turn lifecycle
- Designed for IDE plugins and editor integrations

## Running Modes

| Mode | Command | Description |
|---|---|---|
| **TUI** | `cargo run -- tui` | Interactive terminal UI with message history, tool display, slash commands |
| **Exec** | `cargo run -- exec "prompt"` | Headless single-shot execution, supports `--jsonl` streaming output |
| **Server** | `cargo run --bin astrcode-server` | HTTP/SSE server with JSON-RPC, session management, real-time event streaming |
| **Web** | `cd frontend && npm run dev` | Browser-based chat interface connected to the server via SSE |
| **Desktop** | `cd frontend && npm run tauri:dev` | Tauri desktop app (auto-launches server as sidecar) |

## Acknowledgments

This project drew inspiration and design patterns from several open-source projects:

- **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** — tool execution pipeline, system prompt design
- **[OpenCode](https://github.com/anomalyco/opencode)** — the frontend-backend separation (HTTP/SSE + JSON-RPC) references OpenCode's architecture.
- **[Codex CLI](https://github.com/openai/codex)** — TUI layout and terminal UI design borrow from Codex's approach to rendering agent interactions in the terminal.
- **[pi-mono](https://github.com/anthropics/pi-mono)** — the plugin extension model and lifecycle hook design were influenced by pi-mono's composable, event-driven extension approach.

## License

AGPL-3.0

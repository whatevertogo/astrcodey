# AstrCode

A Rust-built AI coding agent platform.

AstrCode is a full-stack AI coding assistant built from scratch in ~40k lines of Rust across 17 crates. It features an agent loop with tool execution, a streaming SSE-based LLM provider layer, a plugin/hook extension system, context window management with auto-compaction, and both a terminal UI and HTTP/SSE API.

> **Why?** I wanted to understand how an AI coding agent works at every layer — from SSE stream parsing to context window compaction — so I built one. The architecture draws on engineering practices from several coding agents, but all code is original.

## Quick Start

```bash
# Nightly Rust required
rustup toolchain install nightly

# Build
cargo build

# Interactive terminal UI
cargo run -- tui

# Headless single-shot execution
cargo run -- exec "explain the agent loop architecture"

# HTTP/SSE server
cargo run --bin astrcode-server
```

## Architecture

```
                    ┌─────────────┐
                    │  astrcode-cli │  TUI / exec / server launcher
                    └──────┬──────┘
                           │
                    ┌──────┴──────┐
                    │ astrcode-server│  Agent loop, session manager, JSON-RPC + HTTP handler
                    └──────┬──────┘
              ┌────────────┼────────────┐
              │            │            │
     ┌────────┴───┐ ┌─────┴─────┐ ┌───┴──────────┐
     │ astrcode-ai │ │astrcode-  │ │ astrcode-    │
     │             │ │extensions │ │ tools        │
     │ LLM provider│ │Hook system│ │File/shell/   │
     │ SSE+retry   │ │Plugin SDK │ │agent tools   │
     └────────┬───┘ └─────┬─────┘ └──────────────┘
              │            │
    ┌─────────┴──┐  ┌──────┴──────────┐
    │astrcode-   │  │ Extension crates │
    │ context    │  │ ├ mcp            │
    │ Token budget│  │ ├ skill         │
    │ Auto-compact│  │ ├ todo-tool     │
    └────────────┘  │ └ agent-tools   │
                    └─────────────────┘
         ┌─────────────────────────────┐
         │        Shared layer         │
         │ core · protocol · storage   │
         │ support · log · prompt      │
         └─────────────────────────────┘
```

## Crates

| Crate | Lines | Description |
|---|---|---|
| `astrcode-server` | 10.7k | Agent loop, session management, JSON-RPC/HTTP handler |
| `astrcode-cli` | 5.9k | Terminal UI (ratatui), headless exec, server launcher |
| `astrcode-tools` | 4.0k | Built-in tools: read, write, edit, patch, find, grep, shell |
| `astrcode-core` | 3.2k | Shared types, traits, config system, error types |
| `astrcode-extensions` | 3.0k | Extension lifecycle, hook dispatch, plugin loading |
| `astrcode-storage` | 2.1k | JSONL event log, session snapshots, file locking |
| `astrcode-context` | 2.1k | Token estimation, context window budgeting, auto-compact |
| `astrcode-extension-mcp` | 1.8k | MCP protocol client via stdio, tool discovery |
| `astrcode-ai` | 1.6k | OpenAI-compatible provider (Chat Completions + Responses API) |
| `astrcode-prompt` | 839 | System prompt composition from extension contributions |
| `astrcode-protocol` | 848 | JSON-RPC 2.0 wire types, commands, events, HTTP DTOs |
| `astrcode-support` | 831 | Path resolution, shell detection, tool result persistence |
| `astrcode-extension-skill` | 829 | Slash-command skill discovery and dispatch |
| `astrcode-extension-todo-tool` | 743 | Progress tracking todo list tool |
| `astrcode-extension-agent-tools` | 586 | Sub-agent delegation (Agent tool) |
| `astrcode-client` | 496 | Typed JSON-RPC client, transport, stream subscription |
| `astrcode-log` | 344 | File rotation, stderr output, env-filter logging |

**Total: ~40k lines across 17 crates, 135 source files.**

## Key Design Decisions

### Agent Loop

The agent loop (`astrcode-server/src/agent/`) follows a phased pipeline pattern:

1. **Prepare context** — token budget check, auto-compact if needed
2. **Build provider request** — hook dispatch, message assembly, MCP tool discovery
3. **Stream LLM response** — SSE parsing, UTF-8 safe decoding, event accumulation
4. **Execute tools** — parallel batch execution with pre/post hooks, result persistence
5. **Loop or return** — tool calls loop back; text-only responses terminate

The `ToolPipeline` struct owns tool preprocessing, parallel scheduling, and result persistence. The `SharedTurnContext` struct carries session-level identifiers. `consume_llm_stream` returns a `StreamOutcome` enum (`Complete` | `ToolCalls`) that makes the loop body read as a linear sequence of named phases.

### Extension / Hook System

Extensions subscribe to lifecycle events and can intercept at hook points:

- `PreToolUse` / `PostToolUse` — inspect, modify, or block tool execution
- `BeforeProviderRequest` / `AfterProviderResponse` — modify messages or block LLM calls
- `PreCompact` / `PostCompact` — inject compaction instructions
- `PromptBuild` — contribute system prompt fragments

Extensions ship as WASM-style sandboxed units loaded from `extension.json` manifests.

### LLM Provider Layer

`astrcode-ai` supports both OpenAI Chat Completions and Responses API modes. Key components:

- **`Utf8StreamDecoder`** — handles multi-byte UTF-8 boundaries and bad-byte recovery across TCP chunks
- **`SseLineReader`** — generic SSE line buffering (reusable for any future provider)
- **`LlmAccumulator`** — OpenAI-specific event accumulation (tool call tracking, content delta merging)
- **`RetryPolicy`** — exponential backoff with jitter for 429/5xx errors

### Context Window Management

When conversation history approaches the model's context limit, `astrcode-context` triggers automatic compaction:

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

## Running Modes

| Mode | Command | Description |
|---|---|---|
| **TUI** | `cargo run -- tui` | Interactive terminal UI with message history, tool display, slash commands |
| **Exec** | `cargo run -- exec "prompt"` | Headless single-shot execution, supports `--jsonl` streaming output |
| **Server** | `cargo run --bin astrcode-server` | HTTP/SSE server with JSON-RPC, session management, real-time event streaming |

## License

MIT

## Acknowledgments

This project drew inspiration and design patterns from several open-source projects:

- **[Claude Code](https://docs.anthropic.com/en/docs/claude-code)** — tool execution pipeline, system prompt design, and the extension hook architecture were heavily informed by studying Claude Code's public documentation and design philosophy.
- **[OpenCode](https://github.com/anomalyco/opencode)** — the frontend-backend separation (HTTP/SSE + JSON-RPC) and the session/event streaming model reference OpenCode's architecture.
- **[Codex CLI](https://github.com/openai/codex)** — the TUI layout and terminal UI design borrow from Codex's approach to rendering agent interactions in the terminal.
- **[pi-mono](https://github.com/badlogic/pi-mono)** — the plugin extension model and lifecycle hook design were influenced by pi-mono's approach to composable, event-driven extensions.

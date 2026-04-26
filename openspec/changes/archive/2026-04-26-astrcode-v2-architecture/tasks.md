## 1. Workspace Foundation

- [x] 1.1 Create workspace `Cargo.toml` at repo root with `[workspace]` members pointing to `crates/*`
- [x] 1.2 Create `crates/` directory with empty crate skeletons for all 14 crates
- [x] 1.3 Add workspace-level `[workspace.dependencies]` for shared deps: tokio, serde, serde_json, thiserror, async-trait, uuid, chrono, tracing

## 2. Layer 0 — astrcode-core

- [x] 2.1 Define core shared types: `SessionId`, `EventId`, `TurnId`, `MessageId`, `ToolCallId`, `Cursor`, `Timestamp`, `ProjectHash`
- [x] 2.2 Define `Tool` trait with `definition()` and `execute()` signatures
- [x] 2.3 Define `ToolDefinition`, `ToolResult`, `ToolError` types
- [x] 2.4 Define `LlmMessage`, `LlmRole`, `LlmContent` types for LLM message representation
- [x] 2.5 Define `LlmProvider` trait with `generate()` streaming signature
- [x] 2.6 Define `PromptProvider` trait and `BlockSpec`/`PromptBlock` types
- [x] 2.7 Define `EventStore` trait for session persistence
- [x] 2.8 Define `CapabilitySpec`, `CapabilityKind` for tool/capability metadata (used by extensions for tool/skill registration)
- [x] 2.9 Define `AgentProfile` type (basic, used by agent collaboration tools: spawn/send/observe/close)
- [x] 2.10 Define `Config` types: `Config`, `ConfigOverlay`, `Profile`, `ModelConfig`, `RuntimeConfig` (~30 Option fields), `AgentConfig`
- [x] 2.11 Define `OpenAiApiMode` enum (ChatCompletions/Responses)
- [x] 2.12 Define `ConfigStore` trait: load, save, path, load_overlay, save_overlay
- [x] 2.13 Define `ActiveSelection` and `ModelSelection` types
- [x] 2.14 Define `Extension` trait: `id()`, `events()`, `mode()`, `on_event()` — extension types live in core, runner lives in extensions crate
- [ ] 2.15 Core tests: trait object safety, Send+Sync bounds verification

## 3. Layer 0 — astrcode-support

- [x] 3.1 Implement `hostpaths`: `resolve_home_dir()`, `astrcode_dir()`, `projects_dir()`, `project_dir()`
- [x] 3.2 Implement `shell`: `ShellFamily` enum (Posix/PowerShell/Cmd/Wsl), `resolve_shell()` with env override
- [x] 3.3 Implement `tool_results`: `persist_tool_result()`, `maybe_persist_tool_result()` with path traversal prevention
- [ ] 3.4 Support tests: path resolution, shell detection

## 4. astrcode-protocol

- [x] 4.1 Define `ClientCommand` enum: create_session, resume_session, fork_session, delete_session, list_sessions, submit_prompt, abort, set_model, set_thinking_level, compact, switch_mode, get_state, ui_response
- [x] 4.2 Define `ServerEvent` enum: session_created, session_resumed, session_deleted, session_list, agent_started, agent_ended, turn_started, turn_ended, message_start, message_delta, message_end, tool_call_start, tool_call_delta, tool_call_end, compaction_started, compaction_ended, ui_request, error
- [x] 4.3 Define UI sub-protocol types: `UiRequestKind` (confirm/select/input/notify), `UiResponseValue`
- [x] 4.4 Define `SessionSnapshot` DTO for reconnection/recovery
- [x] 4.5 Define protocol error codes: standard JSON-RPC + domain codes
- [x] 4.6 Implement JSONL framing: one JSON per line, newline delimiter
- [x] 4.7 Implement version negotiation handshake
- [x] 4.8 Protocol conformance tests: round-trip for all command/event variants

## 5. astrcode-storage

- [x] 5.1 Implement `EventLog`: append-only JSONL writer with `create()`, `append(event)`, `tail_scan()`
- [x] 5.2 Implement `EventLogIterator`: streaming JSONL reader with line-number tracking
- [x] 5.3 Implement `BatchAppender`: async buffered writer with configurable flush window (50ms)
- [x] 5.4 Implement `FileSystemSessionRepository` implementing `EventStore` trait: create, append, replay, recover, checkpoint, list, delete
- [x] 5.5 Implement snapshot system: create at event offset, list, recover from snapshot + tail
- [x] 5.6 Implement turn-level file locking via `fs2`
- [x] 5.7 Implement session ID validation: alphanumeric + hyphen + underscore + 'T' only
- [x] 5.8 Implement `FileConfigStore`: atomic JSON config writes (temp file + rename)
- [x] 5.9 Storage tests: concurrent append, snapshot recovery, lock contention, invalid ID rejection

## 6. astrcode-ai

- [x] 6.1 Implement `OpenAiProvider` struct implementing `LlmProvider` trait
- [x] 6.2 Implement SSE parser for Chat Completions format (`data: {...}` lines)
- [x] 6.3 Implement SSE parser for Responses API format (`event:` + `data:` blocks)
- [x] 6.4 Implement `Utf8StreamDecoder` for multi-byte safe streaming UTF-8 decoding
- [x] 6.5 Implement `LlmAccumulator` assembling SSE deltas into `LlmOutput`
- [x] 6.6 Implement exponential backoff retry (retryable: 408, 429, 5xx; max 3 attempts)
- [x] 6.7 Implement `CacheTracker`: two-stage cache invalidation, block/tool hashing
- [x] 6.8 Implement `LlmClientConfig`: timeout, retry count, base URL, extra headers
- [x] 6.9 Implement tool definition sorting for cache stability
- [ ] 6.10 AI tests: mock HTTP server, SSE parsing, retry behavior, cache detection

## 7. astrcode-prompt

- [x] 7.1 Implement `PromptContributor` trait with caching support (contributor_id, cache_version, cache_fingerprint, contribute)
- [x] 7.2 Implement `BlockSpec`/`PromptBlock` types: priority, layer, conditions, dependencies, metadata
- [x] 7.3 Implement `PromptComposer`: collect → deduplicate → condition-filter → topological-sort → render → validate
- [x] 7.4 Implement `PromptPlan` output: system_blocks, prepend_messages, append_messages, extra_tools
- [x] 7.5 Implement 4-layer system: `PromptLayer` enum (Stable/SemiStable/Inherited/Dynamic)
- [x] 7.6 Implement `LayeredPromptBuilder`: partition contributors into 4 layers with independent TTL
- [x] 7.7 Implement `PromptTemplate`: `{{variable}}` engine with 4-tier resolution
- [x] 7.8 Implement built-in variables: os, date, shell, working_dir, available_tools
- [x] 7.9 Implement `PromptDiagnostics`: block-skipped, dependency-missing, template-render-failure tracking
- [x] 7.10 Implement 6 built-in contributors: Identity, Environment, AgentsMd, Capability, ResponseStyle, SystemInstruction
- [x] 7.11 Note: AgentProfileSummary and SkillSummary contributors are registered by extensions at SessionStart
- [x] 7.12 Implement `ComposerPromptProvider` adapting to `PromptProvider` trait
- [ ] 7.13 Prompt tests: contributor caching, dependency ordering, template rendering, diagnostics

## 8. astrcode-tools

- [x] 8.1 Implement `ToolRegistry` with registration, lookup, capability metadata
- [x] 8.2 Implement event emission wrapper around tool execution (ToolCallStart → execute → ToolCallDelta → ToolCallEnd)
- [x] 8.3 Implement `readFile`: path normalization, binary/image/PDF detection, chunked reading, UTF-8 output
- [x] 8.4 Implement `writeFile`: file creation/overwrite, text-only validation
- [x] 8.5 Implement `editFile`: unique string replacement with exact match semantics
- [x] 8.6 Implement `applyPatch`: multi-file patch application
- [x] 8.7 Implement `findFiles`: glob pattern matching
- [x] 8.8 Implement `grep`: regex content search with file filtering
- [x] 8.9 Implement `shell`: streaming subprocess execution, stdout/stderr capture, timeout (120s)
- [x] 8.10 Implement `toolSearch`: pattern-based tool discovery (queries extensions for registered tools)
- [x] 8.11 Implement `skillTool`: delegates skill loading to extensions; core provides the tool definition only
- [x] 8.12 Implement `taskWrite`: runtime task snapshot write
- [x] 8.13 Implement `enterPlanMode`/`exitPlanMode`: mode switching
- [x] 8.14 Implement `upsertSessionPlan`: plan artifact persistence
- [x] 8.15 Implement Agent collaboration tools: spawn, send, observe, close
- [x] 8.16 Implement per-tool execution mode (sequential/parallel) with concurrent execution for parallel tools
- [ ] 8.17 Tools tests: each tool tested independently, tool registry, concurrency for parallel tools

## 9. astrcode-context

- [x] 9.1 Implement `TokenUsageTracker`: anchor budget to provider actuals, `estimate_request_tokens()` with 4/3 multiplier
- [x] 9.2 Implement `ToolResultBudget`: aggregate byte budget, persist oversized results, track replacements
- [x] 9.3 Implement `MicroCompact`: clear stale tool results during idle periods
- [x] 9.4 Implement `PrunePass`: truncate oversized tool results, remove from oldest turns
- [x] 9.5 Implement `CompactConfig` and `CompactResult`: retention settings, pre/post token stats
- [x] 9.6 Implement LLM-driven compaction: build compact prompt, call LLM, validate XML output
- [x] 9.7 Implement compact output sanitization: strip sensitive IDs, add boundary markers
- [x] 9.8 Implement `FileAccessTracker`: record readFile calls, re-inject files post-compaction
- [x] 9.9 Implement `ContextWindowSettings` mapped from runtime config
- [ ] 9.10 Context tests: token estimation, budget enforcement, compaction XML parsing, sanitization

## 10. astrcode-extensions

- [x] 10.1 Define `ExtensionEvent` enum with 12 core lifecycle events
- [x] 10.2 Define `HookMode` enum: Blocking, NonBlocking, Advisory
- [x] 10.3 Define `HookEffect` enum: Allow, Block { reason }, Modify { patches }
- [x] 10.4 Define `ExtensionCapabilities`: events (with mode), tools, slash_commands, context_providers
- [x] 10.5 Implement `ExtensionRunner`: dispatch events to all registered extensions, enforce HookMode, handle timeouts (30s default)
- [x] 10.6 Implement `ExtensionLoader`: load global extensions from `~/.astrcode/extensions/`, project extensions from `.astrcode/extensions/`
- [x] 10.7 Implement extension merging: global + project-level, project handlers run first
- [x] 10.8 Implement `ExtensionContext`: restricted view of session + services for extension handlers
- [x] 10.9 Document that skills, agent profiles, custom behaviors are all implemented as extensions
- [ ] 10.10 Extensions tests: event dispatch, blocking/nonblocking/advisory mode, timeout, merge priority

## 11. astrcode-server

- [x] 11.1 Implement `ConfigService`: load config, resolve defaults, apply overlay, track active profile/model
- [x] 11.2 Implement `env_resolver`: parse `env:VAR`/optional-env/literal env values
- [x] 11.3 Implement config validation + migration (auto-migrate old formats on load)
- [x] 11.4 Implement profile/model selection with fallback and warning emission
- [x] 11.5 Implement config hot-reload via filesystem watch (notify crate)
- [x] 11.6 Implement `SessionManager`: create, resume, fork, switch, delete, list operations
- [x] 11.7 Implement `Session` struct: event_log, state (RwLock), agent (Mutex<Option<AgentHandle>>), subscribers
- [x] 11.8 Implement `SessionState`: messages, tool_results, context, model_config, thinking_level, cursor
- [x] 11.9 Implement `Agent` struct: created from Session events via replay, process_turn() pipeline
- [x] 11.10 Implement **agent loop**: prompt→UserPromptSubmit hooks→prompt assembly→LLM call→tool execution loop→event append→TurnEnd hooks
- [x] 11.11 Implement tool orchestration within loop: parse tool calls, dispatch BeforeToolCall/AfterToolCall, handle parallel/sequential, enforce max continuations
- [x] 11.12 Implement turn abortion: cancel LLM stream, terminate running tools, preserve partial results
- [x] 11.13 Implement error recovery: retry on LLM errors, report tool errors to LLM, abort on max_consecutive_failures
- [x] 11.14 Implement `StdioTransport`: read JSON-RPC commands from stdin, write events to stdout
- [x] 11.15 Implement `ServerTransport` trait for future WebSocket support
- [x] 11.16 Implement multi-session concurrency: tokio task per session
- [x] 11.17 Implement graceful shutdown: flush events, emit SessionShutdown
- [x] 11.18 Implement server main entry point: transport setup, service init, run loop
- [ ] 11.19 Server tests: config resolution, session lifecycle, agent loop, turn processing, multi-session concurrency, shutdown

## 12. astrcode-client

- [x] 12.1 Implement `ClientTransport` trait: `execute()` (request-response), `open_sse()` (SSE stream)
- [x] 12.2 Implement `StdioTransport` using stdin/stdout child process communication
- [x] 12.3 Implement `AstrcodeClient<T: ClientTransport>`: typed methods for all commands
- [x] 12.4 Implement auth state management: token exchange, expiry detection, auto-refresh
- [x] 12.5 Implement `ConversationStream`: broadcast channel, Delta/RehydrateRequired/Lagged/Disconnected
- [x] 12.6 Implement `ClientError` with `ClientErrorKind` classification
- [x] 12.7 Implement `MockTransport` for testing
- [ ] 12.8 Client tests: all command methods, stream subscription, error classification

## 13. astrcode-tui

- [x] 13.1 Implement `CliState`: composite state (session, transcript, composer, interaction, stream_view, theme)
- [x] 13.2 Implement `Action` enum: Tick, Key, Paste, Resize, Quit, plus session/stream actions
- [x] 13.3 Implement `AppController`: event loop, action dispatch, dirty-flag tracking
- [x] 13.4 Implement chat surface rendering: scrollable blocks (markdown, code, tool calls, thinking), streaming preview area
- [x] 13.5 Implement bottom pane: composer input, palette (slash commands, model select), mode indicator
- [x] 13.6 Implement session list panel: session info, switch on select
- [x] 13.7 Implement theme system: TrueColor → ANSI256 → ANSI16 → ASCII degradation
- [x] 13.8 Implement keyboard input handling: key → Action mapping
- [x] 13.9 Implement slash command parsing: command recognition, fuzzy matching, completion
- [x] 13.10 Implement stream pacer: smooth vs catch-up modes
- [x] 13.11 Implement launcher: spawn server binary, wait for ready, connect
- [ ] 13.12 TUI tests: state transitions, render output for known states, command parsing, theme degradation

## 14. astrcode-exec

- [x] 14.1 Implement headless exec mode: accept prompt, send to server, stream results, exit
- [x] 14.2 Implement `--jsonl` output mode: each server event as JSONL line
- [x] 14.3 Implement `--text` output mode: human-readable final response only
- [x] 14.4 Implement `--timeout` parameter: max execution duration
- [x] 14.5 Implement `--session-id` parameter: resume existing session
- [x] 14.6 Implement `--no-save` flag: discard session after completion
- [x] 14.7 Implement auto-response for UI requests with `--auto-approve` override
- [ ] 14.8 Exec tests: text/jsonl output, timeout, session resume, no-save

## 15. astrcode-cli

- [x] 15.1 Implement multi-subcommand CLI using clap: tui, exec, server (standalone), version, config
- [x] 15.2 Implement `server` subcommand: start server in standalone mode
- [x] 15.3 Implement global flags: --server-origin, --token, --working-dir, --verbose
- [x] 15.4 Implement version command: display crate version, protocol version, build info
- [ ] 15.5 CLI integration tests: subcommand dispatch, flag parsing, server lifecycle

## 16. Documentation

- [x] 16.1 Create `docs/architecture.md`: full architecture overview, crate dependency diagram, extension-first philosophy
- [x] 16.2 Create `docs/session-first.md`: session-first design, event sourcing, agent lifecycle
- [x] 16.3 Create `docs/protocol.md`: JSON-RPC protocol specification
- [x] 16.4 Create `docs/extensions.md`: extension API, lifecycle events, HookMode, how to build skills/agents as extensions
- [x] 16.5 Create `docs/crates.md`: per-crate quick reference
- [x] 16.6 Create `docs/development.md`: build instructions, test commands, workspace conventions
- [x] 16.7 Add TODO comments for deferred: sandbox, WebSocket transport, web UI, MCP

## 17. Integration & Cleanup

- [x] 17.1 Run full workspace build: `cargo build --workspace`
- [ ] 17.2 Run full workspace tests: `cargo test --workspace`
- [ ] 17.3 Run workspace linting: `cargo clippy --workspace`
- [ ] 17.4 Integration test: server startup → client connect → create session → submit prompt → receive response
- [ ] 17.5 Integration test: two clients → two concurrent sessions → independent processing
- [ ] 17.6 Integration test: session fork → branch independence → session tree listing
- [ ] 17.7 Remove old `adapter-*/`, `cli/`, `client/`, `protocol/`, `context-window/`, `support/` directories
- [ ] 17.8 Final verification: `cargo test --workspace`, `cargo build --workspace --release`

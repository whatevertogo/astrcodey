# AstrCode Extension Hook Matrix

This file is the contract for AstrCode extension hook semantics. Keep it in sync
with `astrcode-core::extension`, `astrcode-extensions::runner`, and session call
sites.

## Capability Boundary

Capabilities are sensitive host API permissions, not ordinary hook context.

Default session context/API, no manifest capability required:

| Context or API | Semantics |
| --- | --- |
| `session_store_dir` in hook/tool/command context | Present when the dispatch is bound to a session store. |
| `astrcode.session.state.read` | Reads state namespaced by current session and extension id. |
| `astrcode.session.state.write` | Writes state namespaced by current session and extension id. |

Sensitive APIs still require capabilities: `session_control`, `session_history`,
`main_model`, `small_model`, `emit_events`, `workspace_read`, `process_spawn`,
and `network_client`.

## Hook Families

| Hook | Registration API | Runtime entry | Result semantics |
| --- | --- | --- | --- |
| Lifecycle | `on_event(event, mode, priority, handler)` | `emit_lifecycle` | Blocking handlers may return `Block`; the call site decides whether that aborts flow. Advisory/nonblocking are notifications. |
| Prompt build | `on_prompt_build(priority, handler)` | `collect_prompt_contributions` | Contributions merge by priority. |
| Pre tool use | `on_pre_tool_use*` | `emit_pre_tool_use` | Blocking handlers may modify input, ask, or block. Advisory/nonblocking do not change flow. |
| Post tool use | `on_post_tool_use*` | `emit_post_tool_use` | Blocking handlers may modify or block result. Advisory/nonblocking do not change flow. |
| Post tool use failure | `on_post_tool_use_failure(priority, handler)` | `emit_post_tool_use_failure` | Notification only. |
| Before provider request | `on_before_provider_request(mode, priority, handler)` | `emit_provider(BeforeRequest, ...)` | Blocking handlers may replace/append messages or block only the current provider call. |
| After provider response | `on_after_provider_response(priority, handler)` | `emit_provider(AfterResponse, ...)` | Observation only; results cannot block or rewrite flow. |
| Compact | `on_compact(event, priority, handler)` | `emit_compact` | Pre-compact may block or contribute instructions; post-compact is notification/contribution collection. |
| Continue after stop | `on_continue_after_stop(priority, options, handler)` | `emit_continue_after_stop` | Blocking-only decision hook. First `ContinueOneStep` wins; `options.max_per_turn` may limit a handler, default is unlimited. |
| User message envelope | `on_user_message_envelope(priority, handler)` | `emit_user_message_envelope` | Blocking-only typed hook before the user message is written to durable transcript. Handlers may replace, append, or block. |
| After tool results | `on_after_tool_results(priority, handler)` | `emit_after_tool_results` | Blocking-only typed hook after a committed tool-result batch. First `EndTurn` wins; otherwise the turn continues. |
| Tool discovery | `tool_discovery(handler)` | `collect_tool_adapters` | Contributes dynamic tools for one collection pass. |

## Decision Hooks

Decision hooks do not accept `HookMode`; their registration API encodes that the
host must await them before it can make progress. Today AstrCode has three typed
decision hooks: `continue_after_stop`, `user_message_envelope`, and
`after_tool_results`.

`continue_after_stop` is also the only hook with a per-turn continuation budget:
`ContinueAfterStopOptions::limited(n)` asks the host to skip that handler after
`n` automatic continuations in the same turn, while
`ContinueAfterStopOptions::unlimited()` and the default do not apply a host
limit.

## Provider Request Scope

Provider request rewrites apply only to the current main-turn provider request.
They must not rewrite durable transcript state. Use `user_message_envelope` when
a plugin intentionally needs durable per-input text injection or input blocking;
use `before_provider_request` for non-durable hidden request context.

## S5R Boundary

Process-internal Rust extensions can register all typed hooks through
`Registrar`. S5R currently supports generic hook events plus the typed
`continue_after_stop` manifest entry with `options.max_per_turn`.
`user_message_envelope` and `after_tool_results` are rejected in S5R manifests
until the wire protocol grows typed input/output adapters for them.

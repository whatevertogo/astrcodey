# AstrCode Extension Hook Matrix

This file is the contract for AstrCode extension hook semantics. Keep it in sync with
`astrcode-extension-sdk::extension`, `astrcode-extensions::runner`, and the session call sites.

## Context Construction Boundary

The host constructs every bundled handler context after selecting an immutable extension
generation. Context fields are private: extension authors read runtime attribution through
accessors and use `astrcode_extension_sdk::testing` builders in tests.

| Shared fact | `ExtensionCallContext` accessor (via `ctx.call()`) |
| --- | --- |
| Manifest identity | `extension_id()` |
| Optional session and turn attribution | `session_id()`, `turn_id()` |
| Optional normalized workspace | `working_dir()` |
| Extension-namespaced storage | `paths()` |
| Scoped typed host clients | `host()` |
| Declared extension events | `events()` |
| Generation-owned tasks | `tasks()` |
| Call/lifecycle cancellation | `cancellation()` |

Specialized contexts add only the input for their family: tool call facts, provider messages,
compact statistics, lifecycle summaries, or discovery generation. Production extensions must not
construct these contexts or depend on runtime-only `Runtime*Context` types.

## Capability Boundary

Capabilities authorize sensitive host APIs and registrations; they are not ordinary hook context.

Default session context/API, no manifest capability required:

| Context or API | Semantics |
| --- | --- |
| `ctx.paths().session_data_dir()` in bundled hook/tool/command contexts | Returns a directory already namespaced by current session and extension id; reports context unavailable outside a session. |
| `astrcode.session.state.read` for S5R workers | Reads state namespaced by current session and extension id. |
| `astrcode.session.state.write` for S5R workers | Writes state namespaced by current session and extension id. |

Sensitive APIs include `input_delivery`, `session_control`, `session_history`, `session_inspect`,
`main_model`, `small_model`, `emit_events`, `workspace_read`, `workspace_write`, `process_spawn`,
`network_client`, and `public_http_dispatch`.

The runner rejects privileged registrations that omit their capability:

| Registration | Required capability |
| --- | --- |
| Extension event declaration | `emit_events` |
| Compact hook | `session_history` |
| Before/after provider hook or user-message envelope | `provider_request` |
| Blocking pre/post tool hook | `tool_intercept` |
| Continue after stop | `turn_continuation_control` |

## Hook Families

| Hook | Registration API | Author context | Runtime entry | Result semantics |
| --- | --- | --- | --- | --- |
| Lifecycle | `on_lifecycle(event, mode, priority, handler)` | `LifecycleContext` | `emit_lifecycle` | Only `TurnStart` and `UserPromptSubmit` may be Blocking. Other lifecycle events are observe-only and reject Blocking registration. |
| Prompt build | `on_prompt_build(priority, handler)` | `PromptBuildContext` | `collect_prompt_contributions` | Contributions merge in stable priority order. Initial construction may precede durable `SessionStarted`. |
| Pre tool use | `on_pre_tool_use*` | `PreToolUseContext` | `emit_pre_tool_use` | Blocking handlers may modify input, ask, or block. Advisory/nonblocking handlers cannot change flow. |
| Post tool use | `on_post_tool_use*` | `PostToolUseContext` | `emit_post_tool_use` | Runs after a completed tool result, including semantic errors. Blocking handlers may replace visible content or block; observer results are ignored. |
| Before provider request | `on_before_provider_request(mode, priority, handler)` | `ProviderContext` | `emit_provider(BeforeRequest, ...)` | Blocking handlers may replace/append messages or block only the current provider call. |
| After provider response | `on_after_provider_response(priority, handler)` | `ProviderContext` | `emit_provider(AfterResponse, ...)` | Registrar fixes the mode to Advisory. The handler observes the completed response; returned block/message mutations are discarded and cannot rewrite turn output. |
| Compact | `on_compact(event, priority, handler)` | `CompactContext` | `emit_compact` | Pre-compact may block or contribute instructions; post-compact is notification/contribution collection. |
| Continue after stop | `on_continue_after_stop(priority, options, handler)` | `ContinueAfterStopContext` | `emit_continue_after_stop` | Blocking-only decision hook. First `ContinueOneStep` wins; `options.max_per_turn` may limit a handler and defaults to unlimited. |
| User-message envelope | `on_user_message_envelope(priority, handler)` | `UserMessageEnvelopeContext` | `emit_user_message_envelope` | Blocking-only typed hook before durable transcript write. Handlers may replace, append, or block text. |
| Tool discovery | `tool_discovery(handler)` | `ToolDiscoveryContext` | `tool_catalog_snapshot_typed` | Contributes complete `DiscoveredTool` aggregates for one workspace/generation pass. |
| Command discovery | `command_discovery(handler)` | `CommandDiscoveryContext` | `resolve_commands_for_typed` | Contributes complete command/handler aggregates for one workspace/generation pass. |

Lifecycle registration has one name: `on_lifecycle`. Extension event declaration and emission are a
separate family (`declare_event` and `ctx.events()`), so lifecycle callbacks cannot be confused with
extension-authored events.

## Decision Hooks

Decision hooks do not accept `HookMode`; their registration API encodes that the host must await
them before it can progress. AstrCode has two typed decision hooks:
`continue_after_stop` and `user_message_envelope`.

`ContinueAfterStopOptions::limited(n)` asks the host to skip that handler after `n` automatic
continuations in the same turn. `ContinueAfterStopOptions::unlimited()` and the default apply no
host limit.

## Provider Scope

Before-request message rewrites apply only to the current main-turn provider request and never
rewrite durable transcript state. Use `user_message_envelope` for intentional durable per-input
text injection or input blocking.

After-response handlers are observers even though they share `ProviderHandler` and
`ProviderContext` with before-request handlers. `on_after_provider_response` always registers
Advisory mode, and the dispatcher ignores `ProviderResult` mutations for that event.

## S5R Boundary

Process-internal Rust extensions register all typed hooks through `Registrar` and receive the
host-constructed contexts above. The S5R adapter converts the same attributed runtime facts to wire
input for external workers; worker authors do not receive or construct bundled Rust contexts.

`Worker::hook(event, mode, handler)` only accepts hook families whose dispatcher implements a
caller-selected mode: lifecycle hooks, pre/post tool hooks, and before-provider hooks. Fixed-mode
families use typed authoring methods and emit exactly one valid wire mode:

| S5R hook | Worker authoring API | Required wire mode |
| --- | --- | --- |
| Prompt build | `on_prompt_build` | Blocking |
| Pre compact | `on_pre_compact` | Blocking |
| Post compact | `on_post_compact` | Blocking |
| After provider response | `on_after_provider_response` | Advisory |
| Continue after stop | `on_continue_after_stop` | Blocking |

The manifest normalization boundary rejects every other event/mode combination, including Blocking
on observe-only lifecycle events. `user_message_envelope` remains unavailable to S5R workers until
the wire protocol has a typed input/output adapter for it.

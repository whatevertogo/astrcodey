# Configuration Guide

AstrCode uses a hierarchical configuration system with JSON files and environment variables. The configuration is stored in `~/.astrcode/config.json` and supports project-level overrides via `.astrcode/config.json`.

## Configuration File Location

- **Global config**: `~/.astrcode/config.json`
- **Project override**: `<project>/.astrcode/config.json`
- **Extension data**: `~/.astrcode/projects/<project_key>/extension_data/<extension-id>/` (project-scoped)

## Configuration Structure

null in runtime is default

```json
{
  "version": "1",
  "activeProfile": "deepseek",
  "activeModel": "deepseek-v4-flash",
  "activeSmallProfile": "deepseek",
  "activeSmallModel": "deepseek-v4-flash",
  "runtime": {
    "llmConnectTimeoutSecs": 60,
    "llmReadTimeoutSecs": 120,
    "llmMaxRetries": 3,
    "compactAutoEnabled": true,
    "compactThresholdPercent": 83.5,
    "compactKeepRecentTurns": null,
    "agentMaxDepth": 3,
    "agentToolMaxParallelCalls": 5
  },
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
          "modelOptions": {
            "reasoning": false,
            "thinkingLevel": "medium"
          }
        }
      ],
      "apiMode": "chat_completions"
    }
  ]
}
```

## Configuration Fields

### Top-level Fields

| Field | Type | Description |
|-------|------|-------------|
| `version` | string | Config format version (currently "1") |
| `activeProfile` | string | Name of the active LLM profile |
| `activeModel` | string | Model ID to use from the active profile |
| `activeSmallProfile` | string (optional) | Profile name for small LLM (used by extensions like memory) |
| `activeSmallModel` | string (optional) | Model ID for small LLM |
| `runtime` | object | Runtime behavior settings (see below) |
| `profiles` | array | Available LLM provider configurations |

### Profile Fields

Each profile in `profiles` array represents an LLM provider configuration:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Profile identifier (referenced by `activeProfile`) |
| `providerKind` | string | Provider type: `openai`, `anthropic`, `google` |
| `baseUrl` | string | API endpoint URL. For Anthropic profiles, `/v1/messages` is auto-appended if the URL does not already include a version segment (e.g., `/v1`). So both `https://api.anthropic.com/v1` and `https://api.anthropic.com` work. |
| `apiKey` | string | API key resolver expression. Supported formats: `"env:VAR_NAME"`, `"!command"`, or a literal key string |
| `models` | array | Available models for this profile |
| `apiMode` | string | API mode: `chat_completions` or `responses` (only for `openai` providerKind) |

### Model Fields

Each model in `models` array:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Model identifier |
| `maxTokens` | number | Maximum output tokens |
| `contextLimit` | number | Context window size |
| `modelOptions` | object | Model capability options (see below) |

### Model Options (`modelOptions`)

| Field | Type | Description |
|-------|------|-------------|
| `reasoning` | boolean (optional) | Enable reasoning mode (provider-dependent) |
| `thinkingLevel` | `"low" \| "medium" \| "high"` (optional) | Reasoning effort level (currently wired to OpenAI Responses `reasoning.effort`) |

### Runtime Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `llmConnectTimeoutSecs` | number | 60 | LLM connection timeout (seconds) |
| `llmReadTimeoutSecs` | number | 120 | LLM read timeout (seconds) |
| `llmMaxRetries` | number | 3 | Maximum retry attempts for failed requests |
| `llmRetryBaseDelayMs` | number | 500 | Base delay for exponential backoff (milliseconds) |
| `compactAutoEnabled` | boolean | true | Enable automatic context compaction |
| `compactThresholdPercent` | number | 83.5 | Trigger auto-compact when context usage exceeds this percentage |
| `compactMaxRetryAttempts` | number | 3 | Maximum retry attempts for compaction |
| `compactMaxOutputTokens` | number | 20000 | Maximum tokens for LLM compaction output |
| `compactKeepRecentTurns` | number or null | null | Recent complete user-turn groups to keep for auto/reactive compaction. `null` keeps the default tail, `0` compacts as much history as possible |
| `compactCircuitBreakerThreshold` | number | 3 | Consecutive auto-compact LLM failures before auto compact is temporarily skipped |
| `compactCircuitBreakerCooldownSecs` | number | 60 | Cooldown before retrying auto compact after the circuit breaker opens |
| `predictiveCompactEnabled` | boolean | false | Enable predictive compaction before the current turn is likely to exceed the context window |
| `predictiveCompactBaselineGrowthTokens` | number | 15000 | Minimum estimated token growth used by predictive compaction |
| `postCompactMaxFiles` | number | 5 | Maximum files to restore after compaction |
| `postCompactTokenBudget` | number | 50000 | Token budget for file restoration |
| `postCompactMaxTokensPerFile` | number | 5000 | Maximum tokens per restored file |
| `agentMaxDepth` | number | 3 | Maximum sub-agent nesting depth (root=0, child=1, ...) |
| `agentToolMaxParallelCalls` | number | 5 | Maximum parallel tool calls per turn |

> **Compact vs turn scheduling:** Manual compact (idle) refuses to start while `TurnRegistry` has an active turn (HTTP 409). Auto/reactive compact runs inside a turn and does not occupy a separate registry slot. Extension `query_session.has_active_turn` reflects registry only; UI may still show `Compacting` during manual compact. See [architecture.md §2](architecture.md#compact-与-turn-调度).

## Environment Variables

API keys can be referenced using `"env:VARIABLE_NAME"` in the `apiKey` field. The system will resolve these from the environment at runtime.

You can also use `"!command"` to run a command and use stdout as the key, for example:

```json
{ "apiKey": "!security find-generic-password -ws 'openai'" }
```

Supported environment variables:
- `OPENAI_API_KEY` - OpenAI API key
- `ANTHROPIC_API_KEY` - Anthropic API key
- `GOOGLE_API_KEY` - Google API key
- Any custom variable referenced in config

## Small LLM Configuration

Some extensions (like `astrcode.memory`) require a small LLM for efficient processing. Configure it by setting `activeSmallProfile` and `activeSmallModel`:

```json
{
  "activeProfile": "openai",
  "activeModel": "gpt-4o",
  "activeSmallProfile": "openai",
  "activeSmallModel": "gpt-4o-mini"
}
```

If `activeSmallProfile` is not set, the small LLM will fall back to the main model configuration.

## Project-level Overrides

Create `.astrcode/config.json` in your project directory to override global settings:

```json
{
  "activeProfile": "project-specific",
  "activeModel": "custom-model",
  "runtime": {
    "llmMaxRetries": 5
  }
}
```

Project overrides are merged with the global config, with project values taking precedence.

## Extension Configuration

Extensions can store their own data on a **per-project basis** in `~/.astrcode/projects/<project_key>/extension_data/<extension-id>/`.

For example, the memory extension stores for each project:
- `MEMORY.md` - Clean markdown file with persistent memories (project-scoped)
- `contexts/` - Historical context files extracted from past sessions (project-scoped)
- `processed_sessions.json` - Track which sessions have been processed (project-scoped)

**Note**: Memory and other extension data are now isolated per project. Each project has its own separate memory store.

## Default Values

All configuration fields have sensible defaults defined in [`crates/astrcode-core/src/config/defaults.rs`](../crates/astrcode-core/src/config/defaults.rs). Missing fields will be filled with these defaults automatically.

## Config Hot Reload

Configuration changes take effect immediately for new sessions. Existing sessions continue using their original configuration. Use the `/new` command to start a session with updated config.

## Validation

The configuration system validates:
- Required fields are present
- Profile and model references exist
- Numeric values are within acceptable ranges
- Environment variables can be resolved

Invalid configurations will prevent AstrCode from starting with a descriptive error message.

## Extension Settings (New in v0.1.4)

You can configure individual extensions via the top-level `extensions` field. This allows extensions to receive structured configuration without requiring separate files.

```json
{
  "version": "1",
  "extensions": {
    "astrcode.memory": {
      "maxContexts": 10,
      "autoExtract": true
    },
    "astrcode.mcp": {
      "mcpServers": {
        "filesystem": {
          "command": "npx",
          "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed"]
        }
      }
    },
    "astrcode.mode": {
      "defaultMode": "code"
    }
  }
}
```

Each extension receives its configuration via `ExtensionCtx::config` during `start()` and `on_config_changed()`.

### Hot Reload

Extension configuration supports hot reload:
1. Modify `config.json`
2. Save the file
3. Extensions receive `on_config_changed()` callback

Project-level `.astrcode/config.json` can also override extension settings using the same merge rules as other config fields.

### s5r Extension Capabilities

Disk s5r extensions declare required host capabilities via `capabilities` (snake_case, e.g. `small_model`, `emit_events`) in the **`Initialize.metadata`** handshake; undeclared sensitive capabilities are rejected by `HostRouter`.

`extension.json` handles discovery and process launching (**`protocol.s5r`** + **`command`** array); tools / commands / hooks are sent by the worker in its `Initialize.metadata`:

```json
{
  "protocol": { "s5r": "1.0" },
  "command": ["./my-extension"]
}
```

Initialize metadata example (excerpt):

```json
{
  "extension_id": "my-ext",
  "protocol": { "s5r": "1.0" },
  "capabilities": ["session_state"],
  "tools": [],
  "commands": [],
  "hooks": [],
  "extension_events": []
}
```

Declorable capabilities include `session_state`, `session_control`, `small_model`, `session_history`, `emit_events`, `workspace_read`, `process_spawn`, and `network_client`. See [extension-system.md](./extension-system.md) for details.

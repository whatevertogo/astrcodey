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
    "llmTemperature": 0.7,
    "compactAutoEnabled": true,
    "compactThresholdPercent": 83.5,
    "agentMaxDepth": 3,
    "agentToolMaxParallelCalls": 5
  },
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
| `baseUrl` | string | API endpoint URL |
| `apiKey` | string | API key or environment variable reference (e.g., `${OPENAI_API_KEY}`) |
| `models` | array | Available models for this profile |
| `apiMode` | string | API mode: `chat_completions` or `responses` |

### Model Fields

Each model in `models` array:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Model identifier |
| `maxTokens` | number | Maximum output tokens |
| `contextLimit` | number | Context window size |
| `reasoning` | boolean | Whether the model supports extended reasoning |
| `reasoningSplit` | boolean | Request separated reasoning/thinking fields |

### Runtime Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `llmConnectTimeoutSecs` | number | 60 | LLM connection timeout (seconds) |
| `llmReadTimeoutSecs` | number | 120 | LLM read timeout (seconds) |
| `llmMaxRetries` | number | 3 | Maximum retry attempts for failed requests |
| `llmRetryBaseDelayMs` | number | 500 | Base delay for exponential backoff (milliseconds) |
| `llmTemperature` | number | null | Sampling temperature (0.0-2.0), null uses API default |
| `compactAutoEnabled` | boolean | true | Enable automatic context compaction |
| `compactThresholdPercent` | number | 83.5 | Trigger auto-compact when context usage exceeds this percentage |
| `compactMaxRetryAttempts` | number | 3 | Maximum retry attempts for compaction |
| `compactMaxOutputTokens` | number | 8000 | Maximum tokens for LLM compaction output |
| `postCompactMaxFiles` | number | 10 | Maximum files to restore after compaction |
| `postCompactTokenBudget` | number | 16000 | Token budget for file restoration |
| `postCompactMaxTokensPerFile` | number | 4000 | Maximum tokens per restored file |
| `agentMaxDepth` | number | 3 | Maximum sub-agent nesting depth (root=0, child=1, ...) |
| `agentToolMaxParallelCalls` | number | 5 | Maximum parallel tool calls per turn |
| `wasmFuel` | number | 100000000 | Fuel limit for WASM extensions (instruction count) |
| `wasmMemoryMb` | number | 128 | Memory limit for WASM extensions (MB) |

## Environment Variables

API keys can be referenced using `${VARIABLE_NAME}` syntax in the `apiKey` field. The system will resolve these from the environment at runtime.

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
    "llmTemperature": 0.5
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

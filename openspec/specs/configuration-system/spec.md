# configuration-system Specification

## Purpose
TBD - created by archiving change astrcode-v2-architecture. Update Purpose after archive.
## Requirements
### Requirement: Multi-layer configuration loading
The system SHALL load configuration from 4 layers in priority order (low to high): Defaults, User config (`~/.astrcode/config.json`), Project overlay (`<workspace>/.astrcode/config.json`), Environment variables.
Higher layers SHALL override lower layers for matching fields.
Project overlay SHALL only override explicitly set fields; unset fields fall through to lower layers.

#### Scenario: User config overrides defaults
- **WHEN** default config has active_model="deepseek-chat"
- **THEN** user config has active_model="gpt-4.1"
- **THEN** resolved config uses "gpt-4.1"

#### Scenario: Project overlay narrows scope
- **WHEN** user config has active_profile="openai"
- **THEN** project overlay has active_profile="deepseek"
- **THEN** resolved config for that project uses active_profile="deepseek"

#### Scenario: Environment variable overrides all
- **WHEN** ASTRCODE_MAX_TOOL_CONCURRENCY=5 env var is set
- **THEN** the resolved runtime.max_tool_concurrency is 5 regardless of file config

### Requirement: Profile-based LLM provider configuration
The system SHALL support multiple named `Profile` entries, each describing an LLM provider connection.
Each Profile SHALL have: name, provider_kind (default "openai"), base_url, api_key (supporting env:VAR/literal/plain-text resolution), models list, api_mode (ChatCompletions/Responses), and optional openai_capabilities.
The system SHALL support an `active_profile` field selecting the current profile.

#### Scenario: Two profiles configured
- **WHEN** config has profiles: [deepseek (base_url=https://api.deepseek.com), openai (base_url=https://api.openai.com)]
- **THEN** both profiles are available for model selection
- **THEN** active_profile determines which profile is used by default

#### Scenario: API key resolved from environment
- **WHEN** profile api_key="env:DEEPSEEK_API_KEY" and DEEPSEEK_API_KEY env var is "sk-abc123"
- **THEN** the resolved api_key is "sk-abc123"

#### Scenario: API key literal fallback
- **WHEN** profile api_key="sk-literal-key" (no env: prefix) and no env var with that name exists
- **THEN** the resolved api_key is "sk-literal-key"

### Requirement: RuntimeConfig with ~30 tunable parameters
The system SHALL provide a `RuntimeConfig` with fields for: max_tool_concurrency, auto_compact_enabled, compact_threshold_percent, tool_result_max_bytes, compact_keep_recent_turns, compact_keep_recent_user_messages, max_consecutive_failures, max_output_continuation_attempts, recovery_truncate_bytes, llm_connect_timeout_secs, llm_read_timeout_secs, llm_max_retries, llm_retry_base_delay_ms, compact_max_retry_attempts, reserved_context_size, summary_reserve_tokens, compact_max_output_tokens, max_tracked_files, max_recovered_files, recovery_token_budget, tool_result_inline_limit, tool_result_preview_limit, max_image_size, max_grep_lines, session_broadcast_capacity, session_recent_record_limit, max_concurrent_branch_depth, aggregate_result_bytes_budget, micro_compact_gap_threshold_secs, micro_compact_keep_recent_results, api_session_ttl_hours.
All RuntimeConfig fields SHALL be `Option<T>` in the config file, with sensible defaults applied at resolution time.

#### Scenario: Default runtime config
- **WHEN** user does not set any runtime fields in config.json
- **THEN** all RuntimeConfig fields resolve to their built-in defaults

#### Scenario: User narrows tool concurrency
- **WHEN** user sets runtime.max_tool_concurrency=3
- **THEN** the resolved runtime uses 3 instead of the default 10

### Requirement: AgentConfig for sub-agent limits
The system SHALL provide an `AgentConfig` nested in RuntimeConfig with: max_subrun_depth, max_spawn_per_turn, max_concurrent, finalized_retain_limit, inbox_capacity, parent_delivery_capacity.
These SHALL control the behavior of the agent collaboration system (spawn/send/observe/close tools).

#### Scenario: Limit sub-agent depth
- **WHEN** agent_config.max_subrun_depth=2
- **THEN** a spawned agent can spawn its own sub-agent (depth 2)
- **THEN** a sub-agent at depth 2 attempting to spawn returns an error

#### Scenario: Limit concurrent agents
- **WHEN** agent_config.max_concurrent=8
- **THEN** at most 8 agent instances can exist concurrently across all sessions

### Requirement: API key resolution
The system SHALL resolve api_key values using three forms:
- `env:VAR_NAME` — required environment variable (error if unset)
- `OPTIONAL_VAR` — optional environment variable (use literal if env var not set)
- `literal_value` — use as-is (if not matching an env var name pattern)
Resolution SHALL happen at config load time with clear error messages.

#### Scenario: Missing required env var
- **WHEN** api_key="env:MISSING_KEY" and MISSING_KEY is not set
- **THEN** config resolution fails with error: "Environment variable MISSING_KEY is not set"

### Requirement: Configuration hot reload
The system SHALL watch user and project config files for changes via filesystem events.
When config changes are detected, the system SHALL reload and re-resolve configuration.
Active session settings SHALL update on next turn (not mid-turn).

#### Scenario: Profile changed while server running
- **WHEN** user edits config.json to change active_profile from "deepseek" to "openai"
- **THEN** the config watcher detects the change within 2 seconds
- **THEN** newly started turns use the "openai" profile

### Requirement: Configuration validation and migration
The system SHALL validate config on load: schema version check, required fields, type validation.
The system SHALL support config migration: when config format changes (e.g., `mcpServers` → `mcp`), old format SHALL be automatically migrated on load.
Invalid config SHALL produce clear error messages.

#### Scenario: Schema version mismatch
- **WHEN** config.json has version="99" (unsupported)
- **THEN** config load fails with: "Unsupported config version 99. Supported: [1]"

#### Scenario: Old format auto-migration
- **WHEN** config.json has top-level "mcpServers" key (old format)
- **THEN** on load, mcpServers content is migrated into the "mcp" field
- **THEN** the migrated config is saved back on next write

### Requirement: ConfigStore trait
The system SHALL define a `ConfigStore` trait for configuration persistence.
The trait SHALL expose: `load()`, `save(config)`, `path()`, `load_overlay(working_dir)`, `save_overlay(working_dir, overlay)`.
The default implementation SHALL be `FileConfigStore` using atomic writes (temp file + rename).

#### Scenario: Atomic config save
- **WHEN** config is saved
- **THEN** the new config is written to config.json.tmp first
- **THEN** config.json.tmp is renamed to config.json (atomic on most filesystems)
- **THEN** if rename fails, config.json is untouched

### Requirement: Active profile/model selection
The system SHALL track active_profile and active_model in Config.
When active_profile or active_model is changed, the system SHALL validate that the profile exists and the model is in the profile's models list.
If the active profile/model combination is invalid, the system SHALL emit a warning and fall back to the first available.

#### Scenario: Active model not in profile
- **WHEN** active_model="gpt-5" does not exist in any profile
- **THEN** a warning is emitted: "Model gpt-5 not found in profile deepseek, using first available model"
- **THEN** resolution falls back to the first model in the active profile


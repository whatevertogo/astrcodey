//! 配置解析：将原始 Config 转换为 EffectiveConfig。纯函数，无 IO 操作。
//!
//! 本模块包含：
//! - [`Config::into_effective()`]：将原始配置解析为有效配置
//! - [`resolve_api_key()`]：解析 API 密钥（支持环境变量引用）
//! - [`merge_overlay()`]：合并项目级覆盖配置

use std::{collections::BTreeMap, process::Command};

use crate::config::{effective::*, raw::*};

/// 配置解析过程中可能发生的错误。
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// 找不到指定的配置文件。
    #[error("Profile not found: {0}")]
    ProfileNotFound(String),
    /// 在指定配置文件中找不到模型。
    #[error("Model not found in profile '{profile}': {model}")]
    ModelNotFound { profile: String, model: String },
    /// 缺少必需的配置字段。
    #[error("Missing field: {0}")]
    MissingField(String),
    /// 缺少必需的环境变量。
    #[error("Missing environment variable: {0}")]
    MissingEnvVar(String),
    /// shell 命令执行失败（用于动态获取 API key）。
    #[error("API key command failed: {0}")]
    ApiKeyCommandFailed(String),
    /// 配置策略禁止执行 API key shell 命令。
    #[error("API key shell command is disabled by runtime config")]
    ApiKeyCommandDisabled,
}

fn known_env_keys_for_profile(profile: &Profile) -> &'static [&'static str] {
    // Note: keep this list intentionally small and stable. It is a fallback when
    // the user does not specify `api_key` in config.toml.
    match profile.name.as_str() {
        "openai" => &["OPENAI_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        // astrcode historically uses GOOGLE_API_KEY; pi-mono uses GEMINI_API_KEY.
        // Accept both (prefer the one that's set).
        "gemini" | "google" => &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        _ => match profile.wire_format {
            ProviderWireFormat::AnthropicMessages => &["ANTHROPIC_API_KEY"],
            ProviderWireFormat::GoogleGenAi => &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
            // openai-compatible providers vary widely; do not guess here.
            ProviderWireFormat::OpenAiChatCompletions | ProviderWireFormat::OpenAiResponses => &[],
        },
    }
}

fn resolve_shell_command(raw_command: &str) -> Result<String, ResolveError> {
    let output = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", raw_command])
            .output()
            .map_err(|e| ResolveError::ApiKeyCommandFailed(e.to_string()))?
    } else {
        Command::new("sh")
            .args(["-lc", raw_command])
            .output()
            .map_err(|e| ResolveError::ApiKeyCommandFailed(e.to_string()))?
    };

    if !output.status.success() {
        return Err(ResolveError::ApiKeyCommandFailed(format!(
            "exit_code={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let key = stdout.trim().to_string();
    if key.is_empty() {
        return Err(ResolveError::ApiKeyCommandFailed(
            "command returned empty stdout".into(),
        ));
    }
    Ok(key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiKeyInputKind {
    EnvRef,
    ShellCommand,
    EnvVarLike,
    PlainText,
}

fn classify_api_key_input(raw: &str) -> Option<(ApiKeyInputKind, &str)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(var) = trimmed.strip_prefix("env:") {
        return Some((ApiKeyInputKind::EnvRef, var));
    }
    if let Some(command) = trimmed.strip_prefix('!') {
        return Some((ApiKeyInputKind::ShellCommand, command.trim()));
    }
    if trimmed.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        return Some((ApiKeyInputKind::EnvVarLike, trimmed));
    }
    Some((ApiKeyInputKind::PlainText, trimmed))
}

fn resolve_profile_api_key(
    profile: &Profile,
    allow_shell_command: bool,
) -> Result<String, ResolveError> {
    // 1) explicit api_key in config
    if let Some(raw) = profile.api_key.as_deref().filter(|s| !s.trim().is_empty()) {
        return resolve_api_key_with_policy(raw, allow_shell_command);
    }

    // 2) fallback to known env keys (pi-mono style)
    for key in known_env_keys_for_profile(profile) {
        if let Ok(val) = std::env::var(key) {
            if !val.trim().is_empty() {
                return Ok(val);
            }
        }
    }

    Err(ResolveError::MissingField("api_key".into()))
}

/// 解析单个 profile + model 对为 [`LlmSettings`]。
fn resolve_llm_settings(
    profiles: &[Profile],
    profile_name: &str,
    model_name: &str,
    runtime: &RuntimeSection,
) -> Result<LlmSettings, ResolveError> {
    let profile = profiles
        .iter()
        .find(|p| p.name == profile_name)
        .ok_or_else(|| ResolveError::ProfileNotFound(profile_name.into()))?;

    let model = profile
        .models
        .iter()
        .find(|m| m.id == model_name)
        .ok_or_else(|| ResolveError::ModelNotFound {
            profile: profile.name.clone(),
            model: model_name.into(),
        })?;

    let api_key = resolve_profile_api_key(
        profile,
        runtime.allow_api_key_shell_command.unwrap_or(false),
    )?;

    let options = model.model_options.as_ref();
    let reasoning = options.and_then(|o| o.reasoning).unwrap_or(false);
    let thinking_level = options.and_then(|o| o.thinking_level);

    Ok(LlmSettings {
        provider_kind: profile.provider_kind.clone(),
        wire_format: profile.wire_format,
        auth_scheme: profile.auth_scheme,
        base_url: profile.base_url.clone(),
        api_key,
        model_id: model_name.into(),
        max_tokens: model.max_tokens.unwrap_or(8192),
        context_limit: model.context_limit.unwrap_or(65536),
        connect_timeout_secs: runtime
            .llm_connect_timeout_secs
            .unwrap_or(super::defaults::DEFAULT_LLM_CONNECT_TIMEOUT_SECS),
        read_timeout_secs: runtime
            .llm_read_timeout_secs
            .unwrap_or(super::defaults::DEFAULT_LLM_READ_TIMEOUT_SECS),
        max_retries: runtime
            .llm_max_retries
            .unwrap_or(super::defaults::DEFAULT_LLM_MAX_RETRIES),
        retry_base_delay_ms: runtime
            .llm_retry_base_delay_ms
            .unwrap_or(super::defaults::DEFAULT_LLM_RETRY_BASE_DELAY_MS),
        supports_prompt_cache_key: profile
            .capabilities
            .supports_prompt_cache_key
            .unwrap_or(false),
        supports_stream_usage: profile.capabilities.supports_stream_usage.unwrap_or(false),
        prompt_cache_retention: profile.capabilities.prompt_cache_retention,
        reasoning,
        thinking_level,
    })
}

impl Config {
    /// 将原始配置解析为 [`EffectiveConfig`]（借用版本，不消耗 [`Config`]）。
    ///
    /// 解析流程：
    /// 1. 根据 `active_profile` 查找对应的配置文件
    /// 2. 根据 `active_model` 查找对应的模型配置
    /// 3. 解析 API 密钥（支持 `env:` 前缀和环境变量名）
    /// 4. 合并运行时配置段的超时/重试参数与默认值
    pub fn effective_from(&self) -> Result<EffectiveConfig, ResolveError> {
        let llm = resolve_llm_settings(
            &self.profiles,
            &self.active_profile,
            &self.active_model,
            &self.runtime,
        )?;

        let small_llm = match (&self.active_small_profile, &self.active_small_model) {
            (Some(profile), Some(model)) => {
                resolve_llm_settings(&self.profiles, profile, model, &self.runtime)?
            },
            _ => llm.clone(),
        };

        Ok(EffectiveConfig {
            llm,
            small_llm,
            context: build_context_settings(&self.runtime),
            agent: build_agent_settings(&self.runtime),
            permissions: self.permissions.clone().unwrap_or_default(),
            extensions: build_extension_settings(&self.runtime, self.extensions.as_ref()),
        })
    }

    /// 消耗 [`Config`] 并解析为 [`EffectiveConfig`]。
    pub fn into_effective(self) -> Result<EffectiveConfig, ResolveError> {
        self.effective_from()
    }
}

/// 解析 API 密钥：支持 `env:VAR` 前缀和环境变量名。
///
/// 解析规则：
/// - `env:VAR_NAME` 前缀：从环境变量 `VAR_NAME` 读取，不存在则报错
/// - `!command` 前缀：执行 shell 命令，使用 stdout 作为 key（trim 后不能为空）
/// - 全大写加下划线的字符串：尝试作为环境变量名，不存在时 emit warning 后使用原始值
/// - 其他字符串：直接作为密钥使用
///
/// 空字符串在此函数被调用前已由调用方（`into_effective`）拦截。
pub fn resolve_api_key(raw: &str) -> Result<String, ResolveError> {
    resolve_api_key_with_policy(raw, true)
}

fn resolve_api_key_with_policy(
    raw: &str,
    allow_shell_command: bool,
) -> Result<String, ResolveError> {
    let Some((kind, value)) = classify_api_key_input(raw) else {
        return Ok(String::new());
    };
    match kind {
        ApiKeyInputKind::EnvRef => {
            std::env::var(value).map_err(|_| ResolveError::MissingEnvVar(value.into()))
        },
        ApiKeyInputKind::ShellCommand => {
            if !allow_shell_command {
                return Err(ResolveError::ApiKeyCommandDisabled);
            }
            resolve_shell_command(value)
        },
        ApiKeyInputKind::EnvVarLike => match std::env::var(value) {
            Ok(val) => Ok(val),
            Err(_) => {
                tracing::warn!(
                    key = value,
                    "Config value looks like an env var name but the variable is not set; using \
                     the raw value as API key. Use 'env:{value}' prefix for explicit env var \
                     reference."
                );
                Ok(value.into())
            },
        },
        ApiKeyInputKind::PlainText => Ok(value.into()),
    }
}

/// 用于 UI 展示：profile 是否“看起来”能解析出 API key。
///
/// 注意：此函数不会执行 `!command`，只做静态判断和 env var presence 检测。
pub fn profile_has_resolvable_api_key(profile: &Profile) -> bool {
    if let Some(raw) = profile.api_key.as_deref() {
        if let Some((kind, value)) = classify_api_key_input(raw) {
            match kind {
                ApiKeyInputKind::EnvRef => {
                    return std::env::var(value).is_ok_and(|v| !v.trim().is_empty());
                },
                ApiKeyInputKind::ShellCommand
                | ApiKeyInputKind::EnvVarLike
                | ApiKeyInputKind::PlainText => return true,
            }
        }
    }

    known_env_keys_for_profile(profile)
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

fn build_context_settings(runtime: &RuntimeSection) -> ContextSettings {
    ContextSettings {
        auto_compact_enabled: runtime
            .compact_auto_enabled
            .unwrap_or(super::defaults::DEFAULT_COMPACT_AUTO_ENABLED),
        predictive_compact_enabled: runtime
            .predictive_compact_enabled
            .unwrap_or(super::defaults::DEFAULT_PREDICTIVE_COMPACT_ENABLED),
        compact_threshold_percent: runtime
            .compact_threshold_percent
            .unwrap_or(super::defaults::DEFAULT_COMPACT_THRESHOLD_PERCENT),
        compact_max_retry_attempts: runtime
            .compact_max_retry_attempts
            .unwrap_or(super::defaults::DEFAULT_COMPACT_MAX_RETRY_ATTEMPTS),
        compact_max_output_tokens: runtime
            .compact_max_output_tokens
            .unwrap_or(super::defaults::DEFAULT_COMPACT_MAX_OUTPUT_TOKENS),
        compact_keep_recent_turns: runtime
            .compact_keep_recent_turns
            .or(super::defaults::DEFAULT_COMPACT_KEEP_RECENT_TURNS),
        predictive_compact_baseline_growth_tokens: runtime
            .predictive_compact_baseline_growth_tokens
            .unwrap_or(super::defaults::DEFAULT_PREDICTIVE_COMPACT_BASELINE_GROWTH_TOKENS),
        compact_circuit_breaker_threshold: runtime
            .compact_circuit_breaker_threshold
            .unwrap_or(super::defaults::DEFAULT_COMPACT_CIRCUIT_BREAKER_THRESHOLD),
        compact_circuit_breaker_cooldown_secs: runtime
            .compact_circuit_breaker_cooldown_secs
            .unwrap_or(super::defaults::DEFAULT_COMPACT_CIRCUIT_BREAKER_COOLDOWN_SECS),
        post_compact_max_files: runtime
            .post_compact_max_files
            .unwrap_or(super::defaults::DEFAULT_POST_COMPACT_MAX_FILES),
        post_compact_token_budget: runtime
            .post_compact_token_budget
            .unwrap_or(super::defaults::DEFAULT_POST_COMPACT_TOKEN_BUDGET),
        post_compact_max_tokens_per_file: runtime
            .post_compact_max_tokens_per_file
            .unwrap_or(super::defaults::DEFAULT_POST_COMPACT_MAX_TOKENS_PER_FILE),
    }
}

fn build_agent_settings(runtime: &RuntimeSection) -> AgentSettings {
    AgentSettings {
        max_depth: runtime
            .agent_max_depth
            .unwrap_or(super::defaults::DEFAULT_AGENT_MAX_DEPTH),
        tool_max_parallel_calls: runtime
            .agent_tool_max_parallel_calls
            .unwrap_or(super::defaults::DEFAULT_AGENT_TOOL_MAX_PARALLEL_CALLS),
        shell_timeout_secs: runtime
            .shell_timeout_secs
            .unwrap_or(super::defaults::DEFAULT_SHELL_TIMEOUT_SECS),
        approval_mode: runtime
            .approval_mode
            .as_deref()
            .and_then(crate::permission::ApprovalMode::parse)
            .unwrap_or_default(),
    }
}

fn build_extension_settings(
    runtime: &RuntimeSection,
    extensions: Option<&BTreeMap<String, ExtensionRawConfig>>,
) -> ExtensionSettings {
    ExtensionSettings {
        extension_states: runtime.extension_states.clone().unwrap_or_default(),
        extension_configs: extensions.cloned().unwrap_or_default(),
    }
}

/// 将项目级覆盖配置合并到基础配置中。
///
/// - 标量字段（`activeProfile` 等）：覆盖层有值则替换。
/// - `profiles`：有值则整体替换全局列表。
/// - `runtime`：按字段合并（仅覆盖出现的键）。
/// - `permissions`：有值则整体替换。
/// - `extensions`：同扩展 id 合并；双方均为 JSON 对象时按 key 合并，否则覆盖层整段替换。
pub fn merge_overlay(mut base: Config, overlay: ConfigOverlay) -> Config {
    if let Some(p) = overlay.active_profile {
        base.active_profile = p;
    }
    if let Some(m) = overlay.active_model {
        base.active_model = m;
    }
    if let Some(p) = overlay.active_small_profile {
        base.active_small_profile = Some(p);
    }
    if let Some(m) = overlay.active_small_model {
        base.active_small_model = Some(m);
    }
    if let Some(profiles) = overlay.profiles {
        base.profiles = profiles;
    }
    if let Some(runtime) = overlay.runtime {
        merge_runtime_section(&mut base.runtime, runtime);
    }
    if let Some(permissions) = overlay.permissions {
        base.permissions = Some(permissions);
    }
    if let Some(extensions) = overlay.extensions {
        let base_extensions = base.extensions.get_or_insert_with(BTreeMap::new);
        merge_extension_configs(base_extensions, extensions);
    }
    base
}

/// 合并 `extensions` 段：同 id 且双方均为 object 时递归合并 JSON，否则覆盖层替换整段。
fn merge_extension_configs(
    base: &mut BTreeMap<String, ExtensionRawConfig>,
    overlay: BTreeMap<String, ExtensionRawConfig>,
) {
    for (extension_id, overlay_value) in overlay {
        match base.get_mut(&extension_id) {
            Some(base_value) if base_value.is_object() && overlay_value.is_object() => {
                merge_json_values(base_value, overlay_value);
            },
            Some(_) | None => {
                base.insert(extension_id, overlay_value);
            },
        }
    }
}

/// 递归合并 JSON object；类型不一致时以覆盖层为准。
fn merge_json_values(base: &mut serde_json::Value, overlay: serde_json::Value) {
    let (Some(base_map), Some(overlay_map)) = (base.as_object_mut(), overlay.as_object()) else {
        *base = overlay;
        return;
    };
    for (key, overlay_field) in overlay_map {
        match base_map.get_mut(key) {
            Some(base_field) if base_field.is_object() && overlay_field.is_object() => {
                merge_json_values(base_field, overlay_field.clone());
            },
            Some(_) | None => {
                base_map.insert(key.clone(), overlay_field.clone());
            },
        }
    }
}

/// 将覆盖层 `runtime` 中已设置的字段合并进基础 `runtime`。
fn merge_runtime_section(base: &mut RuntimeSection, overlay: RuntimeSection) {
    macro_rules! merge_field {
        ($field:ident) => {
            if overlay.$field.is_some() {
                base.$field = overlay.$field;
            }
        };
    }
    merge_field!(llm_connect_timeout_secs);
    merge_field!(llm_read_timeout_secs);
    merge_field!(llm_max_retries);
    merge_field!(llm_retry_base_delay_ms);
    merge_field!(compact_auto_enabled);
    merge_field!(compact_threshold_percent);
    merge_field!(compact_max_retry_attempts);
    merge_field!(compact_max_output_tokens);
    merge_field!(compact_keep_recent_turns);
    merge_field!(compact_circuit_breaker_threshold);
    merge_field!(compact_circuit_breaker_cooldown_secs);
    merge_field!(predictive_compact_enabled);
    merge_field!(predictive_compact_baseline_growth_tokens);
    merge_field!(post_compact_max_files);
    merge_field!(post_compact_token_budget);
    merge_field!(post_compact_max_tokens_per_file);
    merge_field!(agent_max_depth);
    merge_field!(agent_tool_max_parallel_calls);
    merge_field!(shell_timeout_secs);
    merge_field!(allow_api_key_shell_command);
    merge_field!(approval_mode);
    if let Some(states) = overlay.extension_states {
        let base_states = base.extension_states.get_or_insert_with(BTreeMap::new);
        for (k, v) in states {
            base_states.insert(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_api_key_env_prefix() {
        let key = format!("TEST_API_KEY_{}", std::process::id());
        std::env::set_var(&key, "sk-test-123");
        assert_eq!(
            resolve_api_key(&format!("env:{key}")).unwrap(),
            "sk-test-123"
        );
        std::env::remove_var(&key);
    }

    #[test]
    fn test_resolve_api_key_plain_text() {
        assert_eq!(
            resolve_api_key("sk-test-placeholder-not-a-real-key").unwrap(),
            "sk-test-placeholder-not-a-real-key"
        );
    }

    #[test]
    fn test_resolve_api_key_shell_command() {
        // use a minimal cross-platform command that prints a known token
        let key = if cfg!(windows) {
            resolve_api_key("!echo sk-test-123").unwrap()
        } else {
            resolve_api_key("!printf 'sk-test-123'").unwrap()
        };
        assert_eq!(key, "sk-test-123");
    }

    #[test]
    fn test_resolve_api_key_empty_not_treated_as_env_var() {
        // 空字符串走明文路径（全大写的空真条件由 !is_empty() 守卫）。
        // 调用方（into_effective）在到达此函数之前已拦截缺失的密钥。
        assert_eq!(resolve_api_key("").unwrap(), "");
    }

    #[test]
    fn test_missing_api_key_returns_error() {
        // 没有 api_key 且无可用 env fallback 的 Config 应产生 MissingField 错误
        let config = Config {
            profiles: vec![Profile {
                name: "test".into(),
                provider_kind: "openai".into(),
                base_url: "https://api.test.com".into(),
                api_key: None,
                wire_format: ProviderWireFormat::OpenAiChatCompletions,
                auth_scheme: ProviderAuthScheme::Bearer,
                capabilities: ProviderCapabilities::default(),
                models: vec![ModelConfig {
                    id: "test-model".into(),
                    max_tokens: Some(1024),
                    context_limit: Some(4096),
                    model_options: None,
                }],
            }],
            active_profile: "test".into(),
            active_model: "test-model".into(),
            ..Config::default()
        };
        let result = config.into_effective();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("api_key"));
    }

    #[test]
    fn test_resolve_default_config() {
        let config = Config {
            profiles: vec![Profile {
                name: "deepseek".into(),
                provider_kind: "openai".into(),
                base_url: "https://api.deepseek.com".into(),
                api_key: Some("sk-test".into()),
                wire_format: ProviderWireFormat::OpenAiChatCompletions,
                auth_scheme: ProviderAuthScheme::Bearer,
                capabilities: ProviderCapabilities::default(),
                models: vec![ModelConfig {
                    id: "deepseek-chat".into(),
                    max_tokens: Some(8192),
                    context_limit: Some(65536),
                    model_options: None,
                }],
            }],
            active_profile: "deepseek".into(),
            active_model: "deepseek-chat".into(),
            ..Config::default()
        };
        let effective = config.into_effective().unwrap();
        assert_eq!(effective.llm.model_id, "deepseek-chat");
        // 未配置小模型时回退到主模型
        assert_eq!(effective.small_llm.model_id, "deepseek-chat");
    }

    #[test]
    fn test_openai_prompt_cache_capabilities_are_resolved() {
        let config = Config {
            active_profile: "openai".into(),
            active_model: "gpt-4.1".into(),
            ..Config::default()
        };
        // Replace env-referenced api_key with a plain value to avoid env var dependency
        let mut profiles = config.profiles;
        for p in &mut profiles {
            if p.name == "openai" {
                p.api_key = Some("sk-test".into());
            }
        }
        let config = Config {
            profiles,
            active_profile: "openai".into(),
            active_model: "gpt-4.1".into(),
            ..Config::default()
        };

        let effective = config.into_effective().unwrap();

        assert!(effective.llm.supports_prompt_cache_key);
        assert_eq!(effective.llm.prompt_cache_retention, None);
    }

    #[test]
    fn test_small_model_resolves_from_different_profile() {
        let config = Config {
            profiles: vec![
                Profile {
                    name: "deepseek".into(),
                    provider_kind: "openai".into(),
                    base_url: "https://api.deepseek.com".into(),
                    api_key: Some("sk-deep".into()),
                    wire_format: ProviderWireFormat::OpenAiChatCompletions,
                    auth_scheme: ProviderAuthScheme::Bearer,
                    capabilities: ProviderCapabilities::default(),
                    models: vec![ModelConfig {
                        id: "deepseek-chat".into(),
                        max_tokens: Some(8192),
                        context_limit: Some(65536),
                        model_options: None,
                    }],
                },
                Profile {
                    name: "anthropic".into(),
                    provider_kind: "anthropic".into(),
                    base_url: "https://api.anthropic.com/v1".into(),
                    api_key: Some("sk-ant".into()),
                    wire_format: ProviderWireFormat::AnthropicMessages,
                    auth_scheme: ProviderAuthScheme::XApiKey,
                    capabilities: ProviderCapabilities::default(),
                    models: vec![ModelConfig {
                        id: "claude-haiku-4-5-20251001".into(),
                        max_tokens: Some(8192),
                        context_limit: Some(200000),
                        model_options: None,
                    }],
                },
            ],
            active_profile: "deepseek".into(),
            active_model: "deepseek-chat".into(),
            active_small_profile: Some("anthropic".into()),
            active_small_model: Some("claude-haiku-4-5-20251001".into()),
            ..Config::default()
        };

        let effective = config.into_effective().unwrap();
        assert_eq!(effective.llm.model_id, "deepseek-chat");
        assert_eq!(effective.small_llm.model_id, "claude-haiku-4-5-20251001");
        assert_eq!(effective.small_llm.provider_kind, "anthropic");
    }

    #[test]
    fn merge_overlay_extension_configs_merges_json_objects() {
        let base = Config {
            extensions: Some(BTreeMap::from([(
                "astrcode-web-tools".into(),
                serde_json::json!({ "search": { "defaultMaxResults": 3 }, "fetch": { "cacheTtlSecs": 900 } }),
            )])),
            ..Config::default()
        };
        let overlay = ConfigOverlay {
            extensions: Some(BTreeMap::from([(
                "astrcode-web-tools".into(),
                serde_json::json!({ "search": { "provider": "brave" } }),
            )])),
            ..ConfigOverlay::default()
        };
        let merged = merge_overlay(base, overlay);
        let ext = merged.extensions.as_ref().unwrap();
        assert_eq!(ext["astrcode-web-tools"]["search"]["defaultMaxResults"], 3);
        assert_eq!(ext["astrcode-web-tools"]["search"]["provider"], "brave");
        assert_eq!(ext["astrcode-web-tools"]["fetch"]["cacheTtlSecs"], 900);
    }

    #[test]
    fn merge_overlay_runtime_and_extensions() {
        let base = Config {
            runtime: RuntimeSection {
                llm_max_retries: Some(2),
                extension_states: Some(BTreeMap::from([("astrcode.memory".into(), false)])),
                ..RuntimeSection::default()
            },
            extensions: Some(BTreeMap::from([(
                "astrcode-web-tools".into(),
                serde_json::json!({ "search": { "defaultMaxResults": 3 } }),
            )])),
            ..Config::default()
        };
        let overlay = ConfigOverlay {
            runtime: Some(RuntimeSection {
                llm_max_retries: Some(5),
                extension_states: Some(BTreeMap::from([("astrcode.memory".into(), true)])),
                ..RuntimeSection::default()
            }),
            extensions: Some(BTreeMap::from([(
                "astrcode-web-tools".into(),
                serde_json::json!({ "fetch": { "cacheTtlSecs": 60 } }),
            )])),
            ..ConfigOverlay::default()
        };
        let merged = merge_overlay(base, overlay);
        assert_eq!(merged.runtime.llm_max_retries, Some(5));
        assert!(merged.runtime.extension_states.as_ref().unwrap()["astrcode.memory"]);
        let ext = merged.extensions.as_ref().unwrap();
        assert_eq!(ext["astrcode-web-tools"]["search"]["defaultMaxResults"], 3);
        assert_eq!(ext["astrcode-web-tools"]["fetch"]["cacheTtlSecs"], 60);
    }

    #[test]
    fn test_model_options_are_resolved_and_override_legacy_fields() {
        let config = Config {
            profiles: vec![Profile {
                name: "openai".into(),
                provider_kind: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: Some("sk-test".into()),
                wire_format: ProviderWireFormat::OpenAiResponses,
                auth_scheme: ProviderAuthScheme::Bearer,
                capabilities: ProviderCapabilities::default(),
                models: vec![ModelConfig {
                    id: "gpt-4.1".into(),
                    max_tokens: Some(8192),
                    context_limit: Some(128000),
                    model_options: Some(ModelOptionsConfig {
                        reasoning: Some(true),
                        thinking_level: Some(crate::llm::ThinkingLevel::High),
                    }),
                }],
            }],
            active_profile: "openai".into(),
            active_model: "gpt-4.1".into(),
            ..Config::default()
        };

        let effective = config.into_effective().unwrap();
        assert!(effective.llm.reasoning);
        assert_eq!(
            effective.llm.thinking_level,
            Some(crate::llm::ThinkingLevel::High)
        );
    }
}

//! 配置解析：将原始 Config 转换为 EffectiveConfig。纯函数，无 IO 操作。
//!
//! 本模块包含：
//! - [`Config::into_effective()`]：将原始配置解析为有效配置
//! - [`resolve_api_key()`]：解析 API 密钥（支持环境变量引用）
//! - [`merge_overlay()`]：合并项目级覆盖配置

use std::collections::BTreeMap;

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

    let api_key = match profile.api_key.as_deref() {
        Some(s) if !s.is_empty() => resolve_api_key(s)?,
        _ => return Err(ResolveError::MissingField("api_key".into())),
    };

    let api_mode = profile.api_mode.unwrap_or(OpenAiApiMode::ChatCompletions);
    let openai_capabilities = profile.openai_capabilities.as_ref();

    Ok(LlmSettings {
        provider_kind: profile.provider_kind.clone(),
        base_url: profile.base_url.clone(),
        api_key,
        api_mode,
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
        supports_prompt_cache_key: openai_capabilities
            .and_then(|c| c.supports_prompt_cache_key)
            .unwrap_or(false),
        prompt_cache_retention: openai_capabilities.and_then(|c| c.prompt_cache_retention),
        reasoning: model.reasoning.unwrap_or(false),
        reasoning_split: model.reasoning_split.unwrap_or(false),
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
            context: ContextSettings {
                auto_compact_enabled: self
                    .runtime
                    .compact_auto_enabled
                    .unwrap_or(super::defaults::DEFAULT_COMPACT_AUTO_ENABLED),
                predictive_compact_enabled: self
                    .runtime
                    .predictive_compact_enabled
                    .unwrap_or(super::defaults::DEFAULT_PREDICTIVE_COMPACT_ENABLED),
                compact_threshold_percent: self
                    .runtime
                    .compact_threshold_percent
                    .unwrap_or(super::defaults::DEFAULT_COMPACT_THRESHOLD_PERCENT),
                compact_max_retry_attempts: self
                    .runtime
                    .compact_max_retry_attempts
                    .unwrap_or(super::defaults::DEFAULT_COMPACT_MAX_RETRY_ATTEMPTS),
                compact_max_output_tokens: self
                    .runtime
                    .compact_max_output_tokens
                    .unwrap_or(super::defaults::DEFAULT_COMPACT_MAX_OUTPUT_TOKENS),
                compact_keep_recent_turns: self
                    .runtime
                    .compact_keep_recent_turns
                    .or(super::defaults::DEFAULT_COMPACT_KEEP_RECENT_TURNS),
                predictive_compact_baseline_growth_tokens: self
                    .runtime
                    .predictive_compact_baseline_growth_tokens
                    .unwrap_or(super::defaults::DEFAULT_PREDICTIVE_COMPACT_BASELINE_GROWTH_TOKENS),
                compact_circuit_breaker_threshold: self
                    .runtime
                    .compact_circuit_breaker_threshold
                    .unwrap_or(super::defaults::DEFAULT_COMPACT_CIRCUIT_BREAKER_THRESHOLD),
                compact_circuit_breaker_cooldown_secs: self
                    .runtime
                    .compact_circuit_breaker_cooldown_secs
                    .unwrap_or(super::defaults::DEFAULT_COMPACT_CIRCUIT_BREAKER_COOLDOWN_SECS),
                post_compact_max_files: self
                    .runtime
                    .post_compact_max_files
                    .unwrap_or(super::defaults::DEFAULT_POST_COMPACT_MAX_FILES),
                post_compact_token_budget: self
                    .runtime
                    .post_compact_token_budget
                    .unwrap_or(super::defaults::DEFAULT_POST_COMPACT_TOKEN_BUDGET),
                post_compact_max_tokens_per_file: self
                    .runtime
                    .post_compact_max_tokens_per_file
                    .unwrap_or(super::defaults::DEFAULT_POST_COMPACT_MAX_TOKENS_PER_FILE),
            },
            agent: AgentSettings {
                max_depth: self
                    .runtime
                    .agent_max_depth
                    .unwrap_or(super::defaults::DEFAULT_AGENT_MAX_DEPTH),
                tool_max_parallel_calls: self
                    .runtime
                    .agent_tool_max_parallel_calls
                    .unwrap_or(super::defaults::DEFAULT_AGENT_TOOL_MAX_PARALLEL_CALLS),
                shell_timeout_secs: self
                    .runtime
                    .shell_timeout_secs
                    .unwrap_or(super::defaults::DEFAULT_SHELL_TIMEOUT_SECS),
            },
            extensions: ExtensionSettings {
                extension_states: self.runtime.extension_states.clone().unwrap_or_default(),
                extension_configs: self.extensions.clone().unwrap_or_default(),
            },
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
/// - 全大写加下划线的字符串：尝试作为环境变量名，不存在时 emit warning 后使用原始值
/// - 其他字符串：直接作为密钥使用
///
/// 空字符串在此函数被调用前已由调用方（`into_effective`）拦截。
pub fn resolve_api_key(raw: &str) -> Result<String, ResolveError> {
    if let Some(var) = raw.strip_prefix("env:") {
        // "env:VAR_NAME" 格式：必须存在该环境变量
        std::env::var(var).map_err(|_| ResolveError::MissingEnvVar(var.into()))
    } else if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        // 全大写加下划线：尝试作为环境变量名，失败则 emit warning 后使用原始值
        match std::env::var(raw) {
            Ok(val) => Ok(val),
            Err(_) => {
                tracing::warn!(
                    key = raw,
                    "Config value looks like an env var name but the variable is not set; using \
                     the raw value as API key. Use 'env:{raw}' prefix for explicit env var \
                     reference."
                );
                Ok(raw.into())
            },
        }
    } else {
        // 其他情况：直接作为密钥
        Ok(raw.into())
    }
}

/// 将项目级覆盖配置合并到基础配置中。
///
/// 覆盖配置中的非 `None` 字段会替换基础配置中的对应字段。
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
    if let Some(extensions) = overlay.extensions {
        // 同 key 覆盖，异 key 保留
        let base_extensions = base.extensions.get_or_insert_with(BTreeMap::new);
        for (k, v) in extensions {
            base_extensions.insert(k, v);
        }
    }
    base
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
    fn test_resolve_api_key_empty_not_treated_as_env_var() {
        // 空字符串走明文路径（全大写的空真条件由 !is_empty() 守卫）。
        // 调用方（into_effective）在到达此函数之前已拦截缺失的密钥。
        assert_eq!(resolve_api_key("").unwrap(), "");
    }

    #[test]
    fn test_missing_api_key_returns_error() {
        // 没有 api_key 的 Config 应产生 MissingField 错误
        let config = Config {
            profiles: vec![Profile {
                name: "test".into(),
                provider_kind: "openai".into(),
                base_url: "https://api.test.com".into(),
                api_key: None,
                api_mode: None,
                openai_capabilities: None,
                models: vec![ModelConfig {
                    id: "test-model".into(),
                    max_tokens: Some(1024),
                    context_limit: Some(4096),
                    reasoning: None,
                    reasoning_split: None,
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
                api_mode: Some(OpenAiApiMode::ChatCompletions),
                openai_capabilities: None,
                models: vec![ModelConfig {
                    id: "deepseek-chat".into(),
                    max_tokens: Some(8192),
                    context_limit: Some(65536),
                    reasoning: None,
                    reasoning_split: None,
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
                    api_mode: Some(OpenAiApiMode::ChatCompletions),
                    openai_capabilities: None,
                    models: vec![ModelConfig {
                        id: "deepseek-chat".into(),
                        max_tokens: Some(8192),
                        context_limit: Some(65536),
                        reasoning: None,
                        reasoning_split: None,
                    }],
                },
                Profile {
                    name: "anthropic".into(),
                    provider_kind: "anthropic".into(),
                    base_url: "https://api.anthropic.com/v1".into(),
                    api_key: Some("sk-ant".into()),
                    api_mode: None,
                    openai_capabilities: None,
                    models: vec![ModelConfig {
                        id: "claude-haiku-4-5-20251001".into(),
                        max_tokens: Some(8192),
                        context_limit: Some(200000),
                        reasoning: None,
                        reasoning_split: None,
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
}

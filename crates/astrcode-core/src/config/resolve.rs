//! 配置解析：将原始 Config 转换为 EffectiveConfig。纯函数，无 IO 操作。
//!
//! 本模块包含：
//! - [`Config::into_effective()`]：将原始配置解析为有效配置
//! - [`resolve_api_key()`]：解析 API 密钥（支持环境变量引用）
//! - [`merge_overlay()`]：合并项目级覆盖配置

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

impl Config {
    /// 将原始配置解析为 [`EffectiveConfig`]，填充所有默认值。
    ///
    /// 解析流程：
    /// 1. 根据 `active_profile` 查找对应的配置文件
    /// 2. 根据 `active_model` 查找对应的模型配置
    /// 3. 解析 API 密钥（支持 `env:` 前缀和环境变量名）
    /// 4. 合并运行时配置段的超时/重试参数与默认值
    pub fn into_effective(self) -> Result<EffectiveConfig, ResolveError> {
        // 查找激活的配置文件
        let profile = self
            .profiles
            .iter()
            .find(|p| p.name == self.active_profile)
            .ok_or_else(|| ResolveError::ProfileNotFound(self.active_profile.clone()))?;

        // 查找激活的模型
        let model = profile
            .models
            .iter()
            .find(|m| m.id == self.active_model)
            .ok_or_else(|| ResolveError::ModelNotFound {
                profile: profile.name.clone(),
                model: self.active_model.clone(),
            })?;

        // 解析 API 密钥
        let api_key = match profile.api_key.as_deref() {
            Some(s) if !s.is_empty() => resolve_api_key(s)?,
            _ => return Err(ResolveError::MissingField("api_key".into())),
        };

        // 默认使用 ChatCompletions 模式
        let api_mode = profile.api_mode.unwrap_or(OpenAiApiMode::ChatCompletions);
        let openai_capabilities = profile.openai_capabilities.as_ref();

        let llm = LlmSettings {
            provider_kind: profile.provider_kind.clone(),
            base_url: profile.base_url.clone(),
            api_key,
            api_mode,
            model_id: self.active_model.clone(),
            // 模型参数使用配置值或默认值
            max_tokens: model.max_tokens.unwrap_or(8192),
            context_limit: model.context_limit.unwrap_or(65536),
            // 运行时参数优先使用配置值，否则使用全局默认值
            connect_timeout_secs: self
                .runtime
                .llm_connect_timeout_secs
                .unwrap_or(super::defaults::DEFAULT_LLM_CONNECT_TIMEOUT_SECS),
            read_timeout_secs: self
                .runtime
                .llm_read_timeout_secs
                .unwrap_or(super::defaults::DEFAULT_LLM_READ_TIMEOUT_SECS),
            max_retries: self
                .runtime
                .llm_max_retries
                .unwrap_or(super::defaults::DEFAULT_LLM_MAX_RETRIES),
            retry_base_delay_ms: self
                .runtime
                .llm_retry_base_delay_ms
                .unwrap_or(super::defaults::DEFAULT_LLM_RETRY_BASE_DELAY_MS),
            temperature: self.runtime.llm_temperature,
            supports_prompt_cache_key: openai_capabilities
                .and_then(|c| c.supports_prompt_cache_key)
                .unwrap_or(false),
            prompt_cache_retention: openai_capabilities.and_then(|c| c.prompt_cache_retention),
        };

        Ok(EffectiveConfig { llm })
    }
}

/// 解析 API 密钥：支持 `env:VAR` 前缀和环境变量名。
///
/// 解析规则：
/// - `env:VAR_NAME` 前缀：从环境变量 `VAR_NAME` 读取，不存在则报错
/// - 全大写加下划线的字符串：视为环境变量名，读取失败则使用原始值作为回退
/// - 其他字符串：直接作为密钥使用
///
/// 空字符串在此函数被调用前已由调用方（`into_effective`）拦截。
pub fn resolve_api_key(raw: &str) -> Result<String, ResolveError> {
    if let Some(var) = raw.strip_prefix("env:") {
        // "env:VAR_NAME" 格式：必须存在该环境变量
        std::env::var(var).map_err(|_| ResolveError::MissingEnvVar(var.into()))
    } else if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        // 全大写加下划线：尝试作为环境变量名，失败则使用原始值
        Ok(std::env::var(raw).unwrap_or_else(|_| raw.into()))
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
    if let Some(profiles) = overlay.profiles {
        base.profiles = profiles;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_api_key_env_prefix() {
        std::env::set_var("TEST_API_KEY", "sk-test-123");
        assert_eq!(resolve_api_key("env:TEST_API_KEY").unwrap(), "sk-test-123");
        std::env::remove_var("TEST_API_KEY");
    }

    #[test]
    fn test_resolve_api_key_plain_text() {
        assert_eq!(
            resolve_api_key("sk-cp-as4tt4umyhkgeyur").unwrap(),
            "sk-cp-as4tt4umyhkgeyur"
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
        let config = Config::default();
        let effective = config.into_effective().unwrap();
        assert_eq!(effective.llm.model_id, "deepseek-chat");
    }

    #[test]
    fn test_runtime_temperature_is_resolved() {
        let mut config = Config::default();
        config.runtime.llm_temperature = Some(0.2);

        let effective = config.into_effective().unwrap();

        assert_eq!(effective.llm.temperature, Some(0.2));
    }

    #[test]
    fn test_openai_prompt_cache_capabilities_are_resolved() {
        let config = Config {
            active_profile: "openai".into(),
            active_model: "gpt-4.1".into(),
            ..Config::default()
        };
        let previous = std::env::var("OPENAI_API_KEY").ok();
        std::env::set_var("OPENAI_API_KEY", "sk-test");

        let effective = config.into_effective().unwrap();

        assert!(effective.llm.supports_prompt_cache_key);
        assert_eq!(effective.llm.prompt_cache_retention, None);
        if let Some(value) = previous {
            std::env::set_var("OPENAI_API_KEY", value);
        } else {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }
}

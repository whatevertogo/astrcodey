//! Resolution: raw Config → EffectiveConfig. Pure functions, no IO.

use crate::config::{effective::*, raw::*};

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("Profile not found: {0}")]
    ProfileNotFound(String),
    #[error("Model not found in profile '{profile}': {model}")]
    ModelNotFound { profile: String, model: String },
    #[error("Missing field: {0}")]
    MissingField(String),
    #[error("Missing environment variable: {0}")]
    MissingEnvVar(String),
}

impl Config {
    /// Resolve raw config into an `EffectiveConfig` with all defaults filled.
    pub fn into_effective(self) -> Result<EffectiveConfig, ResolveError> {
        let profile = self
            .profiles
            .iter()
            .find(|p| p.name == self.active_profile)
            .ok_or_else(|| ResolveError::ProfileNotFound(self.active_profile.clone()))?;

        let model = profile
            .models
            .iter()
            .find(|m| m.id == self.active_model)
            .ok_or_else(|| ResolveError::ModelNotFound {
                profile: profile.name.clone(),
                model: self.active_model.clone(),
            })?;

        let api_key = match profile.api_key.as_deref() {
            Some(s) if !s.is_empty() => resolve_api_key(s)?,
            _ => return Err(ResolveError::MissingField("api_key".into())),
        };
        let api_mode = profile.api_mode.unwrap_or(OpenAiApiMode::ChatCompletions);

        let llm = LlmSettings {
            provider_kind: profile.provider_kind.clone(),
            base_url: profile.base_url.clone(),
            api_key,
            api_mode,
            model_id: self.active_model.clone(),
            max_tokens: model.max_tokens.unwrap_or(8192),
            context_limit: model.context_limit.unwrap_or(65536),
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
        };

        Ok(EffectiveConfig { llm })
    }
}

/// Resolve API key: handles `env:VAR` prefix and plain text.
///
/// Plain text values that are all-uppercase with underscores are treated as
/// environment variable names (with the raw value as fallback). Empty strings
/// are rejected before reaching this function.
pub fn resolve_api_key(raw: &str) -> Result<String, ResolveError> {
    if let Some(var) = raw.strip_prefix("env:") {
        std::env::var(var).map_err(|_| ResolveError::MissingEnvVar(var.into()))
    } else if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        Ok(std::env::var(raw).unwrap_or_else(|_| raw.into()))
    } else {
        Ok(raw.into())
    }
}

/// Merge a project overlay into base config.
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
        // Empty string falls through to plain-text (the uppercase vacuous-truth
        // is guarded by !is_empty()). The caller (into_effective) rejects
        // missing keys with MissingField before reaching this function.
        assert_eq!(resolve_api_key("").unwrap(), "");
    }

    #[test]
    fn test_missing_api_key_returns_error() {
        // A Config with no api_key should produce MissingField
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
}

//! Resolution: raw Config → EffectiveConfig. Pure functions, no IO.

use crate::config::effective::*;
use crate::config::raw::*;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("Profile not found: {0}")]
    ProfileNotFound(String),
    #[error("Model not found in profile '{profile}': {model}")]
    ModelNotFound { profile: String, model: String },
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

        let api_key = resolve_api_key(profile.api_key.as_deref().unwrap_or(""))?;
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

/// Resolve API key: handles `env:VAR`, optional env, and plain text.
pub fn resolve_api_key(raw: &str) -> Result<String, ResolveError> {
    if let Some(var) = raw.strip_prefix("env:") {
        std::env::var(var).map_err(|_| ResolveError::MissingEnvVar(var.into()))
    } else if raw.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
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
    fn test_resolve_default_config() {
        let config = Config::default();
        let effective = config.into_effective().unwrap();
        assert_eq!(effective.llm.model_id, "deepseek-chat");
    }
}

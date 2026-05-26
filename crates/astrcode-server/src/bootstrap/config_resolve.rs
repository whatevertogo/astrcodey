//! 启动期配置解析与回退策略。

use astrcode_core::config::{Config, EffectiveConfig};
use astrcode_storage::config_store::FileConfigStore;

/// 从已加载的 raw 配置解析 `EffectiveConfig`，失败时按 last-known-good → 内置默认回退。
pub(super) async fn resolve_effective_config(
    config_store: &FileConfigStore,
    config: &Config,
) -> EffectiveConfig {
    match config.effective_from() {
        Ok(effective) => {
            if let Err(err) = config_store.save_last_known_good(config).await {
                tracing::warn!("Failed to save last-known-good config snapshot: {err}");
            }
            effective
        },
        Err(error) => {
            tracing::warn!("Config resolution failed: {error}");
            resolve_from_last_known_good_or_default(config_store).await
        },
    }
}

async fn resolve_from_last_known_good_or_default(
    config_store: &FileConfigStore,
) -> EffectiveConfig {
    match config_store.load_last_known_good().await {
        Ok(Some(snapshot)) => match snapshot.into_effective() {
            Ok(effective) => {
                tracing::warn!(
                    "Loaded last-known-good config snapshot as fallback. Fix your config via \
                     Settings or POST /api/config/active-selection."
                );
                effective
            },
            Err(snapshot_err) => {
                tracing::warn!("Last-known-good snapshot also invalid: {snapshot_err}");
                fallback_default_effective()
            },
        },
        Ok(None) => {
            tracing::warn!(
                "No last-known-good snapshot found. Using built-in defaults. Fix your config via \
                 Settings or POST /api/config/active-selection."
            );
            fallback_default_effective()
        },
        Err(err) => {
            tracing::warn!("Failed to load last-known-good snapshot: {err}");
            fallback_default_effective()
        },
    }
}

/// 所有配置来源均失败时的兜底：LLM 不可用，HTTP API 仍可工作。
fn fallback_default_effective() -> EffectiveConfig {
    use astrcode_core::config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings,
    };

    EffectiveConfig {
        llm: dummy_llm_settings(),
        small_llm: dummy_llm_settings(),
        context: ContextSettings::default(),
        agent: AgentSettings::default(),
        extensions: ExtensionSettings::default(),
    }
}

fn dummy_llm_settings() -> astrcode_core::config::LlmSettings {
    use astrcode_core::config::{LlmSettings, raw::OpenAiApiMode};

    LlmSettings {
        provider_kind: "openai".into(),
        base_url: String::new(),
        api_key: String::new(),
        api_mode: OpenAiApiMode::ChatCompletions,
        model_id: "fallback".into(),
        max_tokens: 1024,
        context_limit: 4096,
        connect_timeout_secs: 10,
        read_timeout_secs: 90,
        max_retries: 0,
        retry_base_delay_ms: 250,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        reasoning: false,
        reasoning_split: false,
    }
}

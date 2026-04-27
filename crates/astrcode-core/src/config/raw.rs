//! Raw configuration types — what comes from disk (all fields optional/defaulted).

use serde::{Deserialize, Serialize};

// ─── Top-level Config ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default = "super::defaults::default_version")]
    pub version: String,
    #[serde(default = "super::defaults::default_active_profile")]
    pub active_profile: String,
    #[serde(default = "super::defaults::default_active_model")]
    pub active_model: String,
    #[serde(default)]
    pub runtime: RuntimeSection,
    #[serde(default = "super::defaults::default_profiles")]
    pub profiles: Vec<Profile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: super::defaults::default_version(),
            active_profile: super::defaults::default_active_profile(),
            active_model: super::defaults::default_active_model(),
            runtime: RuntimeSection::default(),
            profiles: super::defaults::default_profiles(),
        }
    }
}

// ─── Profile ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub provider_kind: String,
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub api_mode: Option<OpenAiApiMode>,
    pub openai_capabilities: Option<OpenAiProfileCapabilities>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApiMode {
    ChatCompletions,
    Responses,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiProfileCapabilities {
    pub supports_prompt_cache_key: Option<bool>,
    pub supports_stream_usage: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub id: String,
    pub max_tokens: Option<u32>,
    pub context_limit: Option<usize>,
}

// ─── Runtime Section (placeholder for future use) ────────────────────────

/// Runtime section — kept for JSON compatibility. Fields added when implemented.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSection {
    pub llm_connect_timeout_secs: Option<u64>,
    pub llm_read_timeout_secs: Option<u64>,
    pub llm_max_retries: Option<u32>,
    pub llm_retry_base_delay_ms: Option<u64>,
    // TODO: compaction fields
    // TODO: tool concurrency fields
    // TODO: agent limits fields
}

// ─── Config Overlay ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOverlay {
    pub active_profile: Option<String>,
    pub active_model: Option<String>,
    pub profiles: Option<Vec<Profile>>,
}

// ─── Selection Types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSelection {
    pub active_profile: String,
    pub active_model: String,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelection {
    pub profile_name: String,
    pub model: String,
    pub provider_kind: String,
}

// ─── Default profiles (built-in) ─────────────────────────────────────────

pub(crate) fn raw_default_profiles() -> Vec<Profile> {
    vec![
        Profile {
            name: "deepseek".into(),
            provider_kind: "openai".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: Some("env:DEEPSEEK_API_KEY".into()),
            api_mode: Some(OpenAiApiMode::ChatCompletions),
            openai_capabilities: None,
            models: vec![
                ModelConfig {
                    id: "deepseek-chat".into(),
                    max_tokens: Some(8192),
                    context_limit: Some(65536),
                },
                ModelConfig {
                    id: "deepseek-reasoner".into(),
                    max_tokens: Some(8192),
                    context_limit: Some(65536),
                },
            ],
        },
        Profile {
            name: "openai".into(),
            provider_kind: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some("env:OPENAI_API_KEY".into()),
            api_mode: Some(OpenAiApiMode::Responses),
            openai_capabilities: Some(OpenAiProfileCapabilities {
                supports_prompt_cache_key: Some(true),
                supports_stream_usage: Some(true),
            }),
            models: vec![ModelConfig {
                id: "gpt-4.1".into(),
                max_tokens: Some(16384),
                context_limit: Some(1000000),
            }],
        },
    ]
}

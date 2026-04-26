//! Resolved / effective configuration — all defaults filled, all values concrete.
//!
//! Only fields with actual implementations are included. New capabilities
//! add their config here when wired up.

/// Top-level resolved config.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// LLM provider settings — fully wired into OpenAiProvider + Agent.
    pub llm: LlmSettings,
}

// ─── LLM Settings ────────────────────────────────────────────────────────

/// Resolved LLM provider configuration.
#[derive(Debug, Clone)]
pub struct LlmSettings {
    pub provider_kind: String,
    pub base_url: String,
    pub api_key: String,
    pub api_mode: super::raw::OpenAiApiMode,
    pub model_id: String,
    pub max_tokens: u32,
    pub context_limit: usize,
    pub connect_timeout_secs: u64,
    pub read_timeout_secs: u64,
    pub max_retries: u32,
    pub retry_base_delay_ms: u64,
}

// TODO: RuntimeSettings — when compaction / tool concurrency / etc. are wired up.
// TODO: SessionSettings — when session broadcast / branch depth are wired up.
// TODO: AgentSettings — when spawn depth / concurrency are wired up.

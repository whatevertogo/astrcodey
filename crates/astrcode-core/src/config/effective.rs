//! 已解析的有效配置——所有默认值已填充，所有字段均为具体值。
//!
//! 仅包含已实际接入实现的字段。新功能在接入时将其配置添加到此处。

/// 顶层已解析配置。
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// LLM 提供者设置——已完整接入 OpenAiProvider 和 Agent。
    pub llm: LlmSettings,
    /// 上下文窗口 / compact 设置。
    pub context: ContextSettings,
}

// ─── LLM Settings ────────────────────────────────────────────────────────

/// 已解析的 LLM 提供者配置。
///
/// 所有字段均为具体值（非 Option），由 [`crate::config::resolve::Config::into_effective()`]
/// 从原始配置解析并填充默认值后生成。
#[derive(Debug, Clone)]
pub struct LlmSettings {
    /// 提供者类型（如 "openai"）。
    pub provider_kind: String,
    /// API 端点的基础 URL。
    pub base_url: String,
    /// API 密钥（已从环境变量解析）。
    pub api_key: String,
    /// OpenAI API 调用模式（ChatCompletions 或 Responses）。
    pub api_mode: super::raw::OpenAiApiMode,
    /// 模型标识。
    pub model_id: String,
    /// 最大输出 token 数。
    pub max_tokens: u32,
    /// 上下文窗口大小限制（token 数）。
    pub context_limit: usize,
    /// 连接超时时间（秒）。
    pub connect_timeout_secs: u64,
    /// 读取超时时间（秒）。
    pub read_timeout_secs: u64,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 重试的指数退避基础延迟（毫秒）。
    pub retry_base_delay_ms: u64,
    /// 采样温度（0.0-2.0），None 使用 API 默认值。
    pub temperature: Option<f32>,
    /// 当前 provider 是否支持 OpenAI `prompt_cache_key`。
    pub supports_prompt_cache_key: bool,
    /// 可选的 OpenAI prompt cache retention。
    pub prompt_cache_retention: Option<crate::llm::PromptCacheRetention>,
    /// 是否启用推理模式（如 DeepSeek reasoner）。
    pub reasoning: bool,
    /// 是否请求 provider 分离 reasoning/thinking 到独立字段。
    pub reasoning_split: bool,
}

// ─── Context Settings ────────────────────────────────────────────────────

/// 已解析的上下文窗口 / compact 配置。
#[derive(Debug, Clone)]
pub struct ContextSettings {
    /// 是否启用自动压缩（当上下文占用达到阈值时自动触发）。
    pub auto_compact_enabled: bool,
    /// 触发自动压缩的上下文占用百分比阈值（0–100）。
    pub compact_threshold_percent: f32,
    /// 压缩失败时的最大重试次数。
    pub compact_max_retry_attempts: u8,
    /// LLM 压缩输出的最大 token 数。
    pub compact_max_output_tokens: usize,
    /// 压缩后恢复的最近读取文件数量上限。
    pub post_compact_max_files: usize,
    /// 压缩后恢复文件的总 token 预算。
    pub post_compact_token_budget: usize,
    /// 单个恢复文件的最大 token 数。
    pub post_compact_max_tokens_per_file: usize,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            auto_compact_enabled: super::defaults::DEFAULT_COMPACT_AUTO_ENABLED,
            compact_threshold_percent: super::defaults::DEFAULT_COMPACT_THRESHOLD_PERCENT,
            compact_max_retry_attempts: super::defaults::DEFAULT_COMPACT_MAX_RETRY_ATTEMPTS,
            compact_max_output_tokens: super::defaults::DEFAULT_COMPACT_MAX_OUTPUT_TOKENS,
            post_compact_max_files: super::defaults::DEFAULT_POST_COMPACT_MAX_FILES,
            post_compact_token_budget: super::defaults::DEFAULT_POST_COMPACT_TOKEN_BUDGET,
            post_compact_max_tokens_per_file:
                super::defaults::DEFAULT_POST_COMPACT_MAX_TOKENS_PER_FILE,
        }
    }
}

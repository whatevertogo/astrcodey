//! 已解析的有效配置——所有默认值已填充，所有字段均为具体值。
//!
//! 仅包含已实际接入实现的字段。新功能在接入时将其配置添加到此处。

/// 顶层已解析配置。
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// LLM 提供者设置——已完整接入 OpenAiProvider 和 Agent。
    pub llm: LlmSettings,
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
}

// TODO: RuntimeSettings——当压缩/工具并发等功能接入时添加。
// TODO: SessionSettings——当会话广播/分支深度等功能接入时添加。
// TODO: AgentSettings——当 spawn 深度/并发等功能接入时添加。

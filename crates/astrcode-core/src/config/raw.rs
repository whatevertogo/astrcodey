//! 原始配置类型——从磁盘读取的 JSON 结构（所有字段可选/带默认值）。
//!
//! 这些类型直接对应配置文件的 JSON 结构，使用 `serde` 进行序列化/反序列化。
//! 字段使用 `camelCase` 命名约定以匹配 JSON 约定。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::llm::PromptCacheRetention;

/// 扩展配置的原始 JSON 值类型。
/// 用户可在 `config.json` 的 `extensions.<id>` 下写入任意 JSON，
/// 由扩展在 `start()` 时自行反序列化为具体类型。
pub type ExtensionRawConfig = serde_json::Value;

// ─── 顶层 Config ────────────────────────────────────────────────────────

/// 顶层配置结构，对应配置文件的完整 JSON。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// 配置文件格式版本。
    #[serde(default = "super::defaults::default_version")]
    pub version: String,
    /// 当前激活的配置文件名称。
    #[serde(default = "super::defaults::default_active_profile")]
    pub active_profile: String,
    /// 当前激活的模型标识。
    #[serde(default = "super::defaults::default_active_model")]
    pub active_model: String,
    /// 小模型的配置文件名称（可选，未设置时回退到主模型）。
    #[serde(default)]
    pub active_small_profile: Option<String>,
    /// 小模型的模型标识（可选，未设置时回退到主模型）。
    #[serde(default)]
    pub active_small_model: Option<String>,
    /// 运行时配置段（超时、重试等）。
    #[serde(default)]
    pub runtime: RuntimeSection,
    /// 可用的 LLM 配置文件列表。
    #[serde(default = "super::defaults::default_profiles")]
    pub profiles: Vec<Profile>,
    /// 扩展专有配置。key 为扩展 id（如 `"astrcode.mcp"`），value 为任意 JSON。
    ///
    /// 通过此字段，用户可在配置文件中统一管理各扩展的参数，无需扩展各自从额外文件读取。
    /// 扩展在 `start(ctx)` 时通过 `ctx.config.deserialize::<T>()` 获取。
    ///
    /// 例：
    /// ```json
    /// { "astrcode.memory": { "maxContexts": 10, "autoExtract": true } }
    /// ```
    #[serde(default)]
    pub extensions: Option<BTreeMap<String, ExtensionRawConfig>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: super::defaults::default_version(),
            active_profile: super::defaults::default_active_profile(),
            active_model: super::defaults::default_active_model(),
            active_small_profile: None,
            active_small_model: None,
            runtime: RuntimeSection::default(),
            profiles: super::defaults::default_profiles(),
            extensions: None,
        }
    }
}

// ─── Profile ─────────────────────────────────────────────────────────────

/// LLM 提供者配置文件，定义一个 API 端点的完整连接信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    /// 配置文件名称（如 "deepseek"、"openai"）。
    pub name: String,
    /// 提供者类型（如 "openai"）。
    pub provider_kind: String,
    /// API 端点的基础 URL。
    pub base_url: String,
    /// API 密钥，支持 `env:VAR_NAME` 前缀引用环境变量。
    pub api_key: Option<String>,
    /// 此配置文件下可用的模型列表。
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    /// OpenAI API 调用模式。
    #[serde(default)]
    pub api_mode: Option<OpenAiApiMode>,
    /// OpenAI 特有的能力声明。
    pub openai_capabilities: Option<OpenAiProfileCapabilities>,
}

/// OpenAI API 的调用模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApiMode {
    /// 使用 Chat Completions API。
    ChatCompletions,
    /// 使用 Responses API。
    Responses,
}

/// OpenAI 配置文件的能力声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiProfileCapabilities {
    /// 是否支持 prompt cache key。
    pub supports_prompt_cache_key: Option<bool>,
    /// 可选的 prompt cache retention。
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    /// 是否支持流式用量统计。
    pub supports_stream_usage: Option<bool>,
}

/// 模型配置，定义一个具体模型的参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelConfig {
    /// 模型标识（如 "deepseek-chat"、"gpt-4.1"）。
    pub id: String,
    /// 最大输出 token 数。
    pub max_tokens: Option<u32>,
    /// 上下文窗口大小限制（token 数）。
    pub context_limit: Option<usize>,
    /// 是否启用推理模式（如 DeepSeek reasoner）。启用后会正确回传 reasoning_content。
    #[serde(default)]
    pub reasoning: Option<bool>,
    /// 是否请求 provider 分离 reasoning/thinking 到独立字段（如 MiniMax reasoning_split）。
    #[serde(default)]
    pub reasoning_split: Option<bool>,
}

// ─── Runtime Section (placeholder for future use) ────────────────────────

/// 运行时配置段——保留用于 JSON 兼容性。字段在功能实现时添加。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSection {
    /// LLM 连接超时时间（秒）。
    pub llm_connect_timeout_secs: Option<u64>,
    /// LLM 读取超时时间（秒）。
    pub llm_read_timeout_secs: Option<u64>,
    /// LLM 最大重试次数。
    pub llm_max_retries: Option<u32>,
    /// LLM 重试的指数退避基础延迟（毫秒）。
    pub llm_retry_base_delay_ms: Option<u64>,
    // ── Compact ───────────────────────────────────────────────────────
    /// 是否启用自动压缩。
    pub compact_auto_enabled: Option<bool>,
    /// 触发自动压缩的上下文占用百分比阈值（0–100）。
    pub compact_threshold_percent: Option<f32>,
    /// 压缩失败时的最大重试次数。
    pub compact_max_retry_attempts: Option<u8>,
    /// LLM 压缩输出的最大 token 数。
    pub compact_max_output_tokens: Option<usize>,
    /// 自动/反应式 compact 保留的最近完整 turn 数。
    pub compact_keep_recent_turns: Option<usize>,
    /// auto-compact LLM 熔断器触发阈值。
    pub compact_circuit_breaker_threshold: Option<u32>,
    /// auto-compact LLM 熔断器冷却时间（秒）。
    pub compact_circuit_breaker_cooldown_secs: Option<u64>,
    /// 是否启用预测性 compact。
    pub predictive_compact_enabled: Option<bool>,
    /// 预测下一轮 token 增长时的保底值。
    pub predictive_compact_baseline_growth_tokens: Option<usize>,
    /// 压缩后恢复的最近读取文件数量上限。
    pub post_compact_max_files: Option<usize>,
    /// 压缩后恢复文件的总 token 预算。
    pub post_compact_token_budget: Option<usize>,
    /// 单个恢复文件的最大 token 数。
    pub post_compact_max_tokens_per_file: Option<usize>,
    // ── Agent ─────────────────────────────────────────────────────────
    /// 子 agent 最大嵌套深度（root=0, child=1, grandchild=2）。
    pub agent_max_depth: Option<usize>,
    /// 单轮中允许同时执行的并行工具调用数上限。
    pub agent_tool_max_parallel_calls: Option<usize>,
    /// Shell 工具默认超时时间（秒）。
    pub shell_timeout_secs: Option<u64>,
    // ── Extensions ───────────────────────────────────────────────────
    /// 通用扩展启停覆盖。适用于内置扩展和磁盘扩展。
    ///
    /// 例：`{ "astrcode.memory": false, "my.ipc.extension": false }`
    pub extension_states: Option<BTreeMap<String, bool>>,
}

// ─── Config Overlay ──────────────────────────────────────────────────────

/// 项目级配置覆盖层。
///
/// 用于 `.astrcode/config.json` 中的项目特定配置，
/// 可覆盖全局配置中的部分字段。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigOverlay {
    /// 覆盖激活的配置文件名。
    pub active_profile: Option<String>,
    /// 覆盖激活的模型标识。
    pub active_model: Option<String>,
    /// 覆盖小模型的配置文件名。
    pub active_small_profile: Option<String>,
    /// 覆盖小模型的模型标识。
    pub active_small_model: Option<String>,
    /// 覆盖配置文件列表。
    pub profiles: Option<Vec<Profile>>,
    /// 覆盖扩展专有配置。同 key 覆盖，异 key 保留。
    #[serde(default)]
    pub extensions: Option<BTreeMap<String, ExtensionRawConfig>>,
}

// ─── Selection Types ─────────────────────────────────────────────────────

/// 当前激活的配置选择结果，包含可能的警告信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSelection {
    /// 激活的配置文件名。
    pub active_profile: String,
    /// 激活的模型标识。
    pub active_model: String,
    /// 可选的警告信息（如模型不存在时的提示）。
    pub warning: Option<String>,
}

/// 模型选择信息，描述当前选择的完整模型上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelection {
    /// 配置文件名称。
    pub profile_name: String,
    /// 模型标识。
    pub model: String,
    /// 提供者类型。
    pub provider_kind: String,
}

impl ModelSelection {
    /// 构造只有模型 ID 的选择信息，用于尚未携带完整 profile/provider 上下文的边界。
    pub fn simple(model: impl Into<String>) -> Self {
        Self {
            profile_name: String::new(),
            model: model.into(),
            provider_kind: String::new(),
        }
    }
}

// ─── Default profiles (built-in) ─────────────────────────────────────────

/// 生成内置的默认配置文件列表。
///
/// 包含 DeepSeek、OpenAI、Anthropic 和 Google Gemini 四个预配置的 LLM 提供者。
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
                    id: "deepseek-v4-pro".into(),
                    max_tokens: Some(393216),
                    context_limit: Some(1000000),
                    reasoning: Some(true),
                    reasoning_split: None,
                },
                ModelConfig {
                    id: "deepseek-v4-flash".into(),
                    max_tokens: Some(393216),
                    context_limit: Some(1000000),
                    reasoning: Some(true),
                    reasoning_split: None,
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
                prompt_cache_retention: None,
                supports_stream_usage: Some(true),
            }),
            models: vec![ModelConfig {
                id: "gpt-4.1".into(),
                max_tokens: Some(16384),
                context_limit: Some(128000),
                reasoning: None,
                reasoning_split: None,
            }],
        },
        Profile {
            name: "anthropic".into(),
            provider_kind: "anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: Some("env:ANTHROPIC_API_KEY".into()),
            api_mode: None,
            openai_capabilities: None,
            models: vec![
                ModelConfig {
                    id: "claude-sonnet-4-6".into(),
                    max_tokens: Some(64000),
                    context_limit: Some(1000000),
                    reasoning: None,
                    reasoning_split: None,
                },
                ModelConfig {
                    id: "claude-opus-4-7".into(),
                    max_tokens: Some(128000),
                    context_limit: Some(1000000),
                    reasoning: None,
                    reasoning_split: None,
                },
            ],
        },
        Profile {
            name: "gemini".into(),
            provider_kind: "google_genai".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            api_key: Some("env:GOOGLE_API_KEY".into()),
            api_mode: None,
            openai_capabilities: None,
            models: vec![
                ModelConfig {
                    id: "gemini-2.5-pro".into(),
                    max_tokens: Some(16384),
                    context_limit: Some(1_048_576),
                    reasoning: None,
                    reasoning_split: None,
                },
                ModelConfig {
                    id: "gemini-2.5-flash".into(),
                    max_tokens: Some(16384),
                    context_limit: Some(1_048_576),
                    reasoning: None,
                    reasoning_split: None,
                },
            ],
        },
    ]
}

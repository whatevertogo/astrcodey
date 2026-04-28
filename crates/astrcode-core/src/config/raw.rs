//! 原始配置类型——从磁盘读取的 JSON 结构（所有字段可选/带默认值）。
//!
//! 这些类型直接对应配置文件的 JSON 结构，使用 `serde` 进行序列化/反序列化。
//! 字段使用 `camelCase` 命名约定以匹配 JSON 约定。

use serde::{Deserialize, Serialize};

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
    /// 运行时配置段（超时、重试等）。
    #[serde(default)]
    pub runtime: RuntimeSection,
    /// 可用的 LLM 配置文件列表。
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
    /// 是否支持流式用量统计。
    pub supports_stream_usage: Option<bool>,
}

/// 模型配置，定义一个具体模型的参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// 模型标识（如 "deepseek-chat"、"gpt-4.1"）。
    pub id: String,
    /// 最大输出 token 数。
    pub max_tokens: Option<u32>,
    /// 上下文窗口大小限制（token 数）。
    pub context_limit: Option<usize>,
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
    // TODO: 压缩相关字段
    // TODO: 工具并发相关字段
    // TODO: Agent 限制相关字段
}

// ─── Config Overlay ──────────────────────────────────────────────────────

/// 项目级配置覆盖层。
///
/// 用于 `.astrcode/config.json` 中的项目特定配置，
/// 可覆盖全局配置中的部分字段。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOverlay {
    /// 覆盖激活的配置文件名。
    pub active_profile: Option<String>,
    /// 覆盖激活的模型标识。
    pub active_model: Option<String>,
    /// 覆盖配置文件列表。
    pub profiles: Option<Vec<Profile>>,
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

// ─── Default profiles (built-in) ─────────────────────────────────────────

/// 生成内置的默认配置文件列表。
///
/// 包含 DeepSeek 和 OpenAI 两个预配置的 LLM 提供者。
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

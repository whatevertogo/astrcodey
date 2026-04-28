//! LLM 提供者抽象与消息类型。
//!
//! 本模块定义了与 LLM 交互所需的核心类型：
//! - [`LlmMessage`] / [`LlmContent`]：对话消息和内容类型
//! - [`LlmEvent`]：LLM 流式输出事件
//! - [`LlmProvider`] trait：所有 LLM 后端的统一接口
//! - [`LlmClientConfig`]：LLM 客户端配置
//! - [`LlmError`]：LLM 操作错误类型

use serde::{Deserialize, Serialize};

use crate::tool::ToolDefinition;

/// LLM 对话消息中的角色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmRole {
    /// 系统指令消息。
    System,
    /// 用户消息。
    User,
    /// 助手回复消息。
    Assistant,
    /// 工具结果消息。
    Tool,
}

impl LlmRole {
    /// 返回角色的字符串表示，用于协议序列化。
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmRole::System => "system",
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::Tool => "tool",
        }
    }
}

/// LLM 消息的内容——可以是文本、图片、工具调用或工具结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmContent {
    /// 纯文本内容。
    Text { text: String },
    /// Base64 编码的图片。
    Image {
        /// Base64 编码的图片数据。
        base64: String,
        /// 图片的 MIME 类型（如 "image/png"）。
        media_type: String,
    },
    /// 助手请求的工具调用内容。
    ToolCall {
        /// 工具调用的唯一标识。
        call_id: String,
        /// 要调用的工具名称。
        name: String,
        /// 工具调用参数（JSON 值）。
        arguments: serde_json::Value,
    },
    /// 工具执行结果内容。
    ToolResult {
        /// 对应的工具调用 ID。
        tool_call_id: String,
        /// 工具输出的内容文本。
        content: String,
        /// 是否为错误结果。
        is_error: bool,
    },
}

/// LLM 对话中的一条消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    /// 消息角色。
    pub role: LlmRole,
    /// 消息内容列表（支持混合文本、图片、工具调用等）。
    pub content: Vec<LlmContent>,
    /// 可选的工具消息名称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl LlmMessage {
    /// 创建一条用户文本消息。
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::User,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
        }
    }

    /// 创建一条助手文本消息。
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::Assistant,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
        }
    }

    /// 创建一条系统指令消息。
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::System,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
        }
    }

    /// 创建一条工具结果消息。
    ///
    /// - `name`：工具名称
    /// - `tool_call_id`：对应的工具调用 ID
    /// - `content`：工具输出的内容
    /// - `is_error`：是否为错误结果
    pub fn tool(
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: tool_call_id.into(),
                content: content.into(),
                is_error,
            }],
            name: Some(name.into()),
        }
    }
}

/// LLM 流式输出过程中的事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmEvent {
    /// 文本增量（部分响应）。
    ContentDelta { delta: String },
    /// 工具调用已开始。
    ToolCallStart {
        /// 工具调用 ID。
        call_id: String,
        /// 工具名称。
        name: String,
        /// 初始参数片段。
        arguments: String,
    },
    /// 工具调用参数增量。
    ToolCallDelta {
        /// 工具调用 ID。
        call_id: String,
        /// 本次增量参数片段。
        delta: String,
    },
    /// 流式输出已完成。
    Done { finish_reason: String },
    /// 流式输出过程中发生错误。
    Error { message: String },
}

/// LLM 调用的完整输出结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmOutput {
    /// 累积的文本内容。
    pub text: String,
    /// LLM 请求的工具调用列表（如有）。
    pub tool_calls: Vec<ParsedToolCall>,
    /// 提供者返回的完成原因。
    pub finish_reason: String,
}

/// 从 LLM 响应中解析出的工具调用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedToolCall {
    /// 工具调用的唯一标识。
    pub call_id: String,
    /// 要调用的工具名称。
    pub name: String,
    /// 工具调用参数（JSON 值）。
    pub arguments: serde_json::Value,
}

/// LLM 提供者操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// 提示词超出模型上下文长度限制。
    #[error("Prompt too long: {0}")]
    PromptTooLong(String),
    /// 客户端错误（4xx 状态码）。
    #[error("Client error ({status}): {message}")]
    ClientError { status: u16, message: String },
    /// 服务端错误（5xx 状态码）。
    #[error("Server error ({status}): {message}")]
    ServerError { status: u16, message: String },
    /// 网络传输错误。
    #[error("Transport error: {0}")]
    Transport(String),
    /// 请求被中断（用户取消）。
    #[error("Request interrupted")]
    Interrupted,
    /// 流式响应解析错误。
    #[error("Stream parse error: {0}")]
    StreamParse(String),
}

/// LLM 客户端配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmClientConfig {
    /// API 端点的基础 URL。
    pub base_url: String,
    /// API 密钥。
    pub api_key: String,
    /// 连接超时时间（秒）。
    pub connect_timeout_secs: u64,
    /// 读取超时时间（秒）。
    pub read_timeout_secs: u64,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 指数退避的基础延迟（毫秒）。
    pub retry_base_delay_ms: u64,
    /// 额外的 HTTP 请求头。
    pub extra_headers: std::collections::HashMap<String, String>,
}

impl Default for LlmClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            api_key: String::new(),
            connect_timeout_secs: 10,
            read_timeout_secs: 90,
            max_retries: 2,
            retry_base_delay_ms: 250,
            extra_headers: std::collections::HashMap::new(),
        }
    }
}

/// `LlmProvider` trait——所有 LLM 后端都实现此接口。
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// 生成流式 LLM 响应。
    ///
    /// 返回一个通道接收端，按到达顺序产生 [`LlmEvent`] 值。
    /// 当流式输出完成或出错时通道关闭。
    ///
    /// - `messages`：对话消息列表
    /// - `tools`：可供 LLM 调用的工具定义列表
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError>;

    /// 返回模型的上下文窗口限制。
    fn model_limits(&self) -> ModelLimits;
}

/// 模型的上下文窗口限制。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    /// 最大输入 token 数。
    pub max_input_tokens: usize,
    /// 最大输出 token 数。
    pub max_output_tokens: usize,
}

//! LLM provider abstraction and message types.

use serde::{Deserialize, Serialize};

use crate::tool::ToolDefinition;

/// Role of a message in an LLM conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmRole {
    System,
    User,
    Assistant,
    Tool,
}

impl LlmRole {
    /// Returns the role as a lowercase string for protocol serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmRole::System => "system",
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::Tool => "tool",
        }
    }
}

/// Content of an LLM message — can be text, image, tool call, or tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmContent {
    /// Plain text content.
    Text { text: String },
    /// Base64-encoded image.
    Image { base64: String, media_type: String },
    /// Assistant-requested tool call content.
    ToolCall {
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// Tool result content.
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

/// A message in an LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: Vec<LlmContent>,
    /// Optional name for tool messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl LlmMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::User,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::Assistant,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: LlmRole::System,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
        }
    }

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

/// Event emitted during LLM streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmEvent {
    /// A text delta (partial response).
    ContentDelta { delta: String },
    /// A tool call has started.
    ToolCallStart {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Tool call arguments delta.
    ToolCallDelta { call_id: String, delta: String },
    /// Streaming has finished.
    Done { finish_reason: String },
    /// An error occurred during streaming.
    Error { message: String },
}

/// The complete output from an LLM call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmOutput {
    /// Accumulated text content.
    pub text: String,
    /// Tool calls requested by the LLM (if any).
    pub tool_calls: Vec<ParsedToolCall>,
    /// Finish reason from the provider.
    pub finish_reason: String,
}

/// A parsed tool call from the LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Error from LLM provider operations.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("Prompt too long: {0}")]
    PromptTooLong(String),
    #[error("Client error ({status}): {message}")]
    ClientError { status: u16, message: String },
    #[error("Server error ({status}): {message}")]
    ServerError { status: u16, message: String },
    #[error("Transport error: {0}")]
    Transport(String),
    #[error("Request interrupted")]
    Interrupted,
    #[error("Stream parse error: {0}")]
    StreamParse(String),
}

/// Configuration for an LLM client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmClientConfig {
    /// Base URL for the API endpoint.
    pub base_url: String,
    /// API key.
    pub api_key: String,
    /// Connect timeout in seconds.
    pub connect_timeout_secs: u64,
    /// Read timeout in seconds.
    pub read_timeout_secs: u64,
    /// Maximum number of retry attempts.
    pub max_retries: u32,
    /// Base delay for exponential backoff (milliseconds).
    pub retry_base_delay_ms: u64,
    /// Extra HTTP headers.
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

/// The `LlmProvider` trait — all LLM backends implement this.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// Generate a streaming response from the LLM.
    ///
    /// Returns a channel receiver that yields `LlmEvent` values as they arrive.
    /// The channel is closed when streaming completes or errors.
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError>;

    /// Returns the model's context window limits.
    fn model_limits(&self) -> ModelLimits;
}

/// Context window limits for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    /// Maximum input tokens.
    pub max_input_tokens: usize,
    /// Maximum output tokens.
    pub max_output_tokens: usize,
}

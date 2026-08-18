//! Wire contracts for the LLM host capability.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Typed request shared by bundled and worker model clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostLlmChatRequest {
    pub messages: Vec<HostLlmMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
}

impl HostLlmChatRequest {
    pub fn new(messages: Vec<HostLlmMessage>) -> Self {
        Self {
            messages,
            max_output_tokens: None,
        }
    }

    /// Caps this request's generated output without changing the configured model limit.
    pub fn with_max_output_tokens(mut self, max_output_tokens: usize) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostLlmRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostLlmContent {
    Text {
        text: String,
    },
    Image {
        base64: String,
        media_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_arguments: Option<String>,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmMessage {
    pub role: HostLlmRole,
    pub content: Vec<HostLlmContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// Completed non-streaming model response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmChatOutput {
    pub content: String,
    pub model: String,
}

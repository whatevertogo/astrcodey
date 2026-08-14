//! LLM 宿主能力线缆契约。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Typed request shared by bundled and worker model clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmChatRequest {
    pub messages: Vec<HostLlmMessage>,
}

impl HostLlmChatRequest {
    pub fn new(messages: Vec<HostLlmMessage>) -> Self {
        Self { messages }
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

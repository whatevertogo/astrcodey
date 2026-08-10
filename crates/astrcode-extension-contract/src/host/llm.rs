//! LLM 宿主能力线缆契约。

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    error::WireErrorCode,
    protocol::{ErrorPayload, ModelStreamEvent},
};

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

/// One ordered text delta emitted by a model stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmTextDelta {
    pub delta: String,
}

/// Collected model stream returned after generation completes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmCollectedStreamOutput {
    pub content: String,
    pub model: String,
    pub chunks: Vec<HostLlmTextDelta>,
}

fn collect_model_stream_output(
    completed: &Value,
    chunks: Vec<HostLlmTextDelta>,
) -> Result<HostLlmCollectedStreamOutput, ErrorPayload> {
    let model = completed
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::InvalidResponse,
                "completed model stream is missing string field `model`",
            )
        })?
        .to_owned();
    let content = completed
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| chunks.iter().map(|chunk| chunk.delta.as_str()).collect());
    Ok(HostLlmCollectedStreamOutput {
        content,
        model,
        chunks,
    })
}

/// Collects a model event stream into the typed response used by host clients.
#[doc(hidden)]
pub async fn collect_model_stream<S>(
    mut stream: S,
) -> Result<HostLlmCollectedStreamOutput, ErrorPayload>
where
    S: Stream<Item = ModelStreamEvent> + Unpin,
{
    let mut chunks = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            ModelStreamEvent::ContentDelta { content } => {
                chunks.push(HostLlmTextDelta { delta: content });
            },
            ModelStreamEvent::Completed { output } => {
                return collect_model_stream_output(&output, chunks);
            },
            ModelStreamEvent::Failed { error } => return Err(error),
            _ => {},
        }
    }
    Err(ErrorPayload::new(
        WireErrorCode::StreamClosed,
        "model stream closed without a terminal event",
    ))
}

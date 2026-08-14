use futures_util::StreamExt;

use crate::{
    llm::{LlmContent, LlmMessage, LlmRole},
    model_stream::{ModelStream, ModelStreamEvent},
    wire::{WireErrorCode, host::*, protocol::ErrorPayload},
};

pub(crate) async fn collect_model_stream(
    mut stream: ModelStream,
) -> Result<HostLlmChatOutput, ErrorPayload> {
    let mut streamed_content = String::new();
    while let Some(event) = stream.next().await {
        match event {
            ModelStreamEvent::ContentDelta { content } => streamed_content.push_str(&content),
            ModelStreamEvent::Completed { output } => {
                let model = output
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        ErrorPayload::new(
                            WireErrorCode::InvalidResponse,
                            "completed model stream is missing string field `model`",
                        )
                    })?
                    .to_owned();
                let content = output
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or(streamed_content);
                return Ok(HostLlmChatOutput { content, model });
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

/// Builds an author-facing host request from domain LLM messages.
pub fn llm_chat_request(messages: Vec<LlmMessage>) -> HostLlmChatRequest {
    HostLlmChatRequest::new(llm_messages_to_wire(messages))
}

pub fn llm_messages_from_wire(messages: Vec<HostLlmMessage>) -> Vec<LlmMessage> {
    messages.into_iter().map(llm_message_from_wire).collect()
}

pub fn llm_messages_to_wire(messages: Vec<LlmMessage>) -> Vec<HostLlmMessage> {
    messages.into_iter().map(llm_message_to_wire).collect()
}

pub fn llm_message_to_wire(message: LlmMessage) -> HostLlmMessage {
    HostLlmMessage {
        role: match message.role {
            LlmRole::System => HostLlmRole::System,
            LlmRole::User => HostLlmRole::User,
            LlmRole::Assistant => HostLlmRole::Assistant,
            LlmRole::Tool => HostLlmRole::Tool,
        },
        content: message
            .content
            .into_iter()
            .map(llm_content_to_wire)
            .collect(),
        name: message.name,
        reasoning_content: message.reasoning_content,
    }
}

fn llm_message_from_wire(message: HostLlmMessage) -> LlmMessage {
    LlmMessage {
        role: match message.role {
            HostLlmRole::System => LlmRole::System,
            HostLlmRole::User => LlmRole::User,
            HostLlmRole::Assistant => LlmRole::Assistant,
            HostLlmRole::Tool => LlmRole::Tool,
        },
        content: message
            .content
            .into_iter()
            .map(llm_content_from_wire)
            .collect(),
        name: message.name,
        reasoning_content: message.reasoning_content,
    }
}

fn llm_content_to_wire(content: LlmContent) -> HostLlmContent {
    match content {
        LlmContent::Text { text } => HostLlmContent::Text { text },
        LlmContent::Image {
            base64,
            media_type,
            filename,
        } => HostLlmContent::Image {
            base64,
            media_type,
            filename,
        },
        LlmContent::ToolCall {
            call_id,
            name,
            arguments,
            raw_arguments,
        } => HostLlmContent::ToolCall {
            call_id,
            name,
            arguments,
            raw_arguments,
        },
        LlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => HostLlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        },
    }
}

fn llm_content_from_wire(content: HostLlmContent) -> LlmContent {
    match content {
        HostLlmContent::Text { text } => LlmContent::Text { text },
        HostLlmContent::Image {
            base64,
            media_type,
            filename,
        } => LlmContent::Image {
            base64,
            media_type,
            filename,
        },
        HostLlmContent::ToolCall {
            call_id,
            name,
            arguments,
            raw_arguments,
        } => LlmContent::ToolCall {
            call_id,
            name,
            arguments,
            raw_arguments,
        },
        HostLlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => LlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::wire::protocol::{ErrorPayload, ModelStreamEvent};

    fn stream<I>(events: I) -> ModelStream
    where
        I: IntoIterator<Item = ModelStreamEvent>,
        I::IntoIter: Send + 'static,
    {
        ModelStream::from_stream(futures_util::stream::iter(events), CancellationToken::new())
    }

    #[tokio::test]
    async fn collected_model_stream_requires_and_preserves_one_terminal_result() {
        let collected = collect_model_stream(stream([
            ModelStreamEvent::ContentDelta {
                content: "hel".into(),
            },
            ModelStreamEvent::ContentDelta {
                content: "lo".into(),
            },
            ModelStreamEvent::Completed {
                output: json!({ "model": "main", "finish_reason": "stop" }),
            },
        ]))
        .await
        .unwrap();
        assert_eq!(collected.content, "hello");
        assert_eq!(collected.model, "main");

        let missing_model = collect_model_stream(stream([ModelStreamEvent::Completed {
            output: json!({ "content": "hello" }),
        }]))
        .await
        .unwrap_err();
        assert_eq!(
            missing_model.code_enum(),
            Some(WireErrorCode::InvalidResponse)
        );

        let closed = collect_model_stream(stream([])).await.unwrap_err();
        assert_eq!(closed.code_enum(), Some(WireErrorCode::StreamClosed));

        let expected = ErrorPayload::new(WireErrorCode::BackendUnavailable, "model unavailable");
        let failed = collect_model_stream(stream([ModelStreamEvent::Failed {
            error: expected.clone(),
        }]))
        .await
        .unwrap_err();
        assert_eq!(failed, expected);
    }
}

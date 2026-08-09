pub use astrcode_extension_contract::host::*;

use crate::llm::{LlmContent, LlmMessage, LlmRole};

#[doc(hidden)]
pub fn llm_chat_request(messages: Vec<LlmMessage>) -> HostLlmChatRequest {
    HostLlmChatRequest::new(llm_messages_to_wire(messages))
}

#[doc(hidden)]
pub fn llm_messages_from_wire(messages: Vec<HostLlmMessage>) -> Vec<LlmMessage> {
    messages.into_iter().map(llm_message_from_wire).collect()
}

#[doc(hidden)]
pub fn llm_messages_to_wire(messages: Vec<LlmMessage>) -> Vec<HostLlmMessage> {
    messages.into_iter().map(llm_message_to_wire).collect()
}

#[doc(hidden)]
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

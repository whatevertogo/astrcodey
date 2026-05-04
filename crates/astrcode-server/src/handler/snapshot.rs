use astrcode_core::llm::{LlmContent, LlmMessage};
use astrcode_protocol::events::{MessageDto, SessionSnapshot};

pub(super) fn session_snapshot(
    state: &astrcode_core::storage::SessionReadModel,
) -> SessionSnapshot {
    SessionSnapshot {
        session_id: state.session_id.clone(),
        cursor: state.cursor(),
        messages: state.messages.iter().map(message_to_dto).collect(),
        model_id: state.model_id.clone(),
        working_dir: state.working_dir.clone(),
    }
}

/// 将 LLM 消息转换为传输层 DTO，用于会话快照。
pub(super) fn message_to_dto(message: &LlmMessage) -> MessageDto {
    MessageDto {
        role: message.role.as_str().to_string(),
        content: message
            .content
            .iter()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// 将 LLM 内容块转换为纯文本表示，用于客户端展示。
fn content_to_text(content: &LlmContent) -> String {
    match content {
        LlmContent::Text { text } => text.clone(),
        LlmContent::Image { .. } => "[image]".into(),
        LlmContent::ToolCall {
            name, arguments, ..
        } => format!("tool call: {name}({arguments})"),
        LlmContent::ToolResult { content, .. } => content.clone(),
    }
}

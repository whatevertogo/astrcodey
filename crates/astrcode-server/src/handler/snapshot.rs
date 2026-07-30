//! 会话快照 — 内部模型转传输层 DTO。

use astrcode_context::is_compact_summary_message;
use astrcode_core::llm::{LlmContent, LlmMessage};
use astrcode_protocol::{
    events::{MessageDto, SessionSnapshot},
    wire::MessageRoleDto,
};

use crate::protocol_mapping::agent_session_link_to_dto;

/// 构建会话快照 DTO，用于客户端同步。
pub(crate) fn session_snapshot(
    state: &astrcode_session_projection::SessionReadModel,
) -> SessionSnapshot {
    SessionSnapshot {
        session_id: state.identity.session_id.to_string(),
        cursor: state.cursor(),
        messages: state
            .transcript
            .messages
            .iter()
            .map(|m| message_to_dto(&m.message))
            .collect(),
        model_id: state.identity.model_id.clone(),
        working_dir: state.identity.working_dir.clone(),
        agent_sessions: state
            .agent_sessions
            .iter()
            .map(agent_session_link_to_dto)
            .collect(),
    }
}

/// 将 LLM 消息转换为传输层 DTO。
///
/// Compact summary 消息（synthetic user message）会被转换为 system 角色，
/// 以便客户端能正确识别其为系统生成的上下文摘要。
pub(crate) fn message_to_dto(message: &LlmMessage) -> MessageDto {
    let content = message
        .content
        .iter()
        .map(content_display_text)
        .collect::<String>();
    let is_compact_summary = is_compact_summary_message(message);

    // Compact summary 消息是 synthetic user message，但在客户端应显示为系统消息。
    let role = if is_compact_summary {
        MessageRoleDto::System
    } else {
        message.role.into()
    };

    MessageDto {
        role,
        content,
        is_compact_summary: Some(is_compact_summary),
    }
}

/// 单块内容的展示文本：`upsertSessionPlan` 提取 plan 正文，其余走契约层的
/// [`LlmContent::to_display_text`]。
fn content_display_text(content: &LlmContent) -> String {
    match content {
        LlmContent::ToolCall {
            name, arguments, ..
        } if name == "upsertSessionPlan" => arguments
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        other => other.to_display_text(),
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmContent, LlmRole};

    use super::*;

    fn simple_text_message(text: &str) -> LlmMessage {
        LlmMessage {
            role: LlmRole::User,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
            reasoning_content: None,
        }
    }

    #[test]
    fn message_dto_preserves_wire_shape_and_compact_summary_semantics() {
        let regular = message_to_dto(&simple_text_message("Hello, how are you?"));
        assert_eq!(regular.role, MessageRoleDto::User);
        assert_eq!(regular.content, "Hello, how are you?");
        assert_eq!(regular.is_compact_summary, Some(false));
        assert_eq!(
            serde_json::to_value(&regular).unwrap(),
            serde_json::json!({
                "role": "user",
                "content": "Hello, how are you?",
                "is_compact_summary": false
            })
        );

        let compact =
            simple_text_message("<compact_summary>\nSummary:\nTest summary\n</compact_summary>");
        let compact = message_to_dto(&compact);
        assert_eq!(compact.role, MessageRoleDto::System);
        assert!(compact.content.contains("<compact_summary>"));
        assert_eq!(compact.is_compact_summary, Some(true));
        assert_eq!(
            serde_json::to_value(&compact).unwrap(),
            serde_json::json!({
                "role": "system",
                "content": "<compact_summary>\nSummary:\nTest summary\n</compact_summary>",
                "is_compact_summary": true
            })
        );

        let legacy: MessageDto = serde_json::from_value(serde_json::json!({
            "role": "system",
            "content": "Legacy system message"
        }))
        .unwrap();
        assert_eq!(legacy.is_compact_summary, None);
        assert!(!legacy.compact_summary_semantics());
    }
}

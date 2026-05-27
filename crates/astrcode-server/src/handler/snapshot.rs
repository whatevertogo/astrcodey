//! 会话快照 — 内部模型转传输层 DTO。

use astrcode_core::llm::LlmMessage;
use astrcode_protocol::events::{
    AgentSessionLinkDto, MessageDto, SessionControlStateDto, SessionSnapshot,
};

use crate::http::control_from_phase;

/// 构建会话快照 DTO，用于客户端同步。
pub(crate) fn session_snapshot(
    state: &astrcode_core::storage::SessionReadModel,
) -> SessionSnapshot {
    SessionSnapshot {
        session_id: state.session_id.to_string(),
        cursor: state.cursor(),
        messages: state
            .messages
            .iter()
            .map(|m| message_to_dto(&m.message))
            .collect(),
        model_id: state.model_id.clone(),
        working_dir: state.working_dir.clone(),
        agent_sessions: state
            .agent_sessions
            .iter()
            .map(AgentSessionLinkDto::from_view)
            .collect(),
        control: Some(SessionControlStateDto::from_http(&control_from_phase(
            state.phase,
            !state.messages.is_empty(),
        ))),
    }
}

/// 将 LLM 消息转换为传输层 DTO。
///
/// Compact summary 消息（synthetic user message）会被转换为 system 角色，
/// 以便客户端能正确识别其为系统生成的上下文摘要。
pub fn message_to_dto(message: &LlmMessage) -> MessageDto {
    let content = message
        .content
        .iter()
        .map(|c| c.to_display_text())
        .collect::<String>();

    // Compact summary 消息是 synthetic user message，但在客户端应显示为系统消息
    // TODO: 这里的 compact_summary marker 检测依赖了 astrcode_context::compaction 的内部函数，
    // 如果 marker 格式变化会导致快照静默错误。应该在传输边界定义自己的 marker 常量。
    let role = if astrcode_context::compaction::is_compact_summary_text(&content) {
        "system"
    } else {
        message.role.as_str()
    };

    MessageDto {
        role: role.to_string(),
        content,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmContent, LlmRole};

    use super::*;

    /// 辅助函数：创建简单的文本消息
    fn simple_text_message(text: &str) -> LlmMessage {
        LlmMessage {
            role: LlmRole::User,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
            reasoning_content: None,
        }
    }

    #[test]
    fn compact_summary_message_converts_to_system_role() {
        let compact_msg =
            simple_text_message("<compact_summary>\nSummary:\nTest summary\n</compact_summary>");

        let dto = message_to_dto(&compact_msg);

        assert_eq!(dto.role, "system");
        assert!(dto.content.contains("<compact_summary>"));
    }

    #[test]
    fn regular_user_message_preserves_user_role() {
        let user_msg = simple_text_message("Hello, how are you?");

        let dto = message_to_dto(&user_msg);

        assert_eq!(dto.role, "user");
        assert_eq!(dto.content, "Hello, how are you?");
    }

    #[test]
    fn is_compact_summary_message_detects_marker() {
        use astrcode_context::compaction::is_compact_summary_text;
        assert!(is_compact_summary_text("<compact_summary>\nContent"));
        assert!(is_compact_summary_text("  <compact_summary>\nContent"));
        assert!(is_compact_summary_text("\n<compact_summary>\nContent"));
        assert!(!is_compact_summary_text("Regular message"));
        assert!(!is_compact_summary_text("</compact_summary>"));
    }
}

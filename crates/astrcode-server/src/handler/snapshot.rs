//! 会话快照 — 内部模型转传输层 DTO。

use astrcode_core::{
    llm::{LlmContent, LlmMessage},
    storage::AgentSessionStatus,
};
use astrcode_protocol::events::{
    AgentSessionLinkDto, AgentSessionStatusDto, MessageDto, SessionSnapshot,
};

/// Agent 会话状态转换。
pub(crate) fn agent_status_to_dto(status: AgentSessionStatus) -> AgentSessionStatusDto {
    match status {
        AgentSessionStatus::Running => AgentSessionStatusDto::Running,
        AgentSessionStatus::Completed => AgentSessionStatusDto::Completed,
        AgentSessionStatus::Failed => AgentSessionStatusDto::Failed,
    }
}

/// 构建会话快照 DTO，用于客户端同步。
pub(crate) fn session_snapshot(
    state: &astrcode_core::storage::SessionReadModel,
) -> SessionSnapshot {
    SessionSnapshot {
        session_id: state.session_id.to_string(),
        cursor: state.cursor(),
        messages: state.messages.iter().map(message_to_dto).collect(),
        model_id: state.model_id.clone(),
        working_dir: state.working_dir.clone(),
        agent_sessions: state
            .agent_sessions
            .iter()
            .map(|link| AgentSessionLinkDto {
                child_session_id: link.child_session_id.to_string(),
                agent_name: link.agent_name.clone(),
                task: link.task.clone(),
                status: agent_status_to_dto(link.status),
            })
            .collect(),
    }
}

/// 将 LLM 消息转换为传输层 DTO。
///
/// Compact summary 消息（synthetic user message）会被转换为 system 角色，
/// 以便客户端能正确识别其为系统生成的上下文摘要。
pub(super) fn message_to_dto(message: &LlmMessage) -> MessageDto {
    let content = message
        .content
        .iter()
        .map(content_to_text)
        .collect::<String>();

    // Compact summary 消息是 synthetic user message，但在客户端应显示为系统消息
    let role = if is_compact_summary_message(&content) {
        "system"
    } else {
        message.role.as_str()
    };

    MessageDto {
        role: role.to_string(),
        content,
    }
}

/// 检测消息是否是 compact summary synthetic message。
fn is_compact_summary_message(content: &str) -> bool {
    content.trim_start().starts_with("<compact_summary>")
}

/// 将 LLM 内容块转换为快照纯文本。
///
/// 快照的目的是让客户端在 resume 时能重建可读的对话视图。
/// 这是有损转换——不可能完全还原原始渲染效果（如 RenderSpec、折叠等）。
///
/// 设计决策：
/// - `Text` / `ToolResult`：原样输出，这些就是用户看到的内容。
/// - `ToolCall`：大部分工具调用（shell、write 等）在 resume 时不需要回放参数， 但
///   `upsertSessionPlan` 的 arguments.content 携带 plan 正文，需要提取并保留。
///   其他工具调用只输出工具名。
/// - 如果后续有更多工具需要在快照中展示参数内容，可以在这里扩展， 或改为让 ToolRenderer 参与
///   snapshot 生成（目前没必要）。
pub(crate) fn content_to_text(content: &LlmContent) -> String {
    match content {
        LlmContent::Text { text } => text.clone(),
        LlmContent::Image { .. } => "[image]".into(),
        LlmContent::ToolCall {
            name, arguments, ..
        } => match name.as_str() {
            "upsertSessionPlan" => arguments
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default(),
            _ => format!("tool call: {name}"),
        },
        LlmContent::ToolResult { content, .. } => content.clone(),
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
        assert!(is_compact_summary_message("<compact_summary>\nContent"));
        assert!(is_compact_summary_message("  <compact_summary>\nContent"));
        assert!(is_compact_summary_message("\n<compact_summary>\nContent"));
        assert!(!is_compact_summary_message("Regular message"));
        assert!(!is_compact_summary_message("</compact_summary>"));
    }
}

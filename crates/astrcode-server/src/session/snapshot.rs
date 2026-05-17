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
pub(crate) fn message_to_dto(message: &LlmMessage) -> MessageDto {
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

/// 将 LLM 内容块转换为纯文本表示。
pub(crate) fn content_to_text(content: &LlmContent) -> String {
    match content {
        LlmContent::Text { text } => text.clone(),
        LlmContent::Image { .. } => "[image]".into(),
        LlmContent::ToolCall {
            name, arguments, ..
        } => format!("tool call: {name}({arguments})"),
        LlmContent::ToolResult { content, .. } => content.clone(),
    }
}

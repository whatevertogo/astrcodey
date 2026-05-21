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

//! 从 [`EventPayload`] 推导会话/子 Agent 阶段（SSE 与控制态的唯一规则源）。
//!
//! durable 读模型在 [`super::reduce`] 中维护；本模块为「单事件 → 控制态/子 Agent 阶段」
//! 提供与 HTTP live 投影、子 session SSE 一致的规则。

use astrcode_core::event::{EventPayload, Phase};

/// 子 Agent 链接上的 live 阶段更新。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildAgentPhaseUpdate {
    pub phase: Phase,
    pub current_tool: Option<String>,
}

/// 该事件是否应推送会话 control phase 更新；`None` 表示不改变控制态。
pub fn phase_for_control_update(payload: &EventPayload) -> Option<Phase> {
    match payload {
        EventPayload::TurnStarted
        | EventPayload::UserMessage { .. }
        | EventPayload::AgentRunStarted => Some(Phase::Thinking),
        EventPayload::AssistantMessageStarted { .. }
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. } => Some(Phase::Streaming),
        EventPayload::ToolCallStarted { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolCallRequested { .. }
        | EventPayload::ToolOutputDelta { .. }
        | EventPayload::ToolCallCompleted { .. }
        | EventPayload::ToolCallBackgrounded { .. } => Some(Phase::CallingTool),
        EventPayload::CompactionStarted => Some(Phase::Compacting),
        EventPayload::ErrorOccurred { .. } => Some(Phase::Error),
        EventPayload::TurnCompleted { .. } | EventPayload::AgentRunCompleted { .. } => {
            Some(Phase::Idle)
        },
        EventPayload::CompactionCompleted { .. }
        | EventPayload::CompactionSkipped { .. }
        | EventPayload::CompactionFailed { .. } => None,
        _ => None,
    }
}

/// 子 session 事件流上的 Agent 卡片阶段；`None` 表示无需更新。
pub fn child_agent_phase_update(payload: &EventPayload) -> Option<ChildAgentPhaseUpdate> {
    let (phase, current_tool) = match payload {
        EventPayload::TurnStarted | EventPayload::AgentRunStarted => (Phase::Thinking, None),
        EventPayload::AssistantMessageStarted { .. } | EventPayload::AssistantTextDelta { .. } => {
            (Phase::Streaming, None)
        },
        EventPayload::ToolCallStarted { tool_name, .. }
        | EventPayload::ToolCallRequested { tool_name, .. } => {
            (Phase::CallingTool, Some(tool_name.clone()))
        },
        EventPayload::ToolCallCompleted { .. } => (Phase::Thinking, None),
        EventPayload::TurnCompleted { .. } | EventPayload::AgentRunCompleted { .. } => {
            (Phase::Idle, None)
        },
        EventPayload::ErrorOccurred { .. } => (Phase::Error, None),
        _ => return None,
    };
    Some(ChildAgentPhaseUpdate {
        phase,
        current_tool,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_completed_yields_idle_control_phase() {
        assert_eq!(
            phase_for_control_update(&EventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            }),
            Some(Phase::Idle)
        );
    }

    #[test]
    fn tool_call_started_yields_child_calling_tool() {
        let update = child_agent_phase_update(&EventPayload::ToolCallStarted {
            call_id: "c1".into(),
            tool_name: "read".into(),
        })
        .unwrap();
        assert_eq!(update.phase, Phase::CallingTool);
        assert_eq!(update.current_tool.as_deref(), Some("read"));
    }
}

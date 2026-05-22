//! Turn 生命周期与 compact continuation 事件载荷构造。
//!
//! Durable 事件和 live 事件分开构造，调用方按需选择 `emit_durable` 或 `emit_live`。

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::EventPayload,
    extension::CompactStrategy,
    types::{Cursor, MessageId, SessionId},
};

/// 构造 agent turn 开始时需要持久化的状态事件。
pub fn agent_turn_started_durable_payloads(
    message_id: MessageId,
    user_text: String,
) -> [EventPayload; 2] {
    [
        EventPayload::TurnStarted,
        EventPayload::UserMessage {
            message_id,
            text: user_text,
        },
    ]
}

/// 构造 agent turn 开始时只需 fanout 的 live 事件。
pub fn agent_turn_started_live_payload() -> EventPayload {
    EventPayload::AgentRunStarted
}

/// 构造 agent turn 正常结束时需要持久化的状态事件。
pub fn agent_turn_completed_durable_payload(reason: String) -> EventPayload {
    EventPayload::TurnCompleted {
        finish_reason: reason,
    }
}

/// 构造 agent turn 正常结束时只需 fanout 的 live 事件。
pub fn agent_turn_completed_live_payload(reason: String) -> EventPayload {
    EventPayload::AgentRunCompleted { reason }
}

/// 构造 compact continuation 边界事件载荷。
pub fn compact_boundary_payload(
    trigger: impl Into<String>,
    compaction: &CompactResult,
    continued_session_id: SessionId,
    base_event_seq: u64,
    strategy: CompactStrategy,
) -> EventPayload {
    EventPayload::CompactBoundaryCreated {
        trigger: trigger.into(),
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        summary: compaction.summary.clone(),
        transcript_path: compaction.transcript_path.clone(),
        continued_session_id,
        base_event_seq,
        strategy,
    }
}

/// 构造子会话 compact continuation 投影事件载荷。
pub fn session_continued_from_compaction_payload(
    parent_session_id: SessionId,
    parent_cursor: Cursor,
    compaction: &CompactResult,
) -> EventPayload {
    EventPayload::SessionContinuedFromCompaction {
        parent_session_id,
        parent_cursor,
        summary: compaction.summary.clone(),
        transcript_path: compaction.transcript_path.clone(),
        context_messages: compaction.context_messages.clone(),
        retained_messages: compaction.retained_messages.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_turn_started_durable_returns_turn_started_and_user_message() {
        let payloads = agent_turn_started_durable_payloads("message-1".into(), "hello".into());
        assert!(matches!(payloads[0], EventPayload::TurnStarted));
        assert!(matches!(
            &payloads[1],
            EventPayload::UserMessage { message_id, text }
                if message_id.as_str() == "message-1" && text == "hello"
        ));
    }

    #[test]
    fn agent_turn_started_live_returns_agent_run_started() {
        let payload = agent_turn_started_live_payload();
        assert!(matches!(payload, EventPayload::AgentRunStarted));
    }

    #[test]
    fn agent_turn_completed_durable_returns_turn_completed() {
        let payload = agent_turn_completed_durable_payload("stop".into());
        assert!(matches!(
            payload,
            EventPayload::TurnCompleted { finish_reason } if finish_reason == "stop"
        ));
    }

    #[test]
    fn agent_turn_completed_live_returns_agent_run_completed() {
        let payload = agent_turn_completed_live_payload("stop".into());
        assert!(matches!(
            payload,
            EventPayload::AgentRunCompleted { reason } if reason == "stop"
        ));
    }
}

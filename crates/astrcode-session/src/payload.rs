//! Compact continuation 事件载荷构造。

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::EventPayload,
    types::{Cursor, MessageId, SessionId},
};

/// 构造一轮 agent 对话开始时的标准事件序列。
pub fn agent_turn_started_payloads(message_id: MessageId, user_text: String) -> [EventPayload; 3] {
    [
        EventPayload::TurnStarted,
        EventPayload::UserMessage {
            message_id,
            text: user_text,
        },
        EventPayload::AgentRunStarted,
    ]
}

/// 构造一轮 agent 对话正常结束时的标准事件序列。
pub fn agent_turn_completed_payloads(reason: String) -> [EventPayload; 2] {
    [
        EventPayload::TurnCompleted {
            finish_reason: reason.clone(),
        },
        EventPayload::AgentRunCompleted { reason },
    ]
}

/// 构造一轮 agent 对话失败结束时的标准事件序列。
pub fn agent_turn_failed_payloads(
    error_message: Option<String>,
    reason: String,
) -> Vec<EventPayload> {
    let mut payloads = Vec::with_capacity(if error_message.is_some() { 3 } else { 2 });
    if let Some(message) = error_message {
        payloads.push(EventPayload::ErrorOccurred {
            code: -32603,
            message,
            recoverable: false,
        });
    }
    payloads.extend(agent_turn_completed_payloads(reason));
    payloads
}

/// 构造父会话 compact continuation 边界事件载荷。
pub fn compact_boundary_payload(
    trigger: impl Into<String>,
    compaction: &CompactResult,
    continued_session_id: SessionId,
) -> EventPayload {
    EventPayload::CompactBoundaryCreated {
        trigger: trigger.into(),
        pre_tokens: compaction.pre_tokens,
        post_tokens: compaction.post_tokens,
        summary: compaction.summary.clone(),
        transcript_path: compaction.transcript_path.clone(),
        continued_session_id,
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
    fn agent_turn_payload_helpers_keep_lifecycle_order() {
        let started = agent_turn_started_payloads("message-1".into(), "hello".into());
        assert!(matches!(started[0], EventPayload::TurnStarted));
        assert!(matches!(
            &started[1],
            EventPayload::UserMessage { message_id, text }
                if message_id.as_str() == "message-1" && text == "hello"
        ));
        assert!(matches!(started[2], EventPayload::AgentRunStarted));

        let completed = agent_turn_completed_payloads("stop".into());
        assert!(matches!(
            &completed[0],
            EventPayload::TurnCompleted { finish_reason } if finish_reason == "stop"
        ));
        assert!(matches!(
            &completed[1],
            EventPayload::AgentRunCompleted { reason } if reason == "stop"
        ));

        let failed = agent_turn_failed_payloads(Some("boom".into()), "error".into());
        assert!(matches!(
            &failed[0],
            EventPayload::ErrorOccurred { message, .. } if message == "boom"
        ));
        assert!(matches!(
            &failed[1],
            EventPayload::TurnCompleted { finish_reason } if finish_reason == "error"
        ));
        assert!(matches!(
            &failed[2],
            EventPayload::AgentRunCompleted { reason } if reason == "error"
        ));
    }
}

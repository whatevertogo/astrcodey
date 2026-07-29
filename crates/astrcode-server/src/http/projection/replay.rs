//! 重放历史事件 → ConversationDeltaDto。

use astrcode_core::event::{DurableEventPayload, Event, Phase};
use astrcode_protocol::http::ConversationDeltaDto;

use super::{
    blocks::{block_from_payload, streaming_tool_call_block},
    cross_session_compact_deltas,
    live::control_from_phase,
};

pub(in crate::http) fn event_to_replay_deltas(
    event: &Event,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    let Some(payload) = event.payload.as_durable() else {
        return Vec::new();
    };
    if let DurableEventPayload::CompactBoundaryCreated {
        continued_session_id,
        ..
    } = payload
    {
        return cross_session_compact_deltas(event, continued_session_id);
    }

    if matches!(
        payload,
        DurableEventPayload::SessionContinuedFromCompaction { .. }
            | DurableEventPayload::SessionForked { .. }
    ) {
        return vec![ConversationDeltaDto::RehydrateRequired];
    }

    if let Some(block) = block_from_payload(event) {
        return vec![ConversationDeltaDto::AppendBlock { block }];
    }
    if let DurableEventPayload::ToolCallRequested {
        call_id,
        tool_name,
        arguments,
        raw_arguments,
    } = payload
    {
        return vec![ConversationDeltaDto::AppendBlock {
            block: streaming_tool_call_block(
                call_id.to_string(),
                tool_name,
                raw_arguments.is_none().then_some(arguments),
            ),
        }];
    }
    if matches!(payload, DurableEventPayload::TurnCompleted { .. }) {
        return vec![ConversationDeltaDto::UpdateControlState {
            control: control_from_phase(Phase::Idle, has_messages),
        }];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{DurableEvent, StoredEvent},
        llm::LlmMessage,
    };

    use super::*;

    #[test]
    fn compact_replay_preserves_rehydrate_signal() {
        let boundary = Event::from(StoredEvent::new(
            7,
            DurableEvent::session(
                "session-1".into(),
                DurableEventPayload::CompactBoundaryCreated {
                    trigger: "manual_command".into(),
                    pre_tokens: 100,
                    post_tokens: 20,
                    summary: "summary".into(),
                    transcript_path: Some("compact.jsonl".into()),
                    continued_session_id: "session-1".into(),
                    base_event_seq: 0,
                    strategy: astrcode_core::compaction::CompactStrategy::Manual {
                        keep_recent_turns: None,
                    },
                },
            ),
        ));

        let deltas = event_to_replay_deltas(&boundary, true);
        assert!(deltas.is_empty());

        let continued = Event::from(StoredEvent::new(
            8,
            DurableEvent::session(
                "session-1".into(),
                DurableEventPayload::SessionContinuedFromCompaction {
                    parent_session_id: "session-1".into(),
                    parent_cursor: "7".into(),
                    summary: "summary".into(),
                    transcript_path: Some("compact.jsonl".into()),
                    context_messages: vec![LlmMessage::system("summary")],
                    retained_messages: vec![LlmMessage::user("recent")],
                },
            ),
        ));

        assert!(matches!(
            event_to_replay_deltas(&continued, true).as_slice(),
            [ConversationDeltaDto::RehydrateRequired]
        ));
    }
}

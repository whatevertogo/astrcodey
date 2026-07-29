//! 重放历史事件 → ConversationDeltaDto。

use astrcode_core::event::{DurableEventPayload, Event, Phase};
use astrcode_protocol::http::ConversationDeltaDto;

use super::{
    blocks::{block_from_payload, streaming_tool_call_block},
    live::control_from_phase,
};

pub(in crate::http) fn event_to_replay_deltas(
    event: &Event,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    let Some(payload) = event.payload.as_durable() else {
        return Vec::new();
    };
    if matches!(
        payload,
        DurableEventPayload::TranscriptRewritten { .. } | DurableEventPayload::SessionForked { .. }
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
    use astrcode_core::event::{
        CompactionDetails, DurableEvent, StoredEvent, TranscriptRewriteReason,
    };

    use super::*;

    #[test]
    fn compact_replay_preserves_rehydrate_signal() {
        let rewrite = Event::from(StoredEvent::new(
            7,
            DurableEvent::session(
                "session-1".into(),
                DurableEventPayload::TranscriptRewritten {
                    source_seq: 0,
                    messages: Vec::new(),
                    reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                        trigger: "manual_command".into(),
                        pre_tokens: 100,
                        post_tokens: 20,
                        summary: "summary".into(),
                        transcript_path: Some("compact.jsonl".into()),
                        strategy: astrcode_core::compaction::CompactStrategy::Manual {
                            keep_recent_turns: None,
                        },
                    }),
                },
            ),
        ));

        assert!(matches!(
            event_to_replay_deltas(&rewrite, true).as_slice(),
            [ConversationDeltaDto::RehydrateRequired]
        ));
    }
}

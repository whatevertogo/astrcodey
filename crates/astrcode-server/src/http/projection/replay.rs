//! 重放历史事件 → ConversationDeltaDto。

use astrcode_core::event::{Event, EventPayload, Phase};
use astrcode_protocol::http::{
    ConversationBlockDto, ConversationBlockStatusDto, ConversationCursorDto, ConversationDeltaDto,
};

use super::{
    args::format_args_inline, blocks::completed_block_from_payload, live::control_from_phase,
};

pub(in crate::http) fn event_to_replay_deltas(
    event: &Event,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    if let EventPayload::CompactBoundaryCreated {
        continued_session_id,
        ..
    } = &event.payload
    {
        let mut deltas = Vec::new();
        if continued_session_id != &event.session_id {
            deltas.extend(
                completed_block_from_payload(event)
                    .map(|block| ConversationDeltaDto::AppendBlock { block }),
            );
            deltas.push(ConversationDeltaDto::SessionContinued {
                parent_session_id: event.session_id.to_string(),
                new_session_id: continued_session_id.to_string(),
                parent_cursor: ConversationCursorDto {
                    value: event.seq.unwrap_or_default().to_string(),
                },
            });
        }
        return deltas;
    }

    if matches!(
        &event.payload,
        EventPayload::SessionContinuedFromCompaction { .. } | EventPayload::SessionForked { .. }
    ) {
        return vec![ConversationDeltaDto::RehydrateRequired];
    }

    if let Some(block) = completed_block_from_payload(event) {
        return vec![ConversationDeltaDto::AppendBlock { block }];
    }
    // 子会话重放时，AssistantMessageStarted 应产生流式 AppendBlock，
    // 让前端为后续的 PatchBlock / FinalizeBlock 准备占位。
    if let EventPayload::AssistantMessageStarted { message_id } = &event.payload {
        return vec![ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::Assistant {
                id: message_id.to_string(),
                text: String::new(),
                reasoning_content: None,
                status: ConversationBlockStatusDto::Streaming,
            },
        }];
    }
    if let EventPayload::ToolCallRequested {
        call_id,
        tool_name,
        arguments,
    } = &event.payload
    {
        return vec![ConversationDeltaDto::AppendBlock {
            block: ConversationBlockDto::ToolCall {
                id: call_id.to_string(),
                name: tool_name.clone(),
                arguments: format_args_inline(tool_name, arguments),
                text: String::new(),
                status: ConversationBlockStatusDto::Streaming,
                metadata: None,
                arguments_json: Some(arguments.clone()),
            },
        }];
    }
    if matches!(&event.payload, EventPayload::TurnCompleted { .. }) {
        return vec![ConversationDeltaDto::UpdateControlState {
            control: control_from_phase(Phase::Idle, has_messages),
        }];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::LlmMessage;

    use super::*;

    #[test]
    fn compact_replay_preserves_rehydrate_signal() {
        let mut boundary = Event::new(
            "session-1".into(),
            None,
            EventPayload::CompactBoundaryCreated {
                trigger: "manual_command".into(),
                pre_tokens: 100,
                post_tokens: 20,
                summary: "summary".into(),
                transcript_path: Some("compact.jsonl".into()),
                continued_session_id: "session-1".into(),
                base_event_seq: 0,
                strategy: astrcode_core::extension::CompactStrategy::Manual {
                    keep_recent_turns: None,
                },
            },
        );
        boundary.seq = Some(7);

        let deltas = event_to_replay_deltas(&boundary, true);
        assert!(deltas.is_empty());

        let continued = Event::new(
            "session-1".into(),
            None,
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id: "session-1".into(),
                parent_cursor: "7".into(),
                summary: "summary".into(),
                transcript_path: Some("compact.jsonl".into()),
                context_messages: vec![LlmMessage::system("summary")],
                retained_messages: vec![LlmMessage::user("recent")],
            },
        );

        assert!(matches!(
            event_to_replay_deltas(&continued, true).as_slice(),
            [ConversationDeltaDto::RehydrateRequired]
        ));
    }
}

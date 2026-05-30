//! 实时 EventPayload → ConversationDeltaDto 投影 + 控制态推算。

use astrcode_core::event::{Event, EventPayload, Phase};
use astrcode_protocol::{
    agent_session_link::AgentSessionLinkDto,
    http::{
        ConversationBlockDto, ConversationBlockStatusDto, ConversationControlStateDto,
        ConversationCursorDto, ConversationDeltaDto,
    },
};

use super::{args::format_args_inline, blocks::completed_block_from_payload};

pub(in crate::http) fn event_to_deltas(
    event: &Event,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    match &event.payload {
        EventPayload::AssistantMessageStarted { message_id } => {
            vec![ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::Assistant {
                    id: message_id.to_string(),
                    text: String::new(),
                    reasoning_content: None,
                    status: ConversationBlockStatusDto::Streaming,
                },
            }]
        },
        EventPayload::AssistantTextDelta { message_id, delta } => {
            vec![ConversationDeltaDto::PatchBlock {
                block_id: message_id.to_string(),
                text_delta: delta.clone(),
            }]
        },
        EventPayload::ToolCallStarted { call_id, tool_name } => {
            vec![ConversationDeltaDto::AppendBlock {
                block: ConversationBlockDto::ToolCall {
                    id: call_id.to_string(),
                    name: tool_name.clone(),
                    arguments: String::new(),
                    text: String::new(),
                    status: ConversationBlockStatusDto::Streaming,
                    metadata: None,
                    arguments_json: None,
                },
            }]
        },
        EventPayload::ToolOutputDelta {
            call_id,
            stream,
            delta,
        } => vec![ConversationDeltaDto::ToolOutput {
            call_id: call_id.to_string(),
            stream: *stream,
            delta: delta.clone(),
        }],

        // Completed blocks — shared construction, different delta wrappers
        EventPayload::UserMessage { .. }
        | EventPayload::ErrorOccurred { .. }
        | EventPayload::RecapGenerated { .. } => completed_block_from_payload(event)
            .map(|block| ConversationDeltaDto::AppendBlock { block })
            .into_iter()
            .collect(),
        EventPayload::AssistantMessageCompleted { .. } | EventPayload::ToolCallCompleted { .. } => {
            completed_block_from_payload(event)
                .map(|block| ConversationDeltaDto::FinalizeBlock { block })
                .into_iter()
                .collect()
        },
        EventPayload::CompactBoundaryCreated {
            continued_session_id,
            ..
        } => {
            let mut deltas = Vec::new();
            // 同会话 compact：等 SessionContinued + snapshot 刷新后再插入卡片，
            // 避免 AppendBlock 把摘要误追加到列表末尾。
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
            deltas
        },
        EventPayload::SessionContinuedFromCompaction {
            parent_session_id,
            parent_cursor,
            ..
        } if parent_session_id == &event.session_id => {
            vec![ConversationDeltaDto::SessionContinued {
                parent_session_id: parent_session_id.to_string(),
                new_session_id: event.session_id.to_string(),
                parent_cursor: ConversationCursorDto {
                    value: parent_cursor.to_string(),
                },
            }]
        },

        // Phase transitions
        EventPayload::TurnStarted
        | EventPayload::AgentRunStarted
        | EventPayload::CompactionStarted => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(projected_phase(&event.payload), has_messages),
            }]
        },
        EventPayload::TurnCompleted { .. } | EventPayload::AgentRunCompleted { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(Phase::Idle, has_messages),
            }]
        },
        EventPayload::CompactionCompleted { .. }
        | EventPayload::CompactionSkipped { .. }
        | EventPayload::CompactionFailed { .. } => {
            let resume_phase = if event.turn_id.is_some() {
                Phase::Thinking
            } else {
                Phase::Idle
            };
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(resume_phase, has_messages),
            }]
        },
        EventPayload::ThinkingDelta { message_id, delta } => {
            vec![ConversationDeltaDto::ThinkingDelta {
                block_id: message_id.to_string(),
                delta: delta.clone(),
            }]
        },

        // ToolCallRequested — 将参数写入 block.arguments 作为折叠摘要行
        EventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
        } => {
            let args_text = format_args_inline(tool_name, arguments);
            vec![ConversationDeltaDto::PatchArguments {
                block_id: call_id.to_string(),
                arguments: args_text,
                arguments_json: Some(arguments.clone()),
            }]
        },

        EventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_policy: _,
            tool_call_id,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionLinkDto::spawned(
                child_session_id,
                tool_call_id,
                agent_name,
                task,
            ),
        }],

        EventPayload::AgentSessionCompleted {
            child_session_id,
            final_session_id,
            summary,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionLinkDto::completed(
                child_session_id,
                final_session_id,
                summary,
            ),
        }],

        EventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            error,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionLinkDto::failed(child_session_id, final_session_id, error),
        }],

        EventPayload::AgentSessionRecycled { child_session_id } => {
            vec![ConversationDeltaDto::AgentSessionRemoved {
                child_session_id: child_session_id.to_string(),
            }]
        },

        // Events the client doesn't need as visible deltas
        EventPayload::SystemPromptConfigured { .. }
        | EventPayload::TurnAbortedContext
        | EventPayload::SessionContinuedFromCompaction { .. }
        | EventPayload::SessionForked { .. }
        | EventPayload::ToolCallArgumentsDelta { .. } => vec![],
        _ => vec![],
    }
}

fn projected_phase(payload: &EventPayload) -> Phase {
    match payload {
        EventPayload::TurnStarted
        | EventPayload::UserMessage { .. }
        | EventPayload::AgentRunStarted => Phase::Thinking,
        EventPayload::AssistantMessageStarted { .. }
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. } => Phase::Streaming,
        EventPayload::ToolCallStarted { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolCallRequested { .. }
        | EventPayload::ToolOutputDelta { .. }
        | EventPayload::ToolCallCompleted { .. } => Phase::CallingTool,
        EventPayload::CompactionStarted => Phase::Compacting,
        EventPayload::ErrorOccurred { .. } => Phase::Error,
        _ => Phase::Idle,
    }
}

pub(in crate::http) fn control_from_phase(
    phase: Phase,
    has_messages: bool,
) -> ConversationControlStateDto {
    let can_submit_prompt = matches!(phase, Phase::Idle | Phase::Error);
    ConversationControlStateDto {
        phase,
        can_submit_prompt,
        can_request_compact: can_submit_prompt && has_messages,
        compact_pending: false,
        compacting: matches!(phase, Phase::Compacting),
        current_mode_id: None,
        active_turn_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_request_patches_concise_arguments() {
        let event = Event::new(
            "session-1".into(),
            None,
            EventPayload::ToolCallRequested {
                call_id: "tool-1".into(),
                tool_name: "agent".into(),
                arguments: serde_json::json!({
                    "description": "Explore crate architecture",
                    "prompt": "Read every module and provide a very long report that should not appear in the collapsed summary line.",
                    "subagent_type": "explorer",
                }),
            },
        );

        let deltas = event_to_deltas(&event, true);

        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            ConversationDeltaDto::PatchArguments {
                block_id,
                arguments,
                arguments_json,
            } => {
                assert_eq!(block_id, "tool-1");
                assert_eq!(arguments, "Explore crate architecture (explorer)");
                assert!(!arguments.contains("Read every module"));
                assert!(arguments_json.is_some());
                let json = arguments_json.as_ref().unwrap();
                assert_eq!(json["description"], "Explore crate architecture");
                assert_eq!(json["subagent_type"], "explorer");
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[test]
    fn assistant_completion_finalizes_with_full_text() {
        let event = Event::new(
            "session-1".into(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: "assistant-1".into(),
                text: "complete answer".into(),
                reasoning_content: None,
            },
        );

        let deltas = event_to_deltas(&event, true);
        assert_eq!(
            deltas.len(),
            1,
            "assistant completion should produce one delta"
        );
        let delta = deltas.into_iter().next().unwrap();

        match delta {
            ConversationDeltaDto::FinalizeBlock {
                block:
                    ConversationBlockDto::Assistant {
                        id,
                        text,
                        reasoning_content: _,
                        status,
                    },
            } => {
                assert_eq!(id, "assistant-1");
                assert_eq!(text, "complete answer");
                assert!(matches!(status, ConversationBlockStatusDto::Complete));
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[test]
    fn thinking_delta_targets_assistant_block() {
        let event = Event::new(
            "session-1".into(),
            None,
            EventPayload::ThinkingDelta {
                message_id: "assistant-1".into(),
                delta: "reasoning".into(),
            },
        );

        let deltas = event_to_deltas(&event, true);

        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            ConversationDeltaDto::ThinkingDelta { block_id, delta } => {
                assert_eq!(block_id, "assistant-1");
                assert_eq!(delta, "reasoning");
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[test]
    fn tool_completion_finalizes_with_result_content() {
        let event = Event::new(
            "session-1".into(),
            None,
            EventPayload::ToolCallCompleted {
                call_id: "tool-1".into(),
                tool_name: "read".into(),
                result: astrcode_core::tool::ToolResult {
                    call_id: "tool-1".into(),
                    content: "file contents".into(),
                    is_error: false,
                    error: None,
                    metadata: Default::default(),
                    duration_ms: None,
                },
                arguments: String::new(),
                arguments_json: None,
            },
        );

        let deltas = event_to_deltas(&event, true);
        assert_eq!(deltas.len(), 1, "tool completion should produce one delta");
        let delta = deltas.into_iter().next().unwrap();

        match delta {
            ConversationDeltaDto::FinalizeBlock { block } => {
                let (tool_id, tool_name, tool_text, tool_status) = match block {
                    ConversationBlockDto::ToolCall {
                        id,
                        name,
                        text,
                        status,
                        ..
                    } => (id, name, text, status),
                    _ => panic!("expected ToolCall block"),
                };
                assert_eq!(tool_id, "tool-1");
                assert_eq!(tool_name, "read");
                assert_eq!(tool_text, "file contents");
                assert!(matches!(tool_status, ConversationBlockStatusDto::Complete));
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[test]
    fn in_turn_compact_completion_restores_thinking_control_state() {
        let event = Event::new(
            "session-1".into(),
            Some("turn-1".into()),
            EventPayload::CompactionCompleted {
                messages_removed: 2,
            },
        );

        let deltas = event_to_deltas(&event, true);

        assert!(matches!(
            deltas.as_slice(),
            [ConversationDeltaDto::UpdateControlState { control }]
                if control.phase == Phase::Thinking && !control.compacting
        ));
    }

    #[test]
    fn manual_compact_completion_restores_idle_control_state() {
        let event = Event::new(
            "session-1".into(),
            None,
            EventPayload::CompactionCompleted {
                messages_removed: 2,
            },
        );

        let deltas = event_to_deltas(&event, true);

        assert!(matches!(
            deltas.as_slice(),
            [ConversationDeltaDto::UpdateControlState { control }]
                if control.phase == Phase::Idle && control.can_submit_prompt
        ));
    }

    #[test]
    fn same_session_compact_refreshes_only_after_continuation_is_persisted() {
        let mut boundary = Event::new(
            "session-1".into(),
            None,
            EventPayload::CompactBoundaryCreated {
                trigger: "auto_threshold".into(),
                pre_tokens: 100,
                post_tokens: 20,
                summary: "summary".into(),
                transcript_path: None,
                continued_session_id: "session-1".into(),
                base_event_seq: 3,
                strategy: astrcode_core::extension::CompactStrategy::Auto,
            },
        );
        boundary.seq = Some(4);

        let boundary_deltas = event_to_deltas(&boundary, true);
        assert!(boundary_deltas.is_empty());

        let continuation = Event::new(
            "session-1".into(),
            None,
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id: "session-1".into(),
                parent_cursor: "4".into(),
                summary: "summary".into(),
                transcript_path: None,
                context_messages: Vec::new(),
                retained_messages: Vec::new(),
            },
        );
        let continuation_deltas = event_to_deltas(&continuation, true);
        assert!(matches!(
            continuation_deltas.as_slice(),
            [ConversationDeltaDto::SessionContinued {
                parent_session_id,
                new_session_id,
                ..
            }] if parent_session_id == "session-1" && new_session_id == "session-1"
        ));
    }
}

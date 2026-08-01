//! 实时 EventPayload → ConversationDeltaDto 投影 + 控制态推算。

use astrcode_core::event::{DurableEventPayload, Event, EventPayload, LiveEventPayload, Phase};
use astrcode_protocol::{
    agent_session_link::AgentSessionLinkDto,
    http::{ConversationControlStateDto, ConversationDeltaDto, ToolApprovalDto},
};

use super::{
    args::format_args_inline,
    blocks::{block_from_payload, streaming_assistant_block, streaming_tool_call_block},
};

pub(in crate::http) fn event_to_deltas(
    event: &Event,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    match &event.payload {
        EventPayload::Durable(payload) => durable_event_to_deltas(event, payload, has_messages),
        EventPayload::Live(payload) => live_event_to_deltas(event, payload, has_messages),
    }
}

fn durable_event_to_deltas(
    event: &Event,
    payload: &DurableEventPayload,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    match payload {
        DurableEventPayload::UserMessage { .. }
        | DurableEventPayload::ErrorOccurred { .. }
        | DurableEventPayload::RecapGenerated { .. } => block_from_payload(event)
            .map(|block| ConversationDeltaDto::AppendBlock { block })
            .into_iter()
            .collect(),
        DurableEventPayload::AssistantMessageCompleted { .. } => block_from_payload(event)
            .map(|block| ConversationDeltaDto::FinalizeBlock { block })
            .into_iter()
            .collect(),
        DurableEventPayload::ToolCallCompleted { .. }
        | DurableEventPayload::ToolCallFailed { .. }
        | DurableEventPayload::ToolCallCancelled { .. } => {
            let Some(block) = block_from_payload(event) else {
                return Vec::new();
            };
            vec![
                ConversationDeltaDto::FinalizeBlock { block },
                ConversationDeltaDto::UpdateControlState {
                    control: control_from_event(event, has_messages),
                },
            ]
        },
        DurableEventPayload::TranscriptRewritten { .. } => {
            vec![ConversationDeltaDto::RehydrateRequired]
        },
        DurableEventPayload::TurnStarted | DurableEventPayload::TurnCompleted { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            }]
        },
        DurableEventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
            raw_arguments,
        } => {
            let args_text = format_args_inline(tool_name, arguments);
            vec![ConversationDeltaDto::PatchArguments {
                block_id: call_id.to_string(),
                arguments: args_text,
                arguments_json: raw_arguments.is_none().then(|| arguments.clone()),
            }]
        },
        DurableEventPayload::ToolApprovalRequested {
            call_id,
            prompt,
            rule_key,
            ..
        } => vec![ConversationDeltaDto::ToolApprovalRequested {
            approval: ToolApprovalDto {
                call_id: call_id.to_string(),
                prompt: prompt.clone(),
                rule_key: rule_key.clone(),
            },
        }],
        DurableEventPayload::ToolApprovalResolved {
            call_id, decision, ..
        } => {
            vec![ConversationDeltaDto::ToolApprovalResolved {
                call_id: call_id.to_string(),
                decision: (*decision).into(),
            }]
        },
        DurableEventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_selection: _,
            tool_call_id,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionLinkDto::spawned(
                child_session_id,
                tool_call_id,
                agent_name,
                task,
            ),
        }],
        DurableEventPayload::AgentSessionCompleted {
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
        DurableEventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            error,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionLinkDto::failed(child_session_id, final_session_id, error),
        }],
        DurableEventPayload::AgentSessionRecycled { child_session_id } => {
            vec![ConversationDeltaDto::AgentSessionRemoved {
                child_session_id: child_session_id.to_string(),
            }]
        },
        DurableEventPayload::ExtensionEvent(extension_event) => {
            vec![ConversationDeltaDto::ExtensionEvent {
                extension_id: extension_event.extension_id.clone(),
                event_type: extension_event.event_type.clone(),
                schema_version: extension_event.schema_version,
                payload: extension_event.payload.clone(),
            }]
        },
        DurableEventPayload::SystemPromptConfigured { .. }
        | DurableEventPayload::TurnAbortedContext
        | DurableEventPayload::SessionForked { .. } => vec![],
        _ => vec![],
    }
}

fn live_event_to_deltas(
    event: &Event,
    payload: &LiveEventPayload,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    match payload {
        LiveEventPayload::AssistantMessageStarted { message_id } => vec![
            ConversationDeltaDto::AppendBlock {
                block: streaming_assistant_block(message_id.to_string(), String::new(), None),
            },
            ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            },
        ],
        LiveEventPayload::AssistantTextDelta { message_id, delta } => {
            vec![ConversationDeltaDto::PatchBlock {
                block_id: message_id.to_string(),
                text_delta: delta.clone(),
            }]
        },
        LiveEventPayload::ThinkingDelta { message_id, delta } => {
            vec![ConversationDeltaDto::ThinkingDelta {
                block_id: message_id.to_string(),
                delta: delta.clone(),
            }]
        },
        LiveEventPayload::ToolCallStarted { call_id, tool_name } => vec![
            ConversationDeltaDto::AppendBlock {
                block: streaming_tool_call_block(call_id.to_string(), tool_name, None),
            },
            ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            },
        ],
        LiveEventPayload::ToolOutputDelta {
            call_id,
            stream,
            delta,
        } => vec![ConversationDeltaDto::ToolOutput {
            call_id: call_id.to_string(),
            stream: (*stream).into(),
            delta: delta.clone(),
        }],
        LiveEventPayload::ErrorOccurred { .. } => block_from_payload(event)
            .map(|block| ConversationDeltaDto::AppendBlock { block })
            .into_iter()
            .collect(),
        LiveEventPayload::AgentRunStarted
        | LiveEventPayload::AgentRunCompleted { .. }
        | LiveEventPayload::CompactionStarted
        | LiveEventPayload::CompactionCompleted { .. }
        | LiveEventPayload::CompactionSkipped { .. }
        | LiveEventPayload::CompactionFailed { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            }]
        },
        LiveEventPayload::ExtensionEvent(extension_event) => {
            vec![ConversationDeltaDto::ExtensionEvent {
                extension_id: extension_event.extension_id.clone(),
                event_type: extension_event.event_type.clone(),
                schema_version: extension_event.schema_version,
                payload: extension_event.payload.clone(),
            }]
        },
        LiveEventPayload::ToolCallArgumentsDelta { .. } => vec![],
    }
}

fn projected_phase(payload: &EventPayload) -> Phase {
    match payload {
        EventPayload::Durable(
            DurableEventPayload::TurnStarted | DurableEventPayload::UserMessage { .. },
        )
        | EventPayload::Live(LiveEventPayload::AgentRunStarted) => Phase::Thinking,
        EventPayload::Live(
            LiveEventPayload::AssistantMessageStarted { .. }
            | LiveEventPayload::AssistantTextDelta { .. }
            | LiveEventPayload::ThinkingDelta { .. },
        ) => Phase::Streaming,
        EventPayload::Durable(DurableEventPayload::ToolCallRequested { .. })
        | EventPayload::Live(
            LiveEventPayload::ToolCallStarted { .. }
            | LiveEventPayload::ToolCallArgumentsDelta { .. }
            | LiveEventPayload::ToolOutputDelta { .. },
        ) => Phase::CallingTool,
        EventPayload::Durable(
            DurableEventPayload::ToolCallCompleted { .. }
            | DurableEventPayload::ToolCallFailed { .. }
            | DurableEventPayload::ToolCallCancelled { .. },
        ) => Phase::Thinking,
        EventPayload::Live(LiveEventPayload::CompactionStarted) => Phase::Compacting,
        EventPayload::Durable(DurableEventPayload::ErrorOccurred { .. })
        | EventPayload::Live(LiveEventPayload::ErrorOccurred { .. }) => Phase::Error,
        _ => Phase::Idle,
    }
}

fn active_turn_id_for_event(event: &Event) -> Option<String> {
    match &event.payload {
        EventPayload::Durable(DurableEventPayload::TurnCompleted { .. })
        | EventPayload::Live(LiveEventPayload::AgentRunCompleted { .. }) => None,
        _ => event.turn_id.as_ref().map(|turn_id| turn_id.to_string()),
    }
}

fn control_from_event(event: &Event, has_messages: bool) -> ConversationControlStateDto {
    let phase = match &event.payload {
        EventPayload::Durable(DurableEventPayload::TurnCompleted { .. })
        | EventPayload::Live(LiveEventPayload::AgentRunCompleted { .. }) => Phase::Idle,
        EventPayload::Live(
            LiveEventPayload::CompactionCompleted { .. }
            | LiveEventPayload::CompactionSkipped { .. }
            | LiveEventPayload::CompactionFailed { .. },
        ) => {
            if event.turn_id.is_some() {
                Phase::Thinking
            } else {
                Phase::Idle
            }
        },
        _ => projected_phase(&event.payload),
    };
    control_from_state(phase, has_messages, active_turn_id_for_event(event))
}

pub(in crate::http) fn control_from_phase(
    phase: Phase,
    has_messages: bool,
) -> ConversationControlStateDto {
    control_from_state(phase, has_messages, None)
}

fn control_from_state(
    phase: Phase,
    has_messages: bool,
    active_turn_id: Option<String>,
) -> ConversationControlStateDto {
    let can_submit_prompt = matches!(phase, Phase::Idle | Phase::Error);
    ConversationControlStateDto {
        phase: phase.into(),
        can_submit_prompt,
        can_request_compact: can_submit_prompt && has_messages,
        compact_pending: false,
        compacting: matches!(phase, Phase::Compacting),
        active_turn_id,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::event::{DurableEvent, ExtensionEventData, LiveEvent, StoredEvent};
    use astrcode_protocol::{
        http::{ConversationBlockDto, ConversationBlockStatusDto, ToolCallStatusDto},
        wire::PhaseDto,
    };

    use super::*;

    const ASK_USER_EXTENSION_ID: &str = "astrcode-ask-user";
    const ASK_USER_PENDING_EVENT_TYPE: &str = "ask_user.pending";

    fn event(payload: EventPayload, turn_id: Option<&str>) -> Event {
        match payload {
            EventPayload::Durable(payload) => StoredEvent::new(
                1,
                DurableEvent::new("session-1".into(), turn_id.map(Into::into), payload),
            )
            .into(),
            EventPayload::Live(payload) => {
                LiveEvent::new("session-1".into(), turn_id.map(Into::into), payload).into()
            },
        }
    }

    #[test]
    fn tool_request_patches_concise_arguments() {
        let event = event(
            EventPayload::Durable(DurableEventPayload::ToolCallRequested {
                call_id: "tool-1".into(),
                tool_name: "agent".into(),
                arguments: serde_json::json!({
                    "description": "Explore crate architecture",
                    "prompt": "Read every module and provide a very long report that should not appear in the collapsed summary line.",
                    "subagent_type": "explorer",
                }),
                raw_arguments: None,
            }),
            None,
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
        let event = event(
            EventPayload::Durable(DurableEventPayload::AssistantMessageCompleted {
                message_id: "assistant-1".into(),
                text: "complete answer".into(),
                reasoning_content: None,
            }),
            None,
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
        let event = event(
            EventPayload::Live(LiveEventPayload::ThinkingDelta {
                message_id: "assistant-1".into(),
                delta: "reasoning".into(),
            }),
            None,
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
    fn extension_event_preserves_namespaced_live_payload() {
        let event = event(
            EventPayload::Live(LiveEventPayload::ExtensionEvent(ExtensionEventData {
                extension_id: ASK_USER_EXTENSION_ID.into(),
                event_type: ASK_USER_PENDING_EVENT_TYPE.into(),
                schema_version: 1,
                payload: serde_json::json!({ "callId": "call-1" }),
            })),
            None,
        );

        assert!(matches!(
            event_to_deltas(&event, true).as_slice(),
            [ConversationDeltaDto::ExtensionEvent {
                extension_id,
                event_type,
                schema_version: 1,
                payload,
            }] if extension_id == ASK_USER_EXTENSION_ID
                && event_type == ASK_USER_PENDING_EVENT_TYPE
                && payload["callId"] == "call-1"
        ));
    }

    #[test]
    fn tool_terminal_events_preserve_status_content_and_duration() {
        let cases = [
            (
                EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
                    call_id: "complete".into(),
                    tool_name: "read".into(),
                    result: astrcode_core::tool::ToolResult::success("file contents")
                        .with_duration_ms(Some(4)),
                    arguments: String::new(),
                    arguments_json: None,
                }),
                ToolCallStatusDto::Complete,
                "file contents",
                Some(4),
            ),
            (
                EventPayload::Durable(DurableEventPayload::ToolCallCompleted {
                    call_id: "error".into(),
                    tool_name: "read".into(),
                    result: astrcode_core::tool::ToolResult::error("domain error"),
                    arguments: String::new(),
                    arguments_json: None,
                }),
                ToolCallStatusDto::Complete,
                "domain error",
                None,
            ),
            (
                EventPayload::Durable(DurableEventPayload::ToolCallFailed {
                    call_id: "failed".into(),
                    tool_name: "read".into(),
                    error: "executor failed".into(),
                    metadata: Default::default(),
                    duration_ms: Some(7),
                    arguments: String::new(),
                    arguments_json: None,
                }),
                ToolCallStatusDto::Failed,
                "executor failed",
                Some(7),
            ),
            (
                EventPayload::Durable(DurableEventPayload::ToolCallCancelled {
                    call_id: "cancelled".into(),
                    tool_name: "read".into(),
                    reason: "turn aborted".into(),
                    duration_ms: Some(8),
                    arguments: String::new(),
                    arguments_json: None,
                }),
                ToolCallStatusDto::Cancelled,
                "Tool cancelled: turn aborted",
                Some(8),
            ),
        ];

        for (payload, expected_status, expected_text, expected_duration) in cases {
            let event = event(payload, None);
            let deltas = event_to_deltas(&event, true);
            let [
                ConversationDeltaDto::FinalizeBlock {
                    block:
                        ConversationBlockDto::ToolCall {
                            text,
                            status,
                            metadata,
                            ..
                        },
                },
                ConversationDeltaDto::UpdateControlState { control },
            ] = deltas.as_slice()
            else {
                panic!("expected a finalized tool block and updated control state");
            };

            assert_eq!(*status, expected_status);
            assert_eq!(text, expected_text);
            assert!(matches!(control.phase, PhaseDto::Thinking));
            assert_eq!(
                metadata
                    .as_ref()
                    .and_then(|value| value.get("durationMs"))
                    .and_then(serde_json::Value::as_u64),
                expected_duration
            );
        }
    }

    #[test]
    fn lifecycle_events_project_control_state() {
        let cases = [
            (
                EventPayload::Live(LiveEventPayload::CompactionCompleted {
                    messages_removed: 2,
                }),
                Some("turn-1"),
                PhaseDto::Thinking,
                false,
                Some("turn-1"),
            ),
            (
                EventPayload::Durable(DurableEventPayload::TurnStarted),
                Some("turn-42"),
                PhaseDto::Thinking,
                false,
                Some("turn-42"),
            ),
            (
                EventPayload::Durable(DurableEventPayload::TurnCompleted {
                    finish_reason: "stop".into(),
                }),
                Some("turn-42"),
                PhaseDto::Idle,
                true,
                None,
            ),
            (
                EventPayload::Live(LiveEventPayload::CompactionCompleted {
                    messages_removed: 2,
                }),
                None,
                PhaseDto::Idle,
                true,
                None,
            ),
        ];

        for (payload, turn_id, phase, can_submit_prompt, active_turn_id) in cases {
            let event = event(payload, turn_id);
            let deltas = event_to_deltas(&event, true);
            let [ConversationDeltaDto::UpdateControlState { control }] = deltas.as_slice() else {
                panic!("expected one control-state delta, got {deltas:?}");
            };
            assert_eq!(control.phase, phase);
            assert_eq!(control.can_submit_prompt, can_submit_prompt);
            assert_eq!(control.active_turn_id.as_deref(), active_turn_id);
        }
    }

    #[test]
    fn transcript_rewrite_requests_rehydrate() {
        let rewrite = event(
            EventPayload::Durable(DurableEventPayload::TranscriptRewritten {
                source_seq: 3,
                messages: Vec::new(),
                reason: astrcode_core::event::TranscriptRewriteReason::Compaction(
                    astrcode_core::event::CompactionDetails {
                        trigger: "auto_threshold".into(),
                        pre_tokens: 100,
                        post_tokens: 20,
                        summary: "summary".into(),
                        transcript_path: None,
                        strategy: astrcode_core::compaction::CompactStrategy::Auto,
                    },
                ),
            }),
            None,
        );

        assert!(matches!(
            event_to_deltas(&rewrite, true).as_slice(),
            [ConversationDeltaDto::RehydrateRequired]
        ));
    }
}

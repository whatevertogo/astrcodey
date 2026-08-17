//! 实时 EventPayload → ConversationDeltaDto 投影 + 控制态推算。

use astrcode_core::event::{
    CustomEventData, DurableEventPayload, Event, EventPayload, LiveEventPayload, Phase,
};
use astrcode_protocol::{
    agent_session_link::AgentSessionUpdateDto,
    http::{ConversationControlStateDto, ConversationDeltaDto, LlmRetryStatusDto, ToolApprovalDto},
};

use super::blocks::{block_from_payload, streaming_assistant_block, streaming_tool_call_block};

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
        DurableEventPayload::TurnStarted => vec![ConversationDeltaDto::UpdateControlState {
            control: control_from_event(event, has_messages),
        }],
        DurableEventPayload::TurnCompleted { .. } => {
            let mut deltas = clear_transient_turn(event);
            deltas.push(ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            });
            deltas
        },
        DurableEventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
            raw_arguments,
        } => vec![ConversationDeltaDto::AppendBlock {
            block: streaming_tool_call_block(
                call_id.to_string(),
                tool_name,
                raw_arguments.is_none().then_some(arguments),
            ),
        }],
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
            agent_session: AgentSessionUpdateDto::Spawned {
                child_session_id: child_session_id.to_string(),
                tool_call_id: tool_call_id.as_ref().map(ToString::to_string),
                agent_name: agent_name.clone(),
                task: task.clone(),
            },
        }],
        DurableEventPayload::AgentSessionCompleted {
            child_session_id,
            final_session_id,
            summary,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionUpdateDto::Completed {
                child_session_id: child_session_id.to_string(),
                final_session_id: final_session_id.to_string(),
                summary: summary.clone(),
            },
        }],
        DurableEventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            error,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: AgentSessionUpdateDto::Failed {
                child_session_id: child_session_id.to_string(),
                final_session_id: final_session_id.to_string(),
                error: error.clone(),
            },
        }],
        DurableEventPayload::AgentSessionRecycled { child_session_id } => {
            vec![ConversationDeltaDto::AgentSessionRemoved {
                child_session_id: child_session_id.to_string(),
            }]
        },
        DurableEventPayload::CustomEvent(extension_event) => {
            vec![custom_event_delta(extension_event)]
        },
        // SystemPromptConfigured / TurnAbortedContext / SessionForked 无 live delta。
        _ => vec![],
    }
}

fn live_event_to_deltas(
    event: &Event,
    payload: &LiveEventPayload,
    has_messages: bool,
) -> Vec<ConversationDeltaDto> {
    match payload {
        LiveEventPayload::AssistantMessageStarted { message_id } => {
            let mut deltas = append_transient_block(
                event,
                streaming_assistant_block(message_id.to_string(), String::new(), None),
            );
            deltas.push(ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            });
            deltas
        },
        LiveEventPayload::AssistantMessageReset { message_id } => {
            vec![ConversationDeltaDto::ResetBlock {
                block_id: message_id.to_string(),
            }]
        },
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
        LiveEventPayload::ToolCallStarted { call_id, tool_name } => {
            let mut deltas = append_transient_block(
                event,
                streaming_tool_call_block(call_id.to_string(), tool_name, None),
            );
            deltas.push(ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            });
            deltas
        },
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
        LiveEventPayload::LlmRetrying { .. } => {
            let mut deltas = clear_transient_turn(event);
            deltas.push(ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            });
            deltas
        },
        LiveEventPayload::AgentRunStarted
        | LiveEventPayload::AgentRunCompleted { .. }
        | LiveEventPayload::LlmRetryRecovered
        | LiveEventPayload::CompactionStarted
        | LiveEventPayload::CompactionCompleted { .. }
        | LiveEventPayload::CompactionSkipped { .. }
        | LiveEventPayload::CompactionFailed { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_event(event, has_messages),
            }]
        },
        LiveEventPayload::CustomEvent(extension_event) => {
            vec![custom_event_delta(extension_event)]
        },
        LiveEventPayload::ToolCallArgumentsDelta { .. } => vec![],
    }
}

fn append_transient_block(
    event: &Event,
    block: astrcode_protocol::http::ConversationBlockDto,
) -> Vec<ConversationDeltaDto> {
    event
        .turn_id
        .as_ref()
        .map(|turn_id| ConversationDeltaDto::AppendTransientBlock {
            turn_id: turn_id.to_string(),
            block,
        })
        .into_iter()
        .collect()
}

fn clear_transient_turn(event: &Event) -> Vec<ConversationDeltaDto> {
    event
        .turn_id
        .as_ref()
        .map(|turn_id| ConversationDeltaDto::ClearTransientBlocks {
            turn_id: turn_id.to_string(),
        })
        .into_iter()
        .collect()
}

pub(in crate::http) fn custom_event_delta(custom_event: &CustomEventData) -> ConversationDeltaDto {
    ConversationDeltaDto::CustomEvent {
        extension_id: custom_event.extension_id.clone(),
        event_type: custom_event.event_type.clone(),
        schema_version: custom_event.schema_version,
        payload: custom_event.payload.clone(),
    }
}

fn projected_phase(payload: &EventPayload) -> Phase {
    match payload {
        EventPayload::Durable(
            DurableEventPayload::TurnStarted | DurableEventPayload::UserMessage { .. },
        )
        | EventPayload::Live(
            LiveEventPayload::AgentRunStarted
            | LiveEventPayload::LlmRetrying { .. }
            | LiveEventPayload::LlmRetryRecovered
            | LiveEventPayload::AssistantMessageReset { .. },
        ) => Phase::Thinking,
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
    let mut control = control_from_state(phase, has_messages, active_turn_id_for_event(event));
    if let EventPayload::Live(LiveEventPayload::LlmRetrying {
        status,
        attempt,
        max_retries,
        delay_ms,
    }) = &event.payload
    {
        control.retry_status = Some(LlmRetryStatusDto {
            status: *status,
            attempt: *attempt,
            max_retries: *max_retries,
            delay_ms: *delay_ms,
        });
    }
    control
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
        active_turn_id,
        retry_status: None,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::event::{CustomEventData, DurableEvent, LiveEvent, StoredEvent};
    use astrcode_protocol::{
        http::{ConversationBlockDto, ConversationBlockStatusDto, ToolCallStatusDto},
        wire::PhaseDto,
    };

    use super::*;

    const TEST_EXTENSION_ID: &str = "example-extension";
    const TEST_EVENT_TYPE: &str = "example.updated";

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
    fn tool_request_upserts_durable_block_with_concise_arguments() {
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
            ConversationDeltaDto::AppendBlock {
                block:
                    ConversationBlockDto::ToolCall {
                        id,
                        arguments,
                        arguments_json,
                        status,
                        ..
                    },
            } => {
                assert_eq!(id, "tool-1");
                assert_eq!(arguments, "Explore crate architecture");
                assert!(!arguments.contains("Read every module"));
                assert!(arguments_json.is_some());
                let json = arguments_json.as_ref().unwrap();
                assert_eq!(json["description"], "Explore crate architecture");
                assert_eq!(json["subagent_type"], "explorer");
                assert_eq!(*status, ToolCallStatusDto::Streaming);
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    #[test]
    fn live_block_starts_are_owned_by_their_turn() {
        let cases = [
            (
                EventPayload::Live(LiveEventPayload::AssistantMessageStarted {
                    message_id: "assistant-1".into(),
                }),
                "assistant-1",
            ),
            (
                EventPayload::Live(LiveEventPayload::ToolCallStarted {
                    call_id: "tool-1".into(),
                    tool_name: "read".into(),
                }),
                "tool-1",
            ),
        ];

        for (payload, expected_block_id) in cases {
            let deltas = event_to_deltas(&event(payload, Some("turn-1")), true);
            let Some(ConversationDeltaDto::AppendTransientBlock { turn_id, block }) =
                deltas.first()
            else {
                panic!("expected a turn-owned transient block, got {deltas:?}");
            };
            let block_id = match block {
                ConversationBlockDto::Assistant { id, .. }
                | ConversationBlockDto::ToolCall { id, .. } => id,
                other => panic!("unexpected transient block: {other:?}"),
            };

            assert_eq!(turn_id, "turn-1");
            assert_eq!(block_id, expected_block_id);
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
                        storage_seq,
                        status,
                    },
            } => {
                assert_eq!(id, "assistant-1");
                assert_eq!(text, "complete answer");
                assert_eq!(storage_seq, Some(1));
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
    fn llm_retry_projects_transient_control_status() {
        let event = event(
            EventPayload::Live(LiveEventPayload::LlmRetrying {
                status: Some(503),
                attempt: 2,
                max_retries: 5,
                delay_ms: 2_000,
            }),
            Some("turn-1"),
        );

        let deltas = event_to_deltas(&event, true);

        assert!(matches!(
            deltas.as_slice(),
            [
                ConversationDeltaDto::ClearTransientBlocks { turn_id: cleared_turn },
                ConversationDeltaDto::UpdateControlState {
                control: ConversationControlStateDto {
                    phase: PhaseDto::Thinking,
                    active_turn_id: Some(turn_id),
                    retry_status: Some(LlmRetryStatusDto {
                        status: Some(503),
                        attempt: 2,
                        max_retries: 5,
                        delay_ms: 2_000,
                    }),
                    ..
                }
            }
            ] if cleared_turn == "turn-1" && turn_id == "turn-1"
        ));
    }

    #[test]
    fn assistant_message_reset_projects_explicit_block_reset() {
        let event = event(
            EventPayload::Live(LiveEventPayload::AssistantMessageReset {
                message_id: "assistant-1".into(),
            }),
            Some("turn-1"),
        );

        assert!(matches!(
            event_to_deltas(&event, true).as_slice(),
            [ConversationDeltaDto::ResetBlock { block_id }] if block_id == "assistant-1"
        ));
    }

    #[test]
    fn extension_event_preserves_namespaced_live_payload() {
        let event = event(
            EventPayload::Live(LiveEventPayload::CustomEvent(CustomEventData {
                extension_id: TEST_EXTENSION_ID.into(),
                event_type: TEST_EVENT_TYPE.into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Global,
                causation_id: None,
                cascade_depth: 0,
                payload: serde_json::json!({ "callId": "call-1" }),
            })),
            None,
        );

        assert!(matches!(
            event_to_deltas(&event, true).as_slice(),
            [ConversationDeltaDto::CustomEvent {
                extension_id,
                event_type,
                schema_version: 1,
                payload,
            }] if extension_id == TEST_EXTENSION_ID
                && event_type == TEST_EVENT_TYPE
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
            let control = deltas
                .iter()
                .find_map(|delta| match delta {
                    ConversationDeltaDto::UpdateControlState { control } => Some(control),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("expected a control-state delta, got {deltas:?}"));
            if matches!(
                event.payload,
                EventPayload::Durable(DurableEventPayload::TurnCompleted { .. })
            ) {
                assert!(matches!(
                    deltas.first(),
                    Some(ConversationDeltaDto::ClearTransientBlocks { turn_id })
                        if turn_id == "turn-42"
                ));
            }
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
                source_fingerprint: "fingerprint".into(),
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

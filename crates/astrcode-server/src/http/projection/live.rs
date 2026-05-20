//! 实时 EventPayload → ConversationDeltaDto 投影 + 控制态推算。

use astrcode_core::event::{Event, EventPayload, Phase};
use astrcode_protocol::http::{
    ConversationBlockDto, ConversationBlockStatusDto, ConversationControlStateDto,
    ConversationCursorDto, ConversationDeltaDto, HttpAgentSessionLinkDto,
};

use super::{args::format_args_inline, blocks::completed_block_from_payload};
use crate::handler::snapshot;

pub(in crate::http) fn event_to_deltas(event: &Event) -> Vec<ConversationDeltaDto> {
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
                    task_id: None,
                    metadata: None,
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
        | EventPayload::RecapGenerated { .. } => {
            completed_block_from_payload(event)
                .map(|block| ConversationDeltaDto::AppendBlock { block })
                .into_iter()
                .collect()
        },
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
            let mut deltas: Vec<ConversationDeltaDto> = completed_block_from_payload(event)
                .map(|block| ConversationDeltaDto::AppendBlock { block })
                .into_iter()
                .collect();
            deltas.push(ConversationDeltaDto::SessionContinued {
                parent_session_id: event.session_id.to_string(),
                new_session_id: continued_session_id.to_string(),
                parent_cursor: ConversationCursorDto {
                    value: event.seq.unwrap_or_default().to_string(),
                },
            });
            deltas
        },

        // Phase transitions
        EventPayload::TurnStarted
        | EventPayload::AgentRunStarted
        | EventPayload::CompactionStarted
        | EventPayload::BackgroundTaskCompleted { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(projected_phase(&event.payload)),
            }]
        },
        EventPayload::ToolCallBackgrounded {
            call_id, task_id, ..
        } => {
            vec![
                ConversationDeltaDto::UpdateControlState {
                    control: control_from_phase(projected_phase(&event.payload)),
                },
                ConversationDeltaDto::ToolCallBackgrounded {
                    call_id: call_id.to_string(),
                    task_id: task_id.to_string(),
                },
            ]
        },
        EventPayload::TurnCompleted { .. } | EventPayload::AgentRunCompleted { .. } => {
            vec![ConversationDeltaDto::UpdateControlState {
                control: control_from_phase(Phase::Idle),
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
            }]
        },

        EventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_policy: _,
        } => vec![ConversationDeltaDto::AgentSessionUpdated {
            agent_session: HttpAgentSessionLinkDto {
                child_session_id: child_session_id.to_string(),
                agent_name: agent_name.clone(),
                task: task.clone(),
                status: snapshot::agent_status_to_dto(
                    astrcode_core::storage::AgentSessionStatus::Running,
                ),
            },
        }],

        EventPayload::AgentSessionCompleted {
            child_session_id, ..
        }
        | EventPayload::AgentSessionFailed {
            child_session_id, ..
        } => {
            vec![ConversationDeltaDto::AgentSessionUpdated {
                agent_session: HttpAgentSessionLinkDto {
                    child_session_id: child_session_id.to_string(),
                    agent_name: String::new(),
                    task: String::new(),
                    status: match &event.payload {
                        EventPayload::AgentSessionCompleted { .. } => {
                            snapshot::agent_status_to_dto(
                                astrcode_core::storage::AgentSessionStatus::Completed,
                            )
                        },
                        EventPayload::AgentSessionFailed { .. } => snapshot::agent_status_to_dto(
                            astrcode_core::storage::AgentSessionStatus::Failed,
                        ),
                        _ => unreachable!(),
                    },
                },
            }]
        },

        // Events the client doesn't need as visible deltas
        EventPayload::SystemPromptConfigured { .. }
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
        | EventPayload::ToolCallCompleted { .. }
        | EventPayload::ToolCallBackgrounded { .. } => Phase::CallingTool,
        EventPayload::CompactionStarted => Phase::Compacting,
        EventPayload::ErrorOccurred { .. } => Phase::Error,
        _ => Phase::Idle,
    }
}

pub(in crate::http) fn control_from_phase(phase: Phase) -> ConversationControlStateDto {
    let can_submit_prompt = matches!(phase, Phase::Idle | Phase::Error);
    ConversationControlStateDto {
        phase,
        can_submit_prompt,
        can_request_compact: can_submit_prompt,
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

        let deltas = event_to_deltas(&event);

        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            ConversationDeltaDto::PatchArguments {
                block_id,
                arguments,
            } => {
                assert_eq!(block_id, "tool-1");
                assert_eq!(arguments, "Explore crate architecture (explorer)");
                assert!(!arguments.contains("Read every module"));
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

        let deltas = event_to_deltas(&event);
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

        let deltas = event_to_deltas(&event);

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
            },
        );

        let deltas = event_to_deltas(&event);
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
}

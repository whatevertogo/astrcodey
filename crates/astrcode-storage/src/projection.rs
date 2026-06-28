//! 会话事件投影。
//!
//! EventLog 是唯一事实源；本模块只维护可从事件重建的内部读模型。

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmContent, LlmMessage, LlmRole, TURN_ABORTED_SOURCE, turn_aborted_context_message},
    storage::{
        AgentSessionLinkView, AgentSessionStatus, CompactBoundaryView, PendingToolApprovalView,
        SequencedLlmMessage, SessionReadModel,
    },
    types::SessionId,
};

/// 从事件序列重建会话读模型。
pub fn replay(session_id: SessionId, events: &[Event]) -> SessionReadModel {
    let mut model = SessionReadModel::empty(session_id);
    for event in events {
        reduce(event, &mut model);
    }
    model
}

/// 将单个持久事件归约到读模型。
pub fn reduce(event: &Event, model: &mut SessionReadModel) {
    // seq=None 的非持久/异常事件不推进 durable cursor。
    // durable seq 必须单调递增：即使 reducer 被重复调用或遇到乱序输入，也不得回退。
    if let Some(seq) = event.seq {
        model.latest_seq = Some(model.latest_seq.map_or(seq, |current| current.max(seq)));
    }
    model.updated_at = event.timestamp.to_rfc3339();
    let event_seq = event.seq.unwrap_or_default();

    match &event.payload {
        EventPayload::SessionStarted {
            working_dir,
            model_id,
            parent_session_id,
            tool_policy,
            source_extension,
        } => {
            model.working_dir = working_dir.clone();
            model.model_id = model_id.clone();
            model.parent_session_id = parent_session_id.clone();
            model.tool_policy = tool_policy.clone();
            model.source_extension = source_extension.clone();
            model.phase = Phase::Idle;
            if model.created_at.is_empty() {
                model.created_at = event.timestamp.to_rfc3339();
            }
        },
        EventPayload::ModelIdChanged { model_id } => {
            model.model_id = model_id.clone();
        },
        EventPayload::SessionDeleted => {
            model.phase = Phase::Idle;
            model.messages.clear();
            model.context_messages.clear();
            model.system_prompt = None;
            model.extra_system_prompt = None;
            model.system_prompt_fingerprint = None;
            model.pending_tool_calls.clear();
            model.pending_tool_approvals.clear();
            model.pending_tool_interactions.clear();
            model.compact_boundaries.clear();
            model.agent_sessions.clear();
            model.extension_events = Default::default();
        },
        EventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_policy: _,
            tool_call_id,
        } => {
            model.agent_sessions.push(AgentSessionLinkView {
                child_session_id: child_session_id.clone(),
                tool_call_id: Some(tool_call_id.clone()),
                agent_name: agent_name.clone(),
                task: task.clone(),
                status: AgentSessionStatus::Running,
                final_session_id: None,
                summary: None,
                error: None,
                phase: None,
                current_tool: None,
            });
        },
        EventPayload::AgentSessionCompleted {
            child_session_id,
            final_session_id,
            summary,
        } => {
            if let Some(link) = model
                .agent_sessions
                .iter_mut()
                .find(|l| l.child_session_id == *child_session_id)
            {
                link.status = AgentSessionStatus::Completed;
                link.final_session_id = Some(final_session_id.clone());
                link.summary = Some(summary.clone());
                link.error = None;
            }
        },
        EventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            error,
        } => {
            if let Some(link) = model
                .agent_sessions
                .iter_mut()
                .find(|l| l.child_session_id == *child_session_id)
            {
                link.status = AgentSessionStatus::Failed;
                link.final_session_id = Some(final_session_id.clone());
                link.error = Some(error.clone());
                link.summary = None;
            }
        },
        EventPayload::AgentSessionRecycled { child_session_id } => {
            model
                .agent_sessions
                .retain(|l| l.child_session_id != *child_session_id);
        },
        EventPayload::SystemPromptConfigured {
            text,
            fingerprint,
            extra_system_prompt,
        } => {
            model.system_prompt = Some(text.clone());
            model.extra_system_prompt = extra_system_prompt.clone();
            model.system_prompt_fingerprint = Some(fingerprint.clone());
        },
        EventPayload::TurnStarted | EventPayload::UserMessage { .. } => {
            model.phase = Phase::Thinking;
            if let EventPayload::UserMessage {
                text, attachments, ..
            } = &event.payload
            {
                model.messages.push(SequencedLlmMessage {
                    message: LlmMessage::user_with_attachments(text, attachments),
                    updated_seq: event_seq,
                    source: None,
                });
            }
        },
        EventPayload::TurnCompleted { .. } => {
            model.phase = Phase::Idle;
            model.pending_tool_calls.clear();
            model.pending_tool_approvals.clear();
            model.pending_tool_interactions.clear();
        },
        EventPayload::TurnAbortedContext => {
            model.messages.push(SequencedLlmMessage {
                message: turn_aborted_context_message(),
                updated_seq: event_seq,
                source: Some(TURN_ABORTED_SOURCE.into()),
            });
        },
        EventPayload::AssistantMessageStarted { .. } => {
            model.phase = Phase::Streaming;
        },
        EventPayload::AssistantMessageCompleted {
            text,
            reasoning_content,
            ..
        } => {
            let mut msg = LlmMessage::assistant(text);
            msg.reasoning_content = reasoning_content.clone();
            model.messages.push(SequencedLlmMessage {
                message: msg,
                updated_seq: event_seq,
                source: None,
            });
            model.phase = Phase::Thinking;
        },
        EventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
        } => {
            model.pending_tool_calls.insert(call_id.clone());
            let tool_call = LlmContent::ToolCall {
                call_id: call_id.to_string(),
                name: tool_name.clone(),
                arguments: arguments.clone(),
            };
            // Merge into the previous assistant message for this model sub-turn.
            // DeepSeek thinking mode requires reasoning_content and tool_calls to
            // be replayed on the same assistant message after tool use.
            if let Some(last) = model.messages.last_mut() {
                if last.message.role == LlmRole::Assistant {
                    last.message.content.push(tool_call);
                    last.updated_seq = event_seq;
                } else {
                    model.messages.push(SequencedLlmMessage {
                        message: LlmMessage {
                            role: LlmRole::Assistant,
                            content: vec![tool_call],
                            name: None,
                            reasoning_content: None,
                        },
                        updated_seq: event_seq,
                        source: None,
                    });
                }
            } else {
                model.messages.push(SequencedLlmMessage {
                    message: LlmMessage {
                        role: LlmRole::Assistant,
                        content: vec![tool_call],
                        name: None,
                        reasoning_content: None,
                    },
                    updated_seq: event_seq,
                    source: None,
                });
            }
            model.phase = Phase::CallingTool;
        },
        EventPayload::ToolApprovalRequested {
            call_id,
            prompt,
            rule_key,
            ..
        } => {
            model.phase = Phase::CallingTool;
            model.pending_tool_approvals.insert(
                call_id.clone(),
                PendingToolApprovalView {
                    prompt: prompt.clone(),
                    rule_key: rule_key.clone(),
                },
            );
        },
        EventPayload::ToolApprovalResolved { call_id, .. } => {
            model.pending_tool_approvals.remove(call_id);
        },
        EventPayload::ToolCallInteractionPending {
            call_id,
            content,
            metadata,
        } => {
            model.phase = Phase::CallingTool;
            model.pending_tool_interactions.insert(
                call_id.clone(),
                astrcode_core::storage::PendingToolInteractionView {
                    content: content.clone(),
                    metadata: metadata.clone(),
                },
            );
        },
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
            ..
        } => {
            model.pending_tool_calls.remove(call_id);
            model.pending_tool_approvals.remove(call_id);
            model.pending_tool_interactions.remove(call_id);

            // 始终 push（不再 update-in-place）
            model.messages.push(SequencedLlmMessage {
                message: LlmMessage {
                    role: LlmRole::Tool,
                    content: vec![LlmContent::ToolResult {
                        tool_call_id: call_id.to_string(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    }],
                    name: Some(tool_name.clone()),
                    reasoning_content: None,
                },
                updated_seq: event_seq,
                source: None,
            });
            model.phase = if model.pending_tool_calls.is_empty() {
                Phase::Thinking
            } else {
                Phase::CallingTool
            };
        },
        EventPayload::CompactBoundaryCreated {
            trigger,
            pre_tokens,
            post_tokens,
            summary,
            transcript_path,
            base_event_seq,
            strategy,
            ..
        } => {
            model.compact_boundaries.push(CompactBoundaryView {
                trigger: trigger.clone(),
                pre_tokens: *pre_tokens,
                post_tokens: *post_tokens,
                summary: summary.clone(),
                transcript_path: transcript_path.clone(),
                seq: event.seq.unwrap_or_default(),
                base_event_seq: *base_event_seq,
                strategy: strategy.clone(),
            });
            // Auto compact 在 turn 期间发生，不应将 phase 改为 Idle。
            // 手动 compact 时没有 active turn，Idle 是正确状态。
            if trigger != "auto_threshold" {
                model.phase = Phase::Idle;
            }
        },
        EventPayload::SessionContinuedFromCompaction {
            parent_cursor,
            context_messages,
            retained_messages,
            ..
        } => {
            let base_event_seq = parent_cursor.parse::<u64>().unwrap_or(0);
            let tail_messages: Vec<SequencedLlmMessage> = model
                .messages
                .iter()
                .filter(|m| m.updated_seq > base_event_seq)
                .cloned()
                .collect();
            model.context_messages = context_messages
                .iter()
                .cloned()
                .map(|message| SequencedLlmMessage {
                    message,
                    updated_seq: event_seq,
                    source: None,
                })
                .collect();
            let mut messages: Vec<SequencedLlmMessage> = retained_messages
                .iter()
                .cloned()
                .map(|message| SequencedLlmMessage {
                    message,
                    updated_seq: event_seq,
                    source: None,
                })
                .collect();
            messages.extend(tail_messages);
            model.messages = messages;
            // 不改变 phase，保留之前的状态。
            // auto compact 在 turn 期间发生，phase 应保持 Thinking/Streaming。
            // 手动 compact 时 phase 已经是 Idle（由 CompactBoundaryCreated 设置）。
        },
        EventPayload::SessionForked {
            context_messages,
            retained_messages,
            ..
        } => {
            model.context_messages = context_messages
                .iter()
                .cloned()
                .map(|message| SequencedLlmMessage {
                    message,
                    updated_seq: event_seq,
                    source: None,
                })
                .collect();
            model.messages = retained_messages
                .iter()
                .cloned()
                .map(|message| SequencedLlmMessage {
                    message,
                    updated_seq: event_seq,
                    source: None,
                })
                .collect();
            model.phase = Phase::Idle;
        },
        EventPayload::ErrorOccurred { .. } => {
            model.phase = Phase::Error;
        },
        EventPayload::CompactionStarted => {
            model.phase = Phase::Compacting;
        },
        EventPayload::CompactionCompleted { .. } => {
            model.phase = if model.pending_tool_calls.is_empty() {
                Phase::Idle
            } else {
                Phase::CallingTool
            };
        },
        EventPayload::CompactionSkipped { .. } => {
            model.phase = if model.pending_tool_calls.is_empty() {
                Phase::Idle
            } else {
                Phase::CallingTool
            };
        },
        EventPayload::CompactionFailed { .. } => {
            model.phase = if model.pending_tool_calls.is_empty() {
                Phase::Idle
            } else {
                Phase::CallingTool
            };
        },
        EventPayload::Custom { .. } => {},
        EventPayload::RecapGenerated { .. } => {},
        EventPayload::TokenUsageRecorded { .. } => {},
        EventPayload::ExtensionEvent {
            extension_id,
            event_type,
            schema_version,
            ..
        } => {
            model.extension_events.push(
                event.seq.unwrap_or_default(),
                extension_id.clone(),
                event_type.clone(),
                *schema_version,
            );
        },
        // All durable events must be shown in the above
        // Non-durable events: never persisted to JSONL, only broadcast for live UI.
        EventPayload::ToolCallStarted { .. }
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolOutputDelta { .. }
        | EventPayload::AgentRunStarted
        | EventPayload::AgentRunCompleted { .. } => {},
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{Event, EventPayload},
        extension::CompactStrategy,
        llm::{LlmMessage, LlmRole, TURN_ABORTED_SOURCE},
        permission::{ApprovalDecision, ApprovalSource},
        types::{SessionId, new_message_id},
    };

    use super::replay;

    fn event(seq: u64, session_id: &SessionId, payload: EventPayload) -> Event {
        let mut event = Event::new(session_id.clone(), None, payload);
        event.seq = Some(seq);
        event
    }

    #[test]
    fn replay_applies_compact_boundary_as_durable_state_transition() {
        let session_id = SessionId::from("session-compact-replay");
        let mut events = vec![
            event(
                1,
                &session_id,
                EventPayload::SessionStarted {
                    working_dir: ".".into(),
                    model_id: "mock".into(),
                    parent_session_id: None,
                    tool_policy: None,
                    source_extension: None,
                },
            ),
            event(
                2,
                &session_id,
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: "old user".into(),
                    attachments: vec![],
                },
            ),
            event(
                3,
                &session_id,
                EventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: "old assistant".into(),
                    reasoning_content: None,
                },
            ),
            event(
                4,
                &session_id,
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: "recent user".into(),
                    attachments: vec![],
                },
            ),
        ];

        let full = replay(session_id.clone(), &events);
        assert_eq!(
            full.messages
                .iter()
                .map(|m| m.message.clone())
                .collect::<Vec<_>>(),
            vec![
                LlmMessage::user("old user"),
                LlmMessage::assistant("old assistant"),
                LlmMessage::user("recent user"),
            ]
        );

        let context_messages = vec![LlmMessage::user(
            "<compact_summary>summary</compact_summary>",
        )];
        let retained_messages = vec![LlmMessage::user("recent user")];
        events.extend([
            event(
                5,
                &session_id,
                EventPayload::CompactBoundaryCreated {
                    trigger: "auto_threshold".into(),
                    pre_tokens: 100,
                    post_tokens: 20,
                    summary: "summary".into(),
                    transcript_path: None,
                    continued_session_id: session_id.clone(),
                    base_event_seq: 4,
                    strategy: CompactStrategy::Auto,
                },
            ),
            event(
                6,
                &session_id,
                EventPayload::SessionContinuedFromCompaction {
                    parent_session_id: session_id.clone(),
                    parent_cursor: "4".into(),
                    summary: "summary".into(),
                    transcript_path: None,
                    context_messages: context_messages.clone(),
                    retained_messages: retained_messages.clone(),
                },
            ),
        ]);

        let compacted = replay(session_id.clone(), &events);
        assert_eq!(
            compacted
                .context_messages
                .iter()
                .map(|m| m.message.clone())
                .collect::<Vec<_>>(),
            context_messages
        );
        assert_eq!(
            compacted
                .messages
                .iter()
                .map(|m| m.message.clone())
                .collect::<Vec<_>>(),
            retained_messages
        );
        assert_eq!(compacted.compact_boundaries[0].base_event_seq, 4);

        events.push(event(
            7,
            &session_id,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "after compact".into(),
                attachments: vec![],
            },
        ));

        let continued = replay(session_id, &events);
        assert_eq!(
            continued
                .messages
                .iter()
                .map(|m| m.message.clone())
                .collect::<Vec<_>>(),
            vec![
                LlmMessage::user("recent user"),
                LlmMessage::user("after compact"),
            ]
        );
    }

    #[test]
    fn replay_projects_agent_session_link_details() {
        let session_id = SessionId::from("parent-session");
        let child_id = SessionId::from("child-session");
        let events = vec![
            event(
                1,
                &session_id,
                EventPayload::AgentSessionSpawned {
                    child_session_id: child_id.clone(),
                    agent_name: "explorer".into(),
                    task: "read the code".into(),
                    tool_policy: None,
                    tool_call_id: "tool-call-1".into(),
                },
            ),
            event(
                2,
                &session_id,
                EventPayload::AgentSessionCompleted {
                    child_session_id: child_id,
                    final_session_id: "leaf-session".into(),
                    summary: "done".into(),
                },
            ),
        ];

        let model = replay(session_id, &events);
        let link = &model.agent_sessions[0];

        assert_eq!(
            link.tool_call_id.as_ref().map(|id| id.as_str()),
            Some("tool-call-1")
        );
        assert_eq!(
            link.final_session_id.as_ref().map(|id| id.as_str()),
            Some("leaf-session")
        );
        assert_eq!(link.summary.as_deref(), Some("done"));
        assert!(link.error.is_none());
        assert_eq!(
            link.status,
            astrcode_core::storage::AgentSessionStatus::Completed
        );
    }

    #[test]
    fn turn_aborted_context_is_provider_visible_but_source_marked() {
        let session_id = SessionId::from("session-turn-aborted-context");
        let events = vec![
            event(
                1,
                &session_id,
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: "run a long command".into(),
                    attachments: vec![],
                },
            ),
            event(2, &session_id, EventPayload::TurnAbortedContext),
            event(
                3,
                &session_id,
                EventPayload::TurnCompleted {
                    finish_reason: "aborted".into(),
                },
            ),
        ];

        let model = replay(session_id, &events);

        let marker = model
            .messages
            .iter()
            .find(|message| message.source.as_deref() == Some(TURN_ABORTED_SOURCE))
            .expect("turn-aborted context should be projected");
        assert_eq!(marker.message.role, LlmRole::User);
        assert!(
            marker
                .message
                .joined_display_text("")
                .contains("<turn_aborted>")
        );
        assert!(
            model
                .provider_messages()
                .iter()
                .any(|message| message.joined_display_text("").contains("<turn_aborted>")),
            "provider history should include the marker"
        );
    }

    #[test]
    fn replay_tracks_pending_tool_approvals_until_resolved() {
        let session_id = SessionId::from("session-approval");
        let call_id = astrcode_core::types::ToolCallId::from("call-approval");
        let requested = vec![event(
            1,
            &session_id,
            EventPayload::ToolApprovalRequested {
                call_id: call_id.clone(),
                tool_name: "shell".into(),
                prompt: "Run shell command?".into(),
                rule_key: Some("shell:write".into()),
                source: ApprovalSource::Core,
                arguments: serde_json::json!({ "command": "git push" }),
            },
        )];

        let model = replay(session_id.clone(), &requested);
        let approval = model
            .pending_tool_approvals
            .get(&call_id)
            .expect("approval should be pending");
        assert_eq!(approval.prompt, "Run shell command?");
        assert_eq!(approval.rule_key.as_deref(), Some("shell:write"));

        let mut resolved = requested;
        resolved.push(event(
            2,
            &session_id,
            EventPayload::ToolApprovalResolved {
                call_id: call_id.clone(),
                decision: ApprovalDecision::AllowOnce,
                detail: None,
            },
        ));

        let model = replay(session_id, &resolved);
        assert!(!model.pending_tool_approvals.contains_key(&call_id));
    }
}

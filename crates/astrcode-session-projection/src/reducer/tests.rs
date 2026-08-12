use std::sync::Arc;

use astrcode_core::{
    compaction::CompactStrategy,
    event::{
        CompactionDetails, DurableEvent, DurableEventPayload, PersistedSystemPrompt, Phase,
        SessionStarted, StoredEvent, SystemPromptSource, TranscriptRewriteReason,
        transcript_prefix_fingerprint,
    },
    llm::{LlmMessage, LlmRole, LlmTokenUsage, provider_transcript},
    permission::{ApprovalDecision, ApprovalSource},
    tool::{SessionToolSelection, ToolResult},
    types::{SessionId, ToolCallId, TurnId, new_message_id},
    user_input::UserInput,
};

use super::{PreparedProjectionBatch, ProjectionError, SessionReadModelProjection, reduce, replay};
use crate::{
    AgentSessionStatus, SessionReadModel, TOOL_CALL_CANCELLED_SOURCE, TOOL_CALL_FAILED_SOURCE,
    TranscriptArtifactView,
};

fn event(seq: u64, session_id: &SessionId, payload: DurableEventPayload) -> StoredEvent {
    StoredEvent::new(seq, DurableEvent::session(session_id.clone(), payload))
}

fn turn_event(
    seq: u64,
    session_id: &SessionId,
    turn_id: &TurnId,
    payload: DurableEventPayload,
) -> StoredEvent {
    StoredEvent::new(
        seq,
        DurableEvent::turn(session_id.clone(), turn_id.clone(), payload),
    )
}

fn started(seq: u64, session_id: &SessionId) -> StoredEvent {
    event(
        seq,
        session_id,
        DurableEventPayload::SessionStarted(SessionStarted {
            working_dir: "/workspace".into(),
            model_id: "model-a".into(),
            parent: None,
            tool_selection: SessionToolSelection::Only {
                names: vec!["read".into()],
            },
            source_extension: Some("agent".into()),
            initial_system_prompt: PersistedSystemPrompt {
                text: "system".into(),
                fingerprint: "fingerprint".into(),
                extra_system_prompt: Some("extra".into()),
                source: SystemPromptSource::Native,
            },
        }),
    )
}

#[test]
fn accepted_input_stays_pending_until_matching_user_message() {
    let session_id = SessionId::new("session-pending");
    let input = UserInput::text_only("queued");
    let mut model = replay(
        session_id.clone(),
        &[
            started(0, &session_id),
            event(
                1,
                &session_id,
                DurableEventPayload::UserInputAccepted {
                    input: input.clone(),
                },
            ),
        ],
    )
    .unwrap();

    assert_eq!(model.execution.pending_inputs.len(), 1);
    assert!(model.transcript.messages.is_empty());

    reduce(
        &event(
            2,
            &session_id,
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: input.text,
                attachments: input.attachments,
                accepted_seq: Some(1),
            },
        ),
        &mut model,
    )
    .unwrap();

    assert!(model.execution.pending_inputs.is_empty());
    assert_eq!(model.transcript.messages.len(), 1);
}

#[test]
fn provider_usage_anchors_covered_transcript_until_context_identity_changes() {
    let session_id = SessionId::new("session-context-usage");
    let mut model = replay(
        session_id.clone(),
        &[
            started(0, &session_id),
            event(
                1,
                &session_id,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: "first".into(),
                    attachments: vec![],
                    accepted_seq: None,
                },
            ),
            event(
                2,
                &session_id,
                DurableEventPayload::TokenUsageRecorded {
                    usage: LlmTokenUsage {
                        total_tokens: Some(655_859),
                        ..Default::default()
                    },
                    model_context_window: 1_000_000,
                },
            ),
        ],
    )
    .unwrap();

    let usage = model.context_usage.as_ref().unwrap();
    assert_eq!(usage.context_tokens, 655_859);
    assert_eq!(usage.model_context_window, 1_000_000);
    assert_eq!(usage.covered_message_count, 1);

    reduce(
        &event(
            3,
            &session_id,
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: "tail".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ),
        &mut model,
    )
    .unwrap();
    assert_eq!(
        model
            .context_usage
            .as_ref()
            .map(|usage| usage.covered_message_count),
        Some(1)
    );

    reduce(
        &event(
            4,
            &session_id,
            DurableEventPayload::ModelIdChanged {
                model_id: "model-b".into(),
            },
        ),
        &mut model,
    )
    .unwrap();
    assert!(model.context_usage.is_none());
}

#[test]
fn prepared_batches_update_in_place_and_validate_rewrites_against_prior_batch_events() {
    let session_id = SessionId::new("session-prepared-batch");
    let mut current = Arc::new(
        replay(
            session_id.clone(),
            &[
                started(0, &session_id),
                event(
                    1,
                    &session_id,
                    DurableEventPayload::UserMessage {
                        message_id: new_message_id(),
                        text: "history".into(),
                        attachments: vec![],
                        accepted_seq: None,
                    },
                ),
            ],
        )
        .unwrap(),
    );
    let initial_model = Arc::as_ptr(&current);

    let invalid = started(2, &session_id).event;
    assert!(matches!(
        PreparedProjectionBatch::prepare(current.as_ref(), vec![invalid]),
        Err(ProjectionError::DuplicateSessionStarted(2))
    ));
    assert!(std::ptr::eq(initial_model, Arc::as_ptr(&current)));

    let prepared = PreparedProjectionBatch::prepare(
        current.as_ref(),
        vec![DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::ModelIdChanged {
                model_id: "model-b".into(),
            },
        )],
    )
    .unwrap();
    assert_eq!(prepared.first_seq(), 2);
    prepared.apply(&mut current);
    assert!(
        std::ptr::eq(initial_model, Arc::as_ptr(&current)),
        "an unshared projection should be updated without cloning the full model"
    );
    assert_eq!(current.identity.model_id, "model-b");

    let old_snapshot = Arc::clone(&current);
    let prepared = PreparedProjectionBatch::prepare(
        current.as_ref(),
        vec![DurableEvent::session(
            session_id,
            DurableEventPayload::ModelIdChanged {
                model_id: "model-c".into(),
            },
        )],
    )
    .unwrap();
    prepared.apply(&mut current);

    assert!(!std::ptr::eq(
        Arc::as_ptr(&old_snapshot),
        Arc::as_ptr(&current)
    ));
    assert_eq!(old_snapshot.identity.model_id, "model-b");
    assert_eq!(old_snapshot.stats.last_seq, 2);
    assert_eq!(current.identity.model_id, "model-c");
    assert_eq!(current.stats.last_seq, 3);

    drop(old_snapshot);
    let user = DurableEvent::session(
        current.identity.session_id.clone(),
        DurableEventPayload::UserMessage {
            message_id: new_message_id(),
            text: "new prefix".into(),
            attachments: Vec::new(),
            accepted_seq: None,
        },
    );
    let mut expected = current.as_ref().clone();
    reduce(&StoredEvent::new(4, user.clone()), &mut expected).unwrap();
    let rewrite = DurableEvent::session(
        current.identity.session_id.clone(),
        DurableEventPayload::TranscriptRewritten {
            source_seq: 4,
            source_fingerprint: prefix_fingerprint(&expected, 4),
            messages: vec![LlmMessage::user("summary")],
            reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                trigger: "manual".into(),
                pre_tokens: 100,
                post_tokens: 10,
                summary: "summary".into(),
                transcript_path: None,
                strategy: CompactStrategy::Manual {
                    keep_recent_turns: None,
                },
            }),
        },
    );
    let prepared = PreparedProjectionBatch::prepare(current.as_ref(), vec![user, rewrite]).unwrap();
    let before_rewrite = Arc::as_ptr(&current);
    prepared.apply(&mut current);

    assert!(std::ptr::eq(before_rewrite, Arc::as_ptr(&current)));
    assert_eq!(current.stats.last_seq, 5);
    assert_eq!(current.transcript.messages.len(), 1);
    assert_eq!(
        current.transcript.messages[0]
            .message
            .joined_display_text("\n"),
        "summary"
    );
}

#[test]
fn transcript_rewrite_does_not_change_active_execution_state() {
    let session_id = SessionId::new("session-active-compaction");
    let turn_id = TurnId::new("turn-active");
    let cases = [
        ("auto_threshold", CompactStrategy::Auto),
        (
            "manual_command",
            CompactStrategy::Manual {
                keep_recent_turns: None,
            },
        ),
        (
            "reactive_prompt_too_long",
            CompactStrategy::ReactivePromptTooLong,
        ),
    ];

    for (trigger, strategy) in cases {
        let mut model = replay(
            session_id.clone(),
            &[
                started(0, &session_id),
                turn_event(1, &session_id, &turn_id, DurableEventPayload::TurnStarted),
            ],
        )
        .unwrap();
        let fingerprint = prefix_fingerprint(&model, 1);

        reduce(
            &turn_event(
                2,
                &session_id,
                &turn_id,
                DurableEventPayload::TranscriptRewritten {
                    source_seq: 1,
                    source_fingerprint: fingerprint,
                    messages: vec![LlmMessage::user("summary")],
                    reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                        trigger: trigger.into(),
                        pre_tokens: 100,
                        post_tokens: 10,
                        summary: "summary".into(),
                        transcript_path: None,
                        strategy,
                    }),
                },
            ),
            &mut model,
        )
        .unwrap();

        assert_eq!(model.execution.phase, Phase::Thinking, "{trigger}");
        assert_eq!(
            model.execution.unsettled_turn_id.as_ref(),
            Some(&turn_id),
            "{trigger}"
        );
    }
}

#[test]
fn projection_builds_complete_grouped_state_and_evolves_transcript() {
    let session_id = SessionId::new("session-1");
    let call_id = ToolCallId::new("call-1");
    let events = vec![
        started(0, &session_id),
        event(
            1,
            &session_id,
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: "inspect".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ),
        event(
            2,
            &session_id,
            DurableEventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "checking".into(),
                reasoning_content: Some("reasoning".into()),
            },
        ),
        event(
            3,
            &session_id,
            DurableEventPayload::ToolCallRequested {
                call_id: call_id.clone(),
                tool_name: "read".into(),
                arguments: serde_json::json!({"path": "README.md"}),
                raw_arguments: None,
            },
        ),
        event(
            4,
            &session_id,
            DurableEventPayload::ToolCallCompleted {
                call_id,
                tool_name: "read".into(),
                result: ToolResult::success("contents"),
                arguments: r#"{"path":"README.md"}"#.into(),
                arguments_json: Some(serde_json::json!({"path": "README.md"})),
            },
        ),
        event(
            5,
            &session_id,
            DurableEventPayload::RecapGenerated {
                text: "recap".into(),
                source: "manual".into(),
            },
        ),
        event(
            6,
            &session_id,
            DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
        ),
    ];

    let mut model = replay(session_id.clone(), &events).unwrap();

    assert_eq!(model.identity.session_id, session_id);
    assert_eq!(model.identity.working_dir, "/workspace");
    assert_eq!(model.identity.model_id, "model-a");
    assert_eq!(model.system_prompt.text, "system");
    assert_eq!(model.system_prompt.extra.as_deref(), Some("extra"));
    assert_eq!(model.stats.last_seq, 6);
    assert_eq!(model.stats.event_count, events.len());
    assert_eq!(model.transcript.messages.len(), 3);
    assert_eq!(model.transcript.messages[0].message.role, LlmRole::User);
    assert_eq!(
        model.transcript.messages[1].message.role,
        LlmRole::Assistant
    );
    assert_eq!(model.transcript.messages[2].message.role, LlmRole::Tool);
    assert!(matches!(
        model.transcript.artifacts.as_slice(),
        [TranscriptArtifactView::SystemNote { text, .. }] if text == "recap"
    ));
    assert!(model.execution.pending_tool_calls.is_empty());
    assert_eq!(model.cursor(), "6");
    assert_eq!(
        model.to_summary().first_user_message.as_deref(),
        Some("inspect")
    );

    reduce(
        &event(
            7,
            &session_id,
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: "concurrent tail".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ),
        &mut model,
    )
    .unwrap();
    reduce(
        &event(
            8,
            &session_id,
            DurableEventPayload::RecapGenerated {
                text: "tail artifact".into(),
                source: "manual".into(),
            },
        ),
        &mut model,
    )
    .unwrap();
    reduce(
        &event(
            9,
            &session_id,
            DurableEventPayload::TranscriptRewritten {
                source_seq: 6,
                source_fingerprint: prefix_fingerprint(&model, 6),
                messages: vec![LlmMessage::user(
                    "<compact_summary>summary</compact_summary>",
                )],
                reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                    trigger: "manual".into(),
                    pre_tokens: 100,
                    post_tokens: 10,
                    summary: "summary".into(),
                    transcript_path: None,
                    strategy: CompactStrategy::Manual {
                        keep_recent_turns: None,
                    },
                }),
            },
        ),
        &mut model,
    )
    .unwrap();
    assert_eq!(model.transcript.messages.len(), 2);
    assert!(
        model.transcript.messages[0]
            .message
            .joined_display_text("\n")
            .contains("summary")
    );
    assert!(
        model.transcript.messages[1]
            .message
            .joined_display_text("\n")
            .contains("concurrent tail")
    );
    assert!(matches!(
        model.transcript.artifacts.as_slice(),
        [TranscriptArtifactView::SystemNote { text, .. }] if text == "tail artifact"
    ));
    assert_eq!(model.first_user_message(), Some("inspect"));

    let fork_id = SessionId::new("session-fork");
    let fork = replay(
        fork_id.clone(),
        &[
            started(0, &fork_id),
            event(
                1,
                &fork_id,
                DurableEventPayload::SessionForked {
                    source_session_id: session_id,
                    source_cursor: "9".into(),
                    first_user_message: model.first_user_message().map(str::to_owned),
                    messages: model
                        .transcript
                        .messages
                        .iter()
                        .map(|message| message.message.clone())
                        .collect(),
                },
            ),
        ],
    )
    .unwrap();
    assert_eq!(fork.first_user_message(), Some("inspect"));
}

#[test]
fn projection_event_family_matrix_preserves_identity_execution_and_lineage() {
    let session_id = SessionId::new("session-matrix");
    let turn_id = TurnId::new("turn-a");
    let other_turn_id = TurnId::new("turn-b");
    let first_call_id = ToolCallId::new("call-b");
    let second_call_id = ToolCallId::new("call-a");
    let completed_child_id = SessionId::new("child-completed");
    let failed_child_id = SessionId::new("child-failed");
    let mut projection = SessionReadModelProjection::new(session_id.clone());

    let events = vec![
        started(0, &session_id),
        event(
            1,
            &session_id,
            DurableEventPayload::ModelIdChanged {
                model_id: "model-b".into(),
            },
        ),
        event(
            2,
            &session_id,
            DurableEventPayload::SessionToolsConfigured {
                selection: SessionToolSelection::All {
                    except: vec!["write".into()],
                },
            },
        ),
        event(
            3,
            &session_id,
            DurableEventPayload::SystemPromptConfigured {
                text: "replacement system".into(),
                fingerprint: "replacement fingerprint".into(),
                extra_system_prompt: None,
                source: SystemPromptSource::Inherited,
            },
        ),
        event(
            4,
            &session_id,
            DurableEventPayload::AgentSessionSpawned {
                child_session_id: completed_child_id.clone(),
                agent_name: "researcher".into(),
                task: "inspect".into(),
                tool_selection: None,
                tool_call_id: Some(ToolCallId::new("agent-call-completed")),
            },
        ),
        event(
            5,
            &session_id,
            DurableEventPayload::AgentSessionCompleted {
                child_session_id: completed_child_id.clone(),
                final_session_id: completed_child_id.clone(),
                summary: "done".into(),
            },
        ),
        event(
            6,
            &session_id,
            DurableEventPayload::AgentSessionSpawned {
                child_session_id: failed_child_id.clone(),
                agent_name: "reviewer".into(),
                task: "review".into(),
                tool_selection: None,
                tool_call_id: Some(ToolCallId::new("agent-call-failed")),
            },
        ),
        event(
            7,
            &session_id,
            DurableEventPayload::AgentSessionFailed {
                child_session_id: failed_child_id.clone(),
                final_session_id: failed_child_id.clone(),
                error: "failed".into(),
            },
        ),
        turn_event(8, &session_id, &turn_id, DurableEventPayload::TurnStarted),
        turn_event(
            9,
            &session_id,
            &turn_id,
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: "matrix prompt".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ),
        turn_event(
            10,
            &session_id,
            &turn_id,
            DurableEventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "running tools".into(),
                reasoning_content: None,
            },
        ),
        turn_event(
            11,
            &session_id,
            &turn_id,
            DurableEventPayload::ToolCallRequested {
                call_id: first_call_id.clone(),
                tool_name: "read-b".into(),
                arguments: serde_json::json!({}),
                raw_arguments: None,
            },
        ),
        turn_event(
            12,
            &session_id,
            &turn_id,
            DurableEventPayload::ToolCallRequested {
                call_id: second_call_id.clone(),
                tool_name: "read-a".into(),
                arguments: serde_json::json!({}),
                raw_arguments: None,
            },
        ),
        turn_event(
            13,
            &session_id,
            &turn_id,
            DurableEventPayload::ToolApprovalRequested {
                call_id: first_call_id.clone(),
                tool_name: "read-b".into(),
                prompt: "approve read-b".into(),
                rule_key: Some("read-b:*".into()),
                source: ApprovalSource::Core,
                arguments: serde_json::json!({}),
            },
        ),
    ];
    for event in &events {
        projection.apply(event).unwrap();
    }

    let pending = projection.snapshot().unwrap();
    assert_eq!(pending.identity.model_id, "model-b");
    assert_eq!(
        pending.identity.tool_selection,
        SessionToolSelection::All {
            except: vec!["write".into()]
        }
    );
    assert_eq!(pending.system_prompt.text, "replacement system");
    assert_eq!(pending.system_prompt.source, SystemPromptSource::Inherited);
    assert_eq!(pending.execution.phase, Phase::CallingTool);
    assert_eq!(pending.execution.unsettled_turn_id.as_ref(), Some(&turn_id));
    assert_eq!(
        pending
            .tool_calls_needing_interruption()
            .into_iter()
            .map(|call| call.call_id)
            .collect::<Vec<_>>(),
        vec![first_call_id.to_string(), second_call_id.to_string()]
    );
    let mut tail_only = pending.clone();
    tail_only.execution.pending_tool_calls.clear();
    assert_eq!(
        tail_only
            .tool_calls_needing_interruption()
            .into_iter()
            .map(|call| call.call_id)
            .collect::<Vec<_>>(),
        vec![first_call_id.to_string(), second_call_id.to_string()]
    );
    assert_eq!(
        pending
            .execution
            .pending_tool_approvals
            .get(&first_call_id)
            .map(|approval| approval.prompt.as_str()),
        Some("approve read-b")
    );

    let terminal_events = vec![
        turn_event(
            14,
            &session_id,
            &turn_id,
            DurableEventPayload::ToolApprovalResolved {
                call_id: first_call_id.clone(),
                decision: ApprovalDecision::AllowOnce,
                detail: None,
            },
        ),
        turn_event(
            15,
            &session_id,
            &turn_id,
            DurableEventPayload::ToolCallFailed {
                call_id: first_call_id,
                tool_name: "read-b".into(),
                error: "read failed".into(),
                metadata: Default::default(),
                duration_ms: Some(1),
                arguments: "{}".into(),
                arguments_json: Some(serde_json::json!({})),
            },
        ),
        turn_event(
            16,
            &session_id,
            &turn_id,
            DurableEventPayload::ToolCallCancelled {
                call_id: second_call_id,
                tool_name: "read-a".into(),
                reason: "interrupted".into(),
                duration_ms: Some(2),
                arguments: "{}".into(),
                arguments_json: Some(serde_json::json!({})),
            },
        ),
        turn_event(
            17,
            &session_id,
            &other_turn_id,
            DurableEventPayload::TurnCompleted {
                finish_reason: "stale".into(),
            },
        ),
    ];
    for event in &terminal_events {
        projection.apply(event).unwrap();
    }

    let before_matching_completion = projection.snapshot().unwrap();
    assert_eq!(before_matching_completion.execution.phase, Phase::Thinking);
    assert_eq!(
        before_matching_completion
            .execution
            .unsettled_turn_id
            .as_ref(),
        Some(&turn_id)
    );
    assert!(
        before_matching_completion
            .execution
            .pending_tool_calls
            .is_empty()
    );
    assert!(
        before_matching_completion
            .execution
            .pending_tool_approvals
            .is_empty()
    );
    assert_eq!(
        before_matching_completion.agent_sessions[0].status,
        AgentSessionStatus::Completed
    );
    assert_eq!(
        before_matching_completion.agent_sessions[0]
            .summary
            .as_deref(),
        Some("done")
    );
    assert_eq!(
        before_matching_completion.agent_sessions[1].status,
        AgentSessionStatus::Failed
    );
    assert_eq!(
        before_matching_completion.agent_sessions[1]
            .error
            .as_deref(),
        Some("failed")
    );
    assert_eq!(
        before_matching_completion.transcript.messages[2]
            .source
            .as_deref(),
        Some(TOOL_CALL_FAILED_SOURCE)
    );
    assert_eq!(
        before_matching_completion.transcript.messages[3]
            .source
            .as_deref(),
        Some(TOOL_CALL_CANCELLED_SOURCE)
    );

    projection
        .apply(&turn_event(
            18,
            &session_id,
            &turn_id,
            DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
        ))
        .unwrap();
    let completed = projection.snapshot().unwrap();
    assert_eq!(completed.execution.phase, Phase::Idle);
    assert_eq!(completed.execution.unsettled_turn_id, None);

    let fork_id = SessionId::new("session-fork-matrix");
    let fork = replay(
        fork_id.clone(),
        &[
            started(0, &fork_id),
            event(
                1,
                &fork_id,
                DurableEventPayload::SessionForked {
                    source_session_id: session_id.clone(),
                    source_cursor: "18".into(),
                    first_user_message: completed.first_user_message().map(str::to_owned),
                    messages: completed
                        .transcript
                        .messages
                        .iter()
                        .map(|message| message.message.clone())
                        .collect(),
                },
            ),
        ],
    )
    .unwrap();
    let forked_from = fork.identity.forked_from.as_ref().unwrap();
    assert_eq!(forked_from.session_id, session_id);
    assert_eq!(forked_from.cursor, "18");
    assert_eq!(fork.first_user_message(), Some("matrix prompt"));
}

#[test]
fn projection_rejects_invalid_stream_shapes_without_mutating_valid_state() {
    let session_id = SessionId::new("session-1");
    let other_session_id = SessionId::new("session-2");

    let empty = SessionReadModelProjection::new(session_id.clone());
    assert!(matches!(
        empty.snapshot(),
        Err(ProjectionError::MissingSessionStarted(_))
    ));

    let mut projection = SessionReadModelProjection::new(session_id.clone());
    assert_eq!(
        projection.apply(&event(0, &session_id, DurableEventPayload::TurnStarted)),
        Err(ProjectionError::InvalidFirstEvent)
    );
    assert_eq!(
        projection.apply(&started(1, &session_id)),
        Err(ProjectionError::InvalidFirstSequence(1))
    );

    projection.apply(&started(0, &session_id)).unwrap();
    assert!(matches!(
        projection.apply(&event(
            1,
            &other_session_id,
            DurableEventPayload::TurnStarted
        )),
        Err(ProjectionError::SessionMismatch { .. })
    ));
    assert!(matches!(
        projection.apply(&event(
            1,
            &session_id,
            DurableEventPayload::TranscriptRewritten {
                source_seq: 1,
                source_fingerprint: String::new(),
                messages: vec![LlmMessage::user("future rewrite")],
                reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                    trigger: "manual".into(),
                    pre_tokens: 10,
                    post_tokens: 5,
                    summary: "future".into(),
                    transcript_path: None,
                    strategy: CompactStrategy::Manual {
                        keep_recent_turns: None,
                    },
                }),
            }
        )),
        Err(ProjectionError::InvalidTranscriptRewriteSource { .. })
    ));
    assert_eq!(
        projection.apply(&event(2, &session_id, DurableEventPayload::TurnStarted)),
        Err(ProjectionError::NonContiguousSequence {
            expected: 1,
            actual: 2,
        })
    );
    assert_eq!(
        projection.apply(&started(1, &session_id)),
        Err(ProjectionError::DuplicateSessionStarted(1))
    );
    assert_eq!(projection.last_seq(), Some(0));
}

// ── TranscriptRewritten source_fingerprint 乐观并发校验 ──

/// 与被测的 `validate_transcript_rewrite_fingerprint` 走同一归一化原语，只保留
/// 「前缀 = `updated_seq <= source_seq`」这一划分，用于交叉验证指纹校验路径。
fn prefix_fingerprint(model: &SessionReadModel, source_seq: u64) -> String {
    let prefix = provider_transcript(
        model
            .transcript
            .messages
            .iter()
            .filter(|message| message.updated_seq <= source_seq)
            .map(|message| message.message.clone())
            .collect(),
    );
    transcript_prefix_fingerprint(&model.system_prompt.text, &prefix)
}

fn rewrite_event(
    seq: u64,
    session_id: &SessionId,
    source_seq: u64,
    source_fingerprint: String,
    summary: &str,
) -> StoredEvent {
    event(
        seq,
        session_id,
        DurableEventPayload::TranscriptRewritten {
            source_seq,
            source_fingerprint,
            messages: vec![LlmMessage::user(summary)],
            reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                trigger: "manual".into(),
                pre_tokens: 100,
                post_tokens: 10,
                summary: summary.into(),
                transcript_path: None,
                strategy: CompactStrategy::Manual {
                    keep_recent_turns: None,
                },
            }),
        },
    )
}

fn user_message(seq: u64, session_id: &SessionId, text: &str) -> StoredEvent {
    event(
        seq,
        session_id,
        DurableEventPayload::UserMessage {
            message_id: new_message_id(),
            text: text.into(),
            attachments: vec![],
            accepted_seq: None,
        },
    )
}

fn assistant_message(seq: u64, session_id: &SessionId, text: &str) -> StoredEvent {
    event(
        seq,
        session_id,
        DurableEventPayload::AssistantMessageCompleted {
            message_id: new_message_id(),
            text: text.into(),
            reasoning_content: None,
        },
    )
}

#[test]
fn transcript_rewrite_with_matching_fingerprint_applies() {
    let session_id = SessionId::new("session-fp-match");
    let mut model = replay(
        session_id.clone(),
        &[
            started(0, &session_id),
            user_message(1, &session_id, "old user"),
            assistant_message(2, &session_id, "old answer"),
            user_message(3, &session_id, "tail user"),
        ],
    )
    .unwrap();

    let fingerprint = prefix_fingerprint(&model, 2);
    reduce(
        &rewrite_event(4, &session_id, 2, fingerprint, "summary"),
        &mut model,
    )
    .unwrap();

    let texts: Vec<String> = model
        .transcript
        .messages
        .iter()
        .map(|message| message.message.joined_display_text("\n"))
        .collect();
    assert_eq!(texts, ["summary", "tail user"]);
}

#[test]
fn transcript_rewrite_with_stale_fingerprint_is_rejected_without_mutation() {
    let session_id = SessionId::new("session-fp-stale");
    let mut model = replay(
        session_id.clone(),
        &[
            started(0, &session_id),
            user_message(1, &session_id, "old user"),
            assistant_message(2, &session_id, "old answer"),
        ],
    )
    .unwrap();

    let result = reduce(
        &rewrite_event(3, &session_id, 2, "deadbeefdeadbeef".into(), "summary"),
        &mut model,
    );
    assert!(matches!(
        result,
        Err(ProjectionError::TranscriptRewriteSourceFingerprintMismatch { source_seq: 2, .. })
    ));
    assert_eq!(model.transcript.messages.len(), 2);
    assert_eq!(model.stats.last_seq, 2);
}

#[test]
fn consecutive_rewrites_validate_against_updated_prefix() {
    let session_id = SessionId::new("session-fp-chain");
    let mut model = replay(
        session_id.clone(),
        &[
            started(0, &session_id),
            user_message(1, &session_id, "user 1"),
            assistant_message(2, &session_id, "answer 1"),
            user_message(3, &session_id, "user 2"),
            assistant_message(4, &session_id, "answer 2"),
        ],
    )
    .unwrap();

    let first_fingerprint = prefix_fingerprint(&model, 2);
    reduce(
        &rewrite_event(5, &session_id, 2, first_fingerprint.clone(), "summary 1"),
        &mut model,
    )
    .unwrap();

    // 第二次 rewrite 的前缀包含第一次的输出（锚定在 source_seq=2）。
    let second_fingerprint = prefix_fingerprint(&model, 4);
    assert_ne!(first_fingerprint, second_fingerprint);
    reduce(
        &rewrite_event(6, &session_id, 4, second_fingerprint, "summary 2"),
        &mut model,
    )
    .unwrap();

    let texts: Vec<String> = model
        .transcript
        .messages
        .iter()
        .map(|message| message.message.joined_display_text("\n"))
        .collect();
    assert_eq!(texts, ["summary 2"]);

    // 第一次的指纹对已改写前缀失效。
    let mut replayed = replay(
        session_id.clone(),
        &[
            started(0, &session_id),
            user_message(1, &session_id, "user 1"),
            assistant_message(2, &session_id, "answer 1"),
            user_message(3, &session_id, "user 2"),
            assistant_message(4, &session_id, "answer 2"),
        ],
    )
    .unwrap();
    reduce(
        &rewrite_event(5, &session_id, 2, first_fingerprint.clone(), "summary 1"),
        &mut replayed,
    )
    .unwrap();
    assert!(matches!(
        reduce(
            &rewrite_event(6, &session_id, 4, first_fingerprint, "summary 2"),
            &mut replayed,
        ),
        Err(ProjectionError::TranscriptRewriteSourceFingerprintMismatch { source_seq: 4, .. })
    ));
}

use astrcode_core::{
    event::{
        DurableEvent, DurableEventPayload, PersistedSystemPrompt, SessionStarted, StoredEvent,
        SystemPromptSource,
    },
    llm::LlmRole,
    tool::{SessionToolSelection, ToolResult},
    types::{SessionId, ToolCallId, new_message_id},
};

use super::{ProjectionError, SessionReadModelProjection, replay};
use crate::TranscriptArtifactView;

fn event(seq: u64, session_id: &SessionId, payload: DurableEventPayload) -> StoredEvent {
    StoredEvent::new(seq, DurableEvent::session(session_id.clone(), payload))
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

    let model = replay(session_id.clone(), &events).unwrap();

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

    let invalid_cursor = event(
        1,
        &session_id,
        DurableEventPayload::SessionContinuedFromCompaction {
            parent_session_id: session_id.clone(),
            parent_cursor: "not-a-sequence".into(),
            summary: "summary".into(),
            transcript_path: None,
            context_messages: vec![],
            retained_messages: vec![],
        },
    );
    assert_eq!(
        projection.apply(&invalid_cursor),
        Err(ProjectionError::InvalidParentCursor(
            "not-a-sequence".into()
        ))
    );
    assert_eq!(projection.last_seq(), Some(0));
}

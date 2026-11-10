use super::*;
use crate::{tool::SessionToolSelection, types::SessionId};

fn initial_prompt() -> PersistedSystemPrompt {
    PersistedSystemPrompt {
        text: "system".into(),
        fingerprint: "fingerprint".into(),
        extra_system_prompt: None,
        source: SystemPromptSource::Native,
    }
}

#[test]
fn fork_usage_anchor_remains_readable_by_legacy_storage_readers() {
    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
    enum LegacyPayload {
        SessionForked {
            source_session_id: SessionId,
            source_cursor: String,
            first_user_message: Option<String>,
            messages: Vec<crate::llm::TranscriptMessage>,
        },
    }
    #[derive(serde::Deserialize)]
    struct LegacyStoredEvent {
        seq: u64,
        #[serde(flatten)]
        event: EventEnvelope<LegacyPayload>,
    }

    let messages = vec![crate::llm::TranscriptMessage::plain(
        crate::llm::LlmMessage::user("inherited"),
    )];
    let stored = StoredEvent::new(
        1,
        DurableEvent::session(
            SessionId::from("fork"),
            DurableEventPayload::SessionForked {
                source_session_id: SessionId::from("source"),
                source_cursor: "10".into(),
                first_user_message: Some("inherited".into()),
                messages: messages.clone(),
                input_token_anchor: Some(ContextTokenAnchor {
                    context_tokens: 180_000,
                    model_context_window: 200_000,
                    covered_message_count: 1,
                }),
            },
        ),
    );
    let json = serde_json::to_value(&stored).unwrap();
    let legacy: LegacyStoredEvent = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(legacy.seq, 1);
    let LegacyPayload::SessionForked {
        source_session_id,
        source_cursor,
        first_user_message,
        messages: inherited,
    } = legacy.event.payload;
    assert_eq!(source_session_id.as_str(), "source");
    assert_eq!(source_cursor, "10");
    assert_eq!(first_user_message.as_deref(), Some("inherited"));
    assert_eq!(inherited, messages);
    assert_eq!(serde_json::from_value::<StoredEvent>(json).unwrap(), stored);

    let mut misplaced = serde_json::to_value(StoredEvent::new(
        2,
        DurableEvent::session(SessionId::from("fork"), DurableEventPayload::TurnStarted),
    ))
    .unwrap();
    misplaced["input_token_anchor"] = serde_json::json!({
        "context_tokens": 1, "model_context_window": 200_000, "covered_message_count": 0,
    });
    assert!(serde_json::from_value::<StoredEvent>(misplaced).is_err());
}

#[test]
fn event_lifecycles_have_distinct_envelopes_and_wire_shapes() {
    let session_id = SessionId::from("session-1");
    let durable = DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::SessionStarted(SessionStarted {
            working_dir: ".".into(),
            model_id: "model".into(),
            parent: None,
            tool_selection: SessionToolSelection::default(),
            source_extension: None,
            initial_system_prompt: initial_prompt(),
        }),
    );
    let stored = StoredEvent::new(0, durable);
    let live = LiveEvent::session(
        session_id,
        LiveEventPayload::CompactionCompleted {
            messages_removed: 3,
        },
    );

    let stored_json = serde_json::to_value(Event::from(&stored)).unwrap();
    let live_json = serde_json::to_value(Event::from(live)).unwrap();

    assert_eq!(stored_json["seq"], 0);
    assert_eq!(stored_json["payload"]["type"], "session_started");
    assert_eq!(
        stored_json["payload"]["initial_system_prompt"]["text"],
        "system"
    );
    assert!(live_json.get("seq").is_none());
    assert_eq!(live_json["payload"]["type"], "compaction_completed");
    assert_eq!(live_json["payload"]["messages_removed"], 3);

    let decoded: StoredEvent =
        serde_json::from_value(serde_json::to_value(&stored).unwrap()).unwrap();
    assert_eq!(decoded, stored);

    for event in [
        Event::from(StoredEvent::new(
            1,
            DurableEvent::session(
                SessionId::from("session-1"),
                DurableEventPayload::ErrorOccurred {
                    code: 1,
                    message: "durable".into(),
                    recoverable: false,
                },
            ),
        )),
        Event::from(LiveEvent::session(
            SessionId::from("session-1"),
            LiveEventPayload::ErrorOccurred {
                code: 2,
                message: "live".into(),
                recoverable: true,
            },
        )),
    ] {
        let decoded: Event = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        assert_eq!(decoded, event);
    }
}

#[test]
fn session_started_rejects_incomplete_initial_state() {
    let missing_prompt = serde_json::json!({
        "type": "session_started",
        "working_dir": ".",
        "model_id": "model",
        "tool_selection": { "mode": "all", "except": [] }
    });

    assert!(serde_json::from_value::<DurableEventPayload>(missing_prompt).is_err());
}

#[test]
fn agent_session_spawned_accepts_missing_tool_call_id() {
    let payload = serde_json::json!({
        "type": "agent_session_spawned",
        "child_session_id": "child-1",
        "agent_name": "worker",
        "task": "run",
    });

    let decoded = serde_json::from_value::<DurableEventPayload>(payload).unwrap();
    assert!(matches!(
        decoded,
        DurableEventPayload::AgentSessionSpawned {
            tool_call_id: None,
            ..
        }
    ));
}

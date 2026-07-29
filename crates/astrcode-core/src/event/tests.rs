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

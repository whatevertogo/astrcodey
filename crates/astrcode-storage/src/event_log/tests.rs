use astrcode_core::{
    event::{DurableEventPayload, StoredEvent},
    types::SessionId,
};
use tempfile::tempdir;

use super::{EventLog, parse_event_line, replay_events_at_path};
use crate::{
    StorageError,
    test_support::{started_event, user_event},
};

#[tokio::test]
async fn event_log_round_trip_reopen_and_append_guards_share_one_sequence_contract() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let session_id = SessionId::new("session-1");

    let (log, started) = EventLog::create(path.clone(), started_event(&session_id))
        .await
        .unwrap();
    let user = log.append(user_event(&session_id, "hello")).await.unwrap();
    log.force_sync().await.unwrap();
    assert_eq!((started.seq, user.seq), (0, 1));
    assert_eq!(log.replay_all().await.unwrap().len(), 2);
    drop(log);

    let log = EventLog::open(path.clone()).await.unwrap();
    let assistant = log
        .append(astrcode_core::event::DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::AssistantMessageCompleted {
                message_id: "message-1".into(),
                text: "done".into(),
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();
    assert_eq!(assistant.seq, 2);

    let wrong_session = log
        .append(user_event(&SessionId::new("session-2"), "wrong"))
        .await
        .unwrap_err();
    assert!(matches!(wrong_session, StorageError::InvalidEvent(_)));
    let duplicate_start = log.append(started_event(&session_id)).await.unwrap_err();
    assert!(matches!(duplicate_start, StorageError::InvalidEvent(_)));
    assert_eq!(log.count().await.unwrap(), 3);
}

#[test]
fn event_log_rejects_each_invalid_committed_stream_shape() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("session-1");
    let other_session_id = SessionId::new("session-2");
    let valid_start = StoredEvent::new(0, started_event(&session_id));

    let cases = vec![
        vec![StoredEvent::new(
            0,
            user_event(&session_id, "missing start"),
        )],
        vec![
            valid_start.clone(),
            StoredEvent::new(2, user_event(&session_id, "gap")),
        ],
        vec![
            valid_start.clone(),
            StoredEvent::new(1, user_event(&other_session_id, "mixed")),
        ],
        vec![valid_start, StoredEvent::new(1, started_event(&session_id))],
    ];

    for (index, events) in cases.into_iter().enumerate() {
        let path = dir.path().join(format!("invalid-{index}.jsonl"));
        let content = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();

        assert!(
            matches!(
                replay_events_at_path(&path, None, None),
                Err(StorageError::CorruptLog(_))
            ),
            "case {index} unexpectedly passed"
        );
    }
}

#[test]
fn legacy_event_shapes_upgrade_at_the_storage_boundary() {
    let path = std::path::Path::new("legacy-events.jsonl");
    let envelope = |seq: u64, payload: serde_json::Value| {
        serde_json::json!({
            "seq": seq,
            "id": format!("event-{seq}"),
            "session_id": "session-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "payload": payload,
        })
        .to_string()
    };

    let started = parse_event_line(
        path,
        1,
        &envelope(
            0,
            serde_json::json!({
                "type": "session_started",
                "working_dir": "/workspace",
                "model_id": "model",
                "tool_selection": null,
            }),
        ),
    )
    .unwrap();
    let DurableEventPayload::SessionStarted(started) = &started.event.payload else {
        panic!("expected upgraded SessionStarted");
    };
    assert!(matches!(
        started.tool_selection,
        astrcode_core::tool::SessionToolSelection::All { ref except } if except.is_empty()
    ));
    assert!(started.initial_system_prompt.text.is_empty());

    let prompt = parse_event_line(
        path,
        2,
        &envelope(
            1,
            serde_json::json!({
                "type": "system_prompt_configured",
                "text": "prompt",
                "fingerprint": "fingerprint",
            }),
        ),
    )
    .unwrap();
    assert!(matches!(
        &prompt.event.payload,
        DurableEventPayload::SystemPromptConfigured {
            source: astrcode_core::event::SystemPromptSource::Native,
            ..
        }
    ));

    let ignored = parse_event_line(
        path,
        3,
        &envelope(
            2,
            serde_json::json!({
                "type": "assistant_message_started",
                "message_id": "message-1",
            }),
        ),
    )
    .unwrap();
    assert!(matches!(
        &ignored.event.payload,
        DurableEventPayload::ExtensionEvent(event)
            if event.extension_id == "astrcode.legacy"
                && event.event_type == "legacy.assistant_message_started"
    ));

    let compacted = parse_event_line(
        path,
        4,
        &envelope(
            3,
            serde_json::json!({
                "type": "session_continued_from_compaction",
                "parent_session_id": "session-1",
                "parent_cursor": "2",
                "summary": "summary",
                "context_messages": [],
                "retained_messages": [],
            }),
        ),
    )
    .unwrap();
    assert!(matches!(
        &compacted.event.payload,
        DurableEventPayload::TranscriptRewritten { source_seq: 2, .. }
    ));
}

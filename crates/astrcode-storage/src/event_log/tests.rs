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

#[tokio::test]
async fn event_log_batch_is_atomic_and_assigns_consecutive_sequences() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let session_id = SessionId::new("session-1");
    let other_session_id = SessionId::new("session-2");
    let (log, _) = EventLog::create(path, started_event(&session_id))
        .await
        .unwrap();

    let error = log
        .append_batch(vec![
            user_event(&session_id, "valid"),
            user_event(&other_session_id, "wrong session"),
        ])
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::InvalidEvent(_)));
    assert_eq!(log.count().await.unwrap(), 1);
    assert_eq!(log.replay_all().await.unwrap().len(), 1);

    let stored = log
        .append_batch(vec![
            user_event(&session_id, "first"),
            user_event(&session_id, "second"),
        ])
        .await
        .unwrap();
    assert_eq!(
        stored.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
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
fn event_log_rejects_malformed_event_lines() {
    let path = std::path::Path::new("malformed-events.jsonl");

    let cases = vec![
        ("not-json-at-all", "completely invalid JSON"),
        (
            r#"{"seq": 0, "id": "event-0", "session_id": "session-1", "timestamp": "2026-01-01T00:00:00Z", "payload": {}}"#,
            "payload missing required fields",
        ),
        (
            r#"{"seq": "not-a-number", "id": "event-0", "session_id": "session-1", "timestamp": "2026-01-01T00:00:00Z", "payload": {"type": "session_started"}}"#,
            "wrong seq type",
        ),
    ];

    for (index, (line, description)) in cases.into_iter().enumerate() {
        let result = parse_event_line(path, index + 1, line);
        assert!(
            matches!(result, Err(StorageError::CorruptLog(_))),
            "case {index} ({description}) unexpectedly passed: {result:?}"
        );
    }
}

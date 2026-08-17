use astrcode_core::{
    event::{DurableEventPayload, StoredEvent},
    types::SessionId,
};
use tempfile::tempdir;
use tokio::time::{Duration, timeout};

use super::{
    EventLog, parse_event_line, read_summary_at_path, replay_events_at_path, sync_existing_log,
};
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

    let (log, events) = EventLog::open(path.clone()).await.unwrap();
    assert_eq!(events.len(), 2);
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
    assert_eq!(log.count(), 3);
}

#[tokio::test]
async fn event_log_replays_bounded_pages_before_exclusive_cursor() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("paged-session");
    let (log, _) = EventLog::create(dir.path().join("events.jsonl"), started_event(&session_id))
        .await
        .unwrap();
    for text in [
        "one".to_owned(),
        "x".repeat(70 * 1024),
        "three".to_owned(),
        "four".to_owned(),
    ] {
        log.append(user_event(&session_id, &text)).await.unwrap();
    }
    log.force_sync().await.unwrap();

    let latest = log.replay_before_limited(None, 2).await.unwrap();
    let older = log.replay_before_limited(Some(3), 2).await.unwrap();
    let empty = log.replay_before_limited(Some(1), 0).await.unwrap();

    assert_eq!(
        latest.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [3, 4]
    );
    assert_eq!(
        older.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(empty.is_empty());
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
    assert_eq!(log.count(), 1);
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
fn idle_event_logs_do_not_occupy_the_blocking_pool() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        timeout(Duration::from_secs(10), async {
            let dir = tempdir().unwrap();
            let mut logs = Vec::new();
            for index in 0..16 {
                let session_id = SessionId::new(format!("session-{index}"));
                let path = dir.path().join(format!("events-{index}.jsonl"));
                let (log, _) = EventLog::create(path, started_event(&session_id))
                    .await
                    .unwrap();
                logs.push((session_id, log));
            }

            for (session_id, log) in &logs {
                log.append(user_event(session_id, "message")).await.unwrap();
                log.force_sync().await.unwrap();
            }
        })
        .await
        .expect("idle logs must not exhaust the shared blocking pool");
    });
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
                replay_events_at_path(&path, None, None, None),
                Err(StorageError::CorruptLog(_))
            ),
            "case {index} unexpectedly passed"
        );
    }
}

#[test]
fn cold_reads_stop_at_the_length_confirmed_before_a_concurrent_append() {
    use std::io::Write;

    let dir = tempdir().unwrap();
    let path = dir.path().join("cold-read-race.jsonl");
    let session_id = SessionId::new("cold-read-race");
    let started = StoredEvent::new(0, started_event(&session_id));
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&started).unwrap()),
    )
    .unwrap();

    let confirmed_len = sync_existing_log(&path).unwrap();
    let late = StoredEvent::new(1, user_event(&session_id, "late unconfirmed event"));
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(writer, "{}", serde_json::to_string(&late).unwrap()).unwrap();
    writer.flush().unwrap();

    let replay = replay_events_at_path(&path, Some(confirmed_len), None, None).unwrap();
    let summary = read_summary_at_path(&path, session_id, Some(confirmed_len))
        .unwrap()
        .unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(summary.latest_cursor.to_string(), "0");
    assert!(summary.first_user_message.is_none());
    assert_eq!(
        replay_events_at_path(&path, None, None, None)
            .unwrap()
            .len(),
        2,
        "fixture must contain a complete late record"
    );
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

//! Event log 追加与 replay 集成测试。

use astrcode_core::{
    event::{Event, EventPayload},
    types::{SessionId, TurnId},
};
use astrcode_storage::event_log::EventLog;
use tempfile::tempdir;

#[tokio::test]
async fn append_replay_after_preserves_order_and_seq() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let session_id: SessionId = "sess-replay".into();

    let (log, start) = EventLog::create(
        path.clone(),
        Event::new(
            session_id.clone(),
            None,
            EventPayload::SessionStarted {
                working_dir: ".".into(),
                model_id: "test".into(),
                parent_session_id: None,
                tool_policy: None,
                source_extension: None,
            },
        ),
    )
    .await
    .unwrap();
    assert_eq!(start.seq, Some(0));

    for i in 0..3 {
        log.append(Event::new(
            session_id.clone(),
            Some(TurnId::from(format!("turn-{i}"))),
            EventPayload::TurnStarted,
        ))
        .await
        .unwrap();
    }

    let all = log.replay_all().await.unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all.last().and_then(|e| e.seq), Some(3));

    let tail = log.replay_after(1).await.unwrap();
    assert_eq!(tail.len(), 2);
    assert!(tail.iter().all(|e| e.seq.is_some_and(|s| s > 1)));
}

use astrcode_core::types::SessionId;

use super::InMemoryEventStore;
use crate::{
    EventConsumerCheckpointOutcome, EventConsumerCheckpointReset, EventReader, SessionEventJournal,
    SessionReader, SessionStore, StorageError,
    test_support::{started_event, user_event},
};

#[tokio::test]
async fn in_memory_store_enforces_creation_and_updates_projection_atomically() {
    let store = InMemoryEventStore::new();
    let session_id = SessionId::new("session-1");

    let started = store
        .create_session(started_event(&session_id))
        .await
        .unwrap();
    assert_eq!(started.seq, 0);
    assert!(matches!(
        store.create_session(started_event(&session_id)).await,
        Err(StorageError::AlreadyExists(_))
    ));

    let user = store
        .append_event(user_event(&session_id, "hello"))
        .await
        .unwrap();
    let model = store.session_read_model(&session_id).await.unwrap();
    assert_eq!(user.seq, 1);
    assert_eq!(model.stats.last_seq, 1);
    assert_eq!(model.model_context.messages.len(), 1);
    assert_eq!(model.system_prompt.text, "system");
    assert_eq!(
        store.latest_cursor(&session_id).await.unwrap(),
        Some("1".into())
    );

    assert_eq!(
        store
            .event_consumer_state(&session_id, "extension:subscription")
            .await
            .unwrap()
            .checkpoint,
        None
    );
    for seq in [1, 0] {
        assert_eq!(
            store
                .checkpoint_event_consumer(&session_id, "extension:subscription", 0, seq)
                .await
                .unwrap(),
            EventConsumerCheckpointOutcome::Accepted
        );
    }
    assert_eq!(
        store
            .event_consumer_state(&session_id, "extension:subscription")
            .await
            .unwrap()
            .checkpoint,
        Some(1)
    );
    let paused = store
        .set_event_consumer_paused(&session_id, "extension:subscription", true)
        .await
        .unwrap();
    assert!(paused.paused);
    let reset = store
        .reset_event_consumer_checkpoint(
            &session_id,
            "extension:subscription",
            EventConsumerCheckpointReset::Beginning,
        )
        .await
        .unwrap();
    assert_eq!(reset.checkpoint, None);
    assert_eq!(reset.revision, 1);
    assert_eq!(
        store
            .checkpoint_event_consumer(&session_id, "extension:subscription", 0, 1)
            .await
            .unwrap(),
        EventConsumerCheckpointOutcome::StaleRevision
    );
}

#[tokio::test]
async fn in_memory_batch_rejects_all_events_when_one_transition_is_invalid() {
    let store = InMemoryEventStore::new();
    let session_id = SessionId::new("session-1");
    store
        .create_session(started_event(&session_id))
        .await
        .unwrap();

    let error = store
        .append_events(vec![
            user_event(&session_id, "would otherwise be valid"),
            started_event(&session_id),
        ])
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::InvalidEvent(_)));
    assert_eq!(store.replay_events(&session_id).await.unwrap().len(), 1);
    assert_eq!(
        store
            .session_read_model(&session_id)
            .await
            .unwrap()
            .stats
            .last_seq,
        0
    );
}

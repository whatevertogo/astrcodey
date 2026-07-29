use astrcode_core::types::SessionId;

use super::InMemoryEventStore;
use crate::{
    EventReader, EventStore, SessionReader, StorageError,
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
    assert_eq!(model.transcript.messages.len(), 1);
    assert_eq!(model.system_prompt.text, "system");
    assert_eq!(
        store.latest_cursor(&session_id).await.unwrap(),
        Some("1".into())
    );
    assert!(store.checkpoint(&session_id, &"0".into()).await.is_err());
    store.checkpoint(&session_id, &"1".into()).await.unwrap();
}

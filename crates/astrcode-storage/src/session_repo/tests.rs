use std::sync::Arc;

use astrcode_core::types::SessionId;
use tempfile::tempdir;

use super::FileSystemSessionRepository;
use crate::{
    EventReader, EventStore, SessionReader, StorageError,
    test_support::{started_event, user_event},
};

#[tokio::test]
async fn filesystem_repository_rebuilds_grouped_projection_and_snapshot_tail() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("session-1");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());

    repo.create_session(started_event(&session_id))
        .await
        .unwrap();
    repo.append_event(user_event(&session_id, "first"))
        .await
        .unwrap();
    repo.checkpoint(&session_id, &"1".into()).await.unwrap();
    repo.append_event(user_event(&session_id, "second"))
        .await
        .unwrap();
    repo.sync_durable_events(&session_id).await.unwrap();
    drop(repo);

    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    let model = reopened.session_read_model(&session_id).await.unwrap();
    assert_eq!(model.identity.working_dir, "/workspace");
    assert_eq!(model.system_prompt.text, "system");
    assert_eq!(model.stats.last_seq, 2);
    assert_eq!(model.stats.event_count, 3);
    assert_eq!(model.transcript.messages.len(), 2);
    assert_eq!(reopened.replay_events(&session_id).await.unwrap().len(), 3);
}

#[tokio::test]
async fn filesystem_repository_validates_and_orders_appends_before_commit() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("session-ordered");
    let repo = Arc::new(FileSystemSessionRepository::with_projects_base(
        dir.path().into(),
    ));
    repo.create_session(started_event(&session_id))
        .await
        .unwrap();

    let invalid = started_event(&session_id);
    assert!(matches!(
        repo.append_event(invalid).await,
        Err(StorageError::InvalidEvent(_))
    ));
    assert_eq!(repo.replay_events(&session_id).await.unwrap().len(), 1);

    let mut appends = Vec::new();
    for index in 0..32 {
        let repo = Arc::clone(&repo);
        let session_id = session_id.clone();
        appends.push(tokio::spawn(async move {
            repo.append_event(user_event(&session_id, &format!("message-{index}")))
                .await
        }));
    }

    let mut sequences = Vec::new();
    for append in appends {
        sequences.push(append.await.unwrap().unwrap().seq);
    }
    sequences.sort_unstable();

    assert_eq!(sequences, (1..=32).collect::<Vec<_>>());
    let model = repo.session_read_model(&session_id).await.unwrap();
    assert_eq!(model.stats.last_seq, 32);
    assert_eq!(model.stats.event_count, 33);
    assert_eq!(model.transcript.messages.len(), 32);
    assert_eq!(repo.replay_events(&session_id).await.unwrap().len(), 33);
}

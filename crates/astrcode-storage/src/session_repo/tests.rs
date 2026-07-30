use std::sync::Arc;

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, ParentSessionRef, Phase},
    types::{SessionId, TurnId},
};
use tempfile::tempdir;

use super::FileSystemSessionRepository;
use crate::{
    EventReader, SessionEventJournal, SessionReader, SessionStore, StorageError,
    ToolResultArtifactInput, ToolResultArtifactStore,
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
async fn recycled_read_model_is_read_only_and_preserves_child_identity() {
    let dir = tempdir().unwrap();
    let parent_id = SessionId::new("session-parent");
    let child_id = SessionId::new("session-child");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());

    repo.create_session(started_event(&parent_id))
        .await
        .unwrap();
    let mut child_started = started_event(&child_id);
    let DurableEventPayload::SessionStarted(started) = &mut child_started.payload else {
        unreachable!("test fixture must contain SessionStarted");
    };
    started.parent = Some(ParentSessionRef {
        session_id: parent_id.clone(),
    });
    started.source_extension = Some("test".into());
    repo.create_session(child_started).await.unwrap();
    repo.append_event(user_event(&child_id, "preserved"))
        .await
        .unwrap();
    repo.sync_durable_events(&child_id).await.unwrap();

    repo.recycle_session(&child_id).await.unwrap();
    assert!(matches!(
        repo.session_read_model(&child_id).await,
        Err(StorageError::NotFound(_))
    ));

    let recycled = repo.recycled_session_read_model(&child_id).await.unwrap();
    assert_eq!(
        recycled
            .identity
            .parent
            .as_ref()
            .map(|parent| &parent.session_id),
        Some(&parent_id)
    );
    assert_eq!(recycled.first_user_message(), Some("preserved"));
    assert!(matches!(
        repo.session_read_model(&child_id).await,
        Err(StorageError::NotFound(_))
    ));

    repo.restore_session(&child_id).await.unwrap();
    assert_eq!(
        repo.session_read_model(&child_id)
            .await
            .unwrap()
            .identity
            .session_id,
        child_id
    );
}

#[tokio::test]
async fn tool_result_artifacts_stay_inside_the_session_directory() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("session-artifacts");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());
    repo.create_session(started_event(&session_id))
        .await
        .unwrap();

    let artifact = repo
        .write_tool_result_artifact(
            &session_id,
            ToolResultArtifactInput {
                call_id: "call-1".into(),
                tool_name: "shell".into(),
                content: "artifact content".into(),
            },
        )
        .await
        .unwrap();
    let artifact_path = artifact.path.unwrap();
    let slice = repo
        .read_tool_result_artifact_by_path(&session_id, &artifact_path, 0, 100)
        .await
        .unwrap();
    assert_eq!(slice.content, "artifact content");

    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "outside").unwrap();
    let error = repo
        .read_tool_result_artifact_by_path(&session_id, outside.to_str().unwrap(), 0, 100)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::InvalidId(_)));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let artifact_path = std::path::PathBuf::from(artifact_path);
        let artifact_dir = artifact_path.parent().unwrap().to_path_buf();
        std::fs::remove_file(&artifact_path).unwrap();
        std::fs::remove_dir(&artifact_dir).unwrap();
        symlink(dir.path(), &artifact_dir).unwrap();

        let error = repo
            .read_tool_result_artifact_by_path(&session_id, outside.to_str().unwrap(), 0, 100)
            .await
            .unwrap_err();
        assert!(matches!(error, StorageError::InvalidId(_)));
    }
}

#[tokio::test]
async fn filesystem_repository_enforces_owner_validation_and_append_order() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("session-ordered");
    let projects_base = dir.path().to_path_buf();
    let repo = Arc::new(FileSystemSessionRepository::with_projects_base(
        projects_base.clone(),
    ));
    repo.create_session(started_event(&session_id))
        .await
        .unwrap();
    let initial_model = repo.session_read_model(&session_id).await.unwrap();
    assert!(Arc::ptr_eq(
        &initial_model,
        &repo.session_read_model(&session_id).await.unwrap()
    ));

    let competing = FileSystemSessionRepository::with_projects_base(projects_base);
    assert!(matches!(
        competing.session_read_model(&session_id).await,
        Err(StorageError::LockError(_))
    ));

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
    assert!(!Arc::ptr_eq(&initial_model, &model));
    assert_eq!(initial_model.stats.last_seq, 0);
    assert!(Arc::ptr_eq(
        &model,
        &repo.session_read_model(&session_id).await.unwrap()
    ));
    assert_eq!(model.stats.last_seq, 32);
    assert_eq!(model.stats.event_count, 33);
    assert_eq!(model.transcript.messages.len(), 32);
    assert_eq!(repo.replay_events(&session_id).await.unwrap().len(), 33);
}

#[tokio::test]
async fn filesystem_repository_cold_and_hot_summaries_are_equivalent() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("session-summary");
    let turn_id = TurnId::new("turn-summary");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());

    repo.create_session(started_event(&session_id))
        .await
        .unwrap();
    repo.append_event(user_event(&session_id, "summary title"))
        .await
        .unwrap();
    repo.append_event(DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::ModelIdChanged {
            model_id: "model-b".into(),
        },
    ))
    .await
    .unwrap();
    repo.append_event(DurableEvent::turn(
        session_id.clone(),
        turn_id,
        DurableEventPayload::TurnStarted,
    ))
    .await
    .unwrap();
    repo.sync_durable_events(&session_id).await.unwrap();

    let hot = repo.list_session_summaries().await.unwrap();
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0].model_id, "model-b");
    assert_eq!(hot[0].phase, Phase::Thinking);
    assert_eq!(hot[0].first_user_message.as_deref(), Some("summary title"));
    drop(repo);

    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    let cold = reopened.list_session_summaries().await.unwrap();
    assert_eq!(cold, hot);
}

#[tokio::test]
async fn filesystem_repository_rejects_snapshot_from_another_session() {
    let dir = tempdir().unwrap();
    let source_id = SessionId::new("session-snapshot-source");
    let target_id = SessionId::new("session-snapshot-target");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());

    repo.create_session(started_event(&source_id))
        .await
        .unwrap();
    repo.append_event(user_event(&source_id, "source message"))
        .await
        .unwrap();
    repo.checkpoint(&source_id, &"1".into()).await.unwrap();

    repo.create_session(started_event(&target_id))
        .await
        .unwrap();
    repo.append_event(user_event(&target_id, "target message"))
        .await
        .unwrap();
    repo.sync_durable_events(&target_id).await.unwrap();

    let source_dir = repo.find_session_dir(&source_id).await.unwrap();
    let target_dir = repo.find_session_dir(&target_id).await.unwrap();
    drop(repo);

    let target_snapshots = target_dir.join("snapshots");
    tokio::fs::create_dir_all(&target_snapshots).await.unwrap();
    tokio::fs::copy(
        source_dir.join("snapshots/snapshot-1.json"),
        target_snapshots.join("snapshot-1.json"),
    )
    .await
    .unwrap();

    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    let model = reopened.session_read_model(&target_id).await.unwrap();
    assert_eq!(model.identity.session_id, target_id);
    assert_eq!(model.first_user_message(), Some("target message"));
}

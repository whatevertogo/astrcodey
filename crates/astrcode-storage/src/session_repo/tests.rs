use std::sync::Arc;

use astrcode_core::{
    compaction::CompactStrategy,
    event::{
        CompactionDetails, DurableEvent, DurableEventPayload, ParentSessionRef, Phase,
        TranscriptRewriteReason, transcript_prefix_fingerprint,
    },
    llm::{LlmMessage, TranscriptMessage},
    types::{SessionId, TurnId},
};
use tempfile::tempdir;

use super::{FileSystemSessionRepository, consumer_state::event_consumer_state_path};
use crate::{
    EventConsumerCheckpointOutcome, EventConsumerCheckpointReset, EventConsumerFailureOutcome,
    EventReader, SessionEventJournal, SessionPathResolver, SessionReader, SessionStore,
    StorageError, ToolResultArtifactInput, ToolResultArtifactStore,
    event_log::EventLog,
    test_support::{started_event, user_event},
};

#[test]
fn event_consumer_state_path_is_bounded_and_stable_for_long_ids() {
    let dir = std::path::Path::new("session");
    let consumer_id = "extension.".repeat(100);

    let first = event_consumer_state_path(dir, &consumer_id).unwrap();
    let second = event_consumer_state_path(dir, &consumer_id).unwrap();
    let expected_parent = dir.join("event-consumers");

    assert_eq!(first, second);
    assert_eq!(first.parent(), Some(expected_parent.as_path()));
    assert_eq!(first.file_name().unwrap().to_string_lossy().len(), 75);
}

#[tokio::test]
async fn event_consumer_state_persists_pause_and_rejects_stale_checkpoints() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("event-consumer-session");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());
    repo.create_session(started_event(&session_id))
        .await
        .unwrap();
    repo.append_event(user_event(&session_id, "event"))
        .await
        .unwrap();

    assert_eq!(
        repo.checkpoint_event_consumer(&session_id, "extension:subscription", 0, 1)
            .await
            .unwrap(),
        EventConsumerCheckpointOutcome::Accepted
    );
    repo.set_event_consumer_paused(&session_id, "extension:subscription", true)
        .await
        .unwrap();
    let reset = repo
        .reset_event_consumer_checkpoint(
            &session_id,
            "extension:subscription",
            EventConsumerCheckpointReset::Beginning,
        )
        .await
        .unwrap();
    assert!(reset.paused);
    assert_eq!(reset.checkpoint, None);
    assert_eq!(reset.revision, 1);
    drop(repo);

    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    assert_eq!(
        reopened
            .event_consumer_state(&session_id, "extension:subscription")
            .await
            .unwrap(),
        reset
    );
    assert_eq!(
        reopened
            .checkpoint_event_consumer(&session_id, "extension:subscription", 0, 1)
            .await
            .unwrap(),
        EventConsumerCheckpointOutcome::StaleRevision
    );
    let latest = reopened
        .reset_event_consumer_checkpoint(
            &session_id,
            "extension:subscription",
            EventConsumerCheckpointReset::StreamHead,
        )
        .await
        .unwrap();
    assert_eq!(latest.checkpoint, Some(1));
    assert_eq!(latest.revision, 2);
    assert_eq!(latest.skipped_count, 1);
    assert_eq!(latest.skips.len(), 1);

    let session_dir = reopened.find_session_dir(&session_id).await.unwrap();
    let state_path = event_consumer_state_path(&session_dir, "extension:subscription").unwrap();
    assert!(!state_path.with_extension("json.tmp").exists());

    let mut persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(persisted["version"], 3);
    assert_eq!(persisted["skippedCount"], 1);

    persisted["version"] = 2.into();
    std::fs::write(&state_path, serde_json::to_vec(&persisted).unwrap()).unwrap();
    assert!(matches!(
        reopened
            .event_consumer_state(&session_id, "extension:subscription")
            .await,
        Err(StorageError::CorruptLog(message))
            if message.contains("unsupported event consumer state version 2")
    ));
}

#[tokio::test]
async fn event_consumer_quarantines_once_at_the_failure_limit_and_persists_the_audit() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("event-consumer-quarantine");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());
    repo.create_session(started_event(&session_id))
        .await
        .unwrap();
    repo.append_event(user_event(&session_id, "event"))
        .await
        .unwrap();

    for attempt in 1..=20 {
        let outcome = repo
            .record_event_consumer_failure(
                &session_id,
                "extension:subscription:v1",
                0,
                1,
                "injected failure",
                20,
            )
            .await
            .unwrap();
        let expected = if attempt == 20 {
            EventConsumerFailureOutcome::Quarantined { attempts: 20 }
        } else {
            EventConsumerFailureOutcome::Recorded { attempts: attempt }
        };
        assert_eq!(outcome, expected);
    }
    assert_eq!(
        repo.record_event_consumer_failure(
            &session_id,
            "extension:subscription:v1",
            0,
            1,
            "injected failure",
            20,
        )
        .await
        .unwrap(),
        EventConsumerFailureOutcome::AlreadyConsumed
    );
    drop(repo);

    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    let state = reopened
        .event_consumer_state(&session_id, "extension:subscription:v1")
        .await
        .unwrap();
    assert_eq!(state.checkpoint, Some(1));
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(state.quarantined_count, 1);
    assert_eq!(state.quarantined.len(), 1);
    assert_eq!(state.quarantined[0].revision, 0);
    assert_eq!(state.quarantined[0].attempts, 20);
    assert_eq!(state.quarantined[0].last_error, "injected failure");
}

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
    assert_eq!(model.model_context.messages.len(), 2);
    assert_eq!(reopened.replay_events(&session_id).await.unwrap().len(), 3);
}

#[tokio::test]
async fn synced_append_stays_hidden_and_sticky_until_exact_retry_or_reopen() {
    let dir = tempdir().unwrap();
    let session_id = SessionId::new("uncertain-compact");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());
    repo.create_session(started_event(&session_id))
        .await
        .unwrap();
    repo.append_event(user_event(&session_id, "original"))
        .await
        .unwrap();
    repo.sync_durable_events(&session_id).await.unwrap();

    let before = repo.session_read_model(&session_id).await.unwrap();
    let rewrite = DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::TranscriptRewritten {
            source_seq: before.stats.last_seq,
            source_fingerprint: transcript_prefix_fingerprint(
                &before.system_prompt.text,
                &before
                    .model_context
                    .messages
                    .iter()
                    .map(|message| message.message.clone())
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            messages: vec![TranscriptMessage::plain(LlmMessage::user("summary"))],
            reason: TranscriptRewriteReason::Compaction(CompactionDetails {
                trigger: "manual".into(),
                pre_tokens: 10,
                post_tokens: 2,
                summary: "summary".into(),
                transcript_path: None,
                strategy: CompactStrategy::Manual {
                    keep_recent_turns: None,
                },
            }),
        },
    );
    let meta = repo.get_or_open_meta(&session_id).await.unwrap();
    meta.log.fail_next_sync().await.unwrap();

    let error = repo
        .append_events_and_sync(vec![rewrite])
        .await
        .unwrap_err();
    let pending_seq = error.uncertain_through_seq().unwrap();
    assert_eq!(pending_seq, 2);
    assert!(!error.is_retryable());
    let still_confirmed = repo.session_read_model(&session_id).await.unwrap();
    assert_eq!(still_confirmed.stats.last_seq, 1);
    assert_eq!(still_confirmed.first_user_message(), Some("original"));
    assert!(matches!(
        repo.ensure_no_uncertain_durability(&session_id).await,
        Err(StorageError::DurabilityUncertain { through_seq: 2, .. })
    ));
    assert!(matches!(
        repo.replay_events(&session_id).await,
        Err(StorageError::DurabilityUncertain { through_seq: 2, .. })
    ));
    assert!(matches!(
        repo.append_event(user_event(&session_id, "must be blocked"))
            .await,
        Err(StorageError::DurabilityUncertain { through_seq: 2, .. })
    ));
    assert!(matches!(
        repo.checkpoint_event_consumer(&session_id, "test-consumer", 0, 2)
            .await,
        Err(StorageError::DurabilityUncertain { through_seq: 2, .. })
    ));
    assert!(matches!(
        repo.retry_uncertain_sync(&session_id, pending_seq + 1)
            .await,
        Err(StorageError::InvalidEvent(_))
    ));

    let confirmed = repo
        .retry_uncertain_sync(&session_id, pending_seq)
        .await
        .unwrap();
    assert_eq!(confirmed.len(), 1);
    assert!(
        repo.retry_uncertain_sync(&session_id, pending_seq)
            .await
            .unwrap()
            .is_empty()
    );
    let compacted = repo.session_read_model(&session_id).await.unwrap();
    assert_eq!(compacted.stats.last_seq, 2);
    assert_eq!(compacted.model_context.messages.len(), 1);
    assert_eq!(
        compacted.model_context.messages[0]
            .message
            .joined_display_text("\n"),
        "summary"
    );

    meta.log.fail_next_sync().await.unwrap();
    let reopen_error = repo
        .append_events_and_sync(vec![user_event(&session_id, "survives reopen")])
        .await
        .unwrap_err();
    assert_eq!(reopen_error.uncertain_through_seq(), Some(3));
    let event_log_path = FileSystemSessionRepository::event_log_path(&meta.dir, &session_id);
    drop(meta);
    drop(repo);

    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    EventLog::fail_next_open_sync_for_testing(event_log_path.clone());
    assert!(matches!(
        reopened.list_session_summaries().await,
        Err(StorageError::Io(error)) if error.to_string().contains("injected fsync failure")
    ));
    EventLog::fail_next_open_sync_for_testing(event_log_path);
    assert!(matches!(
        reopened.session_read_model(&session_id).await,
        Err(StorageError::Io(error)) if error.to_string().contains("injected fsync failure")
    ));
    let recovered = reopened.session_read_model(&session_id).await.unwrap();
    assert_eq!(recovered.stats.last_seq, 3);
    assert_eq!(recovered.first_user_message(), Some("original"));
    assert_eq!(recovered.model_context.messages.len(), 2);
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
    let artifact_id = artifact.artifact_id;
    let slice = repo
        .read_tool_result_artifact(&session_id, &artifact_id, 0, 100)
        .await
        .unwrap();
    assert_eq!(slice.content, "artifact content");

    let error = repo
        .read_tool_result_artifact(&session_id, "../outside.txt", 0, 100)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::InvalidId(_)));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let artifact_dir = repo
            .session_store_dir(&session_id)
            .await
            .unwrap()
            .unwrap()
            .join("tool-results");
        let artifact_path = artifact_dir.join(&artifact_id);
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        std::fs::remove_file(&artifact_path).unwrap();
        symlink(&outside, &artifact_path).unwrap();

        let error = repo
            .read_tool_result_artifact(&session_id, &artifact_id, 0, 100)
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
    assert_eq!(model.model_context.messages.len(), 32);
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
async fn all_session_summaries_include_nested_lineage_while_catalog_stays_root_only() {
    let dir = tempdir().unwrap();
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());
    let root_id = SessionId::new("summary-root");
    let child_id = SessionId::new("summary-child");
    let grandchild_id = SessionId::new("summary-grandchild");
    let sibling_id = SessionId::new("summary-sibling");

    repo.create_session(started_event(&root_id)).await.unwrap();
    for (session_id, parent_id, extension) in [
        (&child_id, &root_id, "agent-a"),
        (&grandchild_id, &child_id, "agent-b"),
        (&sibling_id, &root_id, "agent-c"),
    ] {
        let mut event = started_event(session_id);
        let DurableEventPayload::SessionStarted(started) = &mut event.payload else {
            unreachable!("fixture must be SessionStarted");
        };
        started.parent = Some(ParentSessionRef {
            session_id: parent_id.clone(),
        });
        started.source_extension = Some(extension.into());
        repo.create_session(event).await.unwrap();
    }

    let catalog = repo.list_session_summaries().await.unwrap();
    assert_eq!(
        catalog
            .iter()
            .map(|summary| &summary.session_id)
            .collect::<Vec<_>>(),
        vec![&root_id]
    );
    let all = repo.list_all_session_summaries().await.unwrap();
    assert_eq!(
        all.iter()
            .map(|summary| summary.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "summary-child",
            "summary-grandchild",
            "summary-root",
            "summary-sibling",
        ]
    );

    drop(repo);
    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    assert_eq!(reopened.list_all_session_summaries().await.unwrap(), all);

    let invalid_base = dir.path().join("not-a-directory");
    let invalid_repo = FileSystemSessionRepository::with_projects_base(invalid_base.clone());
    std::fs::remove_dir(&invalid_base).unwrap();
    std::fs::write(&invalid_base, "occupied").unwrap();
    assert!(matches!(
        invalid_repo.list_all_sessions().await,
        Err(StorageError::Io(_))
    ));
}

#[tokio::test]
async fn session_listing_skips_unreadable_event_logs() {
    let dir = tempdir().unwrap();
    let good_id = SessionId::new("summary-good");
    let corrupt_id = SessionId::new("summary-corrupt");
    let repo = FileSystemSessionRepository::with_projects_base(dir.path().into());
    repo.create_session(started_event(&good_id)).await.unwrap();
    repo.append_event(user_event(&good_id, "visible"))
        .await
        .unwrap();
    repo.sync_durable_events(&good_id).await.unwrap();
    repo.create_session(started_event(&corrupt_id))
        .await
        .unwrap();
    repo.sync_durable_events(&corrupt_id).await.unwrap();

    let corrupt_dir = repo.find_session_dir(&corrupt_id).await.unwrap();
    drop(repo);

    // 模拟旧版本写入的日志：合法 JSON，但缺当前严格解码要求的字段。
    std::fs::write(
        corrupt_dir.join(format!("session-{corrupt_id}.jsonl")),
        "{\"seq\":0,\"id\":\"e01e3057-faa3-4cc0-9a05-fddbd136b8e9\"}\n",
    )
    .unwrap();

    let reopened = FileSystemSessionRepository::with_projects_base(dir.path().into());
    let catalog = reopened.list_session_summaries().await.unwrap();
    assert_eq!(
        catalog
            .iter()
            .map(|summary| &summary.session_id)
            .collect::<Vec<_>>(),
        vec![&good_id]
    );
    let all = reopened.list_all_session_summaries().await.unwrap();
    assert_eq!(
        all.iter()
            .map(|summary| &summary.session_id)
            .collect::<Vec<_>>(),
        vec![&good_id]
    );
    // 直接打开该会话仍按严格解码失败，不被列表的跳过掩盖。
    assert!(reopened.session_read_model(&corrupt_id).await.is_err());
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

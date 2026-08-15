//! Repository 门面实现:[`SessionStore`](consumer 状态、checkpoint、生命周期、compact 快照)。

use std::path::Path;

use astrcode_core::types::SessionId;
use chrono::Utc;
use uuid::Uuid;

use super::{
    FileSystemSessionRepository,
    consumer_state::{ConsumerStateEdit, event_consumer_state_path, read_event_consumer_state},
    validate_storage_session_id,
};
use crate::{
    CompactSnapshotInput, EventConsumerCheckpointOutcome, EventConsumerCheckpointReset,
    EventConsumerFailureOutcome, EventConsumerState, SessionEventJournal, SessionStore,
    StorageError,
};

impl FileSystemSessionRepository {
    /// 从内存缓存摘除位于 `dir` 子树下的所有会话元数据(含子 agent 会话)。
    async fn evict_cached_sessions_under(&self, dir: &Path) {
        self.sessions
            .write()
            .await
            .retain(|_, meta| !meta.dir.starts_with(dir));
    }
}

#[async_trait::async_trait]
impl SessionStore for FileSystemSessionRepository {
    async fn event_consumer_state(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
    ) -> Result<EventConsumerState, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let path = event_consumer_state_path(&meta.dir, consumer_id)?;
        read_event_consumer_state(&path).await
    }

    async fn checkpoint_event_consumer(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
    ) -> Result<EventConsumerCheckpointOutcome, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        meta.edit_consumer_state(
            session_id,
            consumer_id,
            Some(("checkpoint", seq)),
            |state, _| {
                if state.revision != expected_revision {
                    return Ok(ConsumerStateEdit::Unchanged(
                        EventConsumerCheckpointOutcome::StaleRevision,
                    ));
                }
                if state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq) {
                    return Ok(ConsumerStateEdit::Unchanged(
                        EventConsumerCheckpointOutcome::Accepted,
                    ));
                }
                state.checkpoint = Some(seq);
                state.consecutive_failures = 0;
                Ok(ConsumerStateEdit::Changed(
                    EventConsumerCheckpointOutcome::Accepted,
                ))
            },
        )
        .await
    }

    async fn record_event_consumer_failure(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
        error: &str,
        quarantine_after: u32,
    ) -> Result<EventConsumerFailureOutcome, StorageError> {
        if quarantine_after == 0 {
            return Err(StorageError::InvalidId(
                "event consumer quarantine limit must be greater than zero".into(),
            ));
        }
        let meta = self.get_or_open_meta(session_id).await?;
        meta.edit_consumer_state(
            session_id,
            consumer_id,
            Some(("failure seq", seq)),
            |state, _| {
                if state.revision != expected_revision {
                    return Ok(ConsumerStateEdit::Unchanged(
                        EventConsumerFailureOutcome::StaleRevision,
                    ));
                }
                if state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq) {
                    return Ok(ConsumerStateEdit::Unchanged(
                        EventConsumerFailureOutcome::AlreadyConsumed,
                    ));
                }
                state.consecutive_failures =
                    state.consecutive_failures.checked_add(1).ok_or_else(|| {
                        StorageError::CorruptLog("consumer failure count overflow".into())
                    })?;
                let attempts = state.consecutive_failures;
                if attempts >= quarantine_after {
                    state.record_quarantine(seq, attempts, error)?;
                    state.checkpoint = Some(seq);
                    state.consecutive_failures = 0;
                    Ok(ConsumerStateEdit::Changed(
                        EventConsumerFailureOutcome::Quarantined { attempts },
                    ))
                } else {
                    Ok(ConsumerStateEdit::Changed(
                        EventConsumerFailureOutcome::Recorded { attempts },
                    ))
                }
            },
        )
        .await
    }

    async fn set_event_consumer_paused(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        paused: bool,
    ) -> Result<EventConsumerState, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        meta.edit_consumer_state(session_id, consumer_id, None, |state, _| {
            if state.paused == paused {
                return Ok(ConsumerStateEdit::Unchanged(state.clone()));
            }
            state.paused = paused;
            Ok(ConsumerStateEdit::Changed(state.clone()))
        })
        .await
    }

    async fn reset_event_consumer_checkpoint(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        reset: EventConsumerCheckpointReset,
    ) -> Result<EventConsumerState, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        meta.edit_consumer_state(session_id, consumer_id, None, |state, event_count| {
            let previous_checkpoint = state.checkpoint;
            state.checkpoint = match reset {
                EventConsumerCheckpointReset::Beginning => None,
                EventConsumerCheckpointReset::StreamHead => event_count.checked_sub(1),
            };
            state.revision = state.revision.checked_add(1).ok_or_else(|| {
                StorageError::CorruptLog("event consumer revision overflow".into())
            })?;
            state.consecutive_failures = 0;
            if reset == EventConsumerCheckpointReset::StreamHead
                && state.checkpoint != previous_checkpoint
            {
                state.record_skip(previous_checkpoint, state.checkpoint)?;
            }
            Ok(ConsumerStateEdit::Changed(state.clone()))
        })
        .await
    }

    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.ensure_no_uncertain_durability(session_id).await
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_storage_session_id(session_id)?;

        if let Some(dir) = self.find_session_dir(session_id).await {
            self.evict_cached_sessions_under(&dir).await;
            tokio::fs::remove_dir_all(&dir).await?;
        } else {
            self.sessions.write().await.remove(session_id);
        }
        Ok(())
    }

    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_storage_session_id(session_id)?;

        let dir = self
            .find_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;

        self.evict_cached_sessions_under(&dir).await;

        // 结构：subagents/{extension}/{child_id}/
        // 回收到：subagents/.recycled/{extension}/{child_id}/
        // 这样 restore 时能直接 rename 回原位。
        let extension_dir = dir.parent(); // subagents/{extension}/
        let subagents_dir = extension_dir.and_then(|p| p.parent()); // subagents/

        if let (Some(extension_dir), Some(subagents_dir)) = (extension_dir, subagents_dir)
            && subagents_dir.file_name().is_some_and(|n| n == "subagents")
        {
            let extension_name = extension_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let recycled = subagents_dir
                .join(".recycled")
                .join(extension_name.as_ref());
            tokio::fs::create_dir_all(&recycled).await?;
            let dir_name = dir
                .file_name()
                .ok_or_else(|| StorageError::InvalidId("unexpected session dir path".into()))?;
            let dest = recycled.join(dir_name);
            tokio::fs::rename(&dir, &dest).await?;
            return Ok(());
        }

        // 非子 session 或非标准目录结构，回退到删除
        tokio::fs::remove_dir_all(&dir).await?;
        Ok(())
    }

    async fn restore_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_storage_session_id(session_id)?;

        // 在所有 .recycled/{extension}/{session_id} 目录中搜索
        let recycled_path = self
            .find_recycled_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;

        // 结构：subagents/.recycled/{extension}/{session_id}
        // 还原到：subagents/{extension}/{session_id}
        let extension_dir = recycled_path
            .parent() // .recycled/{extension}/
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?;
        let extension_name = extension_dir
            .file_name()
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?
            .to_string_lossy()
            .to_string();
        let recycled_root = extension_dir
            .parent() // .recycled/
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?;
        let subagents_dir = recycled_root
            .parent() // subagents/
            .ok_or_else(|| StorageError::InvalidId("unexpected recycled path".into()))?;

        let dest_parent = subagents_dir.join(&extension_name);
        tokio::fs::create_dir_all(&dest_parent).await?;
        let dest = dest_parent.join(session_id.as_str());

        tokio::fs::rename(&recycled_path, &dest).await?;

        Ok(())
    }

    async fn write_compact_snapshot(
        &self,
        session_id: &SessionId,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;

        let dir = meta.dir.join("compact-snapshots");
        tokio::fs::create_dir_all(&dir).await?;

        let created_at = Utc::now();
        let path = dir.join(format!(
            "compact-{}-{}.jsonl",
            created_at.timestamp_millis(),
            Uuid::new_v4()
        ));

        let mut lines = Vec::with_capacity(snapshot.provider_messages.len() + 1);
        lines.push(
            serde_json::json!({
                "type": "metadata",
                "session_id": session_id,
                "trigger": snapshot.trigger,
                "created_at": created_at.to_rfc3339(),
                "model_id": snapshot.model_id,
                "working_dir": snapshot.working_dir,
                "system_prompt": snapshot.system_prompt,
                "message_count": snapshot.provider_messages.len(),
            })
            .to_string(),
        );
        for (index, message) in snapshot.provider_messages.into_iter().enumerate() {
            lines.push(
                serde_json::json!({
                    "type": "message",
                    "index": index,
                    "message": message,
                })
                .to_string(),
            );
        }

        let mut content = lines.join("\n");
        content.push('\n');
        tokio::fs::write(&path, content).await?;

        Ok(Some(path.to_string_lossy().to_string()))
    }
}

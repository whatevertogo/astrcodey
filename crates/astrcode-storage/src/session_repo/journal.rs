//! 写端口实现:[`SessionEventJournal`]。

use std::sync::Arc;

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, StoredEvent},
    types::SessionId,
};
use astrcode_session_projection::replay;

use super::{
    FileSystemSessionRepository, SessionMeta, append_session_id,
    durability::{
        self, FSYNC_CONFIRMATION_PENDING, UncertainDurability, ensure_log_positioned_for,
    },
    invalid_event,
    owner_lease::SessionOwnerLease,
    validate_storage_session_id,
};
use crate::{SessionEventJournal, StorageError, event_log::EventLog, snapshot::SnapshotManager};

#[async_trait::async_trait]
impl SessionEventJournal for FileSystemSessionRepository {
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        validate_storage_session_id(&event.session_id)?;
        if event.turn_id.is_some() {
            return Err(StorageError::InvalidId(
                "SessionStarted must be a session-level event".into(),
            ));
        }
        let DurableEventPayload::SessionStarted(started) = &event.payload else {
            return Err(StorageError::InvalidId(
                "create_session requires SessionStarted".into(),
            ));
        };
        let session_id = event.session_id.clone();
        let parent_session_id = started.parent.as_ref().map(|parent| &parent.session_id);

        let dir = self
            .new_session_dir(
                &session_id,
                &started.working_dir,
                parent_session_id,
                started.source_extension.as_deref(),
            )
            .await?;
        tokio::fs::create_dir_all(&dir).await?;
        let owner_lease = SessionOwnerLease::acquire(&dir, &self.owner).await?;

        let (log, stored_event) =
            EventLog::create(Self::event_log_path(&dir, &session_id), event).await?;

        let projection = replay(session_id.clone(), std::slice::from_ref(&stored_event))
            .map_err(invalid_event)?;

        let snapshot_mgr = SnapshotManager::new(dir.join("snapshots"));
        let meta = SessionMeta::new(dir, owner_lease, log, snapshot_mgr, projection).await?;
        self.sessions
            .write()
            .await
            .insert(session_id, Arc::new(meta));

        Ok(stored_event)
    }

    async fn append_events(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let session_id = append_session_id(&events)?;
        let meta = self.get_or_open_meta(&session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(&session_id).await?;
        let prepared = meta.projection.prepare_batch(events).await?;
        ensure_log_positioned_for(&meta, &prepared).await?;
        let committed = meta.log.append_prepared_batch(prepared).await?;
        let stored = meta.projection.apply_committed(committed).await;
        Ok(stored)
    }

    async fn append_events_and_sync(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let session_id = append_session_id(&events)?;
        let meta = self.get_or_open_meta(&session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(&session_id).await?;
        let prepared = meta.projection.prepare_batch(events).await?;
        ensure_log_positioned_for(&meta, &prepared).await?;

        // 只在 append 成功后登记 marker:append 失败意味着没有字节被提交
        // (EventLog 会回滚部分写入),无需 durability marker。commit lane 由
        // 本次调用全程持有,marker 的登记与确认不会被其他写者插入。
        let committed = meta.log.append_prepared_batch(prepared).await?;
        let through_seq = committed
            .events()
            .last()
            .map(|event| event.seq)
            .ok_or_else(crate::error::short_batch_result)?;
        durability::set_uncertain(
            &meta,
            UncertainDurability::PendingProjection {
                batch: committed,
                through_seq,
                reason: FSYNC_CONFIRMATION_PENDING.into(),
            },
        );
        if let Err(error) = meta.log.force_sync().await {
            return Err(durability::fail_uncertain(
                &meta,
                &session_id,
                through_seq,
                error.to_string(),
            ));
        }

        durability::apply_confirmed_projection(&meta, through_seq).await
    }

    async fn sync_durable_events(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let next_seq = meta.log.count();
        let through_seq = next_seq.checked_sub(1).ok_or_else(|| {
            StorageError::CorruptLog("created session has an empty event log".into())
        })?;
        durability::set_uncertain(
            &meta,
            UncertainDurability::Published {
                through_seq,
                reason: FSYNC_CONFIRMATION_PENDING.into(),
            },
        );

        if let Err(error) = meta.log.force_sync().await {
            return Err(durability::fail_uncertain(
                &meta,
                session_id,
                through_seq,
                error.to_string(),
            ));
        }
        durability::take_uncertain(&meta, through_seq)?;
        Ok(())
    }

    async fn retry_uncertain_sync(
        &self,
        session_id: &SessionId,
        expected_through_seq: u64,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_commit_lane().await?;
        match durability::pending_through_seq(&meta) {
            Some(actual_through_seq) => {
                if actual_through_seq != expected_through_seq {
                    return Err(durability::uncertain_seq_mismatch(
                        expected_through_seq,
                        actual_through_seq,
                    ));
                }
            },
            None => {
                let log_next_seq = meta.log.count();
                let projection = meta.projection.snapshot().await;
                let expected_next_seq = expected_through_seq
                    .checked_add(1)
                    .ok_or_else(durability::sequence_overflow)?;
                if log_next_seq == expected_next_seq
                    && projection.stats.last_seq == expected_through_seq
                {
                    return Ok(Vec::new());
                }
                return Err(StorageError::InvalidEvent(format!(
                    "session {session_id} has no pending durability confirmation for seq \
                     {expected_through_seq}"
                )));
            },
        }

        if let Err(error) = meta.log.force_sync().await {
            return Err(durability::fail_uncertain(
                &meta,
                session_id,
                expected_through_seq,
                error.to_string(),
            ));
        }

        durability::apply_confirmed_projection(&meta, expected_through_seq).await
    }

    async fn ensure_no_uncertain_durability(
        &self,
        session_id: &SessionId,
    ) -> Result<(), StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        Ok(())
    }
}

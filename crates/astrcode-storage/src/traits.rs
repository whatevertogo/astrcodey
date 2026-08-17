use std::{path::PathBuf, sync::Arc};

use astrcode_core::{
    event::{DurableEvent, StoredEvent},
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId},
};
use astrcode_session_projection::{AgentSessionLinkView, SessionReadModel, SessionSummary};
use serde::{Deserialize, Serialize};

use crate::{CompactSnapshotInput, StorageError, ToolResultArtifactInput, ToolResultArtifactRef};

pub(crate) const EVENT_CONSUMER_AUDIT_RECORD_LIMIT: usize = 128;
const EVENT_CONSUMER_ERROR_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventConsumerState {
    pub checkpoint: Option<u64>,
    pub paused: bool,
    pub revision: u64,
    pub consecutive_failures: u32,
    pub quarantined_count: u64,
    pub skipped_count: u64,
    pub quarantined: Vec<EventConsumerQuarantine>,
    pub skips: Vec<EventConsumerSkip>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventConsumerQuarantine {
    pub seq: u64,
    pub revision: u64,
    pub attempts: u32,
    pub last_error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventConsumerSkip {
    pub from_seq: Option<u64>,
    pub to_seq: Option<u64>,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventConsumerCheckpointReset {
    Beginning,
    StreamHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventConsumerCheckpointOutcome {
    Accepted,
    StaleRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventConsumerFailureOutcome {
    Recorded { attempts: u32 },
    Quarantined { attempts: u32 },
    AlreadyConsumed,
    StaleRevision,
}

impl EventConsumerState {
    pub(crate) fn record_quarantine(
        &mut self,
        seq: u64,
        attempts: u32,
        error: &str,
    ) -> Result<(), StorageError> {
        if self
            .quarantined
            .iter()
            .any(|record| record.seq == seq && record.revision == self.revision)
        {
            return Ok(());
        }
        self.quarantined_count = self
            .quarantined_count
            .checked_add(1)
            .ok_or_else(|| StorageError::CorruptLog("consumer quarantine count overflow".into()))?;
        push_recent(
            &mut self.quarantined,
            EventConsumerQuarantine {
                seq,
                revision: self.revision,
                attempts,
                last_error: truncate_utf8(error, EVENT_CONSUMER_ERROR_MAX_BYTES),
            },
        );
        Ok(())
    }

    pub(crate) fn record_skip(
        &mut self,
        from_seq: Option<u64>,
        to_seq: Option<u64>,
    ) -> Result<(), StorageError> {
        self.skipped_count = self
            .skipped_count
            .checked_add(1)
            .ok_or_else(|| StorageError::CorruptLog("consumer skip count overflow".into()))?;
        push_recent(
            &mut self.skips,
            EventConsumerSkip {
                from_seq,
                to_seq,
                revision: self.revision,
            },
        );
        Ok(())
    }

    pub(crate) fn validate_audit_bounds(&self) -> Result<(), StorageError> {
        if self.quarantined.len() > EVENT_CONSUMER_AUDIT_RECORD_LIMIT {
            return Err(StorageError::CorruptLog(
                "consumer quarantine audit exceeds its persisted record limit".into(),
            ));
        }
        if self.skips.len() > EVENT_CONSUMER_AUDIT_RECORD_LIMIT {
            return Err(StorageError::CorruptLog(
                "consumer skip audit exceeds its persisted record limit".into(),
            ));
        }
        let quarantined_records = u64::try_from(self.quarantined.len()).map_err(|_| {
            StorageError::CorruptLog("consumer quarantine audit is too large".into())
        })?;
        let skip_records = u64::try_from(self.skips.len())
            .map_err(|_| StorageError::CorruptLog("consumer skip audit is too large".into()))?;
        if self.quarantined_count < quarantined_records {
            return Err(StorageError::CorruptLog(
                "consumer quarantine count is smaller than its persisted audit".into(),
            ));
        }
        if self.skipped_count < skip_records {
            return Err(StorageError::CorruptLog(
                "consumer skip count is smaller than its persisted audit".into(),
            ));
        }
        if self
            .quarantined
            .iter()
            .any(|record| record.last_error.len() > EVENT_CONSUMER_ERROR_MAX_BYTES)
        {
            return Err(StorageError::CorruptLog(
                "consumer quarantine error exceeds its persisted byte limit".into(),
            ));
        }
        Ok(())
    }
}

fn push_recent<T>(records: &mut Vec<T>, record: T) {
    if records.len() == EVENT_CONSUMER_AUDIT_RECORD_LIMIT {
        records.remove(0);
    }
    records.push(record);
}

/// 校验 event consumer id 非空；磁盘与内存存储实现共用。
pub(crate) fn validate_event_consumer_id(consumer_id: &str) -> Result<(), StorageError> {
    if consumer_id.is_empty() {
        return Err(StorageError::InvalidId(
            "event consumer id cannot be empty".into(),
        ));
    }
    Ok(())
}

/// 按字节上限截断字符串，回退到最后一个 UTF-8 字符边界。
///
/// 供消费方审计记录与 event log 解析预览共用。
pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[async_trait::async_trait]
pub trait EventReader: Send + Sync {
    async fn replay_events(&self, session_id: &SessionId)
    -> Result<Vec<StoredEvent>, StorageError>;

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError>;

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<StoredEvent>, StorageError>;

    async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let mut events = self.replay_from(session_id, cursor).await?;
        events.truncate(max_events);
        Ok(events)
    }

    /// Replays the first page, including the event at sequence zero.
    async fn replay_from_start_limited(
        &self,
        session_id: &SessionId,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let mut events = self.replay_events(session_id).await?;
        events.truncate(max_events);
        Ok(events)
    }

    /// Replays the newest events whose sequence is strictly less than `before`.
    ///
    /// The returned events remain in ascending sequence order. `None` selects
    /// the newest page of the session.
    async fn replay_before_limited(
        &self,
        session_id: &SessionId,
        before: Option<&Cursor>,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let before_seq = before
            .map(|cursor| {
                cursor
                    .parse::<u64>()
                    .map_err(|_| StorageError::InvalidId(format!("Invalid cursor: {cursor}")))
            })
            .transpose()?;
        let mut events = self.replay_events(session_id).await?;
        if let Some(before_seq) = before_seq {
            events.retain(|event| event.seq < before_seq);
        }
        let keep_from = events.len().saturating_sub(max_events);
        Ok(events.split_off(keep_from))
    }

    async fn replay_events_active_or_recycled(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.replay_events(session_id).await
    }

    async fn replay_from_active_or_recycled_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.replay_from_limited(session_id, cursor, max_events)
            .await
    }

    async fn replay_from_start_active_or_recycled_limited(
        &self,
        session_id: &SessionId,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.replay_from_start_limited(session_id, max_events).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError>;

    /// Lists every active session, including nested subagent sessions.
    async fn list_all_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        self.list_sessions().await
    }
}

#[async_trait::async_trait]
pub trait SessionReader: Send + Sync {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError>;

    /// Rebuild a recycled session without making it active or registering runtime state.
    async fn recycled_session_read_model(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        Err(StorageError::Unsupported(
            "reading recycled sessions is not supported by this storage implementation".into(),
        ))
    }

    /// Read the active read model, falling back to the recycled record when no active
    /// session exists for `session_id`. Only `NotFound` triggers the fallback; other
    /// errors propagate unchanged.
    async fn session_read_model_active_or_recycled(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        match self.session_read_model(session_id).await {
            Ok(model) => Ok(model),
            Err(StorageError::NotFound(_)) => self.recycled_session_read_model(session_id).await,
            Err(error) => Err(error),
        }
    }

    async fn session_has_messages(&self, session_id: &SessionId) -> Result<bool, StorageError> {
        Ok(self.session_read_model(session_id).await?.has_messages())
    }

    async fn session_agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AgentSessionLinkView>, StorageError> {
        Ok(self
            .session_read_model(session_id)
            .await?
            .agent_sessions
            .clone())
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError>;

    /// Lists every active session, including nested subagent sessions.
    ///
    /// User-facing catalog APIs may intentionally use [`Self::list_session_summaries`]
    /// to show roots only. Lineage and administrative APIs need this complete view.
    async fn list_all_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        self.list_session_summaries().await
    }
}

#[async_trait::async_trait]
pub trait SessionPathResolver: Send + Sync {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, StorageError>;

    async fn planned_session_store_dir(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        parent_session_id: Option<&SessionId>,
        source_extension: Option<&str>,
    ) -> Result<Option<PathBuf>, StorageError>;
}

#[async_trait::async_trait]
pub trait ToolResultArtifactStore: Send + Sync {
    async fn read_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
        byte_offset: usize,
        max_bytes: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError>;

    async fn write_tool_result_artifact(
        &self,
        _session_id: &SessionId,
        _artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        Err(StorageError::Unsupported(
            "tool result artifact storage is not supported".into(),
        ))
    }
}

/// Durable session event 的存储写端口。
#[async_trait::async_trait]
pub trait SessionEventJournal: Send + Sync {
    /// 原子创建 session 并提交 seq=0 的 `SessionStarted`。
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError>;

    /// Atomically append one or more consecutive events from the same session.
    ///
    /// Implementations commit the whole batch as one recoverable append before advancing their
    /// projection. Sequence numbers are assigned in input order, and the returned batch has
    /// exactly one stored event per input event.
    async fn append_events(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError>;

    /// Append a batch and fsync it before making it visible through the read model.
    ///
    /// Implementations with an in-memory projection must not apply the batch until fsync
    /// succeeds. If fsync returns an ambiguous failure after the write, they must retain that
    /// exact batch for [`Self::retry_uncertain_sync`] and reject later mutations.
    ///
    /// Default: delegates to [`Self::append_events`], for implementations without a durability
    /// barrier (mirroring the [`Self::sync_durable_events`] default).
    async fn append_events_and_sync(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.append_events(events).await
    }

    async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        self.append_events(vec![event])
            .await?
            .pop()
            .ok_or_else(crate::error::short_batch_result)
    }

    async fn sync_durable_events(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Ok(())
    }

    /// Retry fsync for the exact sequence boundary left uncertain by a prior durable operation.
    ///
    /// Newly confirmed records are returned so the ordered event sink can publish them after
    /// durability confirmation. An empty result means no unpublished batch remains.
    ///
    /// Default: `Ok(Vec::new())`, for implementations that never report uncertain durability.
    async fn retry_uncertain_sync(
        &self,
        _session_id: &SessionId,
        _expected_through_seq: u64,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        Ok(Vec::new())
    }

    /// Reject operation admission while this process owns an ambiguous fsync result.
    ///
    /// Default: `Ok(())`, for implementations that never report uncertain durability.
    async fn ensure_no_uncertain_durability(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), StorageError> {
        Ok(())
    }
}

/// 完整 session repository 的组合入口。
///
/// 独立调用方使用上面的 reader/journal 窄端口；生命周期、checkpoint 和辅助快照只由
/// repository 门面使用，不再为它们各建一个单实现 trait。
#[async_trait::async_trait]
pub trait SessionStore:
    EventReader + SessionEventJournal + SessionReader + SessionPathResolver + ToolResultArtifactStore
{
    async fn event_consumer_state(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
    ) -> Result<EventConsumerState, StorageError>;

    /// Monotonically advances an at-least-once event consumer checkpoint.
    async fn checkpoint_event_consumer(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
    ) -> Result<EventConsumerCheckpointOutcome, StorageError>;

    /// Records a failed delivery and atomically quarantines plus checkpoints it at the limit.
    async fn record_event_consumer_failure(
        &self,
        _session_id: &SessionId,
        _consumer_id: &str,
        _expected_revision: u64,
        _seq: u64,
        _error: &str,
        _quarantine_after: u32,
    ) -> Result<EventConsumerFailureOutcome, StorageError> {
        Err(StorageError::Unsupported(
            "event consumer quarantine is not supported by this store".into(),
        ))
    }

    async fn set_event_consumer_paused(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        paused: bool,
    ) -> Result<EventConsumerState, StorageError>;

    async fn reset_event_consumer_checkpoint(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        reset: EventConsumerCheckpointReset,
    ) -> Result<EventConsumerState, StorageError>;

    async fn open_session(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Ok(())
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError>;

    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        tracing::warn!(
            session_id = %session_id,
            "SessionStore::recycle_session fell back to delete_session; this storage implementation does not preserve recycled session data"
        );
        self.delete_session(session_id).await
    }

    async fn restore_session(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Err(StorageError::Unsupported(
            "restore_session is not supported by this storage implementation".into(),
        ))
    }

    async fn write_compact_snapshot(
        &self,
        _session_id: &SessionId,
        _snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, StorageError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_audit_keeps_totals_and_bounded_recent_records() {
        let mut state = EventConsumerState::default();
        let record_count = EVENT_CONSUMER_AUDIT_RECORD_LIMIT + 2;
        let error = "故".repeat(EVENT_CONSUMER_ERROR_MAX_BYTES);

        for index in 0..record_count {
            state.revision = index as u64;
            state.record_quarantine(index as u64, 20, &error).unwrap();
            state
                .record_skip(
                    index.checked_sub(1).map(|seq| seq as u64),
                    Some(index as u64),
                )
                .unwrap();
        }

        assert_eq!(state.quarantined_count, record_count as u64);
        assert_eq!(state.skipped_count, record_count as u64);
        assert_eq!(state.quarantined.len(), EVENT_CONSUMER_AUDIT_RECORD_LIMIT);
        assert_eq!(state.skips.len(), EVENT_CONSUMER_AUDIT_RECORD_LIMIT);
        assert_eq!(state.quarantined[0].seq, 2);
        assert!(
            state
                .quarantined
                .iter()
                .all(|record| record.last_error.len() <= EVENT_CONSUMER_ERROR_MAX_BYTES)
        );
        state.validate_audit_bounds().unwrap();

        state.quarantined_count = 0;
        assert!(matches!(
            state.validate_audit_bounds(),
            Err(StorageError::CorruptLog(message))
                if message.contains("quarantine count")
        ));
    }
}

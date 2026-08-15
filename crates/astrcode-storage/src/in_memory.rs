//! InMemoryEventStore — 纯内存事件存储和投影，仅用于测试。
//!
//! 通过 crate feature `testing` 暴露给跨 crate 集成测试；本 crate 内单元测试也可启用该 feature。
//! 不要用 `#[cfg(test)]` 单独 gating：否则 `astrcode-server` 等集成测试无法链接此模块。

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, StoredEvent},
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId},
};
use astrcode_session_projection::{
    AgentSessionLinkView, PreparedProjectionBatch, SessionReadModel, SessionReadModelProjection,
    SessionSummary,
};
use tokio::sync::Mutex;

use crate::{
    CompactSnapshotInput, EventConsumerCheckpointOutcome, EventConsumerCheckpointReset,
    EventConsumerFailureOutcome, EventConsumerState, EventReader, SessionEventJournal,
    SessionPathResolver, SessionReader, SessionStore, StorageError, ToolResultArtifactInput,
    ToolResultArtifactRef, ToolResultArtifactStore,
    tool_artifacts::{slice_tool_result_content, tool_result_artifact_id},
};

/// 纯内存 session persistence 实现。
///
/// 这个类型维护完整事件列表和同步投影，因此不是 no-op；测试使用它能覆盖
/// 文件系统存储相同的读模型语义。
#[derive(Default)]
pub struct InMemoryEventStore {
    sessions: Mutex<HashMap<SessionId, InMemorySession>>,
    fail_next_sync: AtomicBool,
    sync_count: AtomicUsize,
}

struct InMemorySession {
    events: Vec<StoredEvent>,
    projection: SessionReadModel,
    tool_results: HashMap<String, String>,
    event_consumers: HashMap<String, EventConsumerState>,
}

fn validate_event_consumer_id(consumer_id: &str) -> Result<(), StorageError> {
    if consumer_id.is_empty() {
        return Err(StorageError::InvalidId(
            "event consumer id cannot be empty".into(),
        ));
    }
    Ok(())
}

impl InMemoryEventStore {
    /// 创建新的空内存存储。
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next_sync(&self) {
        self.fail_next_sync.store(true, Ordering::Release);
    }

    pub fn sync_count(&self) -> usize {
        self.sync_count.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl EventReader for InMemoryEventStore {
    async fn replay_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .map(|session| session.events.clone())
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .map(|session| Some(session.projection.cursor()))
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let events = self.replay_events(session_id).await?;
        let Ok(seq) = cursor.parse::<u64>() else {
            return Err(StorageError::InvalidId(format!("Invalid cursor: {cursor}")));
        };
        Ok(events.into_iter().filter(|event| event.seq > seq).collect())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        Ok(self.sessions.lock().await.keys().cloned().collect())
    }
}

#[async_trait::async_trait]
impl SessionReader for InMemoryEventStore {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .map(|session| Arc::new(session.projection.clone()))
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn session_has_messages(&self, session_id: &SessionId) -> Result<bool, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .map(|session| session.projection.has_messages())
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn session_agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AgentSessionLinkView>, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .map(|session| session.projection.agent_sessions.clone())
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let mut summaries = self
            .sessions
            .lock()
            .await
            .values()
            .map(|session| session.projection.to_summary())
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(summaries)
    }
}

#[async_trait::async_trait]
impl ToolResultArtifactStore for InMemoryEventStore {
    async fn read_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
        byte_offset: usize,
        max_bytes: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        crate::tool_artifacts::validate_tool_result_artifact_id(artifact_id)
            .map_err(|message| StorageError::InvalidId(message.into()))?;
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let content = session
            .tool_results
            .get(artifact_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        slice_tool_result_content(artifact_id, content, byte_offset, max_bytes)
            .map_err(StorageError::Io)
    }

    async fn write_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        for suffix in 0..1000 {
            let artifact_id =
                tool_result_artifact_id(&artifact.tool_name, &artifact.call_id, suffix);
            match session.tool_results.get(&artifact_id) {
                Some(existing) if existing == &artifact.content => {
                    return Ok(ToolResultArtifactRef {
                        bytes: artifact.content.len(),
                        artifact_id,
                    });
                },
                Some(_) => continue,
                None => {
                    session
                        .tool_results
                        .insert(artifact_id.clone(), artifact.content.clone());
                    return Ok(ToolResultArtifactRef {
                        bytes: artifact.content.len(),
                        artifact_id,
                    });
                },
            }
        }
        Err(StorageError::InvalidId(
            "too many tool result artifact filename collisions".into(),
        ))
    }
}

#[async_trait::async_trait]
impl SessionPathResolver for InMemoryEventStore {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<std::path::PathBuf>, StorageError> {
        let map = self.sessions.lock().await;
        if map.contains_key(session_id) {
            Ok(None)
        } else {
            Err(StorageError::NotFound(session_id.clone()))
        }
    }

    async fn planned_session_store_dir(
        &self,
        _session_id: &SessionId,
        _working_dir: &str,
        _parent_session_id: Option<&SessionId>,
        _source_extension: Option<&str>,
    ) -> Result<Option<std::path::PathBuf>, StorageError> {
        Ok(None)
    }
}

#[async_trait::async_trait]
impl SessionEventJournal for InMemoryEventStore {
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        if event.turn_id.is_some()
            || !matches!(&event.payload, DurableEventPayload::SessionStarted(_))
        {
            return Err(StorageError::InvalidId(
                "create_session requires a session-level SessionStarted event".into(),
            ));
        }
        let session_id = event.session_id.clone();
        let stored = StoredEvent::new(0, event);
        let mut projection = SessionReadModelProjection::new(session_id.clone());
        projection
            .apply(&stored)
            .map_err(|error| StorageError::InvalidEvent(error.to_string()))?;
        let projection = projection
            .snapshot()
            .map_err(|error| StorageError::InvalidEvent(error.to_string()))?;
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id) {
            return Err(StorageError::AlreadyExists(session_id));
        }
        sessions.insert(
            session_id,
            InMemorySession {
                events: vec![stored.clone()],
                projection,
                tool_results: HashMap::new(),
                event_consumers: HashMap::new(),
            },
        );
        Ok(stored)
    }

    async fn append_events(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let session_id = events
            .first()
            .map(|event| event.session_id.clone())
            .ok_or_else(|| StorageError::InvalidEvent("event batch cannot be empty".into()))?;
        let mut map = self.sessions.lock().await;
        let session = map
            .get_mut(&session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let prepared = PreparedProjectionBatch::prepare(&session.projection, events)
            .map_err(|error| StorageError::InvalidEvent(error.to_string()))?;
        let stored_events = prepared.apply_to_model(&mut session.projection);
        session.events.extend(stored_events.iter().cloned());
        Ok(stored_events)
    }

    async fn sync_durable_events(&self, session_id: &SessionId) -> Result<(), StorageError> {
        if !self.sessions.lock().await.contains_key(session_id) {
            return Err(StorageError::NotFound(session_id.clone()));
        }
        self.sync_count.fetch_add(1, Ordering::AcqRel);
        if self.fail_next_sync.swap(false, Ordering::AcqRel) {
            return Err(StorageError::Io(std::io::Error::other(
                "injected durable sync failure",
            )));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionStore for InMemoryEventStore {
    async fn event_consumer_state(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
    ) -> Result<EventConsumerState, StorageError> {
        validate_event_consumer_id(consumer_id)?;
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        Ok(session
            .event_consumers
            .get(consumer_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn checkpoint_event_consumer(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
    ) -> Result<EventConsumerCheckpointOutcome, StorageError> {
        validate_event_consumer_id(consumer_id)?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let event_count = u64::try_from(session.events.len()).map_err(|_| {
            StorageError::CorruptLog("session event count exceeds consumer checkpoint range".into())
        })?;
        if seq >= event_count {
            return Err(StorageError::InvalidId(format!(
                "event consumer checkpoint {seq} is beyond the session event log"
            )));
        }
        let state = session
            .event_consumers
            .entry(consumer_id.to_owned())
            .or_default();
        if state.revision != expected_revision {
            return Ok(EventConsumerCheckpointOutcome::StaleRevision);
        }
        if state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq) {
            return Ok(EventConsumerCheckpointOutcome::Accepted);
        }
        state.checkpoint = Some(seq);
        state.consecutive_failures = 0;
        Ok(EventConsumerCheckpointOutcome::Accepted)
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
        validate_event_consumer_id(consumer_id)?;
        if quarantine_after == 0 {
            return Err(StorageError::InvalidId(
                "event consumer quarantine limit must be greater than zero".into(),
            ));
        }
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        if seq >= session.events.len() as u64 {
            return Err(StorageError::InvalidId(format!(
                "event consumer failure seq {seq} is beyond the session event log"
            )));
        }
        let state = session
            .event_consumers
            .entry(consumer_id.to_owned())
            .or_default();
        if state.revision != expected_revision {
            return Ok(EventConsumerFailureOutcome::StaleRevision);
        }
        if state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq) {
            return Ok(EventConsumerFailureOutcome::AlreadyConsumed);
        }
        state.consecutive_failures = state
            .consecutive_failures
            .checked_add(1)
            .ok_or_else(|| StorageError::CorruptLog("consumer failure count overflow".into()))?;
        let attempts = state.consecutive_failures;
        if attempts >= quarantine_after {
            state.record_quarantine(seq, attempts, error)?;
            state.checkpoint = Some(seq);
            state.consecutive_failures = 0;
            Ok(EventConsumerFailureOutcome::Quarantined { attempts })
        } else {
            Ok(EventConsumerFailureOutcome::Recorded { attempts })
        }
    }

    async fn set_event_consumer_paused(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        paused: bool,
    ) -> Result<EventConsumerState, StorageError> {
        validate_event_consumer_id(consumer_id)?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let state = session
            .event_consumers
            .entry(consumer_id.to_owned())
            .or_default();
        state.paused = paused;
        Ok(state.clone())
    }

    async fn reset_event_consumer_checkpoint(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        reset: EventConsumerCheckpointReset,
    ) -> Result<EventConsumerState, StorageError> {
        validate_event_consumer_id(consumer_id)?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let latest = session.events.last().map(|event| event.seq);
        let state = session
            .event_consumers
            .entry(consumer_id.to_owned())
            .or_default();
        let previous_checkpoint = state.checkpoint;
        state.checkpoint = match reset {
            EventConsumerCheckpointReset::Beginning => None,
            EventConsumerCheckpointReset::StreamHead => latest,
        };
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| StorageError::CorruptLog("event consumer revision overflow".into()))?;
        state.consecutive_failures = 0;
        if reset == EventConsumerCheckpointReset::StreamHead
            && state.checkpoint != previous_checkpoint
        {
            state.record_skip(previous_checkpoint, state.checkpoint)?;
        }
        Ok(state.clone())
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.sessions.lock().await.remove(session_id);
        Ok(())
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
mod tests;

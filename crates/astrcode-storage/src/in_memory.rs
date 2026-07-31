//! InMemoryEventStore — 纯内存事件存储和投影，仅用于测试。
//!
//! 通过 crate feature `testing` 暴露给跨 crate 集成测试；本 crate 内单元测试也可启用该 feature。
//! 不要用 `#[cfg(test)]` 单独 gating：否则 `astrcode-server` 等集成测试无法链接此模块。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, StoredEvent},
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId},
};
use astrcode_session_projection::{
    AgentSessionLinkView, SessionReadModel, SessionReadModelProjection, SessionSummary, reduce,
};
use tokio::sync::Mutex;

use crate::{
    CompactSnapshotInput, EventReader, SessionEventJournal, SessionPathResolver, SessionReader,
    SessionStore, StorageError, ToolResultArtifactInput, ToolResultArtifactRef,
    ToolResultArtifactStore,
    tool_artifacts::{slice_tool_result, tool_result_file_name_with_suffix},
};

/// 纯内存 session persistence 实现。
///
/// 这个类型维护完整事件列表和同步投影，因此不是 no-op；测试使用它能覆盖
/// 文件系统存储相同的读模型语义。
#[derive(Default)]
pub struct InMemoryEventStore {
    sessions: Mutex<HashMap<SessionId, InMemorySession>>,
}

struct InMemorySession {
    events: Vec<StoredEvent>,
    projection: SessionReadModel,
    tool_results: HashMap<String, String>,
}

impl InMemoryEventStore {
    /// 创建新的空内存存储。
    pub fn new() -> Self {
        Self::default()
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
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        let expected_prefix = format!("memory://{session_id}/tool-results/");
        if !path.starts_with(&expected_prefix) {
            return Err(StorageError::InvalidId(
                "tool result path belongs to a different session".into(),
            ));
        }
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let content = session
            .tool_results
            .get(path)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        Ok(slice_tool_result(path, content, char_offset, max_chars))
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
            let path = format_memory_tool_result_path(
                session_id.as_str(),
                &artifact.tool_name,
                &artifact.call_id,
                suffix,
            );
            match session.tool_results.get(&path) {
                Some(existing) if existing == &artifact.content => {
                    return Ok(ToolResultArtifactRef {
                        bytes: artifact.content.len(),
                        path: Some(path),
                    });
                },
                Some(_) => continue,
                None => {
                    session
                        .tool_results
                        .insert(path.clone(), artifact.content.clone());
                    return Ok(ToolResultArtifactRef {
                        bytes: artifact.content.len(),
                        path: Some(path),
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
            },
        );
        Ok(stored)
    }

    async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        let mut map = self.sessions.lock().await;
        let session = map
            .get_mut(&event.session_id)
            .ok_or_else(|| StorageError::NotFound(event.session_id.clone()))?;
        let stored = StoredEvent::new(session.events.len() as u64, event);
        reduce(&stored, &mut session.projection)
            .map_err(|error| StorageError::InvalidEvent(error.to_string()))?;
        session.events.push(stored.clone());
        Ok(stored)
    }
}

#[async_trait::async_trait]
impl SessionStore for InMemoryEventStore {
    async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), StorageError> {
        let map = self.sessions.lock().await;
        let session = map
            .get(session_id)
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let latest_cursor = session.projection.cursor();
        if cursor != &latest_cursor {
            return Err(StorageError::InvalidId(format!(
                "checkpoint cursor {cursor} does not match latest cursor {latest_cursor}"
            )));
        }
        Ok(())
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

fn format_memory_tool_result_path(
    session_id: &str,
    tool_name: &str,
    call_id: &str,
    suffix: usize,
) -> String {
    let file_name = tool_result_file_name_with_suffix(tool_name, call_id, suffix);
    format!("memory://{session_id}/tool-results/{file_name}")
}

#[cfg(test)]
mod tests;

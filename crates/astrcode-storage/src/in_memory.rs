//! InMemoryEventStore — 纯内存事件存储和投影，用于测试。

use std::collections::HashMap;

use astrcode_core::{
    event::{Event, EventPayload},
    storage::{
        CompactSnapshotInput, ConversationReadModel, EventStore, SessionReadModel, SessionSummary,
        StorageError, ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::{Cursor, SessionId},
};
use astrcode_support::tool_results::{slice_tool_result, tool_result_file_name};
use tokio::sync::Mutex;

use crate::projection;

/// 纯内存 EventStore 实现。
///
/// 这个类型维护完整事件列表和同步投影，因此不是 no-op；测试使用它能覆盖
/// 文件系统存储相同的读模型语义。
pub struct InMemoryEventStore {
    sessions: Mutex<HashMap<SessionId, InMemorySession>>,
}

struct InMemorySession {
    events: Vec<Event>,
    projection: SessionReadModel,
    tool_results: HashMap<String, String>,
}

impl InMemoryEventStore {
    /// 创建新的空内存存储。
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl EventStore for InMemoryEventStore {
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&str>,
    ) -> Result<Event, StorageError> {
        let mut event = Event::new(
            session_id.clone(),
            None,
            EventPayload::SessionStarted {
                working_dir: working_dir.into(),
                model_id: model_id.into(),
                parent_session_id: parent_session_id.map(|s| s.to_string()),
            },
        );
        // EventLog seq 是会话内 0-indexed；第一条 SessionStarted 为 0。
        event.seq = Some(0);

        let mut projection = SessionReadModel::empty(session_id.clone());
        projection::reduce(&event, &mut projection);
        self.sessions.lock().await.insert(
            session_id.clone(),
            InMemorySession {
                events: vec![event.clone()],
                projection,
                tool_results: HashMap::new(),
            },
        );
        Ok(event)
    }

    async fn append_event(&self, mut event: Event) -> Result<Event, StorageError> {
        let mut map = self.sessions.lock().await;
        let session = map
            .get_mut(&event.session_id)
            .ok_or_else(|| StorageError::NotFound(event.session_id.clone()))?;
        event.seq = Some(session.events.len() as u64);
        session.events.push(event.clone());
        projection::reduce(&event, &mut session.projection);
        Ok(event)
    }

    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .map(|session| session.events.clone())
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReadModel, StorageError> {
        let map = self.sessions.lock().await;
        map.get(session_id)
            .map(|session| session.projection.clone())
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let mut summaries = self
            .sessions
            .lock()
            .await
            .values()
            .map(|session| SessionSummary::from(session.projection.clone()))
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(summaries)
    }

    async fn conversation_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<ConversationReadModel, StorageError> {
        Ok(projection::conversation_snapshot(
            self.session_read_model(session_id).await?,
        ))
    }

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError> {
        Ok(self
            .session_read_model(session_id)
            .await?
            .latest_seq
            .map(|seq| seq.to_string()))
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError> {
        let events = self.replay_events(session_id).await?;
        let Ok(seq) = cursor.parse::<u64>() else {
            return Ok(events);
        };
        Ok(events
            .into_iter()
            .filter(|event| event.seq.unwrap_or(0) >= seq)
            .collect())
    }

    async fn checkpoint(
        &self,
        session_id: &SessionId,
        _cursor: &Cursor,
    ) -> Result<(), StorageError> {
        // In-memory storage does not persist checkpoint state. We still read
        // the session model to validate the session exists and to keep the
        // semantics of checkpoint as a no-op that can fail for invalid sessions.
        self.session_read_model(session_id).await?;
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        Ok(self.sessions.lock().await.keys().cloned().collect())
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
                session_id,
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
}

fn format_memory_tool_result_path(
    session_id: &str,
    tool_name: &str,
    call_id: &str,
    suffix: usize,
) -> String {
    let file_name = tool_result_file_name(tool_name, call_id);
    if suffix == 0 {
        return format!("memory://{session_id}/tool-results/{file_name}");
    }
    let stem = file_name.trim_end_matches(".txt");
    format!("memory://{session_id}/tool-results/{stem}-{suffix}.txt")
}

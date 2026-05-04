//! 基于文件系统的会话仓库，实现 EventStore trait。
//!
//! 管理按项目组织的会话事件日志，目录结构为：
//! `~/.astrcode/projects/<project>/sessions/<session>/`

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::{
    event::{Event, EventPayload},
    storage::{
        CompactSnapshotInput, ConversationReadModel, EventStore, SessionReadModel, SessionSummary,
        StorageError, ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::{
        Cursor, ProjectKey, SessionId, project_hash_from_path, project_key_from_path,
        validate_session_id,
    },
};
use astrcode_support::{
    hostpaths,
    tool_results::{slice_tool_result, write_tool_result_file},
};
use chrono::Utc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{event_log::EventLog, projection, snapshot::SnapshotManager};

/// 基于文件系统的会话仓库。
///
/// 管理按项目组织的会话事件日志，目录结构为：
/// `~/.astrcode/projects/<project>/sessions/<session>/`
///
/// 内存中缓存已打开的会话元数据，避免频繁的磁盘 I/O。
pub struct FileSystemSessionRepository {
    /// 已打开的会话元数据缓存，按会话 ID 索引
    sessions: Arc<RwLock<HashMap<SessionId, Arc<SessionMeta>>>>,
    /// 会话存储的基础路径
    base_path: PathBuf,
    /// 旧版 hash 项目目录，存在时用于读取老会话。
    legacy_base_path: Option<PathBuf>,
}

/// 会话的内部元数据，持有事件日志和快照管理器。
struct SessionMeta {
    /// 事件日志实例，负责追加式写入和重放
    log: Arc<EventLog>,
    /// 快照管理器，负责创建和列出恢复点
    snapshot_mgr: SnapshotManager,
    /// 当前会话所在目录。旧 hash 会话打开后继续写回旧目录。
    dir: PathBuf,
    /// 从事件日志同步维护的内部读模型。
    projection: RwLock<SessionReadModel>,
}

impl FileSystemSessionRepository {
    /// 创建新的文件系统会话仓库。
    ///
    /// # 参数
    /// - `project_key`: 项目路径派生的可读目录名，用于确定存储目录
    pub fn new(project_key: ProjectKey) -> Self {
        Self::with_paths(hostpaths::sessions_dir(&project_key), None)
    }

    /// 根据真实项目路径创建仓库，新会话使用可读 project key，旧 hash 会话仍可打开。
    pub fn for_project_path(project_path: &Path) -> Self {
        let current = hostpaths::sessions_dir(&project_key_from_path(project_path));
        let legacy = hostpaths::sessions_dir(&project_hash_from_path(project_path));
        let legacy = (legacy != current).then_some(legacy);
        Self::with_paths(current, legacy)
    }

    fn with_paths(base_path: PathBuf, legacy_base_path: Option<PathBuf>) -> Self {
        if let Err(e) = std::fs::create_dir_all(&base_path) {
            tracing::warn!("Failed to create sessions dir {}: {e}", base_path.display());
        }
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            base_path,
            legacy_base_path,
        }
    }

    /// 获取指定会话的目录路径。
    fn session_dir(&self, id: &SessionId) -> PathBuf {
        self.base_path.join(id)
    }

    fn existing_session_dir(&self, id: &SessionId) -> PathBuf {
        let current = self.session_dir(id);
        if current.exists() {
            return current;
        }

        if let Some(legacy_base_path) = &self.legacy_base_path {
            let legacy = legacy_base_path.join(id);
            if legacy.exists() {
                return legacy;
            }
        }

        current
    }

    /// 获取指定会话的事件日志文件路径。
    fn event_log_path(session_dir: &Path, id: &SessionId) -> PathBuf {
        session_dir.join(format!("session-{}.jsonl", id))
    }

    /// 获取或打开会话元数据。
    ///
    /// 如果会话已在内存中则直接返回缓存；否则从磁盘打开事件日志，
    /// 恢复其内存中的 seq 计数器，并加入缓存。
    /// 使用双重检查锁定模式避免重复打开。
    async fn get_or_open_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionMeta>, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;

        if let Some(meta) = self.sessions.read().await.get(session_id).cloned() {
            return Ok(meta);
        }

        // Opening a log restores its in-memory next_seq from disk. That should
        // happen once per active process/session, not once per append.
        let dir = self.existing_session_dir(session_id);
        let log = Arc::new(EventLog::open(Self::event_log_path(&dir, session_id)).await?);
        let snapshot_mgr = SnapshotManager::new(dir.join("snapshots"));
        let projection = self
            .restore_projection(session_id, &log, &snapshot_mgr)
            .await?;
        let opened = Arc::new(SessionMeta {
            log,
            snapshot_mgr,
            dir,
            projection: RwLock::new(projection),
        });

        let mut sessions = self.sessions.write().await;
        Ok(sessions
            .entry(session_id.clone())
            .or_insert_with(|| opened)
            .clone())
    }

    async fn restore_projection(
        &self,
        session_id: &SessionId,
        log: &EventLog,
        snapshot_mgr: &SnapshotManager,
    ) -> Result<SessionReadModel, StorageError> {
        if let Some(snapshot) = snapshot_mgr.latest_snapshot().await? {
            // Try snapshot restore first because it can make recovery much
            // faster than replaying the entire event log from the beginning.
            match restore_from_snapshot(log, snapshot).await {
                Ok(model) => return Ok(model),
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        "Falling back to full event replay after snapshot restore failed: {error}"
                    );
                },
            }
        }

        let events = log.replay_all().await?;
        Ok(projection::replay(session_id.clone(), &events))
    }
}

async fn restore_from_snapshot(
    log: &EventLog,
    snapshot: crate::snapshot::SessionProjectionSnapshot,
) -> Result<SessionReadModel, StorageError> {
    // Snapshot must include the highest event sequence it is based on so we
    // can safely replay only the later tail of the event log.
    let Some(latest_seq) = snapshot.latest_seq else {
        return Err(StorageError::InvalidId(
            "snapshot latest_seq is missing".into(),
        ));
    };

    let event_count = log.count().await?;
    if latest_seq >= event_count as u64 {
        return Err(StorageError::InvalidId(format!(
            "snapshot latest_seq {latest_seq} is outside event log with {event_count} events"
        )));
    }

    let mut model = snapshot.model;
    // Reapply only the events that occurred after the snapshot. The snapshot
    // serves as a recovery checkpoint, not as an authoritative source of truth.
    for event in log.replay_after(latest_seq).await? {
        projection::reduce(&event, &mut model);
    }
    Ok(model)
}

#[async_trait::async_trait]
impl EventStore for FileSystemSessionRepository {
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&str>,
    ) -> Result<Event, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;

        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir)?;

        let start_event = Event::new(
            session_id.clone(),
            None,
            EventPayload::SessionStarted {
                working_dir: working_dir.into(),
                model_id: model_id.into(),
                parent_session_id: parent_session_id.map(|s| s.to_string()),
            },
        );

        let (log, stored_event) =
            EventLog::create(Self::event_log_path(&dir, session_id), start_event).await?;

        let mut projection = SessionReadModel::empty(session_id.clone());
        projection::reduce(&stored_event, &mut projection);

        self.sessions.write().await.insert(
            session_id.clone(),
            Arc::new(SessionMeta {
                log: Arc::new(log),
                snapshot_mgr: SnapshotManager::new(dir.join("snapshots")),
                dir,
                projection: RwLock::new(projection),
            }),
        );

        Ok(stored_event)
    }

    async fn append_event(&self, event: Event) -> Result<Event, StorageError> {
        let session_id = event.session_id.clone();
        let meta = self.get_or_open_meta(&session_id).await?;
        let stored = meta.log.append(event).await?;
        projection::reduce(&stored, &mut *meta.projection.write().await);
        Ok(stored)
    }

    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        meta.log.replay_all().await
    }

    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReadModel, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let model = meta.projection.read().await.clone();
        Ok(model)
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let mut summaries = Vec::new();
        // TODO: Large histories should use a sidecar summary index. The v1
        // implementation intentionally rebuilds unopened projections from the
        // EventLog so storage keeps a single fact source.
        for session_id in self.list_sessions().await? {
            summaries.push(SessionSummary::from(
                self.session_read_model(&session_id).await?,
            ));
        }
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
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;
        let events = self.replay_events(session_id).await?;
        let Ok(seq) = cursor.parse::<u64>() else {
            return Err(StorageError::InvalidId(format!("Invalid cursor: {cursor}")));
        };
        Ok(events
            .into_iter()
            .filter(|event| event.seq.unwrap_or(0) >= seq)
            .collect())
    }

    async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let model = meta.projection.read().await.clone();
        let latest_cursor = model.cursor();
        // Checkpoints are only written when the cursor matches the current
        // recovered projection state. This prevents stale or out-of-order
        // checkpoint snapshots from being persisted.
        if cursor != &latest_cursor {
            return Err(StorageError::InvalidId(format!(
                "checkpoint cursor {cursor} does not match latest cursor {latest_cursor}"
            )));
        }
        meta.snapshot_mgr.create_snapshot(&model).await?;
        Ok(())
    }

    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let _ = self.get_or_open_meta(session_id).await?;
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        let mut ids: Vec<SessionId> = self.sessions.read().await.keys().cloned().collect();
        for base_path in self.session_roots() {
            if !base_path.exists() {
                continue;
            }
            for entry in std::fs::read_dir(base_path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let id = entry.file_name().to_string_lossy().to_string();
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;

        self.sessions.write().await.remove(session_id);
        for base_path in self.session_roots() {
            let dir = base_path.join(session_id);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
        }
        Ok(())
    }

    async fn write_compact_snapshot(
        &self,
        session_id: &SessionId,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;
        let meta = self.get_or_open_meta(session_id).await?;

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

    async fn write_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;
        let meta = self.get_or_open_meta(session_id).await?;

        let dir = meta.dir.join("tool-results");
        Ok(write_tool_result_file(&dir, &artifact, session_id)?)
    }

    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        validate_session_id(session_id).map_err(|e| StorageError::InvalidId(e.to_string()))?;
        let meta = self.get_or_open_meta(session_id).await?;

        let path = PathBuf::from(path);
        let artifact_dir = meta.dir.join("tool-results");
        if !hostpaths::is_path_within(&path, &artifact_dir) {
            return Err(StorageError::InvalidId(
                "tool result path is outside this session artifact directory".into(),
            ));
        }
        if !path.exists() {
            return Err(StorageError::NotFound(session_id.clone()));
        }
        let content = tokio::fs::read_to_string(&path).await?;
        Ok(slice_tool_result(
            &path.to_string_lossy(),
            &content,
            char_offset,
            max_chars,
        ))
    }
}

impl FileSystemSessionRepository {
    fn session_roots(&self) -> Vec<&PathBuf> {
        let mut roots = vec![&self.base_path];
        if let Some(legacy_base_path) = &self.legacy_base_path {
            roots.push(legacy_base_path);
        }
        roots
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::EventPayload,
        llm::{LlmMessage, LlmRole},
        storage::CompactSnapshotInput,
        types::{new_message_id, project_hash_from_path, project_key_from_path},
    };

    use super::*;

    #[tokio::test]
    async fn compact_snapshot_writes_metadata_and_messages() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = FileSystemSessionRepository {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            base_path: temp_dir.path().join("sessions"),
            legacy_base_path: None,
        };
        let session_id = "session-test".to_string();
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();

        let path = repo
            .write_compact_snapshot(
                &session_id,
                CompactSnapshotInput {
                    trigger: "manual_command".into(),
                    model_id: "mock".into(),
                    working_dir: ".".into(),
                    system_prompt: Some("system".into()),
                    provider_messages: vec![LlmMessage::user("hello")],
                },
            )
            .await
            .unwrap()
            .unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("\"type\":\"metadata\""));
        assert!(content.contains("\"trigger\":\"manual_command\""));
        assert!(content.contains("\"type\":\"message\""));
        assert!(path.contains("compact-snapshots"));
    }

    #[tokio::test]
    async fn tool_result_artifact_writes_under_session_dir_and_reads_slice() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("sessions");
        let repo = test_repo(base_path.clone());
        let session_id = "session-test".to_string();
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();

        let reference = repo
            .write_tool_result_artifact(
                &session_id,
                ToolResultArtifactInput {
                    call_id: "call-1".into(),
                    tool_name: "shell".into(),
                    content: "abcdef".into(),
                },
            )
            .await
            .unwrap();

        let path = reference.path.as_ref().expect("filesystem path");
        assert!(path.contains("tool-results"));
        assert!(path.contains("session-test"));
        assert!(std::path::Path::new(path).starts_with(base_path.join(&session_id)));

        let slice = repo
            .read_tool_result_artifact_by_path(&session_id, path, 2, 3)
            .await
            .unwrap();
        assert_eq!(slice.content, "cde");
        assert_eq!(slice.next_char_offset, Some(5));
        assert!(slice.has_more);
    }

    #[tokio::test]
    async fn tool_result_artifact_reuses_same_content_and_keeps_collisions() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("sessions"));
        let session_id = "session-test".to_string();
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();

        let input = ToolResultArtifactInput {
            call_id: "call-1".into(),
            tool_name: "shell".into(),
            content: "abcdef".into(),
        };
        let first = repo
            .write_tool_result_artifact(&session_id, input.clone())
            .await
            .unwrap();
        let second = repo
            .write_tool_result_artifact(&session_id, input)
            .await
            .unwrap();
        assert_eq!(first.path, second.path);

        let third = repo
            .write_tool_result_artifact(
                &session_id,
                ToolResultArtifactInput {
                    call_id: "call-1".into(),
                    tool_name: "shell".into(),
                    content: "changed".into(),
                },
            )
            .await
            .unwrap();

        let first_path = first.path.as_ref().expect("first path");
        let third_path = third.path.as_ref().expect("third path");
        assert_ne!(first_path, third_path);
        assert_eq!(
            tokio::fs::read_to_string(first_path).await.unwrap(),
            "abcdef"
        );
        assert_eq!(
            tokio::fs::read_to_string(third_path).await.unwrap(),
            "changed"
        );
    }

    #[tokio::test]
    async fn append_updates_projection_immediately() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("sessions"));
        let session_id = "session-test".to_string();
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();

        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "hello".into(),
            },
        ))
        .await
        .unwrap();

        let model = repo.session_read_model(&session_id).await.unwrap();
        assert_eq!(model.latest_seq, Some(1));
        assert_eq!(model.messages.len(), 1);
        assert_eq!(model.messages[0].role, LlmRole::User);
    }

    #[tokio::test]
    async fn reopen_rebuilds_projection_from_event_log() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("sessions");
        let session_id = "session-test".to_string();
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "answer".into(),
            },
        ))
        .await
        .unwrap();

        let reopened = test_repo(base_path);
        let model = reopened.session_read_model(&session_id).await.unwrap();

        assert_eq!(model.latest_seq, Some(1));
        assert_eq!(model.messages.len(), 1);
        assert_eq!(model.messages[0].role, LlmRole::Assistant);
    }

    #[tokio::test]
    async fn checkpoint_writes_projection_snapshot() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("sessions");
        let repo = test_repo(base_path.clone());
        let session_id = "session-test".to_string();
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "visible".into(),
            },
        ))
        .await
        .unwrap();
        repo.checkpoint(&session_id, &"1".into()).await.unwrap();

        let snapshot_path = base_path
            .join(&session_id)
            .join("snapshots")
            .join("snapshot-1.json");
        let snapshot: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(snapshot_path).unwrap()).unwrap();
        assert_eq!(snapshot["version"], 1);
        assert_eq!(snapshot["cursor"], "1");
        assert_eq!(snapshot["latest_seq"], 1);
        assert_eq!(snapshot["model"]["session_id"], session_id);
        assert_eq!(snapshot["model"]["messages"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reopen_restores_projection_from_snapshot_and_tail() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("sessions");
        let session_id = "session-test".to_string();
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "hello".into(),
            },
        ))
        .await
        .unwrap();
        repo.checkpoint(&session_id, &"1".into()).await.unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "answer".into(),
            },
        ))
        .await
        .unwrap();

        let expected = repo.session_read_model(&session_id).await.unwrap();
        let reopened = test_repo(base_path);
        let restored = reopened.session_read_model(&session_id).await.unwrap();

        assert_eq!(restored, expected);
        assert_eq!(restored.latest_seq, Some(2));
        assert_eq!(restored.messages.len(), 2);
    }

    #[tokio::test]
    async fn checkpoint_rejects_stale_cursor() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("sessions"));
        let session_id = "session-test".to_string();
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "hello".into(),
            },
        ))
        .await
        .unwrap();

        let error = repo.checkpoint(&session_id, &"0".into()).await.unwrap_err();

        assert!(matches!(
            error,
            StorageError::InvalidId(message)
                if message.contains("checkpoint cursor 0 does not match latest cursor 1")
        ));
    }

    #[tokio::test]
    async fn corrupt_snapshot_falls_back_to_full_replay() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("sessions");
        let session_id = "session-test".to_string();
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "answer".into(),
            },
        ))
        .await
        .unwrap();
        repo.checkpoint(&session_id, &"1".into()).await.unwrap();
        let expected = repo.session_read_model(&session_id).await.unwrap();
        std::fs::write(
            base_path
                .join(&session_id)
                .join("snapshots")
                .join("snapshot-1.json"),
            "not json",
        )
        .unwrap();

        let reopened = test_repo(base_path);
        let restored = reopened.session_read_model(&session_id).await.unwrap();

        assert_eq!(restored, expected);
    }

    #[tokio::test]
    async fn list_summaries_reads_unopened_session_projection() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("sessions");
        let session_id = "session-test".to_string();
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, "D:/work/project", "mock", Some("parent"))
            .await
            .unwrap();

        let reopened = test_repo(base_path);
        let summaries = reopened.list_session_summaries().await.unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, session_id);
        assert_eq!(summaries[0].working_dir, "D:/work/project");
        assert_eq!(summaries[0].model_id, "mock");
        assert_eq!(summaries[0].parent_session_id.as_deref(), Some("parent"));
    }

    #[tokio::test]
    async fn project_path_repository_writes_readable_dirs_and_reads_legacy_hash_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let current_project_dir = temp_dir
            .path()
            .join("projects")
            .join(project_key_from_path(&workspace));
        let legacy_project_dir = temp_dir
            .path()
            .join("projects")
            .join(project_hash_from_path(&workspace));
        let current_sessions_dir = current_project_dir.join("sessions");
        let legacy_sessions_dir = legacy_project_dir.join("sessions");

        let legacy_session = "legacy-session".to_string();
        let legacy_repo =
            FileSystemSessionRepository::with_paths(legacy_sessions_dir.clone(), None);
        legacy_repo
            .create_session(&legacy_session, workspace.to_str().unwrap(), "mock", None)
            .await
            .unwrap();

        let repo = FileSystemSessionRepository::with_paths(
            current_sessions_dir,
            Some(legacy_sessions_dir),
        );
        let current_session = "current-session".to_string();
        repo.create_session(&current_session, workspace.to_str().unwrap(), "mock", None)
            .await
            .unwrap();

        let sessions = repo.list_sessions().await.unwrap();

        assert!(current_project_dir.exists());
        assert!(legacy_project_dir.exists());
        assert!(sessions.contains(&legacy_session));
        assert!(sessions.contains(&current_session));
        assert!(
            current_project_dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("workspace")
        );
    }

    #[tokio::test]
    async fn custom_event_does_not_change_projection() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("sessions"));
        let session_id = "session-test".to_string();
        repo.create_session(&session_id, ".", "mock", None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "visible".into(),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::Custom {
                name: "extension.note".into(),
                data: serde_json::json!({
                    "message": "projection ignores extension-specific payloads"
                }),
            },
        ))
        .await
        .unwrap();

        let model = repo.session_read_model(&session_id).await.unwrap();

        assert_eq!(model.messages.len(), 1);
        assert!(model.context_messages.is_empty());
    }

    #[tokio::test]
    async fn compact_continuation_child_projection_uses_summary_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("sessions");
        let repo = test_repo(base_path.clone());
        let parent_id = "parent-session".to_string();
        let child_id = "child-session".to_string();
        repo.create_session(&parent_id, ".", "mock", None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            parent_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "visible".into(),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            parent_id.clone(),
            None,
            EventPayload::CompactBoundaryCreated {
                trigger: "manual_command".into(),
                pre_tokens: 100,
                post_tokens: 20,
                summary: "summary".into(),
                transcript_path: Some("compact.jsonl".into()),
                continued_session_id: child_id.clone(),
            },
        ))
        .await
        .unwrap();
        repo.create_session(&child_id, ".", "mock", Some(&parent_id))
            .await
            .unwrap();
        repo.append_event(Event::new(
            child_id.clone(),
            None,
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id: parent_id.clone(),
                parent_cursor: "1".into(),
                summary: "summary".into(),
                transcript_path: Some("compact.jsonl".into()),
                context_messages: vec![LlmMessage::system("hidden summary")],
                retained_messages: vec![LlmMessage::user("recent")],
            },
        ))
        .await
        .unwrap();
        repo.checkpoint(&child_id, &"1".into()).await.unwrap();

        let parent = repo.session_read_model(&parent_id).await.unwrap();
        assert_eq!(parent.messages.len(), 1);
        assert!(parent.context_messages.is_empty());

        let reopened = test_repo(base_path);
        let child = reopened.session_read_model(&child_id).await.unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some(parent_id.as_str()));
        assert_eq!(
            child.context_messages,
            vec![LlmMessage::system("hidden summary")]
        );
        assert_eq!(child.messages, vec![LlmMessage::user("recent")]);
        assert_eq!(child.provider_messages().len(), 2);
    }

    fn test_repo(base_path: PathBuf) -> FileSystemSessionRepository {
        FileSystemSessionRepository {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            base_path,
            legacy_base_path: None,
        }
    }
}

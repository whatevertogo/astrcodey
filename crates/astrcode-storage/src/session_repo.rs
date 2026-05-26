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
        CompactSnapshotInput, EventReader, EventStore, SessionReadModel, SessionSummary,
        StorageError, ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::{Cursor, SessionId, project_key_from_path, validate_session_id},
};
use astrcode_support::{hostpaths, perf_snapshot};
use chrono::Utc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    event_log::EventLog,
    projection,
    snapshot::SnapshotManager,
    tool_artifacts::{slice_tool_result, write_tool_result_file},
};

fn source_extension_dir_component(source_extension: &str) -> Result<String, StorageError> {
    if source_extension.is_empty() {
        return Err(StorageError::InvalidId(
            "source_extension cannot be empty".into(),
        ));
    }

    let mut encoded = String::new();
    for byte in source_extension.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(encoded)
}

/// 基于文件系统的会话仓库。
///
/// 管理按项目组织的会话事件日志，目录结构为：
/// `~/.astrcode/projects/<project>/sessions/<session>/`
///
/// 内存中缓存已打开的会话元数据，避免频繁的磁盘 I/O。
pub struct FileSystemSessionRepository {
    /// 已打开的会话元数据缓存，按会话 ID 索引
    sessions: Arc<RwLock<HashMap<SessionId, Arc<SessionMeta>>>>,
    /// 所有项目目录的父目录：`~/.astrcode/projects/`
    projects_base: PathBuf,
}

/// 会话的内部元数据，持有事件日志和快照管理器。
struct SessionMeta {
    /// 事件日志实例，负责追加式写入和重放
    log: Arc<EventLog>,
    /// 快照管理器，负责创建和列出恢复点
    snapshot_mgr: SnapshotManager,
    /// 当前会话所在目录。
    dir: PathBuf,
    /// 从事件日志同步维护的内部读模型。
    projection: RwLock<SessionReadModel>,
}

impl Default for FileSystemSessionRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystemSessionRepository {
    /// 创建新的文件系统会话仓库。
    ///
    /// 会话按 `working_dir` 动态分发到对应的项目目录，不再绑定启动时的 cwd。
    pub fn new() -> Self {
        Self::with_projects_base(hostpaths::projects_dir())
    }

    /// 根据真实项目路径创建仓库（与 `new` 相同，保留语义入口）。
    pub fn for_project_path(_: &Path) -> Self {
        Self::new()
    }

    fn with_projects_base(projects_base: PathBuf) -> Self {
        if let Err(e) = std::fs::create_dir_all(&projects_base) {
            tracing::warn!(
                "Failed to create projects dir {}: {e}",
                projects_base.display()
            );
        }
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            projects_base,
        }
    }

    /// 根据 `working_dir` 计算新会话的存储目录。
    fn session_dir_from_working_dir(&self, working_dir: &str, id: &SessionId) -> PathBuf {
        let project_key = project_key_from_path(&PathBuf::from(working_dir));
        self.projects_base
            .join(project_key)
            .join("sessions")
            .join(id.as_str())
    }

    /// 扫描 projects_base 下所有项目目录，返回其 sessions 子目录列表。
    async fn all_session_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(&self.projects_base).await else {
            return roots;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let sessions_dir = entry.path().join("sessions");
            if tokio::fs::metadata(&sessions_dir)
                .await
                .is_ok_and(|m| m.is_dir())
            {
                roots.push(sessions_dir);
            }
        }
        roots
    }

    /// 在所有项目目录中查找指定会话的目录。
    ///
    /// 先检查 flat 位置（根 session），再递归搜索 `subagents/` 子目录树。
    async fn find_session_dir(&self, id: &SessionId) -> Option<PathBuf> {
        if let Some(meta) = self.sessions.read().await.get(id).cloned() {
            return Some(meta.dir.clone());
        }

        for root in self.all_session_roots().await {
            let dir = root.join(id.as_str());
            if tokio::fs::metadata(&dir).await.is_ok_and(|m| m.is_dir()) {
                return Some(dir);
            }
            if let Some(found) = self.search_subagents_tree(&root, id).await {
                return Some(found);
            }
        }
        None
    }

    /// 递归搜索 `base` 下所有 session 目录的 `subagents/{extension}/` 子树。
    fn search_subagents_tree<'a>(
        &'a self,
        base: &'a Path,
        id: &'a SessionId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PathBuf>> + Send + 'a>> {
        Box::pin(async move {
            let Ok(mut entries) = tokio::fs::read_dir(base).await else {
                return None;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let subagents = entry.path().join("subagents");
                if !tokio::fs::metadata(&subagents)
                    .await
                    .is_ok_and(|m| m.is_dir())
                {
                    continue;
                }
                let Ok(mut extension_entries) = tokio::fs::read_dir(&subagents).await else {
                    continue;
                };
                while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                    let extension_name = extension_entry.file_name();
                    if extension_name.to_string_lossy() == ".recycled" {
                        continue;
                    }
                    let extension_dir = extension_entry.path();
                    let candidate = extension_dir.join(id.as_str());
                    if tokio::fs::metadata(&candidate)
                        .await
                        .is_ok_and(|m| m.is_dir())
                    {
                        return Some(candidate);
                    }
                    if let Some(found) = self.search_subagents_tree(&extension_dir, id).await {
                        return Some(found);
                    }
                }
            }
            None
        })
    }

    /// 获取指定会话的目录路径。
    async fn session_dir(&self, id: &SessionId) -> Option<PathBuf> {
        self.find_session_dir(id).await
    }

    async fn existing_session_dir(&self, id: &SessionId) -> Result<PathBuf, StorageError> {
        self.find_session_dir(id)
            .await
            .ok_or_else(|| StorageError::NotFound(id.clone()))
    }

    /// 获取指定会话的事件日志文件路径。
    fn event_log_path(session_dir: &Path, id: &SessionId) -> PathBuf {
        session_dir.join(format!("session-{id}.jsonl"))
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
        validate_session_id(session_id.as_str())
            .map_err(|e| StorageError::InvalidId(e.to_string()))?;

        if let Some(meta) = self.sessions.read().await.get(session_id).cloned() {
            return Ok(meta);
        }

        // 磁盘打开与 projection 恢复可能较慢；不得持 `sessions` 写锁跨越 await，
        // 否则会阻塞同实例上的 append/checkpoint，并在 Windows 上与未释放的
        // EventLog 句柄争用同一 JSONL 文件。
        let dir = self.existing_session_dir(session_id).await?;
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
        Ok(if let Some(meta) = sessions.get(session_id) {
            Arc::clone(meta)
        } else {
            sessions.insert(session_id.clone(), Arc::clone(&opened));
            opened
        })
    }

    async fn restore_projection(
        &self,
        session_id: &SessionId,
        log: &EventLog,
        snapshot_mgr: &SnapshotManager,
    ) -> Result<SessionReadModel, StorageError> {
        // Snapshot first because it's faster than replaying the full event log.
        if let Some(snapshot) = snapshot_mgr.latest_snapshot().await? {
            match restore_from_snapshot(log, snapshot).await {
                Ok(model) => return Ok(model),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
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
impl EventReader for FileSystemSessionRepository {
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

    async fn session_system_prompt(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<String>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let prompt = meta.projection.read().await.system_prompt.clone();
        Ok(prompt)
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let session_ids = self.list_session_dirs().await?;
        let sessions = self.sessions.read().await.clone();
        let mut summaries = Vec::new();

        for session_id in session_ids {
            if let Some(meta) = sessions.get(&session_id) {
                // 已打开的会话直接使用内存中的投影
                let model = meta.projection.read().await.clone();
                summaries.push(SessionSummary::from(model));
            } else {
                // 未打开的会话只读首行事件（SessionStarted）构造轻量摘要
                if let Some(summary) = self.read_summary_from_first_event(&session_id).await? {
                    summaries.push(summary);
                }
            }
        }

        summaries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(summaries)
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
        // session_id 验证由 get_or_open_meta 统一守卫
        let meta = self.get_or_open_meta(session_id).await?;
        let Ok(seq) = cursor.parse::<u64>() else {
            return Err(StorageError::InvalidId(format!("Invalid cursor: {cursor}")));
        };
        meta.log.replay_after(seq).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        let mut ids: Vec<SessionId> = self
            .sessions
            .read()
            .await
            .iter()
            .filter(|(_, meta)| !is_subagent_dir(&meta.dir))
            .map(|(id, _)| id.clone())
            .collect();
        for base_path in self.session_roots().await {
            self.collect_session_ids_recursive(&base_path, &mut ids)
                .await;
        }
        ids.sort();
        Ok(ids)
    }

    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
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

    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<std::path::PathBuf>, StorageError> {
        Ok(self.session_dir(session_id).await)
    }
}

#[async_trait::async_trait]
impl EventStore for FileSystemSessionRepository {
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&SessionId>,
        tool_policy: Option<&astrcode_core::extension::ChildToolPolicy>,
        source_extension: Option<&str>,
    ) -> Result<Event, StorageError> {
        validate_session_id(session_id.as_str())
            .map_err(|e| StorageError::InvalidId(e.to_string()))?;

        let dir = match parent_session_id {
            Some(parent_id) => {
                let extension = source_extension.ok_or_else(|| {
                    StorageError::InvalidId(
                        "child session requires source_extension to choose subagents directory"
                            .into(),
                    )
                })?;
                let parent_dir = self.existing_session_dir(parent_id).await?;
                let extension_dir = source_extension_dir_component(extension)?;
                parent_dir
                    .join("subagents")
                    .join(extension_dir)
                    .join(session_id.as_str())
            },
            None => self.session_dir_from_working_dir(working_dir, session_id),
        };
        tokio::fs::create_dir_all(&dir).await?;

        let start_event = Event::new(
            session_id.clone(),
            None,
            EventPayload::SessionStarted {
                working_dir: working_dir.into(),
                model_id: model_id.into(),
                parent_session_id: parent_session_id.cloned(),
                tool_policy: tool_policy.cloned(),
                source_extension: source_extension.map(String::from),
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

        perf_snapshot::capture_event("storage.create_session", &stored_event);

        Ok(stored_event)
    }

    async fn append_event(&self, event: Event) -> Result<Event, StorageError> {
        let session_id = event.session_id.clone();
        let meta = self.get_or_open_meta(&session_id).await?;
        let stored = meta.log.append(event).await?;
        projection::reduce(&stored, &mut *meta.projection.write().await);
        Ok(stored)
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

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_session_id(session_id.as_str())
            .map_err(|e| StorageError::InvalidId(e.to_string()))?;

        if let Some(dir) = self.find_session_dir(session_id).await {
            self.sessions
                .write()
                .await
                .retain(|_, meta| !meta.dir.starts_with(&dir));
            tokio::fs::remove_dir_all(&dir).await?;
        } else {
            self.sessions.write().await.remove(session_id);
        }
        Ok(())
    }

    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_session_id(session_id.as_str())
            .map_err(|e| StorageError::InvalidId(e.to_string()))?;

        let dir = self
            .find_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;

        // 从内存缓存移除
        self.sessions
            .write()
            .await
            .retain(|_, meta| !meta.dir.starts_with(&dir));

        // 结构：subagents/{extension}/{child_id}/
        // 回收到：subagents/.recycled/{extension}/{child_id}/
        // 这样 restore 时能直接 rename 回原位。
        let extension_dir = dir.parent(); // subagents/{extension}/
        let subagents_dir = extension_dir.and_then(|p| p.parent()); // subagents/

        if let (Some(extension_dir), Some(subagents_dir)) = (extension_dir, subagents_dir) {
            if subagents_dir.file_name().is_some_and(|n| n == "subagents") {
                let extension_name = extension_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let recycled = subagents_dir
                    .join(".recycled")
                    .join(extension_name.as_ref());
                tokio::fs::create_dir_all(&recycled).await?;
                let dest = recycled.join(dir.file_name().unwrap_or_default());
                tokio::fs::rename(&dir, &dest).await?;
                return Ok(());
            }
        }

        // 非子 session 或非标准目录结构，回退到删除
        tokio::fs::remove_dir_all(&dir).await?;
        Ok(())
    }

    async fn restore_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        validate_session_id(session_id.as_str())
            .map_err(|e| StorageError::InvalidId(e.to_string()))?;

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
            .unwrap_or_default()
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
        let meta = self.get_or_open_meta(session_id).await?;

        let dir = meta.dir.join("tool-results");
        Ok(write_tool_result_file(&dir, &artifact)?)
    }

    async fn sync_durable_events(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        meta.log.force_sync()
    }
}

/// 判断目录是否位于 subagents 子树下。
fn is_subagent_dir(dir: &Path) -> bool {
    dir.ancestors()
        .any(|a| a.file_name().is_some_and(|n| n == "subagents"))
}

impl FileSystemSessionRepository {
    async fn session_roots(&self) -> Vec<PathBuf> {
        self.all_session_roots().await
    }

    /// 在所有项目的 subagents/.recycled/{extension}/ 目录中搜索指定 session。
    async fn find_recycled_session_dir(&self, id: &SessionId) -> Option<PathBuf> {
        for root in self.all_session_roots().await {
            if let Some(found) = self.search_recycled_in_root(&root, id).await {
                return Some(found);
            }
        }
        None
    }

    /// 搜索 sessions_root 下所有 session 的 subagents/.recycled/{extension}/{id}。
    fn search_recycled_in_root<'a>(
        &'a self,
        sessions_root: &'a Path,
        id: &'a SessionId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PathBuf>> + Send + 'a>> {
        Box::pin(async move {
            let Ok(mut entries) = tokio::fs::read_dir(sessions_root).await else {
                return None;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str == ".recycled"
                    || name_str == "snapshots"
                    || name_str == "compact-snapshots"
                    || name_str == "tool-results"
                {
                    continue;
                }
                // Check subagents/.recycled/{extension}/{id}
                let recycled_dir = entry.path().join("subagents").join(".recycled");
                if tokio::fs::metadata(&recycled_dir)
                    .await
                    .is_ok_and(|m| m.is_dir())
                {
                    if let Ok(mut extension_entries) = tokio::fs::read_dir(&recycled_dir).await {
                        while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                            let candidate = extension_entry.path().join(id.as_str());
                            if tokio::fs::metadata(&candidate)
                                .await
                                .is_ok_and(|m| m.is_dir())
                            {
                                return Some(candidate);
                            }
                        }
                    }
                }
                // Recurse into subagents/{extension}/ for nested sessions
                let subagents = entry.path().join("subagents");
                if tokio::fs::metadata(&subagents)
                    .await
                    .is_ok_and(|m| m.is_dir())
                {
                    let Ok(mut extension_entries) = tokio::fs::read_dir(&subagents).await else {
                        continue;
                    };
                    while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                        let pname = extension_entry.file_name();
                        if pname.to_string_lossy() == ".recycled" {
                            continue;
                        }
                        if extension_entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                            if let Some(found) = self
                                .search_recycled_in_root(&extension_entry.path(), id)
                                .await
                            {
                                return Some(found);
                            }
                        }
                    }
                }
            }
            None
        })
    }

    /// 仅扫描磁盘上的会话目录名，不打开任何文件。
    async fn list_session_dirs(&self) -> Result<Vec<SessionId>, StorageError> {
        let mut ids: Vec<SessionId> = self
            .sessions
            .read()
            .await
            .iter()
            .filter(|(_, meta)| !is_subagent_dir(&meta.dir))
            .map(|(id, _)| id.clone())
            .collect();
        for base_path in self.session_roots().await {
            self.collect_session_ids_recursive(&base_path, &mut ids)
                .await;
        }
        ids.sort();
        Ok(ids)
    }

    /// 递归收集 base 目录下所有 session ID，包含 subagents/ 子树。
    fn collect_session_ids_recursive<'a>(
        &'a self,
        base: &'a Path,
        ids: &'a mut Vec<SessionId>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let Ok(mut entries) = tokio::fs::read_dir(base).await else {
                return;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str == "subagents"
                    || name_str == "snapshots"
                    || name_str == "compact-snapshots"
                    || name_str == "tool-results"
                    || name_str == ".recycled"
                {
                    continue;
                }
                let id = SessionId::from(name_str.to_string());
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        })
    }

    /// 从事件日志的首行和末行事件构造轻量级 SessionSummary。
    ///
    /// 单次遍历读取首行获取 SessionStarted 元数据（working_dir, model_id 等），
    /// 末行获取更准确的 updated_at 和 latest_cursor。
    /// 避免为未打开的会话重放整个事件日志。
    async fn read_summary_from_first_event(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, StorageError> {
        let Some(dir) = self.session_dir(session_id).await else {
            return Ok(None);
        };
        let log_path = Self::event_log_path(&dir, session_id);
        let (first_event, last_event, first_user_message) =
            EventLog::read_first_and_last(&log_path).await?;
        let Some(first_event) = first_event else {
            return Ok(None);
        };

        let (working_dir, model_id, parent_session_id, source_extension) =
            match &first_event.payload {
                EventPayload::SessionStarted {
                    working_dir,
                    model_id,
                    parent_session_id,
                    tool_policy: _,
                    source_extension,
                } => (
                    working_dir.clone(),
                    model_id.clone(),
                    parent_session_id.clone(),
                    source_extension.clone(),
                ),
                _ => return Ok(None),
            };

        let updated_at = last_event
            .as_ref()
            .map(|e| e.timestamp.to_rfc3339())
            .unwrap_or_else(|| first_event.timestamp.to_rfc3339());
        let latest_cursor = last_event
            .and_then(|e| e.seq.map(|s| s.to_string()))
            .unwrap_or_else(|| "0".into());

        Ok(Some(SessionSummary {
            session_id: session_id.clone(),
            working_dir,
            model_id,
            parent_session_id,
            created_at: first_event.timestamp.to_rfc3339(),
            updated_at,
            phase: astrcode_core::event::Phase::default(),
            latest_cursor,
            first_user_message,
            source_extension,
        }))
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::EventPayload,
        llm::{LlmContent, LlmMessage, LlmRole},
        storage::CompactSnapshotInput,
        types::{new_message_id, project_key_from_path},
    };

    use super::*;

    #[tokio::test]
    async fn compact_snapshot_writes_metadata_and_messages() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = FileSystemSessionRepository {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            projects_base: temp_dir.path().join("projects"),
        };
        let session_id = SessionId::from("session-test");
        repo.create_session(&session_id, ".", "mock", None, None, None)
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
        let projects_base = temp_dir.path().join("projects");
        let repo = test_repo(projects_base.clone());
        let session_id = SessionId::from("session-test");
        repo.create_session(&session_id, ".", "mock", None, None, None)
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
        let project_key = project_key_from_path(std::path::Path::new("."));
        assert!(
            std::path::Path::new(path).starts_with(
                projects_base
                    .join(project_key)
                    .join("sessions")
                    .join(session_id.as_str())
            )
        );

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
        let repo = test_repo(temp_dir.path().join("projects"));
        let session_id = SessionId::from("session-test");
        repo.create_session(&session_id, ".", "mock", None, None, None)
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
        let repo = test_repo(temp_dir.path().join("projects"));
        let session_id = SessionId::from("session-test");
        repo.create_session(&session_id, ".", "mock", None, None, None)
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
        let base_path = temp_dir.path().join("projects");
        let session_id = SessionId::from("session-test");
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, ".", "mock", None, None, None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "answer".into(),
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();

        drop(repo);
        let reopened = test_repo(base_path);
        let model = reopened.session_read_model(&session_id).await.unwrap();

        assert_eq!(model.latest_seq, Some(1));
        assert_eq!(model.messages.len(), 1);
        assert_eq!(model.messages[0].role, LlmRole::Assistant);
    }

    #[tokio::test]
    async fn checkpoint_writes_projection_snapshot() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("projects");
        let repo = test_repo(base_path.clone());
        let session_id = SessionId::from("session-test");
        repo.create_session(&session_id, ".", "mock", None, None, None)
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

        let project_key = project_key_from_path(std::path::Path::new("."));
        let snapshot_path = base_path
            .join(project_key)
            .join("sessions")
            .join(session_id.as_str())
            .join("snapshots")
            .join("snapshot-1.json");
        let snapshot: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(snapshot_path).unwrap()).unwrap();
        assert_eq!(snapshot["version"], 1);
        assert_eq!(snapshot["cursor"], "1");
        assert_eq!(snapshot["latest_seq"], 1);
        assert_eq!(snapshot["model"]["session_id"], session_id.as_str());
        assert_eq!(snapshot["model"]["messages"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reopen_restores_projection_from_snapshot_and_tail() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("projects");
        let session_id = SessionId::from("session-test");
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, ".", "mock", None, None, None)
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
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();

        let expected = repo.session_read_model(&session_id).await.unwrap();
        drop(repo);
        let reopened = test_repo(base_path);
        let restored = reopened.session_read_model(&session_id).await.unwrap();

        assert_eq!(restored, expected);
        assert_eq!(restored.latest_seq, Some(2));
        assert_eq!(restored.messages.len(), 2);
    }

    #[tokio::test]
    async fn checkpoint_rejects_stale_cursor() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("projects"));
        let session_id = SessionId::from("session-test");
        repo.create_session(&session_id, ".", "mock", None, None, None)
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
        let base_path = temp_dir.path().join("projects");
        let session_id = SessionId::from("session-test");
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, ".", "mock", None, None, None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "answer".into(),
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();
        repo.checkpoint(&session_id, &"1".into()).await.unwrap();
        let expected = repo.session_read_model(&session_id).await.unwrap();
        let project_key = project_key_from_path(std::path::Path::new("."));
        std::fs::write(
            base_path
                .join(project_key)
                .join("sessions")
                .join(session_id.as_str())
                .join("snapshots")
                .join("snapshot-1.json"),
            "not json",
        )
        .unwrap();

        drop(repo);
        let reopened = test_repo(base_path);
        let restored = reopened.session_read_model(&session_id).await.unwrap();

        assert_eq!(restored, expected);
    }

    #[tokio::test]
    async fn list_summaries_reads_unopened_session_projection() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("projects");
        let session_id = SessionId::from("session-test");
        let repo = test_repo(base_path.clone());
        repo.create_session(&session_id, "D:/work/project", "mock", None, None, None)
            .await
            .unwrap();

        drop(repo);
        let reopened = test_repo(base_path);
        let summaries = reopened.list_session_summaries().await.unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, session_id);
        assert_eq!(summaries[0].working_dir, "D:/work/project");
        assert_eq!(summaries[0].model_id, "mock");
        assert_eq!(summaries[0].parent_session_id, None);
    }

    #[tokio::test]
    async fn child_session_without_source_extension_is_rejected() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("projects"));
        let parent_id = SessionId::from("parent");
        let child_id = SessionId::from("child");
        repo.create_session(&parent_id, ".", "mock", None, None, None)
            .await
            .unwrap();

        let error = repo
            .create_session(&child_id, ".", "mock", Some(&parent_id), None, None)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            StorageError::InvalidId(message) if message.contains("source_extension")
        ));
    }

    #[tokio::test]
    async fn extension_child_sessions_use_safe_extension_dir_and_can_reopen() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("projects");
        let parent_id = SessionId::from("parent");
        let child_id = SessionId::from("child");
        let repo = test_repo(base_path.clone());
        repo.create_session(&parent_id, ".", "mock", None, None, None)
            .await
            .unwrap();
        repo.create_session(
            &child_id,
            ".",
            "mock",
            Some(&parent_id),
            None,
            Some("owner/extension.alpha"),
        )
        .await
        .unwrap();

        let project_key = project_key_from_path(std::path::Path::new("."));
        assert!(
            base_path
                .join(project_key)
                .join("sessions")
                .join(parent_id.as_str())
                .join("subagents")
                .join("owner%2Fextension%2Ealpha")
                .join(child_id.as_str())
                .exists()
        );

        drop(repo);
        let reopened = test_repo(base_path);
        let sessions = reopened.list_sessions().await.unwrap();
        let child = reopened.session_read_model(&child_id).await.unwrap();

        assert_eq!(sessions, vec![parent_id]);
        assert_eq!(
            child.parent_session_id.as_ref(),
            Some(&SessionId::from("parent"))
        );
        assert_eq!(
            child.source_extension.as_deref(),
            Some("owner/extension.alpha")
        );
    }

    #[tokio::test]
    async fn delete_parent_session_removes_cached_child_sessions() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("projects"));
        let parent_id = SessionId::from("parent");
        let child_id = SessionId::from("child");
        repo.create_session(&parent_id, ".", "mock", None, None, None)
            .await
            .unwrap();
        repo.create_session(
            &child_id,
            ".",
            "mock",
            Some(&parent_id),
            None,
            Some("test-extension"),
        )
        .await
        .unwrap();
        repo.session_read_model(&child_id).await.unwrap();

        repo.delete_session(&parent_id).await.unwrap();

        assert!(repo.list_sessions().await.unwrap().is_empty());
        assert!(matches!(
            repo.session_read_model(&child_id).await,
            Err(StorageError::NotFound(id)) if id == child_id
        ));
    }

    #[tokio::test]
    async fn project_path_repository_writes_readable_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let projects_base = temp_dir.path().join("projects");
        let current_project_dir = projects_base.join(project_key_from_path(&workspace));

        let repo = FileSystemSessionRepository::with_projects_base(projects_base);
        let current_session = SessionId::from("current-session");
        repo.create_session(
            &current_session,
            workspace.to_str().unwrap(),
            "mock",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let sessions = repo.list_sessions().await.unwrap();

        assert!(current_project_dir.exists());
        assert_eq!(sessions, vec![current_session]);
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
        let repo = test_repo(temp_dir.path().join("projects"));
        let session_id = SessionId::from("session-test");
        repo.create_session(&session_id, ".", "mock", None, None, None)
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
    async fn same_session_compact_projection_uses_summary_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path().join("projects");
        let repo = test_repo(base_path.clone());
        let session_id = SessionId::from("test-session");
        repo.create_session(&session_id, ".", "mock", None, None, None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "old message".into(),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "old reply".into(),
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::CompactBoundaryCreated {
                trigger: "manual_command".into(),
                pre_tokens: 100,
                post_tokens: 20,
                summary: "summary".into(),
                transcript_path: Some("compact.jsonl".into()),
                continued_session_id: session_id.clone(),
                base_event_seq: 0,
                strategy: astrcode_core::extension::CompactStrategy::Manual {
                    keep_recent_turns: None,
                },
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id: session_id.clone(),
                parent_cursor: "2".into(),
                summary: "summary".into(),
                transcript_path: Some("compact.jsonl".into()),
                context_messages: vec![LlmMessage::system("hidden summary")],
                retained_messages: vec![LlmMessage::user("recent")],
            },
        ))
        .await
        .unwrap();

        let state = repo.session_read_model(&session_id).await.unwrap();
        assert_eq!(
            state.context_messages,
            vec![LlmMessage::system("hidden summary")]
        );
        assert_eq!(state.messages, vec![LlmMessage::user("recent")]);
        let provider = state.provider_messages();
        assert_eq!(provider.len(), 2);
        assert_eq!(provider[0].role, LlmRole::System);
        assert_eq!(provider[1].role, LlmRole::User);

        // Reopen and verify projection restores from compact boundary.
        drop(repo);
        let reopened = test_repo(base_path);
        let restored = reopened.session_read_model(&session_id).await.unwrap();
        assert_eq!(
            restored.context_messages,
            vec![LlmMessage::system("hidden summary")]
        );
        assert_eq!(restored.messages, vec![LlmMessage::user("recent")]);
        assert_eq!(restored.provider_messages().len(), 2);
    }

    fn test_repo(projects_base: PathBuf) -> FileSystemSessionRepository {
        FileSystemSessionRepository {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            projects_base,
        }
    }

    #[tokio::test]
    async fn reasoning_tool_call_events_rebuild_single_assistant_message() {
        use astrcode_core::types::ToolCallId;

        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("projects"));
        let session_id = SessionId::from("session-reasoning-tool");
        repo.create_session(&session_id, ".", "mock", None, None, None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "read file".into(),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: new_message_id(),
                text: "checking".into(),
                reasoning_content: Some("private reasoning".into()),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::ToolCallRequested {
                call_id: ToolCallId::from("call_1"),
                tool_name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
            },
        ))
        .await
        .unwrap();

        let model = repo.session_read_model(&session_id).await.unwrap();

        assert_eq!(model.messages.len(), 2);
        let assistant = &model.messages[1];
        assert_eq!(assistant.role, LlmRole::Assistant);
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("private reasoning")
        );
        assert!(matches!(
            &assistant.content[0],
            LlmContent::Text { text } if text == "checking"
        ));
        assert!(
            assistant
                .content
                .iter()
                .any(|content| matches!(content, LlmContent::ToolCall { .. }))
        );
    }

    #[tokio::test]
    async fn parallel_tool_call_requested_events_produce_single_assistant_message() {
        use astrcode_core::{
            llm::{LlmContent, LlmRole},
            types::ToolCallId,
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let repo = test_repo(temp_dir.path().join("projects"));
        let session_id = SessionId::from("session-parallel");
        repo.create_session(&session_id, ".", "mock", None, None, None)
            .await
            .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "read files".into(),
            },
        ))
        .await
        .unwrap();

        // Simulate the event sequence from a parallel tool call batch:
        // ToolCallStarted + ToolCallRequested for each, then ToolCallCompleted for each.
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::ToolCallStarted {
                call_id: ToolCallId::from("call_1"),
                tool_name: "read".into(),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::ToolCallStarted {
                call_id: ToolCallId::from("call_2"),
                tool_name: "read".into(),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::ToolCallRequested {
                call_id: ToolCallId::from("call_1"),
                tool_name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::ToolCallRequested {
                call_id: ToolCallId::from("call_2"),
                tool_name: "read".into(),
                arguments: serde_json::json!({"path": "b.rs"}),
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::ToolCallCompleted {
                call_id: ToolCallId::from("call_1"),
                tool_name: "read".into(),
                result: astrcode_core::tool::ToolResult {
                    call_id: "call_1".into(),
                    content: "a".into(),
                    is_error: false,
                    error: None,
                    metadata: Default::default(),
                    duration_ms: None,
                },
                arguments: String::new(),
                arguments_json: None,
            },
        ))
        .await
        .unwrap();
        repo.append_event(Event::new(
            session_id.clone(),
            None,
            EventPayload::ToolCallCompleted {
                call_id: ToolCallId::from("call_2"),
                tool_name: "read".into(),
                result: astrcode_core::tool::ToolResult {
                    call_id: "call_2".into(),
                    content: "b".into(),
                    is_error: false,
                    error: None,
                    metadata: Default::default(),
                    duration_ms: None,
                },
                arguments: String::new(),
                arguments_json: None,
            },
        ))
        .await
        .unwrap();

        let model = repo.session_read_model(&session_id).await.unwrap();

        // Expected: [user] [assistant(tool_call_1, tool_call_2)] [tool_result_1] [tool_result_2]
        assert_eq!(model.messages.len(), 4);
        assert_eq!(model.messages[0].role, LlmRole::User);
        assert_eq!(model.messages[1].role, LlmRole::Assistant);

        // The assistant message must contain both tool calls merged into one message.
        let tool_call_count = model.messages[1]
            .content
            .iter()
            .filter(|c| matches!(c, LlmContent::ToolCall { .. }))
            .count();
        assert_eq!(
            tool_call_count, 2,
            "parallel tool calls must be merged into one assistant message"
        );

        assert_eq!(model.messages[2].role, LlmRole::Tool);
        assert_eq!(model.messages[3].role, LlmRole::Tool);

        // provider_messages should also be well-formed
        let provider_msgs = model.provider_messages();
        assert_eq!(provider_msgs.len(), 4);
    }
}

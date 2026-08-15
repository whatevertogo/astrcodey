//! 项目/会话目录扫描与路径布局。
//!
//! 目录结构:
//! `~/.astrcode/projects/<project>/sessions/<session>/[subagents/<extension>/<child>/...]`

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use astrcode_core::types::{SessionId, project_key_from_path};

use super::{FileSystemSessionRepository, validate_storage_session_id};
use crate::{SessionPathResolver, StorageError};

async fn directory_exists(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}

/// 判断目录是否位于 subagents 子树下。
fn is_subagent_dir(dir: &Path) -> bool {
    dir.ancestors()
        .any(|a| a.file_name().is_some_and(|n| n == "subagents"))
}

/// 会话目录内由存储层维护的元数据目录，扫描会话目录时必须跳过。
fn is_session_metadata_dir(name: &str) -> bool {
    matches!(
        name,
        ".recycled" | "subagents" | "snapshots" | "compact-snapshots" | "tool-results"
    )
}

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

impl FileSystemSessionRepository {
    /// 根据 `working_dir` 计算新会话的存储目录。
    fn session_dir_from_working_dir(&self, working_dir: &str, id: &SessionId) -> PathBuf {
        let project_key = project_key_from_path(&PathBuf::from(working_dir));
        self.projects_base
            .join(project_key)
            .join("sessions")
            .join(id.as_str())
    }

    /// 扫描 projects_base 下所有项目目录，返回其 sessions 子目录列表。
    pub(super) async fn all_session_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(&self.projects_base).await else {
            return roots;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let sessions_dir = entry.path().join("sessions");
            if directory_exists(&sessions_dir).await {
                roots.push(sessions_dir);
            }
        }
        roots
    }

    pub(super) async fn all_session_roots_strict(&self) -> Result<Vec<PathBuf>, StorageError> {
        let mut entries = match tokio::fs::read_dir(&self.projects_base).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut roots = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let sessions_dir = entry.path().join("sessions");
            match tokio::fs::metadata(&sessions_dir).await {
                Ok(metadata) if metadata.is_dir() => roots.push(sessions_dir),
                Ok(_) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        Ok(roots)
    }

    /// 在所有项目目录中查找指定会话的目录。
    ///
    /// 先检查 flat 位置（根 session），再递归搜索 `subagents/` 子目录树。
    pub(super) async fn find_session_dir(&self, id: &SessionId) -> Option<PathBuf> {
        if let Some(meta) = self.sessions.read().await.get(id).cloned() {
            return Some(meta.dir.clone());
        }

        for root in self.all_session_roots().await {
            let dir = root.join(id.as_str());
            if directory_exists(&dir).await {
                return Some(dir);
            }
            if let Some(found) = self.search_subagents_tree(&root, id).await {
                return Some(found);
            }
        }
        None
    }

    /// 递归搜索 `base` 下所有 session 目录的 `subagents/{extension}/` 子树。
    async fn search_subagents_tree(&self, base: &Path, id: &SessionId) -> Option<PathBuf> {
        let mut stack = vec![base.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let subagents = entry.path().join("subagents");
                if !directory_exists(&subagents).await {
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
                    if directory_exists(&candidate).await {
                        return Some(candidate);
                    }
                    stack.push(extension_dir);
                }
            }
        }
        None
    }

    pub(super) async fn existing_session_dir(
        &self,
        id: &SessionId,
    ) -> Result<PathBuf, StorageError> {
        self.find_session_dir(id)
            .await
            .ok_or_else(|| StorageError::NotFound(id.clone()))
    }

    pub(super) async fn new_session_dir(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        parent_session_id: Option<&SessionId>,
        source_extension: Option<&str>,
    ) -> Result<PathBuf, StorageError> {
        match parent_session_id {
            Some(parent_id) => {
                let extension = source_extension.ok_or_else(|| {
                    StorageError::InvalidId(
                        "child session requires source_extension to choose subagents directory"
                            .into(),
                    )
                })?;
                let parent_dir = self.existing_session_dir(parent_id).await?;
                let extension_dir = source_extension_dir_component(extension)?;
                Ok(parent_dir
                    .join("subagents")
                    .join(extension_dir)
                    .join(session_id.as_str()))
            },
            None => Ok(self.session_dir_from_working_dir(working_dir, session_id)),
        }
    }

    /// 在所有项目的 subagents/.recycled/{extension}/ 目录中搜索指定 session。
    pub(super) async fn find_recycled_session_dir(&self, id: &SessionId) -> Option<PathBuf> {
        for root in self.all_session_roots().await {
            if let Some(found) = self.search_recycled_in_root(&root, id).await {
                return Some(found);
            }
        }
        None
    }

    /// 搜索 sessions_root 下所有 session 的 subagents/.recycled/{extension}/{id}。
    async fn search_recycled_in_root(
        &self,
        sessions_root: &Path,
        id: &SessionId,
    ) -> Option<PathBuf> {
        let mut stack = vec![sessions_root.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let name = entry.file_name();
                if is_session_metadata_dir(&name.to_string_lossy()) {
                    continue;
                }
                let session_dir = entry.path();
                let recycled_dir = session_dir.join("subagents").join(".recycled");
                if directory_exists(&recycled_dir).await
                    && let Ok(mut extension_entries) = tokio::fs::read_dir(&recycled_dir).await
                {
                    while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                        let candidate = extension_entry.path().join(id.as_str());
                        if directory_exists(&candidate).await {
                            return Some(candidate);
                        }
                    }
                }
                let subagents = session_dir.join("subagents");
                if directory_exists(&subagents).await {
                    let Ok(mut extension_entries) = tokio::fs::read_dir(&subagents).await else {
                        continue;
                    };
                    while let Ok(Some(extension_entry)) = extension_entries.next_entry().await {
                        let pname = extension_entry.file_name();
                        if pname.to_string_lossy() == ".recycled" {
                            continue;
                        }
                        if extension_entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                            stack.push(extension_entry.path());
                        }
                    }
                }
            }
        }
        None
    }

    /// 列出根会话的 id → 目录映射,合并内存缓存与磁盘扫描,不打开任何文件。
    ///
    /// 子 agent 会话存放在 `subagents/<id>/`，由 `find_session_dir` 按需解析，
    /// 不出现在根会话列表中。磁盘扫描容错:不可读的项目目录被跳过。
    pub(super) async fn list_root_session_locations(&self) -> BTreeMap<SessionId, PathBuf> {
        let mut locations: BTreeMap<SessionId, PathBuf> = self
            .sessions
            .read()
            .await
            .iter()
            .filter(|(_, meta)| !is_subagent_dir(&meta.dir))
            .map(|(id, meta)| (id.clone(), meta.dir.clone()))
            .collect();
        for base_path in self.all_session_roots().await {
            self.collect_root_session_locations(&base_path, &mut locations)
                .await;
        }
        locations
    }

    /// 收集 `base` 下一层会话目录（不含 `subagents/` 等元数据目录）。
    async fn collect_root_session_locations(
        &self,
        base: &Path,
        locations: &mut BTreeMap<SessionId, PathBuf>,
    ) {
        let Ok(mut entries) = tokio::fs::read_dir(base).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if is_session_metadata_dir(&name_str) {
                continue;
            }
            locations
                .entry(SessionId::from(name_str.into_owned()))
                .or_insert_with(|| entry.path());
        }
    }

    /// Scans active root and nested subagent session directories without opening logs.
    pub(super) async fn list_all_session_locations(
        &self,
    ) -> Result<BTreeMap<SessionId, PathBuf>, StorageError> {
        let mut locations: BTreeMap<SessionId, PathBuf> = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(id, meta)| (id.clone(), meta.dir.clone()))
            .collect();
        for sessions_root in self.all_session_roots_strict().await? {
            let mut containers = vec![sessions_root];
            while let Some(container) = containers.pop() {
                let mut entries = match tokio::fs::read_dir(container).await {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                while let Some(entry) = entries.next_entry().await? {
                    if !entry.file_type().await?.is_dir() {
                        continue;
                    }
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if is_session_metadata_dir(&name) {
                        continue;
                    }
                    let session_id = SessionId::from(name.into_owned());
                    let session_dir = entry.path();
                    match tokio::fs::metadata(Self::event_log_path(&session_dir, &session_id)).await
                    {
                        Ok(metadata) if metadata.is_file() => {},
                        Ok(_) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    }
                    locations
                        .entry(session_id)
                        .or_insert_with(|| session_dir.clone());
                    let subagents = session_dir.join("subagents");
                    let mut extensions = match tokio::fs::read_dir(subagents).await {
                        Ok(entries) => entries,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    };
                    while let Some(extension) = extensions.next_entry().await? {
                        if extension.file_name().to_string_lossy() != ".recycled"
                            && extension.file_type().await?.is_dir()
                        {
                            containers.push(extension.path());
                        }
                    }
                }
            }
        }
        Ok(locations)
    }
}

#[async_trait::async_trait]
impl SessionPathResolver for FileSystemSessionRepository {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, StorageError> {
        Ok(self.find_session_dir(session_id).await)
    }

    async fn planned_session_store_dir(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        parent_session_id: Option<&SessionId>,
        source_extension: Option<&str>,
    ) -> Result<Option<PathBuf>, StorageError> {
        validate_storage_session_id(session_id)?;
        self.new_session_dir(session_id, working_dir, parent_session_id, source_extension)
            .await
            .map(Some)
    }
}

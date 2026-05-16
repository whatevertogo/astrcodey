//! 会话快照管理，用于加速恢复。
//!
//! 快照是恢复加速器，事件日志仍然是追加式的唯一数据源。
//! 快照不参与正常的追加 seq 分配。

use std::{cmp::Reverse, path::PathBuf};

use astrcode_core::storage::{SessionReadModel, StorageError};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const SNAPSHOT_VERSION: u32 = 1;

/// 保留的最大快照数量。创建新快照后自动清理超出数量的旧快照。
const MAX_SNAPSHOTS: usize = 4;

/// Projection snapshot persisted by astrcode-storage.
///
/// This format is internal to storage. It is a recovery accelerator, not a
/// protocol DTO or a replacement for the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionProjectionSnapshot {
    pub(crate) version: u32,
    pub(crate) cursor: String,
    pub(crate) latest_seq: Option<u64>,
    pub(crate) created_at: String,
    pub(crate) model: SessionReadModel,
}

/// 快照管理器，负责创建和列出会话恢复点。
///
/// 快照文件存储在会话目录的 `snapshots/` 子目录中，
/// 文件名格式为 `snapshot-<cursor>.json`。
pub struct SnapshotManager {
    /// 快照存储目录
    dir: PathBuf,
}

impl SnapshotManager {
    /// 创建新的快照管理器。
    ///
    /// # 参数
    /// - `dir`: 快照存储目录路径
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// 为当前会话读模型创建 projection 快照。
    pub async fn create_snapshot(&self, model: &SessionReadModel) -> Result<(), StorageError> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let cursor = model.cursor();
        let snapshot = SessionProjectionSnapshot {
            version: SNAPSHOT_VERSION,
            cursor: cursor.clone(),
            latest_seq: model.latest_seq,
            created_at: Utc::now().to_rfc3339(),
            model: model.clone(),
        };
        let path = self.dir.join(format!("snapshot-{}.json", cursor));
        let temp_path = self
            .dir
            .join(format!(".snapshot-{}-{}.tmp", cursor, Uuid::new_v4()));
        let content = serde_json::to_vec_pretty(&snapshot)?;
        tokio::fs::write(&temp_path, content).await?;
        if let Err(e) = tokio::fs::remove_file(&path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            }
        }
        tokio::fs::rename(&temp_path, &path).await?;
        self.prune_old_snapshots().await?;
        Ok(())
    }

    /// 返回最新的有效 projection 快照。
    ///
    /// 损坏或版本不匹配的快照会被跳过，继续尝试更旧的快照；全部不可用时返回
    /// `Ok(None)`，由仓库回退到完整事件重放。
    pub(crate) async fn latest_snapshot(
        &self,
    ) -> Result<Option<SessionProjectionSnapshot>, StorageError> {
        let mut candidates = self.snapshot_candidates().await?;
        candidates.sort_by_key(|candidate| Reverse(candidate.cursor));

        for candidate in candidates {
            match self.read_snapshot(&candidate).await {
                Ok(snapshot) => return Ok(Some(snapshot)),
                Err(message) => {
                    tracing::warn!(
                        path = %candidate.path.display(),
                        "Ignoring invalid projection snapshot: {message}"
                    );
                },
            }
        }

        Ok(None)
    }

    /// 列出所有可用的快照文件名，按名称排序。
    pub async fn list_snapshots(&self) -> Result<Vec<String>, StorageError> {
        if !self.dir.exists() {
            return Ok(vec![]);
        }
        let mut snapshots = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    snapshots.push(name.to_string());
                }
            }
        }
        snapshots.sort();
        Ok(snapshots)
    }

    /// 清理旧快照，只保留最新的 [`MAX_SNAPSHOTS`] 个。
    async fn prune_old_snapshots(&self) -> Result<(), StorageError> {
        let mut candidates = self.snapshot_candidates().await?;
        if candidates.len() <= MAX_SNAPSHOTS {
            return Ok(());
        }
        // 按 cursor 降序排列，删除超出保留数量的旧快照
        candidates.sort_by_key(|c| Reverse(c.cursor));
        for old in candidates.into_iter().skip(MAX_SNAPSHOTS) {
            if let Err(e) = tokio::fs::remove_file(&old.path).await {
                tracing::warn!(
                    path = %old.path.display(),
                    "Failed to remove old snapshot: {e}"
                );
            }
        }
        Ok(())
    }

    async fn snapshot_candidates(&self) -> Result<Vec<SnapshotCandidate>, StorageError> {
        if !self.dir.exists() {
            return Ok(vec![]);
        }

        let mut candidates = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            let Some(cursor) = parse_snapshot_cursor(name_str) else {
                continue;
            };
            candidates.push(SnapshotCandidate {
                cursor,
                name: name_str.to_owned(),
                path: entry.path(),
            });
        }
        Ok(candidates)
    }

    async fn read_snapshot(
        &self,
        candidate: &SnapshotCandidate,
    ) -> Result<SessionProjectionSnapshot, String> {
        let content = tokio::fs::read_to_string(&candidate.path)
            .await
            .map_err(|error| error.to_string())?;
        let snapshot: SessionProjectionSnapshot =
            serde_json::from_str(&content).map_err(|error| error.to_string())?;
        validate_snapshot(&snapshot, candidate)?;
        Ok(snapshot)
    }
}

#[derive(Debug)]
struct SnapshotCandidate {
    cursor: u64,
    name: String,
    path: PathBuf,
}

fn parse_snapshot_cursor(name: &str) -> Option<u64> {
    name.strip_prefix("snapshot-")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

fn validate_snapshot(
    snapshot: &SessionProjectionSnapshot,
    candidate: &SnapshotCandidate,
) -> Result<(), String> {
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(format!("unsupported version {}", snapshot.version));
    }
    let file_cursor = candidate.cursor.to_string();
    if snapshot.cursor != file_cursor {
        return Err(format!(
            "snapshot cursor {} does not match file {}",
            snapshot.cursor, candidate.name
        ));
    }
    if snapshot.cursor != snapshot.model.cursor() {
        return Err(format!(
            "snapshot cursor {} does not match model cursor {}",
            snapshot.cursor,
            snapshot.model.cursor()
        ));
    }
    if snapshot.latest_seq != snapshot.model.latest_seq {
        return Err(format!(
            "snapshot latest_seq {:?} does not match model latest_seq {:?}",
            snapshot.latest_seq, snapshot.model.latest_seq
        ));
    }
    Ok(())
}

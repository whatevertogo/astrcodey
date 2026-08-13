//! 会话快照管理，用于加速恢复。
//!
//! 快照是恢复加速器，事件日志仍然是追加式的唯一数据源。
//! 快照不参与正常的追加 seq 分配。

use std::{cmp::Reverse, path::PathBuf};

use astrcode_session_projection::SessionReadModel;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::StorageError;

const SNAPSHOT_VERSION: u32 = 5;

/// 保留的最大快照数量。创建新快照后自动清理超出数量的旧快照。
const MAX_SNAPSHOTS: usize = 4;

/// Projection snapshot persisted by astrcode-storage.
///
/// This format is internal to storage. It is a recovery accelerator, not a
/// protocol DTO or a replacement for the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionProjectionSnapshot {
    pub(crate) version: u32,
    pub(crate) created_at: String,
    pub(crate) model: SessionReadModel,
}

/// 快照管理器，负责创建和读取会话恢复点。
///
/// 快照文件存储在会话目录的 `snapshots/` 子目录中，
/// 文件名格式为 `snapshot-<cursor>.json`。
pub(crate) struct SnapshotManager {
    /// 快照存储目录
    dir: PathBuf,
}

impl SnapshotManager {
    /// 创建新的快照管理器。
    ///
    /// # 参数
    /// - `dir`: 快照存储目录路径
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// 为当前会话读模型创建 projection 快照。
    pub(crate) async fn create_snapshot(
        &self,
        model: SessionReadModel,
    ) -> Result<(), StorageError> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let cursor = model.cursor();
        let path = self.dir.join(format!("snapshot-{}.json", cursor));
        let temp_path = self
            .dir
            .join(format!(".snapshot-{}-{}.tmp", cursor, Uuid::new_v4()));
        let snapshot = SessionProjectionSnapshot {
            version: SNAPSHOT_VERSION,
            created_at: Utc::now().to_rfc3339(),
            model,
        };
        let content = serde_json::to_vec_pretty(&snapshot)?;
        tokio::fs::write(&temp_path, content).await?;
        if let Err(e) = tokio::fs::remove_file(&path).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.into());
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
    if file_cursor != snapshot.model.cursor() {
        return Err(format!(
            "snapshot cursor {} does not match model cursor {} (created_at={})",
            file_cursor,
            snapshot.model.cursor(),
            snapshot.created_at
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use astrcode_core::{event::StoredEvent, types::SessionId};
    use astrcode_session_projection::replay;
    use serde_json::Value;
    use tempfile::tempdir;

    use super::SnapshotManager;
    use crate::test_support::started_event;

    #[tokio::test]
    async fn v5_snapshot_round_trips_and_v4_snapshot_is_ignored() {
        let temp_dir = tempdir().unwrap();
        let session_id = SessionId::new("snapshot-test");
        let model = replay(
            session_id.clone(),
            &[StoredEvent::new(0, started_event(&session_id))],
        )
        .unwrap();
        let manager = SnapshotManager::new(temp_dir.path().to_path_buf());

        manager.create_snapshot(model.clone()).await.unwrap();
        let snapshot = manager.latest_snapshot().await.unwrap().unwrap();
        assert_eq!(snapshot.model, model);

        let path = temp_dir
            .path()
            .join(format!("snapshot-{}.json", model.cursor()));
        let mut json: Value =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        json["version"] = Value::from(4);
        tokio::fs::write(&path, serde_json::to_vec(&json).unwrap())
            .await
            .unwrap();

        assert!(manager.latest_snapshot().await.unwrap().is_none());
    }
}

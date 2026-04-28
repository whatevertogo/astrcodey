//! 会话快照管理，用于加速恢复。
//!
//! 快照是恢复加速器，事件日志仍然是追加式的唯一数据源。
//! 快照不参与正常的追加 seq 分配。

use std::path::PathBuf;

use astrcode_core::{storage::StorageError, types::Cursor};

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

    /// 在指定游标位置创建快照。
    ///
    /// # 参数
    /// - `cursor`: 快照对应的游标位置（通常是事件 seq）
    pub async fn create_snapshot(&self, cursor: &Cursor) -> Result<(), StorageError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("snapshot-{}.json", cursor));
        // TODO: Write actual session state snapshot
        std::fs::write(&path, "{}")?;
        Ok(())
    }

    /// 列出所有可用的快照文件名，按名称排序。
    pub async fn list_snapshots(&self) -> Result<Vec<String>, StorageError> {
        if !self.dir.exists() {
            return Ok(vec![]);
        }
        let mut snapshots = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    snapshots.push(name.to_string());
                }
            }
        }
        snapshots.sort();
        Ok(snapshots)
    }
}

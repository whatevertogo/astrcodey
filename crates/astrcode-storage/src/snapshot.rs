//! Session snapshot management for fast recovery.

use std::path::PathBuf;

use astrcode_core::{storage::StorageError, types::Cursor};

pub struct SnapshotManager {
    dir: PathBuf,
}

impl SnapshotManager {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub async fn create_snapshot(&self, cursor: &Cursor) -> Result<(), StorageError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("snapshot-{}.json", cursor));
        // TODO: Write actual session state snapshot
        std::fs::write(&path, "{}")?;
        Ok(())
    }

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

//! File access tracking for post-compaction recovery.

use std::collections::HashMap;

pub struct FileAccessTracker {
    accessed: HashMap<String, String>,
    max_tracked: usize,
}

impl FileAccessTracker {
    pub fn new(max_tracked: usize) -> Self {
        Self {
            accessed: HashMap::new(),
            max_tracked,
        }
    }

    /// Record a file access.
    pub fn record(&mut self, path: &str, content: &str) {
        if self.accessed.len() < self.max_tracked {
            self.accessed.insert(path.into(), content.into());
        }
    }

    /// Get tracked files for recovery.
    pub fn get_tracked(&self) -> Vec<(String, String)> {
        self.accessed
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

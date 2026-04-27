//! File access tracking for post-compaction recovery.

use std::collections::VecDeque;

/// Tracks recently accessed files with FIFO eviction when capacity is reached.
pub struct FileAccessTracker {
    order: VecDeque<String>,
    max_tracked: usize,
}

impl FileAccessTracker {
    pub fn new(max_tracked: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(max_tracked),
            max_tracked,
        }
    }

    /// Record a file access. Evicts the oldest entry when at capacity.
    pub fn record(&mut self, path: &str) {
        // Remove previous entry if re-accessed (move to end)
        if let Some(pos) = self.order.iter().position(|p| p == path) {
            self.order.remove(pos);
        } else if self.order.len() >= self.max_tracked {
            self.order.pop_front();
        }
        self.order.push_back(path.into());
    }

    /// Get tracked file paths, most recent first.
    pub fn get_tracked(&self) -> Vec<String> {
        self.order.iter().rev().cloned().collect()
    }
}

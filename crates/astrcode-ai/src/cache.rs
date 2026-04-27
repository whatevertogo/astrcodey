//! Prompt caching awareness and cache hit/miss tracking.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

/// Tracks cache breakpoints between requests.
///
/// Detects when system prompts, tool schemas, or other cacheable
/// content changes, triggering re-caching.
pub struct CacheTracker {
    last_system_hash: Option<u64>,
    last_tool_hash: Option<u64>,
}

impl CacheTracker {
    pub fn new() -> Self {
        Self {
            last_system_hash: None,
            last_tool_hash: None,
        }
    }

    /// Check if the system prompt cache is still valid.
    pub fn check_system_cache(&mut self, content: &str) -> CacheStatus {
        let hash = Self::hash(content);
        let is_hit = self.last_system_hash == Some(hash);
        self.last_system_hash = Some(hash);
        if is_hit {
            CacheStatus::Hit
        } else {
            CacheStatus::Miss
        }
    }

    /// Check if the tool schema cache is still valid.
    pub fn check_tool_cache(&mut self, tools_json: &str) -> CacheStatus {
        let hash = Self::hash(tools_json);
        let is_hit = self.last_tool_hash == Some(hash);
        self.last_tool_hash = Some(hash);
        if is_hit {
            CacheStatus::Hit
        } else {
            CacheStatus::Miss
        }
    }

    fn hash(s: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

impl Default for CacheTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    Hit,
    Miss,
}

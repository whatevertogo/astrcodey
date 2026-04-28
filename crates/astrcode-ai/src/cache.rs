//! Prompt 缓存感知与缓存命中/未命中追踪。
//!
//! 通过哈希比较检测 system prompt、tool schema 等可缓存内容是否发生变化，
//! 从而决定是否需要重新设置缓存断点。

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

/// 跨请求追踪缓存断点状态。
///
/// 检测 system prompt、tool schema 或其他可缓存内容是否发生变化，
/// 当内容变化时触发重新缓存。
pub struct CacheTracker {
    /// 上一次 system prompt 的哈希值
    last_system_hash: Option<u64>,
    /// 上一次 tool schema 的哈希值
    last_tool_hash: Option<u64>,
}

impl CacheTracker {
    /// 创建一个新的缓存追踪器，初始状态为无缓存。
    pub fn new() -> Self {
        Self {
            last_system_hash: None,
            last_tool_hash: None,
        }
    }

    /// 检查 system prompt 缓存是否仍然有效。
    ///
    /// 对传入的 `content` 计算哈希并与上次比较，相同则命中缓存。
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

    /// 检查 tool schema 缓存是否仍然有效。
    ///
    /// 对传入的 `tools_json` 计算哈希并与上次比较，相同则命中缓存。
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

    /// 使用 `DefaultHasher` 对字符串计算 u64 哈希值。
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

/// 缓存命中状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    /// 缓存命中：内容与上次请求相同，可复用已缓存的 prompt 前缀
    Hit,
    /// 缓存未命中：内容已变化，需要重新缓存
    Miss,
}

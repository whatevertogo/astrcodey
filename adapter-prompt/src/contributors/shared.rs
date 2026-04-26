//! Shared utilities for prompt contributors.

//! 贡献者共享工具函数。
//!
//! 提供路径解析和缓存标记生成等跨 contributor 复用的基础设施。

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use astrcode_core::env::{ASTRCODE_HOME_DIR_ENV, ASTRCODE_TEST_HOME_ENV};
use log::warn;

/// Resolves a path to a file under `~/.astrcode/` (or the configured home override).
///
/// Checks `ASTRCODE_HOME_DIR`, then `ASTRCODE_TEST_HOME` (for tests),
/// then falls back to `dirs::home_dir()`.
pub fn user_astrcode_file_path(filename: &str) -> Option<PathBuf> {
    if let Some(home) = std::env::var_os(ASTRCODE_HOME_DIR_ENV) {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join(".astrcode").join(filename));
        }
    }

    if let Some(home) = std::env::var_os(ASTRCODE_TEST_HOME_ENV) {
        return Some(PathBuf::from(home).join(".astrcode").join(filename));
    }

    match dirs::home_dir() {
        Some(home) => Some(home.join(".astrcode").join(filename)),
        None => {
            warn!("failed to resolve home dir for {filename}");
            None
        },
    }
}

/// Returns a cache marker for the given path based on file metadata.
///
/// Used by contributors to detect file changes for cache invalidation.
pub fn cache_marker_for_path(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();

            format!("present:{}:{modified}", metadata.len())
        },
        Err(_) => "missing".to_string(),
    }
}

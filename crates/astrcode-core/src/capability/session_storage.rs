//! Session 存储路径能力。
//!
//! 允许扩展获取当前 session 在存储层的真实目录路径，
//! 用于读写附属数据文件（如 todo、mode state、plan 等）。

use std::path::PathBuf;
use std::sync::Arc;

use super::Capability;

/// Session 存储路径能力的 newtype 包装。
///
/// 扩展通过 `ctx.get_capability::<SessionStorageCap>()` 获取，
/// 然后调用 `cap.session_store_dir()` 拿到路径。
pub struct SessionStorageCap(Arc<dyn SessionStorageInner>);

impl SessionStorageCap {
    pub fn new(inner: Arc<dyn SessionStorageInner>) -> Self {
        Self(inner)
    }
}

impl Capability for SessionStorageCap {}

impl SessionStorageCap {
    /// 返回当前 session 在存储层的真实目录路径。
    pub fn session_store_dir(&self) -> PathBuf {
        self.0.session_store_dir()
    }
}

impl std::fmt::Debug for SessionStorageCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStorageCap").finish()
    }
}

/// Session 存储路径的能力接口。由宿主侧实现。
pub trait SessionStorageInner: Send + Sync + 'static {
    /// 返回当前 session 在存储层的真实目录路径。
    fn session_store_dir(&self) -> PathBuf;
}

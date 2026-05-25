//! SessionStorageInner 的服务端实现。

use std::path::PathBuf;
use std::sync::Arc;

use astrcode_core::capability::{SessionStorageCap, SessionStorageInner};

/// 将 per-session 路径适配为 `SessionStorageInner`。
pub struct ServerSessionStorage {
    dir: PathBuf,
}

impl ServerSessionStorage {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn as_capability(self: &Arc<Self>) -> Arc<SessionStorageCap> {
        Arc::new(SessionStorageCap::new(self.clone()))
    }
}

impl SessionStorageInner for ServerSessionStorage {
    fn session_store_dir(&self) -> PathBuf {
        self.dir.clone()
    }
}

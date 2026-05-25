//! SmallModelIdInner 的服务端实现。

use std::sync::Arc;

use astrcode_core::capability::{SmallModelIdCap, SmallModelIdInner};
use astrcode_session::SessionRuntimeServices;

/// 从 `SessionRuntimeServices` 读取 small model ID。
pub struct ServerSmallModelId {
    services: Arc<SessionRuntimeServices>,
}

impl ServerSmallModelId {
    pub fn new(services: Arc<SessionRuntimeServices>) -> Self {
        Self { services }
    }

    pub fn as_capability(self: &Arc<Self>) -> Arc<SmallModelIdCap> {
        Arc::new(SmallModelIdCap::new(self.clone()))
    }
}

impl SmallModelIdInner for ServerSmallModelId {
    fn small_model_id(&self) -> String {
        self.services
            .read_effective()
            .small_llm
            .model_id
            .clone()
    }
}

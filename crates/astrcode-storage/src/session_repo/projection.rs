//! 由事件日志同步维护的会话读模型缓存。

use std::sync::Arc;

use astrcode_core::event::{DurableEvent, StoredEvent};
use astrcode_session_projection::{PreparedProjectionBatch, ProjectionError, SessionReadModel};
use tokio::sync::RwLock;

use super::invalid_event;
use crate::StorageError;

/// `Arc` 快照式读模型:读者零锁拷贝,提交时整体替换。
pub(super) struct SessionProjection {
    model: RwLock<Arc<SessionReadModel>>,
}

impl SessionProjection {
    pub(super) fn new(model: SessionReadModel) -> Self {
        Self {
            model: RwLock::new(Arc::new(model)),
        }
    }

    pub(super) async fn snapshot(&self) -> Arc<SessionReadModel> {
        let model = self.model.read().await;
        Arc::clone(&model)
    }

    pub(super) async fn prepare_batch(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<PreparedProjectionBatch, StorageError> {
        let model = self.model.read().await;
        PreparedProjectionBatch::prepare(model.as_ref(), events).map_err(|error| match error {
            ProjectionError::SequenceOverflow => StorageError::CorruptLog(error.to_string()),
            error => invalid_event(error),
        })
    }

    pub(super) async fn apply_committed(&self, batch: PreparedProjectionBatch) -> Vec<StoredEvent> {
        let mut model = self.model.write().await;
        batch.apply(&mut model)
    }
}

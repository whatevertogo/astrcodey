use std::{collections::HashMap, sync::Arc};

use astrcode_core::types::SessionId;
use parking_lot::Mutex;

use crate::SessionRuntimeState;

#[derive(Clone, Default)]
pub struct SessionResourceStore {
    entries: Arc<Mutex<HashMap<SessionId, Arc<SessionRuntimeState>>>>,
}

impl SessionResourceStore {
    /// 返回 sid 的进程内资源，并将其保留到显式 cleanup。
    pub fn resources_for(
        &self,
        session_id: &SessionId,
        create: impl FnOnce() -> Arc<SessionRuntimeState>,
    ) -> Arc<SessionRuntimeState> {
        let mut entries = self.entries.lock();
        Arc::clone(entries.entry(session_id.clone()).or_insert_with(create))
    }

    pub fn cleanup(&self, session_id: &SessionId) {
        self.entries.lock().remove(session_id);
    }
}

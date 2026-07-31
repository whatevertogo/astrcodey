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
        self.resources_for_with_status(session_id, create).0
    }

    pub fn resources_for_with_status(
        &self,
        session_id: &SessionId,
        create: impl FnOnce() -> Arc<SessionRuntimeState>,
    ) -> (Arc<SessionRuntimeState>, bool) {
        let mut entries = self.entries.lock();
        match entries.entry(session_id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let runtime = create();
                entry.insert(Arc::clone(&runtime));
                (runtime, true)
            },
        }
    }

    pub fn cleanup(&self, session_id: &SessionId) {
        self.entries.lock().remove(session_id);
    }
}

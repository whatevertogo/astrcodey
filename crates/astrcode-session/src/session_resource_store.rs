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
        match entries.entry(session_id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => Arc::clone(entry.get()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let runtime = create();
                entry.insert(Arc::clone(&runtime));
                runtime
            },
        }
    }

    /// 仅当资源表是 `runtime` 之外的唯一持有者时移除该实例。
    pub fn cleanup_if_unshared(
        &self,
        session_id: &SessionId,
        runtime: &Arc<SessionRuntimeState>,
    ) -> bool {
        let mut entries = self.entries.lock();
        let Some(stored) = entries.get(session_id) else {
            return false;
        };
        if !Arc::ptr_eq(stored, runtime) || Arc::strong_count(stored) != 2 {
            return false;
        }
        entries.remove(session_id);
        true
    }

    pub fn cleanup(&self, session_id: &SessionId) {
        self.entries.lock().remove(session_id);
    }
}

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{tool::FileObservationStore, types::SessionId};
use parking_lot::Mutex;

use crate::tool_exec::InMemoryFileObservationStore;

/// 单个 session 在当前进程内持有的瞬态状态。
///
/// 这里的状态随 session 生命周期存在，但不属于可持久化事实。
pub struct SessionRuntimeState {
    file_observation_store: Arc<dyn FileObservationStore>,
}

impl Default for SessionRuntimeState {
    fn default() -> Self {
        Self {
            file_observation_store: Arc::new(InMemoryFileObservationStore::default()),
        }
    }
}

impl SessionRuntimeState {
    pub fn file_observation_store(&self) -> Arc<dyn FileObservationStore> {
        Arc::clone(&self.file_observation_store)
    }
}

/// 当前 server 进程中按 session 复用的瞬态状态注册表。
#[derive(Default)]
pub struct SessionRuntimeRegistry {
    states: Mutex<HashMap<SessionId, Arc<SessionRuntimeState>>>,
}

impl SessionRuntimeRegistry {
    pub fn get_or_create(&self, session_id: &SessionId) -> Arc<SessionRuntimeState> {
        let mut states = self.states.lock();
        states
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(SessionRuntimeState::default()))
            .clone()
    }

    pub fn remove(&self, session_id: &SessionId) {
        self.states.lock().remove(session_id);
    }
}

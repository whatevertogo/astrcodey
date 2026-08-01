use std::{collections::HashMap, sync::Arc};

use astrcode_core::types::SessionId;
use parking_lot::Mutex;

use crate::SessionRuntimeState;

/// [`SessionResourceStore::cleanup_if_unshared`] 期望的 strong_count：1 份来自资源表，
/// 1 份来自调用方。任何第三方多持一份引用都会使计数超过该值，清理将静默失败——
/// 这是有意为之（不能移除仍被使用的 runtime），因此失败时记 warn 日志便于排查。
const EXPECTED_REFCOUNT: usize = 2;

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
    ///
    /// 与 [`Self::cleanup`] 的区别：本方法在移除前校验表中实例与调用方传入的
    /// `runtime` 是同一实例，且没有第三方额外持有引用（`Arc::strong_count` 恰好为
    /// [`EXPECTED_REFCOUNT`]）；不满足任一条件即视为"仍在使用"，跳过移除并返回
    /// false。适用于会话正常结束、按引用计数决定是否释放资源的路径。
    pub fn cleanup_if_unshared(
        &self,
        session_id: &SessionId,
        runtime: &Arc<SessionRuntimeState>,
    ) -> bool {
        let mut entries = self.entries.lock();
        let Some(stored) = entries.get(session_id) else {
            return false;
        };
        if !Arc::ptr_eq(stored, runtime) {
            // 表中已是另一个实例（旧实例被清理后重建），与本次调用无关。
            return false;
        }
        let strong_count = Arc::strong_count(stored);
        if strong_count != EXPECTED_REFCOUNT {
            tracing::warn!(
                session_id = %session_id,
                strong_count,
                expected = EXPECTED_REFCOUNT,
                "session runtime still referenced elsewhere; skipping cleanup"
            );
            return false;
        }
        entries.remove(session_id);
        true
    }

    /// 无条件移除表中条目，不校验引用计数。
    ///
    /// 仅用于确定该 runtime 不再被任何调用方使用的场景（例如子会话创建失败后的
    /// 补偿路径已确认没有其他句柄）；否则应使用 [`Self::cleanup_if_unshared`]。
    pub fn cleanup(&self, session_id: &SessionId) {
        self.entries.lock().remove(session_id);
    }
}

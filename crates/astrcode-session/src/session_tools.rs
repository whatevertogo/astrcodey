use std::{sync::Arc, time::Duration};

use astrcode_core::extension::SessionToolSelection;
use astrcode_extension_sdk::runtime_ports::ToolCatalogCompleteness;
use parking_lot::Mutex;
use tokio::{sync::watch, time::Instant};

use crate::ToolRegistry;

const PARTIAL_CATALOG_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ToolRegistryCacheKey {
    pub(crate) runtime_generation: u64,
    pub(crate) tool_pack_versions: Vec<u64>,
    pub(crate) working_dir: String,
    pub(crate) tool_selection: Option<SessionToolSelection>,
}

struct CachedToolRegistry {
    key: ToolRegistryCacheKey,
    registry: Arc<ToolRegistry>,
    retry_after: Option<Instant>,
}

#[derive(Default)]
struct ToolCacheState {
    current: Option<CachedToolRegistry>,
    active_build: Option<watch::Sender<bool>>,
}

pub(crate) enum ToolCacheLookup<'a> {
    Hit(Arc<ToolRegistry>),
    Build(ToolCacheBuildPermit<'a>),
    Wait(watch::Receiver<bool>),
}

pub(crate) struct ToolCacheBuildPermit<'a> {
    cache: &'a SessionToolCache,
    completed: bool,
}

impl ToolCacheBuildPermit<'_> {
    pub(crate) fn complete(
        mut self,
        key: ToolRegistryCacheKey,
        registry: Arc<ToolRegistry>,
        completeness: ToolCatalogCompleteness,
    ) {
        let retry_after = (completeness == ToolCatalogCompleteness::Partial)
            .then(|| Instant::now() + self.cache.partial_retry_after);
        let notification = {
            let mut state = self.cache.state.lock();
            state.current = Some(CachedToolRegistry {
                key,
                registry,
                retry_after,
            });
            state.active_build.take()
        };
        self.completed = true;
        notify_build_waiters(notification);
    }
}

impl Drop for ToolCacheBuildPermit<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let notification = self.cache.state.lock().active_build.take();
        notify_build_waiters(notification);
    }
}

fn notify_build_waiters(notification: Option<watch::Sender<bool>>) {
    if let Some(notification) = notification {
        notification.send_replace(true);
    }
}

pub(crate) struct SessionToolCache {
    state: Mutex<ToolCacheState>,
    partial_retry_after: Duration,
}

impl SessionToolCache {
    pub(crate) fn new() -> Self {
        Self::with_partial_retry_after(PARTIAL_CATALOG_RETRY_AFTER)
    }

    fn with_partial_retry_after(partial_retry_after: Duration) -> Self {
        Self {
            state: Mutex::new(ToolCacheState::default()),
            partial_retry_after,
        }
    }

    pub(crate) fn lookup_or_reserve(&self, key: &ToolRegistryCacheKey) -> ToolCacheLookup<'_> {
        let mut state = self.state.lock();
        if let Some(registry) = state.current.as_ref().and_then(|cached| {
            (&cached.key == key
                && cached
                    .retry_after
                    .is_none_or(|retry_after| Instant::now() < retry_after))
            .then(|| Arc::clone(&cached.registry))
        }) {
            return ToolCacheLookup::Hit(registry);
        }
        if let Some(active_build) = &state.active_build {
            return ToolCacheLookup::Wait(active_build.subscribe());
        }

        let (notification, _receiver) = watch::channel(false);
        state.active_build = Some(notification);
        ToolCacheLookup::Build(ToolCacheBuildPermit {
            cache: self,
            completed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_key() -> ToolRegistryCacheKey {
        ToolRegistryCacheKey {
            runtime_generation: 1,
            tool_pack_versions: Vec::new(),
            working_dir: ".".into(),
            tool_selection: None,
        }
    }

    #[tokio::test]
    async fn cache_retries_partial_and_abandoned_builds_but_keeps_complete_snapshots() {
        let cache = SessionToolCache::with_partial_retry_after(Duration::ZERO);
        let key = cache_key();

        let ToolCacheLookup::Build(build) = cache.lookup_or_reserve(&key) else {
            panic!("first lookup must reserve the build");
        };
        let ToolCacheLookup::Wait(mut waiter) = cache.lookup_or_reserve(&key) else {
            panic!("concurrent lookup must wait for the active build");
        };
        drop(build);
        waiter.changed().await.unwrap();

        let ToolCacheLookup::Build(build) = cache.lookup_or_reserve(&key) else {
            panic!("abandoned build must release the reservation");
        };
        build.complete(
            key.clone(),
            Arc::new(ToolRegistry::new()),
            ToolCatalogCompleteness::Partial,
        );
        assert!(matches!(
            cache.lookup_or_reserve(&key),
            ToolCacheLookup::Build(_)
        ));

        let ToolCacheLookup::Build(build) = cache.lookup_or_reserve(&key) else {
            panic!("expired partial snapshot must reserve a rebuild");
        };
        build.complete(
            key.clone(),
            Arc::new(ToolRegistry::new()),
            ToolCatalogCompleteness::Complete,
        );
        assert!(matches!(
            cache.lookup_or_reserve(&key),
            ToolCacheLookup::Hit(_)
        ));
    }
}

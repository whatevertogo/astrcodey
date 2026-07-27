use std::{sync::Arc, time::Duration};

use astrcode_core::extension::SessionToolSelection;
use astrcode_extension_sdk::runtime_ports::ToolCatalogCompleteness;
use parking_lot::Mutex;
use tokio::{sync::watch, time::Instant};

use crate::ToolRegistry;

const PARTIAL_CATALOG_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct BaseToolRegistryKey {
    pub(crate) runtime_generation: u64,
    pub(crate) tool_pack_versions: Vec<u64>,
    pub(crate) working_dir: String,
}

struct CachedBaseToolRegistry {
    key: BaseToolRegistryKey,
    registry: Arc<ToolRegistry>,
    retry_after: Option<Instant>,
}

struct CachedFilteredToolRegistry {
    base_registry: Arc<ToolRegistry>,
    selection: SessionToolSelection,
    registry: Arc<ToolRegistry>,
}

#[derive(Default)]
struct ToolCacheState {
    base: Option<CachedBaseToolRegistry>,
    filtered: Option<CachedFilteredToolRegistry>,
    active_build: Option<watch::Sender<bool>>,
}

pub(crate) enum ToolCacheLookup<'a> {
    Hit(Arc<ToolRegistry>),
    Build(ToolCacheBuildPermit<'a>),
    Wait(watch::Receiver<bool>),
}

pub(crate) struct ToolCacheBuildPermit<'a> {
    cache: &'a SessionToolCache,
    key: BaseToolRegistryKey,
    completed: bool,
}

impl ToolCacheBuildPermit<'_> {
    pub(crate) fn complete(
        mut self,
        registry: Arc<ToolRegistry>,
        completeness: ToolCatalogCompleteness,
    ) {
        let retry_after = (completeness == ToolCatalogCompleteness::Partial)
            .then(|| Instant::now() + self.cache.partial_retry_after);
        let notification = {
            let mut state = self.cache.state.lock();
            state.base = Some(CachedBaseToolRegistry {
                key: self.key.clone(),
                registry,
                retry_after,
            });
            state.filtered = None;
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

    pub(crate) fn lookup_or_reserve(&self, key: &BaseToolRegistryKey) -> ToolCacheLookup<'_> {
        let mut state = self.state.lock();
        if let Some(registry) = state.base.as_ref().and_then(|cached| {
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
            key: key.clone(),
            completed: false,
        })
    }

    pub(crate) fn filtered_registry(
        &self,
        base_registry: Arc<ToolRegistry>,
        selection: Option<&SessionToolSelection>,
    ) -> Arc<ToolRegistry> {
        let Some(selection) = selection else {
            return base_registry;
        };
        if matches!(
            selection,
            SessionToolSelection::All { except } if except.is_empty()
        ) {
            return base_registry;
        }
        let mut state = self.state.lock();
        if let Some(registry) = state.filtered.as_ref().and_then(|cached| {
            (Arc::ptr_eq(&cached.base_registry, &base_registry) && &cached.selection == selection)
                .then(|| Arc::clone(&cached.registry))
        }) {
            return registry;
        }

        let registry = Arc::new(base_registry.filtered(selection));
        if state
            .base
            .as_ref()
            .is_some_and(|cached| Arc::ptr_eq(&cached.registry, &base_registry))
        {
            state.filtered = Some(CachedFilteredToolRegistry {
                base_registry,
                selection: selection.clone(),
                registry: Arc::clone(&registry),
            });
        }
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_key() -> BaseToolRegistryKey {
        BaseToolRegistryKey {
            runtime_generation: 1,
            tool_pack_versions: Vec::new(),
            working_dir: ".".into(),
        }
    }

    #[tokio::test]
    async fn cache_retries_incomplete_builds_then_derives_filtered_snapshots() {
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
            Arc::new(ToolRegistry::new()),
            ToolCatalogCompleteness::Complete,
        );
        let ToolCacheLookup::Hit(base_registry) = cache.lookup_or_reserve(&key) else {
            panic!("complete base snapshot must stay cached");
        };
        let unrestricted = SessionToolSelection::All { except: Vec::new() };
        let unrestricted_registry =
            cache.filtered_registry(Arc::clone(&base_registry), Some(&unrestricted));
        assert!(Arc::ptr_eq(&base_registry, &unrestricted_registry));

        let selection = SessionToolSelection::Only { names: Vec::new() };
        let first = cache.filtered_registry(Arc::clone(&base_registry), Some(&selection));
        let second = cache.filtered_registry(base_registry, Some(&selection));
        assert!(Arc::ptr_eq(&first, &second));
    }
}

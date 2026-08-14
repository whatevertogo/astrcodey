use std::{collections::HashMap, time::Duration};

use astrcode_extension_sdk::runtime_ports::{
    ToolCatalogCompleteness, ToolCatalogScope, ToolCatalogSnapshot,
};
use parking_lot::Mutex;
use tokio::{sync::watch, time::Instant};

const PARTIAL_RETRY_AFTER: Duration = Duration::from_secs(30);
const MAX_CACHED_SCOPES: usize = 128;

struct CachedCatalog {
    snapshot: ToolCatalogSnapshot,
    retry_after: Option<Instant>,
    last_used: u64,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<ToolCatalogScope, CachedCatalog>,
    active_builds: HashMap<ToolCatalogScope, watch::Sender<bool>>,
    usage_clock: u64,
}

impl CacheState {
    fn next_usage(&mut self) -> u64 {
        self.usage_clock = self.usage_clock.wrapping_add(1);
        self.usage_clock
    }

    fn evict_lru(&mut self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, cached)| cached.last_used)
            .map(|(scope, _)| scope.clone());
        if let Some(oldest) = oldest {
            self.entries.remove(&oldest);
        }
    }
}

pub(super) enum CatalogCacheLookup<'a> {
    Hit(ToolCatalogSnapshot),
    Build(CatalogBuildPermit<'a>),
    Wait(watch::Receiver<bool>),
}

pub(super) struct CatalogBuildPermit<'a> {
    cache: &'a ToolCatalogCache,
    scope: ToolCatalogScope,
    completed: bool,
}

impl CatalogBuildPermit<'_> {
    pub(super) fn complete(mut self, snapshot: ToolCatalogSnapshot) {
        let retry_after = (snapshot.completeness == ToolCatalogCompleteness::Partial)
            .then(|| Instant::now() + self.cache.partial_retry_after);
        let notification = {
            let mut state = self.cache.state.lock();
            if !state.entries.contains_key(&self.scope)
                && state.entries.len() >= self.cache.max_entries
            {
                state.evict_lru();
            }
            let last_used = state.next_usage();
            state.entries.insert(
                self.scope.clone(),
                CachedCatalog {
                    snapshot,
                    retry_after,
                    last_used,
                },
            );
            state.active_builds.remove(&self.scope)
        };
        self.completed = true;
        notify_waiters(notification);
    }
}

impl Drop for CatalogBuildPermit<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let notification = self.cache.state.lock().active_builds.remove(&self.scope);
        notify_waiters(notification);
    }
}

fn notify_waiters(notification: Option<watch::Sender<bool>>) {
    if let Some(notification) = notification {
        notification.send_replace(true);
    }
}

pub(super) struct ToolCatalogCache {
    state: Mutex<CacheState>,
    partial_retry_after: Duration,
    max_entries: usize,
}

impl Default for ToolCatalogCache {
    fn default() -> Self {
        Self::with_policy(PARTIAL_RETRY_AFTER, MAX_CACHED_SCOPES)
    }
}

impl ToolCatalogCache {
    fn with_policy(partial_retry_after: Duration, max_entries: usize) -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
            partial_retry_after,
            max_entries,
        }
    }

    pub(super) fn lookup_or_reserve(&self, scope: &ToolCatalogScope) -> CatalogCacheLookup<'_> {
        let mut state = self.state.lock();
        let snapshot = state.entries.get(scope).and_then(|cached| {
            cached
                .retry_after
                .is_none_or(|retry_after| Instant::now() < retry_after)
                .then(|| cached.snapshot.clone())
        });
        if let Some(snapshot) = snapshot {
            let last_used = state.next_usage();
            if let Some(cached) = state.entries.get_mut(scope) {
                cached.last_used = last_used;
            }
            return CatalogCacheLookup::Hit(snapshot);
        }
        state.entries.remove(scope);
        if let Some(active_build) = state.active_builds.get(scope) {
            return CatalogCacheLookup::Wait(active_build.subscribe());
        }

        let (notification, _receiver) = watch::channel(false);
        state.active_builds.insert(scope.clone(), notification);
        CatalogCacheLookup::Build(CatalogBuildPermit {
            cache: self,
            scope: scope.clone(),
            completed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(working_dir: &str) -> ToolCatalogScope {
        ToolCatalogScope {
            working_dir: working_dir.into(),
        }
    }

    fn snapshot(completeness: ToolCatalogCompleteness) -> ToolCatalogSnapshot {
        ToolCatalogSnapshot {
            revision: 1,
            tools: Vec::new(),
            completeness,
            diagnostics: Vec::new(),
        }
    }

    #[tokio::test]
    async fn catalog_cache_is_scope_local_and_retries_partial_or_abandoned_builds() {
        let cache = ToolCatalogCache::with_policy(Duration::ZERO, 2);
        let first_scope = scope("workspace-a");
        let second_scope = scope("workspace-b");

        let CatalogCacheLookup::Build(abandoned) = cache.lookup_or_reserve(&first_scope) else {
            panic!("first lookup must build");
        };
        let CatalogCacheLookup::Wait(mut waiter) = cache.lookup_or_reserve(&first_scope) else {
            panic!("same scope must share its active build");
        };
        assert!(matches!(
            cache.lookup_or_reserve(&second_scope),
            CatalogCacheLookup::Build(_)
        ));
        drop(abandoned);
        waiter
            .changed()
            .await
            .expect("abandoned build notification");

        let CatalogCacheLookup::Build(partial) = cache.lookup_or_reserve(&first_scope) else {
            panic!("abandoned build must be retryable");
        };
        partial.complete(snapshot(ToolCatalogCompleteness::Partial));
        let CatalogCacheLookup::Build(complete) = cache.lookup_or_reserve(&first_scope) else {
            panic!("expired partial catalog must rebuild");
        };
        complete.complete(snapshot(ToolCatalogCompleteness::Complete));
        assert!(matches!(
            cache.lookup_or_reserve(&first_scope),
            CatalogCacheLookup::Hit(_)
        ));

        let CatalogCacheLookup::Build(second) = cache.lookup_or_reserve(&second_scope) else {
            panic!("second scope must build independently");
        };
        second.complete(snapshot(ToolCatalogCompleteness::Complete));
        assert!(matches!(
            cache.lookup_or_reserve(&first_scope),
            CatalogCacheLookup::Hit(_)
        ));
        let third_scope = scope("workspace-c");
        let CatalogCacheLookup::Build(third) = cache.lookup_or_reserve(&third_scope) else {
            panic!("third scope must build independently");
        };
        third.complete(snapshot(ToolCatalogCompleteness::Complete));
        assert!(matches!(
            cache.lookup_or_reserve(&second_scope),
            CatalogCacheLookup::Build(_)
        ));
    }
}

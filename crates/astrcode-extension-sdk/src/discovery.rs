//! Per-directory caching for extension-side discovery results.

use std::{collections::HashMap, sync::Mutex};

/// Caches discovery results (skills, agents, ...) keyed by working directory so
/// repeated tool calls do not rescan the filesystem.
///
/// Poisoned locks are recovered rather than propagated: a panic while another
/// thread held the lock must not permanently wedge the cache.
pub struct DiscoveryCache<V> {
    entries: Mutex<HashMap<String, V>>,
}

impl<V> DiscoveryCache<V> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<V> Default for DiscoveryCache<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: Clone> DiscoveryCache<V> {
    /// Return the cached value for `key`, running `discover` and caching its
    /// result on a miss. The lock is not held while `discover` runs, so
    /// concurrent misses may discover more than once; each caller keeps its own
    /// result and the first inserted value stays cached.
    pub fn get_or_discover(&self, key: &str, discover: impl FnOnce() -> V) -> V {
        if let Some(value) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
        {
            return value.clone();
        }
        let value = discover();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| value.clone());
        value
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn caches_per_key_and_discovers_once_per_key() {
        let cache = DiscoveryCache::<Vec<String>>::new();
        let calls = AtomicUsize::new(0);

        let first = cache.get_or_discover("/a", || {
            calls.fetch_add(1, Ordering::SeqCst);
            vec!["one".to_string()]
        });
        let second = cache.get_or_discover("/a", || {
            calls.fetch_add(1, Ordering::SeqCst);
            vec!["two".to_string()]
        });
        let other = cache.get_or_discover("/b", || {
            calls.fetch_add(1, Ordering::SeqCst);
            vec!["three".to_string()]
        });

        assert_eq!(first, vec!["one".to_string()]);
        assert_eq!(second, vec!["one".to_string()]);
        assert_eq!(other, vec!["three".to_string()]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

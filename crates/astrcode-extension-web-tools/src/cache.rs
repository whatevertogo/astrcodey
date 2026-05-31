use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub(crate) struct FetchCacheEntry {
    pub(crate) content: String,
    pub(crate) content_type: String,
    pub(crate) status_code: u16,
    pub(crate) bytes: usize,
    pub(crate) cached_at: Instant,
}

pub(crate) struct FetchUrlCache {
    ttl: Duration,
    max_entries: usize,
    max_bytes: usize,
    entries: HashMap<String, FetchCacheEntry>,
    total_bytes: usize,
}

impl FetchUrlCache {
    pub(crate) fn new(ttl_secs: u64, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_secs.max(1)),
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
            entries: HashMap::new(),
            total_bytes: 0,
        }
    }

    pub(crate) fn get(&mut self, url: &str) -> Option<FetchCacheEntry> {
        self.evict_expired();
        let entry = self.entries.get(url)?;
        if entry.cached_at.elapsed() > self.ttl {
            self.remove(url);
            return None;
        }
        Some(entry.clone())
    }

    pub(crate) fn insert(&mut self, url: String, entry: FetchCacheEntry) {
        self.evict_expired();
        if let Some(previous) = self.entries.remove(&url) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.bytes);
        }
        self.total_bytes = self.total_bytes.saturating_add(entry.bytes);
        self.entries.insert(url, entry);
        self.evict_overflow();
    }

    fn evict_expired(&mut self) {
        let ttl = self.ttl;
        let expired = self
            .entries
            .iter()
            .filter_map(|(url, entry)| (entry.cached_at.elapsed() > ttl).then_some(url.clone()))
            .collect::<Vec<_>>();
        for url in expired {
            self.remove(&url);
        }
    }

    fn evict_overflow(&mut self) {
        while self.entries.len() > self.max_entries || self.total_bytes > self.max_bytes {
            let Some(oldest_url) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.cached_at)
                .map(|(url, _)| url.clone())
            else {
                break;
            };
            self.remove(&oldest_url);
        }
    }

    fn remove(&mut self, url: &str) {
        if let Some(entry) = self.entries.remove(url) {
            self.total_bytes = self.total_bytes.saturating_sub(entry.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expires_entries_after_ttl() {
        let mut cache = FetchUrlCache::new(1, 8, 1024);
        cache.insert(
            "https://example.com".into(),
            FetchCacheEntry {
                content: "hello".into(),
                content_type: "text/plain".into(),
                status_code: 200,
                bytes: 5,
                cached_at: Instant::now() - Duration::from_secs(5),
            },
        );
        assert!(cache.get("https://example.com").is_none());
    }
}

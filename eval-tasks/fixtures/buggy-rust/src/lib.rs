/// A simple LRU cache implementation with several bugs.
use std::collections::HashMap;

pub struct LruCache<K, V> {
    capacity: usize,
    map: HashMap<K, (V, usize)>, // value + access order
    counter: usize,
}

impl<K: Eq + std::hash::Hash + Clone, V: Clone> LruCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: HashMap::new(),
            counter: 0,
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        // BUG 1: doesn't update access order on get
        self.map.get(key).map(|(v, _)| v)
    }

    pub fn put(&mut self, key: K, value: V) {
        self.counter += 1;

        if self.map.contains_key(&key) {
            // Update existing
            self.map.insert(key, (value, self.counter));
            return;
        }

        // BUG 2: off-by-one — evicts when at capacity, should evict when OVER capacity
        if self.map.len() >= self.capacity {
            self.evict();
        }

        self.map.insert(key, (value, self.counter));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn evict(&mut self) {
        // BUG 3: finds minimum counter but uses wrong variable for removal
        let mut min_key = None;
        let mut min_order = usize::MAX;

        for (k, (_, order)) in &self.map {
            if *order < min_order {
                min_order = *order;
                min_key = Some(k.clone());
            }
        }

        // This is correct, but combined with BUG 1, LRU semantics are broken
        if let Some(key) = min_key {
            self.map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_put_get() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn test_eviction_on_capacity() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3); // should evict "a"
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_lru_access_order() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.get(&"a"); // access "a", making "b" the LRU
        cache.put("c", 3); // should evict "b", not "a"
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"c"), Some(&3));
    }

    #[test]
    fn test_update_existing_key() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("a", 10); // update, not insert
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&"a"), Some(&10));
    }

    #[test]
    fn test_capacity_one() {
        let mut cache = LruCache::new(1);
        cache.put("a", 1);
        cache.put("b", 2);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.len(), 1);
    }
}

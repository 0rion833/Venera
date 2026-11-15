//! Runtime cache (M0): prepared context reused inside one process.
//!
//! M0 is the cheapest level and the only one that never touches the disk. It holds the
//! final assembled context, so a repeated request in the same command or session costs one
//! hash lookup instead of a full retrieval.
//!
//! Eviction is LRU: the oldest entry leaves when the capacity is reached. The cache is
//! always safe to clear, because every entry can be rebuilt from the index.

use std::collections::{HashMap, VecDeque};

/// One prepared context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEntry {
    /// Assembled context text.
    pub content: String,
    /// Tokens of [`Self::content`].
    pub token_count: usize,
}

/// LRU cache of prepared contexts.
#[derive(Debug, Default)]
pub struct RuntimeCache {
    capacity: usize,
    entries: HashMap<String, RuntimeEntry>,
    order: VecDeque<String>,
    hits: u64,
    misses: u64,
}

impl RuntimeCache {
    /// Creates a cache holding at most `capacity` entries; `0` disables it.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// Configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of cached content.
    pub fn bytes(&self) -> usize {
        self.entries.values().map(|entry| entry.content.len()).sum()
    }

    /// Served lookups.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Failed lookups.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Looks an entry up and marks it as most recently used.
    pub fn get(&mut self, key: &str) -> Option<&RuntimeEntry> {
        if !self.entries.contains_key(key) {
            self.misses += 1;
            return None;
        }

        self.hits += 1;
        self.touch(key);
        self.entries.get(key)
    }

    /// Inserts or replaces an entry.
    pub fn insert(&mut self, key: String, content: String, token_count: usize) {
        if self.capacity == 0 {
            return;
        }

        if self.entries.contains_key(&key) {
            self.touch(&key);
        } else {
            self.order.push_back(key.clone());
        }

        self.entries.insert(
            key,
            RuntimeEntry {
                content,
                token_count,
            },
        );

        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    /// Empties the cache, keeping the counters.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn touch(&mut self, key: &str) {
        self.order.retain(|existing| existing != key);
        self.order.push_back(key.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_what_was_inserted() {
        let mut cache = RuntimeCache::new(2);
        cache.insert("a".to_owned(), "alpha".to_owned(), 1);

        assert_eq!(
            cache.get("a").map(|entry| entry.content.as_str()),
            Some("alpha")
        );
        assert_eq!(cache.get("missing"), None);
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn evicts_the_least_recently_used_entry() {
        let mut cache = RuntimeCache::new(2);
        cache.insert("a".to_owned(), "alpha".to_owned(), 1);
        cache.insert("b".to_owned(), "beta".to_owned(), 1);

        // Touch "a" so that "b" becomes the oldest.
        assert!(cache.get("a").is_some());
        cache.insert("c".to_owned(), "gamma".to_owned(), 1);

        assert!(cache.get("b").is_none(), "b must have been evicted");
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn replacing_a_key_keeps_the_size() {
        let mut cache = RuntimeCache::new(2);
        cache.insert("a".to_owned(), "one".to_owned(), 1);
        cache.insert("a".to_owned(), "two".to_owned(), 2);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("a").map(|entry| entry.token_count), Some(2));
    }

    #[test]
    fn a_zero_capacity_cache_stores_nothing() {
        let mut cache = RuntimeCache::new(0);
        cache.insert("a".to_owned(), "alpha".to_owned(), 1);

        assert!(cache.is_empty());
        assert!(cache.get("a").is_none());
    }

    #[test]
    fn clearing_keeps_the_statistics() {
        let mut cache = RuntimeCache::new(2);
        cache.insert("a".to_owned(), "alpha".to_owned(), 1);
        assert!(cache.get("a").is_some());
        cache.clear();

        assert!(cache.is_empty());
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn tracks_stored_bytes() {
        let mut cache = RuntimeCache::new(2);
        cache.insert("a".to_owned(), "12345".to_owned(), 1);

        assert_eq!(cache.bytes(), 5);
    }
}

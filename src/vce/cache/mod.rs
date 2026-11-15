//! Multi-level context cache (`TOOL.md` extension §7).
//!
//! Two levels are implemented:
//!
//! - **M0** — RAM, per process, LRU; the final assembled context of a repeated request.
//! - **M1** — a `cache_entries` table in the project database; survives restarts, expires
//!   by TTL, and is invalidated wholesale by the index generation in the key.
//!
//! **M2** is the index itself (`files`, `symbols`, `imports`): there is nothing to store
//! twice. **M3**, a knowledge store with provenance, is deliberately not built yet — it
//! needs verified entries, and nothing verifies them without an agent.
//!
//! Every entry is derived data: clearing either level costs recomputation, never
//! information.

pub mod m0;

pub use m0::RuntimeCache;

use crate::vce::index::m1::{CacheTotals, Store, StoreError, now_unix};

/// Statistics across the implemented levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Entries held in RAM.
    pub m0_entries: usize,
    /// Bytes held in RAM.
    pub m0_bytes: usize,
    /// Served lookups in this process.
    pub m0_hits: u64,
    /// Failed lookups in this process.
    pub m0_misses: u64,
    /// Entries, bytes, and accesses stored in the database.
    pub m1: CacheTotals,
}

/// Two-level cache used by the context distributor.
pub struct CacheManager {
    runtime: RuntimeCache,
    ttl_seconds: i64,
    enabled: bool,
}

impl CacheManager {
    /// Builds a manager; `enabled = false` keeps only the runtime level.
    pub fn new(runtime_capacity: usize, ttl_seconds: i64, enabled: bool) -> Self {
        Self {
            runtime: RuntimeCache::new(runtime_capacity),
            ttl_seconds,
            enabled,
        }
    }

    /// Looks a prepared context up, in M0 and then in M1.
    ///
    /// The boolean says whether the hit came from RAM; the caller reports it in the
    /// statistics and does not need to know more.
    pub fn get(
        &mut self,
        store: &Store,
        key: &str,
        generation: i64,
    ) -> Result<Option<(String, bool)>, StoreError> {
        let runtime_key = format!("{key}:{generation}");
        if let Some(entry) = self.runtime.get(&runtime_key) {
            return Ok(Some((entry.content.clone(), true)));
        }

        if !self.enabled {
            return Ok(None);
        }

        let Some(row) = store.cache_get(key)? else {
            return Ok(None);
        };

        if self.ttl_seconds > 0 && now_unix() - row.created_at > self.ttl_seconds {
            store.cache_delete(key)?;
            return Ok(None);
        }

        // Targeted invalidation (`prirote.md` §8): an entry built from files stays valid
        // while those files keep their hashes, whatever the generation is. An entry with
        // no recorded sources cannot be checked that way and remains generation-bound.
        let sources = store.cache_sources(key)?;
        let valid = if sources.is_empty() {
            row.generation == generation
        } else {
            !store.cache_sources_stale(key)?
        };

        if !valid {
            store.cache_delete(key)?;
            return Ok(None);
        }

        self.runtime
            .insert(runtime_key, row.content.clone(), row.token_count);
        Ok(Some((row.content, false)))
    }

    /// Stores a prepared context in M0 and, when enabled, in M1.
    pub fn put(
        &mut self,
        store: &Store,
        key: &str,
        content: &str,
        token_count: usize,
        generation: i64,
        sources: &[String],
    ) -> Result<(), StoreError> {
        self.runtime.insert(
            format!("{key}:{generation}"),
            content.to_owned(),
            token_count,
        );

        if self.enabled {
            store.cache_put(key, content, token_count, generation)?;
            store.cache_set_sources(key, sources)?;
        }
        Ok(())
    }

    /// Statistics for both levels.
    pub fn stats(&self, store: &Store) -> Result<CacheStats, StoreError> {
        Ok(CacheStats {
            m0_entries: self.runtime.len(),
            m0_bytes: self.runtime.bytes(),
            m0_hits: self.runtime.hits(),
            m0_misses: self.runtime.misses(),
            m1: store.cache_totals()?,
        })
    }

    /// Empties every level, returning how many rows were removed from M1.
    pub fn clear(&mut self, store: &Store) -> Result<usize, StoreError> {
        self.runtime.clear();
        if self.enabled {
            store.cache_clear()
        } else {
            Ok(0)
        }
    }
}

/// Version of the preparation pipeline.
///
/// Bumped whenever a change in routing, segmentation, or compression would produce a
/// different context for the same request; old entries then simply stop matching.
pub const CACHE_VERSION: u32 = 4;

/// Builds the cache key: everything that makes a prepared context valid.
///
/// The key deliberately does **not** contain the index generation (`prirote.md` §8.4):
/// entries are validated against the hashes of the files they were built from, so an
/// unrelated source change cannot throw away a still-valid entry.
pub fn cache_key(
    strategy: &str,
    input: &str,
    max_tokens: usize,
    compression: &str,
    fingerprint: &CacheFingerprint,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&CACHE_VERSION.to_le_bytes());
    for part in [
        strategy,
        input,
        &max_tokens.to_string(),
        compression,
        fingerprint.encoding,
        &fingerprint.target_ratio.to_string(),
        &fingerprint.min_savings.to_string(),
        &fingerprint.validate_output.to_string(),
        &fingerprint.allow_expansion.to_string(),
        &fingerprint.prefer_partial.to_string(),
        &fingerprint.avoid_map_for_direct.to_string(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.finalize().to_hex().to_string()
}

/// Parameters that change the prepared context but are not part of the request itself.
///
/// Without them the persistent M1 cache can return a context built under different
/// compression or distribution rules than the ones in effect.
#[derive(Debug, Clone)]
pub struct CacheFingerprint {
    /// Tokenizer used to count and segment.
    pub encoding: &'static str,
    /// Target compression ratio for aggressive/balanced compression.
    pub target_ratio: f64,
    /// Minimum savings before compression is considered worthwhile.
    pub min_savings: f64,
    /// Whether output validation is enabled.
    pub validate_output: bool,
    /// Whether an oversized but valid context may fall back to the full text.
    pub allow_expansion: bool,
    /// Prefer partial sources over the full map.
    pub prefer_partial: bool,
    /// Avoid the map for direct-read tasks.
    pub avoid_map_for_direct: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vce::index::m1::FileMeta;

    fn store() -> Store {
        Store::open_in_memory().expect("store")
    }

    fn meta(path: &str, hash: &str) -> FileMeta {
        FileMeta {
            path: path.to_owned(),
            content_hash: hash.to_owned(),
            size: 10,
            modified_at: 1,
            language: "rust".to_owned(),
            parser_status: "parsed".to_owned(),
        }
    }

    fn fingerprint() -> CacheFingerprint {
        CacheFingerprint {
            encoding: "o200k_base",
            target_ratio: 0.6,
            min_savings: 0.1,
            validate_output: true,
            allow_expansion: true,
            prefer_partial: true,
            avoid_map_for_direct: true,
        }
    }

    fn key() -> String {
        cache_key(
            "direct-read",
            "read src/a.rs",
            1000,
            "balanced",
            &fingerprint(),
        )
    }

    #[test]
    fn a_repeated_request_is_served_from_ram() {
        let store = store();
        let mut cache = CacheManager::new(8, 3600, true);
        let key = key();

        cache.put(&store, &key, "prepared", 2, 0, &[]).expect("put");
        let (content, from_ram) = cache.get(&store, &key, 0).expect("get").expect("hit");

        assert_eq!(content, "prepared");
        assert!(from_ram);
    }

    #[test]
    fn m1_survives_a_new_manager() {
        let store = store();
        let key = cache_key("project-map", "analyze", 500, "none", &fingerprint());

        let mut first = CacheManager::new(8, 3600, true);
        first.put(&store, &key, "text", 1, 3, &[]).expect("put");

        let mut second = CacheManager::new(8, 3600, true);
        let (content, from_ram) = second.get(&store, &key, 3).expect("get").expect("hit");

        assert_eq!(content, "text");
        assert!(!from_ram, "a fresh process misses M0 and hits M1");
    }

    #[test]
    fn a_disabled_cache_never_writes_m1() {
        let store = store();
        let mut cache = CacheManager::new(8, 3600, false);
        let key = key();

        cache.put(&store, &key, "text", 1, 0, &[]).expect("put");

        assert_eq!(store.cache_totals().expect("totals").entries, 0);
    }

    #[test]
    fn an_expired_entry_is_a_miss_and_is_removed() {
        let store = store();
        let key = key();
        store.cache_put(&key, "old", 1, 0).expect("put");
        store.cache_prune(now_unix() + 1).expect("prune everything");

        let mut cache = CacheManager::new(8, 3600, true);
        assert!(cache.get(&store, &key, 0).expect("get").is_none());
    }

    #[test]
    fn clearing_removes_both_levels() {
        let store = store();
        let mut cache = CacheManager::new(8, 3600, true);
        let key = key();
        cache.put(&store, &key, "text", 1, 0, &[]).expect("put");

        let removed = cache.clear(&store).expect("clear");

        assert_eq!(removed, 1);
        assert!(cache.get(&store, &key, 0).expect("get").is_none());
        assert_eq!(store.cache_totals().expect("totals").entries, 0);
    }

    #[test]
    fn the_key_changes_with_strategy_budget_and_compression() {
        let base = cache_key("direct-read", "x", 10, "none", &fingerprint());

        assert_ne!(
            base,
            cache_key("partial-read", "x", 10, "none", &fingerprint())
        );
        assert_ne!(
            base,
            cache_key("direct-read", "x", 20, "none", &fingerprint())
        );
        assert_ne!(
            base,
            cache_key("direct-read", "x", 10, "balanced", &fingerprint())
        );
        assert_eq!(
            base,
            cache_key("direct-read", "x", 10, "none", &fingerprint())
        );
    }

    #[test]
    fn the_key_changes_with_distributor_settings() {
        let base = fingerprint();
        let base_key = cache_key("direct-read", "x", 10, "none", &base);

        let mut changed = base.clone();
        changed.target_ratio = 0.3;
        assert_ne!(
            base_key,
            cache_key("direct-read", "x", 10, "none", &changed)
        );

        let mut changed = base.clone();
        changed.min_savings = 0.01;
        assert_ne!(
            base_key,
            cache_key("direct-read", "x", 10, "none", &changed)
        );

        let mut changed = base.clone();
        changed.validate_output = false;
        assert_ne!(
            base_key,
            cache_key("direct-read", "x", 10, "none", &changed)
        );

        let mut changed = base.clone();
        changed.encoding = "cl100k_base";
        assert_ne!(
            base_key,
            cache_key("direct-read", "x", 10, "none", &changed)
        );
    }

    #[test]
    fn an_unchanged_source_survives_a_generation_bump() {
        let store = store();
        store
            .upsert_file(&meta("src/a.rs", "h1"))
            .expect("indexed file");
        let key = key();
        let mut cache = CacheManager::new(8, 3600, true);

        cache
            .put(&store, &key, "prepared", 2, 1, &["src/a.rs".to_owned()])
            .expect("put");
        store.bump_generation().expect("bump");

        let (content, _) = cache.get(&store, &key, 2).expect("get").expect("hit");
        assert_eq!(content, "prepared");
    }

    #[test]
    fn a_changed_source_invalidates_the_entry() {
        let store = store();
        store
            .upsert_file(&meta("src/a.rs", "h1"))
            .expect("indexed file");
        let key = key();
        let mut cache = CacheManager::new(8, 3600, true);
        cache
            .put(&store, &key, "prepared", 2, 1, &["src/a.rs".to_owned()])
            .expect("put");

        store
            .upsert_file(&meta("src/a.rs", "h2"))
            .expect("rewritten file");

        let mut fresh = CacheManager::new(8, 3600, true);
        assert!(fresh.get(&store, &key, 1).expect("get").is_none());
        assert_eq!(store.cache_totals().expect("totals").entries, 0);
    }

    #[test]
    fn an_entry_without_sources_is_generation_bound() {
        let store = store();
        let key = key();
        let mut cache = CacheManager::new(8, 3600, true);
        cache.put(&store, &key, "prepared", 2, 1, &[]).expect("put");

        assert!(cache.get(&store, &key, 2).expect("get").is_none());
    }

    #[test]
    fn the_index_generation_bumps_only_on_change() {
        let store = store();
        assert_eq!(store.generation().expect("generation"), 0);

        assert_eq!(store.bump_generation().expect("bump"), 1);
        assert_eq!(store.generation().expect("generation"), 1);
    }
}

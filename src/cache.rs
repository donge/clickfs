//! Tiny TTL cache for ClickHouse metadata listings.
//!
//! Used by `ClickFs` to avoid re-issuing the same SHOW DATABASES /
//! SHOW TABLES / system.parts queries on every `ls` and `stat`. The
//! kernel itself caches FUSE entries, but only per-inode for the TTL
//! we hand back; tools that rebuild the dentry cache (`find`, `du`,
//! shells with autocomplete) hammer readdir() and benefit a lot from
//! coalescing a hundred lookups into one query.
//!
//! Design points:
//!
//! * Generic `K` and `V`; the FS layer instantiates three caches —
//!   one for `Vec<String>` (db/table/partition lists keyed by tuple
//!   of strings) and one for the existence-probe boolean.
//! * Per-entry TTL stored alongside the value, so a cache with TTL=0
//!   short-circuits and behaves as a no-op (every `get` returns None,
//!   `insert` is a no-op).
//! * Backed by `DashMap` for lock-free concurrent access; suitable
//!   for the heavy parallel `ls` traffic that `find -j` produces.
//! * No background eviction thread — entries are checked lazily on
//!   read. This keeps the cache footprint bounded by traffic, not by
//!   wall-clock time, which is fine for the tens-to-hundreds of
//!   databases/tables a typical cluster has.
//!
//! Not used to cache `.schema` text or data streams; both are
//! intentionally always-fresh so an `ALTER TABLE` is visible
//! immediately to a re-cat.

use std::hash::Hash;
use std::time::{Duration, Instant};

use dashmap::DashMap;

#[derive(Debug)]
pub struct TtlCache<K, V>
where
    K: Eq + Hash,
{
    map: DashMap<K, (V, Instant)>,
    ttl: Duration,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash,
    V: Clone,
{
    /// Create a new cache with the given TTL.
    /// A `ttl` of `Duration::ZERO` makes the cache a no-op (always-miss).
    pub fn new(ttl: Duration) -> Self {
        Self {
            map: DashMap::new(),
            ttl,
        }
    }

    /// `true` iff this cache will actually store anything.
    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// Look up a key. Returns `None` on miss, on expiry, or when the
    /// cache is disabled.
    pub fn get(&self, key: &K) -> Option<V> {
        if !self.enabled() {
            return None;
        }
        let entry = self.map.get(key)?;
        let (val, inserted_at) = entry.value();
        if inserted_at.elapsed() <= self.ttl {
            Some(val.clone())
        } else {
            // Expired. Drop the read guard before mutating to avoid
            // a self-deadlock on DashMap's per-shard locks.
            drop(entry);
            self.map.remove(key);
            None
        }
    }

    /// Insert (or refresh) a key. No-op when disabled.
    pub fn insert(&self, key: K, val: V) {
        if !self.enabled() {
            return;
        }
        self.map.insert(key, (val, Instant::now()));
    }

    /// Remove a single entry (used when we observe a stale lookup).
    #[allow(dead_code)]
    pub fn invalidate(&self, key: &K) {
        self.map.remove(key);
    }

    /// Drop everything. Useful for tests; not currently called from
    /// the FS, but cheap enough to keep around for future
    /// "reload-on-SIGHUP" behavior.
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.map.clear();
    }

    /// Approximate entry count (for diagnostics / tests).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn hit_within_ttl() {
        let c = TtlCache::<String, u32>::new(Duration::from_secs(60));
        c.insert("k".into(), 7);
        assert_eq!(c.get(&"k".into()), Some(7));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn miss_after_ttl() {
        let c = TtlCache::<String, u32>::new(Duration::from_millis(20));
        c.insert("k".into(), 7);
        sleep(Duration::from_millis(40));
        assert_eq!(c.get(&"k".into()), None);
        // Expired entries are evicted on access.
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn disabled_is_noop() {
        let c = TtlCache::<String, u32>::new(Duration::ZERO);
        assert!(!c.enabled());
        c.insert("k".into(), 7);
        assert_eq!(c.get(&"k".into()), None);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn refresh_resets_timer() {
        let c = TtlCache::<String, u32>::new(Duration::from_millis(50));
        c.insert("k".into(), 1);
        sleep(Duration::from_millis(30));
        c.insert("k".into(), 2); // should reset timer
        sleep(Duration::from_millis(30));
        // Total 60ms but second insert was 30ms ago: should still hit.
        assert_eq!(c.get(&"k".into()), Some(2));
    }

    #[test]
    fn invalidate_drops_entry() {
        let c = TtlCache::<String, u32>::new(Duration::from_secs(60));
        c.insert("k".into(), 7);
        c.invalidate(&"k".into());
        assert_eq!(c.get(&"k".into()), None);
    }

    #[test]
    fn clear_empties_map() {
        let c = TtlCache::<String, u32>::new(Duration::from_secs(60));
        c.insert("a".into(), 1);
        c.insert("b".into(), 2);
        assert_eq!(c.len(), 2);
        c.clear();
        assert!(c.is_empty());
    }

    #[test]
    fn tuple_keys_work() {
        // Smoke check: real cache uses (String, String) keys.
        let c = TtlCache::<(String, String), Vec<String>>::new(Duration::from_secs(60));
        c.insert(("db".into(), "tbl".into()), vec!["p1".into(), "p2".into()]);
        let got = c.get(&("db".into(), "tbl".into())).unwrap();
        assert_eq!(got, vec!["p1".to_string(), "p2".to_string()]);
    }

    #[test]
    fn concurrent_inserts_do_not_deadlock() {
        // DashMap shards on hash(key); hammer it to make sure our
        // get-then-remove path doesn't self-deadlock under load.
        use std::sync::Arc;
        use std::thread;
        let c = Arc::new(TtlCache::<u64, u64>::new(Duration::from_millis(1)));
        let mut hs = vec![];
        for t in 0..4 {
            let c = c.clone();
            hs.push(thread::spawn(move || {
                for i in 0..200 {
                    c.insert(t * 1000 + i, i);
                    let _ = c.get(&(t * 1000 + i));
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
    }
}

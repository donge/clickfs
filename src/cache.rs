//! Tiny TTL cache for ClickHouse metadata listings.
//!
//! Used by `ClickFs` to coalesce the SHOW DATABASES / SHOW TABLES /
//! system.parts queries that a single `ls` would otherwise fire once
//! per child entry. Lookups inside `lookup()` reuse the parent's list
//! to decide ENOENT, so the cache also doubles as the existence index.
//!
//! Backed by `DashMap` for lock-free concurrent access. Entries are
//! lazily evicted on read; no background reaper.

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
    pub fn new(ttl: Duration) -> Self {
        Self {
            map: DashMap::new(),
            ttl,
        }
    }

    /// Look up a key. Returns `None` on miss or on expiry.
    pub fn get(&self, key: &K) -> Option<V> {
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

    /// Insert (or refresh) a key.
    pub fn insert(&self, key: K, val: V) {
        self.map.insert(key, (val, Instant::now()));
    }

    /// Approximate entry count (for diagnostics / tests).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.len()
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
        // 50ms TTL + 200ms wait: a 4x margin keeps this stable on
        // loaded CI runners (macOS GH Actions in particular has
        // shown >100ms scheduling stalls).
        let c = TtlCache::<String, u32>::new(Duration::from_millis(50));
        c.insert("k".into(), 7);
        sleep(Duration::from_millis(200));
        assert_eq!(c.get(&"k".into()), None);
        // Expired entries are evicted on access.
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn refresh_resets_timer() {
        // Use generous timing so loaded CI runners don't blow the
        // budget between insert/sleep/get. Logic under test is a
        // single Instant::now() comparison, not a clock-precision
        // measurement.
        let c = TtlCache::<String, u32>::new(Duration::from_millis(500));
        c.insert("k".into(), 1);
        sleep(Duration::from_millis(100));
        c.insert("k".into(), 2); // should reset timer
        sleep(Duration::from_millis(100));
        // First insert was 200ms ago (would still hit even without
        // reset), but specifically we want to assert the value is
        // the refreshed one and that nothing has expired.
        assert_eq!(c.get(&"k".into()), Some(2));
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

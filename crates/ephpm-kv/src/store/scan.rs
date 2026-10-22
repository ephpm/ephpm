//! `SCAN`: incremental iteration over the keyspace with a stable cursor.
//!
//! # The cursor
//!
//! The store is two `DashMap`s — string keys and hashes — and a `DashMap` is
//! a fixed array of shards chosen when the map is built. A key's shard is a
//! function of its hash alone, so it never changes for the key's lifetime,
//! however the shard's own table grows, shrinks or rehashes. The cursor is
//! therefore a position in the sequence *string shards, then hash shards*,
//! and each call walks whole shards (under each shard's read lock, copying
//! the live keys out) until it has examined at least `COUNT` entries.
//!
//! That gives Redis' guarantee exactly: **every key present for the whole
//! duration of a scan is returned at least once** — in fact exactly once,
//! since it lives in one shard and every shard is walked once. A key added
//! or removed during the scan may or may not be returned. A name that is both
//! a string and a hash (the store allows it) is returned once per type.
//!
//! The price is granularity: a call returns at least one whole shard, so a
//! very large keyspace returns more than `COUNT` keys per call — `COUNT` is a
//! hint in Redis too. Pattern matching (`MATCH`) runs after the shard lock is
//! released, with the linear-time glob `KEYS` uses.
//!
//! # Clusters
//!
//! `SCAN` (like `KEYS`) iterates **this node's** keyspace. Every value at or
//! under `[cluster.kv] small_key_threshold` (and every hash) is held by every
//! node, so it is always seen; a larger string value is held only by its
//! replica set and is returned only on those nodes.

use std::time::Instant;

use dashmap::DashMap;

use super::Store;

/// Which keys a `SCAN` returns (`TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// String keys only.
    String,
    /// Hash keys only.
    Hash,
    /// A type this store does not hold (`list`, `set`, ...): nothing.
    Other,
}

/// One page of a `SCAN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage {
    /// The cursor to pass to the next call; `0` when the scan is complete.
    pub cursor: u64,
    /// The keys of this page.
    pub keys: Vec<String>,
}

impl Store {
    /// One `SCAN` step from `cursor` (`0` starts a scan). Walks shards until
    /// at least `count` entries were examined, then returns the live keys
    /// matching `pattern` (`None` = all) and, if given, of type `kind`.
    ///
    /// A cursor past the end (or from a store with a different shard layout)
    /// ends the scan with an empty page.
    #[must_use]
    pub fn scan(
        &self,
        cursor: u64,
        pattern: Option<&str>,
        count: usize,
        kind: Option<KeyKind>,
    ) -> ScanPage {
        let data_shards = self.data.shards().len();
        let total = data_shards + self.hashes.shards().len();
        let Ok(mut position) = usize::try_from(cursor) else {
            return ScanPage { cursor: 0, keys: Vec::new() };
        };
        if position >= total {
            return ScanPage { cursor: 0, keys: Vec::new() };
        }
        let want = count.max(1);
        let now = Instant::now();
        let mut keys = Vec::new();
        let mut examined = 0;
        while position < total && examined < want {
            if position < data_shards {
                if kind.is_none_or(|k| k == KeyKind::String) {
                    examined += copy_shard(&self.data, position, |e| !e.is_expired(), &mut keys);
                }
            } else if kind.is_none_or(|k| k == KeyKind::Hash) {
                examined +=
                    copy_shard(&self.hashes, position - data_shards, |h| h.is_live(now), &mut keys);
            }
            position += 1;
        }
        if let Some(p) = pattern.filter(|p| *p != "*") {
            keys.retain(|k| super::glob::glob_match(p.as_bytes(), k.as_bytes()));
        }
        let cursor = if position >= total { 0 } else { position as u64 };
        ScanPage { cursor, keys }
    }
}

/// Copy the live keys of shard `index` of `map` into `out` under the shard's
/// read lock. Returns how many entries the shard held (live or not) — what
/// `COUNT` measures.
// The `raw-api` shard iterator is the only `unsafe` here; justified by the
// SAFETY comment on the loop.
#[allow(unsafe_code)]
fn copy_shard<V>(
    map: &DashMap<String, V>,
    index: usize,
    is_live: impl Fn(&V) -> bool,
    out: &mut Vec<String>,
) -> usize {
    let Some(shard) = map.shards().get(index) else {
        return 0;
    };
    let guard = shard.read();
    let mut seen = 0;
    // SAFETY: `bucket.as_ref()` dereferences a live bucket into a shared
    // reference. The bucket comes from this shard's own `iter()`, and `guard`
    // holds the shard's read lock for the whole loop, so no writer can rehash
    // or remove entries meanwhile. The references do not escape the loop:
    // only owned clones do.
    for bucket in unsafe { guard.iter() } {
        let (key, slot) = unsafe { bucket.as_ref() };
        seen += 1;
        if is_live(slot.get()) {
            out.push(key.clone());
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::store::StoreConfig;

    fn full_scan(
        s: &Store,
        pattern: Option<&str>,
        count: usize,
        kind: Option<KeyKind>,
    ) -> Vec<String> {
        let mut cursor = 0;
        let mut all = Vec::new();
        let mut calls = 0;
        loop {
            let page = s.scan(cursor, pattern, count, kind);
            all.extend(page.keys);
            calls += 1;
            assert!(calls < 10_000, "scan did not terminate");
            if page.cursor == 0 {
                return all;
            }
            cursor = page.cursor;
        }
    }

    #[test]
    fn scan_returns_every_key_exactly_once() {
        let s = Store::new(StoreConfig::default());
        for i in 0..2_000 {
            s.set(format!("k{i}"), b"v".to_vec(), None);
        }
        for i in 0..50 {
            s.hset(&format!("h{i}"), "f", b"v".to_vec()).unwrap();
        }
        let keys = full_scan(&s, None, 10, None);
        assert_eq!(keys.len(), 2_050);
        let unique: HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), 2_050);
    }

    #[test]
    fn scan_filters_by_match_and_type_and_skips_expired() {
        let s = Store::new(StoreConfig::default());
        s.set("user:1".into(), b"v".to_vec(), None);
        s.set("user:2".into(), b"v".to_vec(), Some(Duration::from_millis(1)));
        s.set("post:1".into(), b"v".to_vec(), None);
        s.hset("user:h", "f", b"v".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let mut users = full_scan(&s, Some("user:*"), 100, None);
        users.sort();
        assert_eq!(users, vec!["user:1", "user:h"]);
        assert_eq!(full_scan(&s, None, 100, Some(KeyKind::Hash)), vec!["user:h"]);
        assert_eq!(full_scan(&s, None, 100, Some(KeyKind::Other)), Vec::<String>::new());
        let strings = full_scan(&s, None, 100, Some(KeyKind::String));
        assert_eq!(strings.len(), 2);
    }

    #[test]
    fn invalid_cursor_ends_the_scan() {
        let s = Store::new(StoreConfig::default());
        s.set("k".into(), b"v".to_vec(), None);
        assert_eq!(s.scan(u64::MAX, None, 10, None), ScanPage { cursor: 0, keys: vec![] });
    }

    /// The guarantee under concurrent writes: keys present throughout are all
    /// returned, while another thread inserts and deletes other keys (forcing
    /// table growth and shrink mid-scan).
    #[test]
    fn keys_present_throughout_survive_concurrent_churn() {
        let s = Store::new(StoreConfig { memory_limit: 0, ..StoreConfig::default() });
        for i in 0..5_000 {
            s.set(format!("stable:{i}"), b"v".to_vec(), None);
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn = {
            let s = Arc::clone(&s);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    s.set(format!("churn:{n}"), b"x".to_vec(), None);
                    if n >= 2_000 {
                        s.remove(&format!("churn:{}", n - 2_000));
                    }
                    n += 1;
                }
            })
        };
        for _ in 0..5 {
            let found: HashSet<String> =
                full_scan(&s, Some("stable:*"), 7, None).into_iter().collect();
            assert_eq!(found.len(), 5_000, "a stable key was skipped");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        churn.join().unwrap();
    }
}

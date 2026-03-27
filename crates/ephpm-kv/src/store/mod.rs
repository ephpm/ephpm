//! In-memory KV store engine.
//!
//! Backed by [`dashmap::DashMap`] for lock-free concurrent reads and
//! fine-grained write locking. Supports TTL expiry, LRU eviction, and
//! approximate memory tracking.

mod entry;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::{debug, trace};

pub use entry::Entry;

/// Eviction policy when the memory limit is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvictionPolicy {
    /// Reject writes with an OOM error.
    NoEviction,
    /// Evict the least-recently-used key from all keys.
    #[default]
    AllKeysLru,
    /// Evict the least-recently-used key that has a TTL set.
    VolatileLru,
    /// Evict a random key from all keys.
    AllKeysRandom,
}

impl EvictionPolicy {
    /// Parse from the config string. Falls back to `AllKeysLru` on unknown values.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "noeviction" => Self::NoEviction,
            "allkeys-lru" => Self::AllKeysLru,
            "volatile-lru" => Self::VolatileLru,
            "allkeys-random" => Self::AllKeysRandom,
            _ => Self::AllKeysLru,
        }
    }
}

/// Configuration for the KV store.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Maximum memory in bytes. 0 = unlimited.
    pub memory_limit: usize,
    /// Eviction policy when the memory limit is reached.
    pub eviction_policy: EvictionPolicy,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            memory_limit: 256 * 1024 * 1024, // 256 MiB
            eviction_policy: EvictionPolicy::AllKeysLru,
        }
    }
}

/// Thread-safe in-memory KV store.
#[derive(Debug)]
pub struct Store {
    /// The main data map.
    data: DashMap<String, Entry>,
    /// Approximate total memory used by all entries.
    mem_used: AtomicUsize,
    /// Store configuration.
    config: StoreConfig,
}

impl Store {
    /// Create a new store with the given configuration.
    #[must_use]
    pub fn new(config: StoreConfig) -> Arc<Self> {
        Arc::new(Self {
            data: DashMap::new(),
            mem_used: AtomicUsize::new(0),
            config,
        })
    }

    // ── Read operations ──────────────────────────────────────────

    /// Get a value by key. Returns `None` if missing or expired.
    /// Touches the entry for LRU tracking.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let mut entry = self.data.get_mut(key)?;
        if entry.is_expired() {
            drop(entry);
            self.remove(key);
            return None;
        }
        entry.touch();
        Some(entry.data.clone())
    }

    /// Check if a key exists (and is not expired).
    #[must_use]
    pub fn exists(&self, key: &str) -> bool {
        match self.data.get(key) {
            Some(entry) => {
                if entry.is_expired() {
                    drop(entry);
                    self.remove(key);
                    false
                } else {
                    true
                }
            }
            None => false,
        }
    }

    /// Get the remaining TTL for a key in milliseconds.
    /// Returns `None` if the key doesn't exist, `Some(-1)` if no expiry,
    /// `Some(-2)` if expired/missing.
    #[must_use]
    pub fn pttl(&self, key: &str) -> Option<i64> {
        let entry = self.data.get(key)?;
        if entry.is_expired() {
            drop(entry);
            self.remove(key);
            return Some(-2);
        }
        match entry.expires_at {
            Some(exp) => {
                let remaining = exp.saturating_duration_since(Instant::now());
                Some(i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX))
            }
            None => Some(-1),
        }
    }

    /// Number of keys in the store (including not-yet-reaped expired keys).
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Approximate memory usage in bytes.
    #[must_use]
    pub fn mem_used(&self) -> usize {
        self.mem_used.load(Ordering::Relaxed)
    }

    /// Collect keys matching a glob pattern. `*` matches everything.
    #[must_use]
    pub fn keys(&self, pattern: &str) -> Vec<String> {
        let match_all = pattern == "*";
        self.data
            .iter()
            .filter(|entry| !entry.value().is_expired())
            .filter(|entry| match_all || glob_match(pattern, entry.key()))
            .map(|entry| entry.key().clone())
            .collect()
    }

    // ── Write operations ─────────────────────────────────────────

    /// Set a key to a value with an optional TTL.
    ///
    /// Returns `true` if the write succeeded, `false` if rejected by
    /// the `NoEviction` policy when the memory limit is reached.
    pub fn set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        let entry = match ttl {
            Some(dur) => Entry::with_expiry(value, key.len(), Instant::now() + dur),
            None => Entry::new(value, key.len()),
        };

        let new_size = entry.mem_size;

        // Remove old entry first so we can reclaim its memory.
        if let Some((_, old)) = self.data.remove(&key) {
            self.mem_sub(old.mem_size);
        }

        // Check memory limit before inserting.
        if !self.ensure_memory(new_size) {
            return false;
        }

        self.mem_add(new_size);
        self.data.insert(key, entry);
        true
    }

    /// Remove a key, returning `true` if it existed.
    pub fn remove(&self, key: &str) -> bool {
        if let Some((_, old)) = self.data.remove(key) {
            self.mem_sub(old.mem_size);
            true
        } else {
            false
        }
    }

    /// Set an expiry on an existing key. Returns `false` if the key doesn't exist.
    pub fn expire(&self, key: &str, ttl: Duration) -> bool {
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.remove(key);
                return false;
            }
            entry.expires_at = Some(Instant::now() + ttl);
            true
        } else {
            false
        }
    }

    /// Remove the expiry from a key (make it persistent). Returns `false`
    /// if the key doesn't exist.
    pub fn persist(&self, key: &str) -> bool {
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.remove(key);
                return false;
            }
            let had_ttl = entry.expires_at.is_some();
            entry.expires_at = None;
            had_ttl
        } else {
            false
        }
    }

    /// Increment the value at `key` by `delta`, treating the stored bytes
    /// as a decimal integer string. Creates the key with value `delta` if
    /// it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the stored value is not a valid integer string.
    pub fn incr_by(&self, key: &str, delta: i64) -> Result<i64, String> {
        // Fast path: key exists, try to update in place.
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.remove(key);
                // Fall through to create.
            } else {
                let current = parse_int_value(&entry.data)?;
                let new_val = current.checked_add(delta).ok_or_else(|| {
                    "ERR increment or decrement would overflow".to_string()
                })?;
                let new_bytes = new_val.to_string().into_bytes();
                let old_mem = entry.mem_size;
                entry.data = new_bytes;
                entry.mem_size = Entry::new(entry.data.clone(), key.len()).mem_size;
                entry.touch();
                let new_mem = entry.mem_size;
                drop(entry);
                // Adjust memory tracking.
                if new_mem > old_mem {
                    self.mem_add(new_mem - old_mem);
                } else {
                    self.mem_sub(old_mem - new_mem);
                }
                return Ok(new_val);
            }
        }

        // Key doesn't exist — create it.
        let val_bytes = delta.to_string().into_bytes();
        self.set(key.to_string(), val_bytes, None);
        Ok(delta)
    }

    /// Append `value` to the existing value at `key`, or create it.
    /// Returns the new length of the value.
    pub fn append(&self, key: &str, value: &[u8]) -> usize {
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.remove(key);
                // Fall through to create.
            } else {
                let added = value.len();
                entry.data.extend_from_slice(value);
                entry.mem_size += added;
                entry.touch();
                let new_len = entry.data.len();
                self.mem_add(added);
                return new_len;
            }
        }

        let len = value.len();
        self.set(key.to_string(), value.to_vec(), None);
        len
    }

    /// Remove all keys.
    pub fn flush(&self) {
        self.data.clear();
        self.mem_used.store(0, Ordering::Relaxed);
    }

    // ── Background maintenance ───────────────────────────────────

    /// Run a single pass of lazy expiration, removing up to `sample_size`
    /// expired keys. Called periodically by the background task.
    pub fn expire_pass(&self, sample_size: usize) -> usize {
        let mut removed = 0;
        let mut keys_to_remove = Vec::new();

        for entry in self.data.iter() {
            if removed >= sample_size {
                break;
            }
            if entry.value().is_expired() {
                keys_to_remove.push(entry.key().clone());
                removed += 1;
            }
        }

        for key in &keys_to_remove {
            self.remove(key);
        }

        if removed > 0 {
            trace!(removed, "expired keys reaped");
        }
        removed
    }

    // ── Memory management ────────────────────────────────────────

    fn mem_add(&self, n: usize) {
        self.mem_used.fetch_add(n, Ordering::Relaxed);
    }

    fn mem_sub(&self, n: usize) {
        self.mem_used.fetch_sub(n, Ordering::Relaxed);
    }

    /// Ensure there is room for `needed` bytes. Runs eviction if necessary.
    /// Returns `false` only if the `NoEviction` policy is active and we're
    /// over the limit.
    fn ensure_memory(&self, needed: usize) -> bool {
        let limit = self.config.memory_limit;
        if limit == 0 {
            return true; // unlimited
        }

        let current = self.mem_used.load(Ordering::Relaxed);
        if current + needed <= limit {
            return true;
        }

        match self.config.eviction_policy {
            EvictionPolicy::NoEviction => false,
            EvictionPolicy::AllKeysLru => self.evict_lru(needed, false),
            EvictionPolicy::VolatileLru => self.evict_lru(needed, true),
            EvictionPolicy::AllKeysRandom => self.evict_random(needed),
        }
    }

    /// Sample-based LRU eviction. Samples a batch of keys and evicts the
    /// least-recently-used until we're under the memory limit.
    fn evict_lru(&self, needed: usize, volatile_only: bool) -> bool {
        let limit = self.config.memory_limit;
        let sample_size = 16;

        for _ in 0..100 {
            let current = self.mem_used.load(Ordering::Relaxed);
            if current + needed <= limit {
                return true;
            }

            // Sample keys and find the one with the oldest last_accessed.
            let mut oldest: Option<(String, Instant)> = None;
            let mut count = 0;

            for entry in self.data.iter() {
                if volatile_only && entry.value().expires_at.is_none() {
                    continue;
                }
                match &oldest {
                    Some((_, ts)) if entry.value().last_accessed < *ts => {
                        oldest = Some((entry.key().clone(), entry.value().last_accessed));
                    }
                    None => {
                        oldest = Some((entry.key().clone(), entry.value().last_accessed));
                    }
                    _ => {}
                }
                count += 1;
                if count >= sample_size {
                    break;
                }
            }

            if let Some((key, _)) = oldest {
                debug!(key = %key, "evicting key (LRU)");
                self.remove(&key);
            } else {
                return false; // nothing to evict
            }
        }

        // Gave it a good try.
        self.mem_used.load(Ordering::Relaxed) + needed <= limit
    }

    /// Random eviction — just remove the first key we find.
    fn evict_random(&self, needed: usize) -> bool {
        let limit = self.config.memory_limit;

        for _ in 0..100 {
            let current = self.mem_used.load(Ordering::Relaxed);
            if current + needed <= limit {
                return true;
            }

            let key = self.data.iter().next().map(|e| e.key().clone());
            if let Some(key) = key {
                debug!(key = %key, "evicting key (random)");
                self.remove(&key);
            } else {
                return false;
            }
        }

        self.mem_used.load(Ordering::Relaxed) + needed <= limit
    }
}

/// Parse a stored byte value as a decimal integer.
fn parse_int_value(data: &[u8]) -> Result<i64, String> {
    let s = std::str::from_utf8(data)
        .map_err(|_| "ERR value is not an integer or out of range".to_string())?;
    s.parse::<i64>()
        .map_err(|_| "ERR value is not an integer or out of range".to_string())
}

/// Simple glob matching supporting `*` (any chars) and `?` (single char).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, &txt)
}

fn glob_match_inner(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            // '*' matches zero or more characters.
            glob_match_inner(&pat[1..], txt)
                || (!txt.is_empty() && glob_match_inner(pat, &txt[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&pat[1..], &txt[1..]),
        (Some(a), Some(b)) if a == b => glob_match_inner(&pat[1..], &txt[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    #[test]
    fn set_and_get() {
        let s = test_store();
        s.set("hello".into(), b"world".to_vec(), None);
        assert_eq!(s.get("hello"), Some(b"world".to_vec()));
    }

    #[test]
    fn missing_key() {
        let s = test_store();
        assert_eq!(s.get("nope"), None);
    }

    #[test]
    fn overwrite() {
        let s = test_store();
        s.set("k".into(), b"v1".to_vec(), None);
        s.set("k".into(), b"v2".to_vec(), None);
        assert_eq!(s.get("k"), Some(b"v2".to_vec()));
    }

    #[test]
    fn delete() {
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        assert!(s.remove("k"));
        assert!(!s.remove("k"));
        assert_eq!(s.get("k"), None);
    }

    #[test]
    fn exists() {
        let s = test_store();
        assert!(!s.exists("k"));
        s.set("k".into(), b"v".to_vec(), None);
        assert!(s.exists("k"));
    }

    #[test]
    fn ttl_expiry() {
        let s = test_store();
        // Set with a TTL that has already passed.
        let entry = Entry::with_expiry(
            b"v".to_vec(),
            1,
            Instant::now() - Duration::from_secs(1),
        );
        s.data.insert("k".into(), entry);
        assert_eq!(s.get("k"), None);
    }

    #[test]
    fn pttl_no_expiry() {
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        assert_eq!(s.pttl("k"), Some(-1));
    }

    #[test]
    fn pttl_missing() {
        let s = test_store();
        assert_eq!(s.pttl("k"), None);
    }

    #[test]
    fn incr() {
        let s = test_store();
        assert_eq!(s.incr_by("counter", 1), Ok(1));
        assert_eq!(s.incr_by("counter", 5), Ok(6));
        assert_eq!(s.incr_by("counter", -2), Ok(4));
    }

    #[test]
    fn incr_non_integer() {
        let s = test_store();
        s.set("k".into(), b"hello".to_vec(), None);
        assert!(s.incr_by("k", 1).is_err());
    }

    #[test]
    fn append_new_key() {
        let s = test_store();
        assert_eq!(s.append("k", b"hello"), 5);
        assert_eq!(s.get("k"), Some(b"hello".to_vec()));
    }

    #[test]
    fn append_existing() {
        let s = test_store();
        s.set("k".into(), b"hello".to_vec(), None);
        assert_eq!(s.append("k", b" world"), 11);
        assert_eq!(s.get("k"), Some(b"hello world".to_vec()));
    }

    #[test]
    fn flush() {
        let s = test_store();
        s.set("a".into(), b"1".to_vec(), None);
        s.set("b".into(), b"2".to_vec(), None);
        s.flush();
        assert_eq!(s.len(), 0);
        assert_eq!(s.mem_used(), 0);
    }

    #[test]
    fn keys_pattern() {
        let s = test_store();
        s.set("user:1".into(), b"a".to_vec(), None);
        s.set("user:2".into(), b"b".to_vec(), None);
        s.set("post:1".into(), b"c".to_vec(), None);
        let mut keys = s.keys("user:*");
        keys.sort();
        assert_eq!(keys, vec!["user:1", "user:2"]);
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("user:*", "user:123"));
        assert!(!glob_match("user:*", "post:1"));
        assert!(glob_match("h?llo", "hello"));
        assert!(!glob_match("h?llo", "hllo"));
    }

    #[test]
    fn expire_pass_reaps() {
        let s = test_store();
        let entry = Entry::with_expiry(
            b"v".to_vec(),
            1,
            Instant::now() - Duration::from_secs(1),
        );
        s.data.insert("expired".into(), entry);
        s.set("alive".into(), b"v".to_vec(), None);
        let reaped = s.expire_pass(100);
        assert_eq!(reaped, 1);
        assert!(!s.exists("expired"));
        assert!(s.exists("alive"));
    }

    #[test]
    fn noeviction_rejects_writes() {
        let s = Store::new(StoreConfig {
            memory_limit: 200,
            eviction_policy: EvictionPolicy::NoEviction,
        });
        // First write should succeed.
        assert!(s.set("k".into(), b"v".to_vec(), None));
        // Fill up memory with a large value.
        assert!(!s.set("big".into(), vec![0u8; 1024], None));
    }
}

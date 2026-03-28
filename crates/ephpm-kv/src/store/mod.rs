//! In-memory KV store engine.
//!
//! Backed by [`dashmap::DashMap`] for lock-free concurrent reads and
//! fine-grained write locking. Supports TTL expiry, LRU eviction, and
//! approximate memory tracking.

mod entry;

use std::io::Write;
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
            "volatile-lru" => Self::VolatileLru,
            "allkeys-random" => Self::AllKeysRandom,
            _ => Self::AllKeysLru,
        }
    }
}

/// Compression algorithm for stored values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionAlgo {
    /// No compression.
    #[default]
    None,
    /// Gzip compression.
    Gzip,
    /// Brotli compression.
    Brotli,
    /// Zstandard compression.
    Zstd,
}

impl CompressionAlgo {
    /// Parse from the config string. Falls back to `None` on unknown values.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "gzip" => Self::Gzip,
            "brotli" | "br" => Self::Brotli,
            "zstd" => Self::Zstd,
            _ => Self::None,
        }
    }
}

/// Configuration for value compression in the KV store.
#[derive(Debug, Clone, Copy)]
pub struct CompressionConfig {
    /// The compression algorithm to use.
    pub algo: CompressionAlgo,
    /// Compression level (1 = fastest, 9 = best compression).
    pub level: u32,
    /// Minimum value size in bytes before compression is applied.
    pub min_size: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            algo: CompressionAlgo::None,
            level: 6,
            min_size: 1024,
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
    /// Compression configuration.
    pub compression: CompressionConfig,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            memory_limit: 256 * 1024 * 1024, // 256 MiB
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
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
    /// Transparently decompresses the value if it was stored compressed.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let mut entry = self.data.get_mut(key)?;
        if entry.is_expired() {
            drop(entry);
            self.remove(key);
            return None;
        }
        entry.touch();
        if entry.compressed {
            decompress_value(&entry.data, self.config.compression.algo)
        } else {
            Some(entry.data.clone())
        }
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
        // Try compression if configured and size is above threshold.
        let (data, compressed) =
            if self.config.compression.algo != CompressionAlgo::None
                && value.len() >= self.config.compression.min_size
            {
                let compressed_data = compress_value(&value, self.config.compression);
                if compressed_data.len() < value.len() {
                    (compressed_data, true)
                } else {
                    (value, false)
                }
            } else {
                (value, false)
            };

        let entry = match ttl {
            Some(dur) => Entry::with_expiry(data, key.len(), compressed, Instant::now() + dur),
            None => Entry::new(data, key.len(), compressed),
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
                // Decompress if needed before parsing.
                let data = if entry.compressed {
                    decompress_value(&entry.data, self.config.compression.algo)
                        .ok_or_else(|| "ERR failed to decompress value".to_string())?
                } else {
                    entry.data.clone()
                };

                let current = parse_int_value(&data)?;
                let new_val = current.checked_add(delta).ok_or_else(|| {
                    "ERR increment or decrement would overflow".to_string()
                })?;
                let new_bytes = new_val.to_string().into_bytes();

                // Try compression again if configured.
                let (stored_data, compressed) =
                    if self.config.compression.algo != CompressionAlgo::None
                        && new_bytes.len() >= self.config.compression.min_size
                    {
                        let compressed_data = compress_value(&new_bytes, self.config.compression);
                        if compressed_data.len() < new_bytes.len() {
                            (compressed_data, true)
                        } else {
                            (new_bytes, false)
                        }
                    } else {
                        (new_bytes, false)
                    };

                let old_mem = entry.mem_size;
                entry.data = stored_data;
                entry.compressed = compressed;
                entry.mem_size = Entry::new(entry.data.clone(), key.len(), compressed).mem_size;
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
                // Decompress if needed before appending.
                let mut data = if entry.compressed {
                    decompress_value(&entry.data, self.config.compression.algo)
                        .unwrap_or_else(|| entry.data.clone())
                } else {
                    entry.data.clone()
                };

                let _added = value.len();
                data.extend_from_slice(value);

                // Try compression again if configured.
                let (stored_data, compressed) =
                    if self.config.compression.algo != CompressionAlgo::None
                        && data.len() >= self.config.compression.min_size
                    {
                        let compressed_data = compress_value(&data, self.config.compression);
                        if compressed_data.len() < data.len() {
                            (compressed_data, true)
                        } else {
                            (data.clone(), false)
                        }
                    } else {
                        (data.clone(), false)
                    };

                let old_mem = entry.mem_size;
                entry.data = stored_data;
                entry.compressed = compressed;
                entry.mem_size = Entry::new(entry.data.clone(), key.len(), compressed).mem_size;
                entry.touch();
                let new_mem = entry.mem_size;
                drop(entry);
                // Adjust memory tracking.
                if new_mem > old_mem {
                    self.mem_add(new_mem - old_mem);
                } else {
                    self.mem_sub(old_mem - new_mem);
                }
                return data.len();
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

        for entry in &self.data {
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

            for entry in &self.data {
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
    let chars: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, &chars)
}

fn glob_match_inner(pat: &[char], chars: &[char]) -> bool {
    match (pat.first(), chars.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            // '*' matches zero or more characters.
            glob_match_inner(&pat[1..], chars)
                || (!chars.is_empty() && glob_match_inner(pat, &chars[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&pat[1..], &chars[1..]),
        (Some(a), Some(b)) if a == b => glob_match_inner(&pat[1..], &chars[1..]),
        _ => false,
    }
}

/// Compress a value using the configured algorithm.
/// Returns the compressed bytes (caller should check if size reduction is worth it).
fn compress_value(data: &[u8], config: CompressionConfig) -> Vec<u8> {
    match config.algo {
        CompressionAlgo::None => unreachable!(),
        CompressionAlgo::Gzip => {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(config.level));
            if encoder.write_all(data).is_err() {
                return data.to_vec(); // Fall back to uncompressed
            }
            match encoder.finish() {
                Ok(compressed) => compressed,
                Err(_) => data.to_vec(),
            }
        }
        CompressionAlgo::Brotli => {
            // The brotli crate (FFI bindings to C library) doesn't expose a simple
            // high-level compression function like flate2::Compression or zstd::encode_all.
            // For now, fall back to storing uncompressed. Users should prefer gzip or zstd.
            // TODO: Implement via raw FFI bindings to BrotliEncoderCompress if needed.
            data.to_vec()
        }
        CompressionAlgo::Zstd => {
            #[allow(clippy::cast_possible_wrap)]
            match zstd::encode_all(data, config.level as i32) {
                Ok(compressed) => compressed,
                Err(_) => data.to_vec(),
            }
        }
    }
}

/// Decompress a value using the specified algorithm.
/// Returns `None` if decompression fails.
fn decompress_value(data: &[u8], algo: CompressionAlgo) -> Option<Vec<u8>> {
    use std::io::Read;
    match algo {
        CompressionAlgo::None => unreachable!(),
        CompressionAlgo::Gzip => {
            let decoder = flate2::read::GzDecoder::new(data);
            let mut output = Vec::new();
            match std::io::BufReader::new(decoder).read_to_end(&mut output) {
                Ok(_) => Some(output),
                Err(_) => None,
            }
        }
        CompressionAlgo::Brotli => {
            let mut decompressor = brotli::Decompressor::new(data, 4096);
            let mut output = Vec::new();
            if decompressor.read_to_end(&mut output).is_ok() {
                Some(output)
            } else {
                None
            }
        }
        CompressionAlgo::Zstd => zstd::decode_all(data).ok(),
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
            false,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
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
            false,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
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
            compression: CompressionConfig::default(),
        });
        // First write should succeed.
        assert!(s.set("k".into(), b"v".to_vec(), None));
        // Fill up memory with a large value.
        assert!(!s.set("big".into(), vec![0u8; 1024], None));
    }

    #[test]
    fn compression_gzip_round_trip() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig {
                algo: CompressionAlgo::Gzip,
                level: 6,
                min_size: 10,
            },
        });
        let data = b"hello world this is a test string that should compress well";
        s.set("key".into(), data.to_vec(), None);
        let retrieved = s.get("key");
        assert_eq!(retrieved, Some(data.to_vec()));
    }

    #[test]
    fn compression_brotli_round_trip() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig {
                algo: CompressionAlgo::Brotli,
                level: 6,
                min_size: 10,
            },
        });
        let data = b"hello world this is another test string for brotli";
        s.set("key".into(), data.to_vec(), None);
        let retrieved = s.get("key");
        assert_eq!(retrieved, Some(data.to_vec()));
    }

    #[test]
    fn compression_zstd_round_trip() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig {
                algo: CompressionAlgo::Zstd,
                level: 6,
                min_size: 10,
            },
        });
        let data = b"hello world this is yet another test string for zstd";
        s.set("key".into(), data.to_vec(), None);
        let retrieved = s.get("key");
        assert_eq!(retrieved, Some(data.to_vec()));
    }

    #[test]
    fn compression_below_min_size_not_compressed() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig {
                algo: CompressionAlgo::Gzip,
                level: 6,
                min_size: 1000,
            },
        });
        let data = b"tiny";
        s.set("key".into(), data.to_vec(), None);
        let entry = s.data.get("key").unwrap();
        assert!(!entry.compressed);
        let retrieved = s.get("key");
        assert_eq!(retrieved, Some(data.to_vec()));
    }

    #[test]
    fn compression_incr_by_works() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig {
                algo: CompressionAlgo::Gzip,
                level: 6,
                min_size: 1,
            },
        });
        assert_eq!(s.incr_by("counter", 1), Ok(1));
        assert_eq!(s.incr_by("counter", 5), Ok(6));
        let retrieved = s.get("counter");
        assert_eq!(retrieved, Some(b"6".to_vec()));
    }

    #[test]
    fn compression_append_works() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig {
                algo: CompressionAlgo::Zstd,
                level: 6,
                min_size: 1,
            },
        });
        assert_eq!(s.append("key", b"hello"), 5);
        assert_eq!(s.append("key", b" world"), 11);
        let retrieved = s.get("key");
        assert_eq!(retrieved, Some(b"hello world".to_vec()));
    }
}

//! In-memory KV store engine.
//!
//! Backed by [`dashmap::DashMap`] for lock-free concurrent reads and
//! fine-grained write locking. Supports TTL expiry, LRU eviction, and
//! approximate memory tracking.

mod entry;

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
pub use entry::Entry;
use tracing::{debug, trace};

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
        Self { algo: CompressionAlgo::None, level: 6, min_size: 1024 }
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

/// A hash entry with TTL support.
#[derive(Debug, Clone)]
struct HashEntry {
    /// Field → value map. Values are [`Bytes`] so `hget`/`hgetall`/
    /// `hvals` return `Arc`-clones instead of copying every byte.
    fields: HashMap<String, Bytes>,
    /// Absolute expiry time, or `None` for persistent keys.
    expires_at: Option<Instant>,
}

impl HashEntry {
    fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Instant::now() >= exp)
    }

    /// Rough memory estimate.
    fn mem_size(&self) -> usize {
        self.fields.iter().map(|(k, v)| k.len() + v.len() + 64).sum::<usize>() + 64
    }
}

/// Late-bound replication hook.
///
/// When installed via [`Store::set_replicator`], the store's public
/// `set`/`remove`/`expire` entry points **delegate** the operation to the
/// replicator instead of writing to the local map. The replicator is
/// responsible for tier routing (e.g. gossip vs local, replica fan-out) and
/// for eventually calling back into [`Store::set_local`] / [`Store::remove_local`]
/// / [`Store::expire_local`] for any local-copy write.
///
/// The trait is synchronous because `Store::set` etc. are called from
/// non-async contexts (RESP command dispatch, PHP native FFI callbacks). An
/// async replicator (like `ephpm-cluster::ClusteredStore`) captures a tokio
/// runtime handle at construction time and uses `Handle::spawn` internally
/// for async work.
///
/// # Recursion guard
///
/// A replicator MUST NOT call [`Store::set`] / [`Store::remove`] /
/// [`Store::expire`] on the same store instance — those would re-enter the
/// hook and loop forever. Use the `*_local` variants for local writes.
pub trait Replicator: Send + Sync + std::fmt::Debug {
    /// Handle a public `set`. Returns `true` on success, mirroring
    /// [`Store::set`]'s semantics.
    fn replicate_set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool;

    /// Handle a public `remove`. Returns `true` if the key existed.
    fn replicate_remove(&self, key: &str) -> bool;

    /// Handle a public `expire`. Returns `true` if the key existed and the
    /// TTL was applied.
    fn replicate_expire(&self, key: &str, ttl: Duration) -> bool;
}

/// Thread-safe in-memory KV store.
pub struct Store {
    /// The main data map (string values).
    data: DashMap<String, Entry>,
    /// Hash values stored separately to avoid Entry enum complexity.
    hashes: DashMap<String, HashEntry>,
    /// Approximate total memory used by all entries.
    mem_used: AtomicUsize,
    /// Store configuration.
    config: StoreConfig,
    /// Optional replication hook installed after construction (e.g. by the
    /// server after cluster gossip starts). `None` on single-node — writes
    /// take the direct local path with zero extra atomics beyond this Option
    /// check.
    replicator: std::sync::RwLock<Option<Arc<dyn Replicator>>>,
    /// Anchor `Instant` for `Entry::last_accessed`. Storing nanoseconds
    /// since this anchor as a `u64` lets each `Entry` keep its LRU
    /// timestamp in an `AtomicU64` — so `get()` can touch it under the
    /// shard *read* lock instead of taking the write lock via
    /// `get_mut`. 584 years of headroom in `u64::MAX` nanos.
    anchor: Instant,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("data_len", &self.data.len())
            .field("hashes_len", &self.hashes.len())
            .field("mem_used", &self.mem_used.load(Ordering::Relaxed))
            .field("config", &self.config)
            .field("replicator_installed", &self.replicator.read().is_ok_and(|g| g.is_some()))
            .field("anchor", &self.anchor)
            .finish()
    }
}

impl Store {
    /// Create a new store with the given configuration.
    #[must_use]
    pub fn new(config: StoreConfig) -> Arc<Self> {
        Arc::new(Self {
            data: DashMap::new(),
            hashes: DashMap::new(),
            mem_used: AtomicUsize::new(0),
            config,
            replicator: std::sync::RwLock::new(None),
            anchor: Instant::now(),
        })
    }

    /// Install a [`Replicator`] hook so subsequent [`Store::set`],
    /// [`Store::remove`], and [`Store::expire`] calls delegate the write to
    /// it (typically a clustered store that decides gossip vs local tier and
    /// fans out to replica nodes).
    ///
    /// Passing `None` unhooks. Local writes issued via [`Store::set_local`] /
    /// [`Store::remove_local`] / [`Store::expire_local`] always bypass the
    /// hook — a replicator uses those to write its own local copy without
    /// re-entering itself.
    ///
    /// This method is called at most once at server startup, after cluster
    /// gossip has come up and the wrapping clustered store has been
    /// constructed. See `ephpm-server::start_kv_service` /
    /// `install_kv_replicator`.
    pub fn set_replicator(&self, replicator: Option<Arc<dyn Replicator>>) {
        // Poisoning is impossible here (no `.write()` guard panics inside
        // held-lock scope), but the API is fallible — handle it defensively.
        if let Ok(mut slot) = self.replicator.write() {
            *slot = replicator;
        }
    }

    /// Snapshot the currently-installed replicator, if any.
    fn active_replicator(&self) -> Option<Arc<dyn Replicator>> {
        self.replicator.read().ok().and_then(|g| g.clone())
    }

    /// Current time as nanoseconds since the store anchor. Used for
    /// `AtomicU64`-based LRU timestamps on entries.
    #[inline]
    fn now_nanos(&self) -> u64 {
        // Saturate at u64::MAX rather than panicking on the (unreachable
        // in practice) overflow after 584 years of uptime.
        u64::try_from(self.anchor.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    // ── Read operations ──────────────────────────────────────────

    /// Get a value by key. Returns `None` if missing or expired.
    /// Touches the entry for LRU tracking.
    /// Transparently decompresses the value if it was stored compressed.
    ///
    /// # Performance
    ///
    /// Returns [`Bytes`] rather than `Vec<u8>` so the caller gets a
    /// cheap `Arc`-clone of the stored value, not a full byte copy. Uses
    /// the shard **read** lock (`DashMap::get`) rather than the write
    /// lock (`get_mut`): the LRU touch is an `AtomicU64::store`, so
    /// concurrent GETs on the same shard no longer serialise.
    ///
    /// A compressed value is decoded into a fresh `Bytes` on the read
    /// path; that decode allocation is unavoidable but happens exactly
    /// once per read, not twice.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Bytes> {
        let entry = self.data.get(key)?;
        if entry.is_expired() {
            drop(entry);
            // Lazy-expiry cleanup: local only. Replicas expire the key on
            // their own timelines; no need to broadcast the reap.
            self.remove_local(key);
            return None;
        }
        entry.touch(self.now_nanos());
        if entry.compressed {
            decompress_value(&entry.data, self.config.compression.algo)
        } else {
            // Bytes::clone is an atomic refcount bump — no memcpy.
            Some(entry.data.clone())
        }
    }

    /// Check if a key exists (and is not expired). Checks both string and hash keys.
    #[must_use]
    pub fn exists(&self, key: &str) -> bool {
        // Check string keys.
        if let Some(entry) = self.data.get(key) {
            if entry.is_expired() {
                drop(entry);
                // Lazy-expiry cleanup: local only.
                self.remove_local(key);
            } else {
                return true;
            }
        }
        // Check hash keys.
        self.is_hash(key)
    }

    /// Get the remaining TTL for a key in milliseconds.
    /// Returns `None` if the key doesn't exist, `Some(-1)` if no expiry,
    /// `Some(-2)` if expired/missing.
    #[must_use]
    pub fn pttl(&self, key: &str) -> Option<i64> {
        let entry = self.data.get(key)?;
        if entry.is_expired() {
            drop(entry);
            // Lazy-expiry cleanup: local only.
            self.remove_local(key);
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
    /// Counts both string and hash keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len() + self.hashes.len()
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
    /// Returns both string and hash keys.
    #[must_use]
    pub fn keys(&self, pattern: &str) -> Vec<String> {
        let match_all = pattern == "*";
        let mut result: Vec<String> = self
            .data
            .iter()
            .filter(|entry| !entry.value().is_expired())
            .filter(|entry| match_all || glob_match(pattern, entry.key()))
            .map(|entry| entry.key().clone())
            .collect();
        result.extend(
            self.hashes
                .iter()
                .filter(|entry| !entry.value().is_expired())
                .filter(|entry| match_all || glob_match(pattern, entry.key()))
                .map(|entry| entry.key().clone()),
        );
        result
    }

    // ── Write operations ─────────────────────────────────────────

    /// Set a key to a value with an optional TTL.
    ///
    /// When a [`Replicator`] is installed (clustered mode), the write is
    /// delegated to it — the replicator decides tier routing (gossip vs
    /// local) and replica fan-out, and issues any local-copy write through
    /// [`Store::set_local`]. Otherwise the write goes straight to the local
    /// map.
    ///
    /// Returns `true` if the write succeeded, `false` if rejected by
    /// the `NoEviction` policy when the memory limit is reached (or by the
    /// replicator's own primary write).
    pub fn set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        if let Some(rep) = self.active_replicator() {
            return rep.replicate_set(key, value, ttl);
        }
        self.set_local(key, value, ttl)
    }

    /// Local-only write — bypasses any installed [`Replicator`]. Used by a
    /// replicator implementation to write its own local copy without
    /// re-entering itself. All other callers should use [`Store::set`].
    pub fn set_local(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        // Try compression if configured and size is above threshold.
        let (data, compressed) = if self.config.compression.algo != CompressionAlgo::None
            && value.len() >= self.config.compression.min_size
        {
            let compressed_data = compress_value(&value, self.config.compression);
            if compressed_data.len() < value.len() {
                (Bytes::from(compressed_data), true)
            } else {
                (Bytes::from(value), false)
            }
        } else {
            (Bytes::from(value), false)
        };

        let now = self.now_nanos();
        let entry = match ttl {
            Some(dur) => Entry::with_expiry(data, key.len(), compressed, Instant::now() + dur, now),
            None => Entry::new(data, key.len(), compressed, now),
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

    /// Atomically set a key only if it doesn't already exist (`SETNX`).
    ///
    /// Returns `true` if the value was inserted, `false` if a live entry
    /// was already present at this key. Expired entries are treated as
    /// vacant — they get replaced and `true` returned.
    ///
    /// Unlike `set()`, the existence check and the insert are performed
    /// under the same per-key write lock, so concurrent `set_nx` callers
    /// will see exactly one winner. This is the foundation primitive
    /// for distributed locks, idempotency keys, single-execution
    /// constraints, and leader election.
    ///
    /// Returns `false` if the `NoEviction` policy refuses the write
    /// because of memory pressure (same as `set()`).
    pub fn set_nx(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        // Fast path: peek without taking the per-key write lock. If the
        // key is already present and live we can bail before triggering
        // any eviction work. The TOCTOU window between this peek and the
        // entry() lock below is fine — the locked check below catches
        // it; the peek just saves an unnecessary `ensure_memory` call
        // for the common "already taken" case.
        if let Some(existing) = self.data.get(&key) {
            if !existing.is_expired() {
                return false;
            }
        }

        // Build the candidate entry first so we can compute its size
        // before reserving memory.
        let (data, compressed) = if self.config.compression.algo != CompressionAlgo::None
            && value.len() >= self.config.compression.min_size
        {
            let compressed_data = compress_value(&value, self.config.compression);
            if compressed_data.len() < value.len() {
                (Bytes::from(compressed_data), true)
            } else {
                (Bytes::from(value), false)
            }
        } else {
            (Bytes::from(value), false)
        };

        let now = self.now_nanos();
        let new_entry = match ttl {
            Some(dur) => Entry::with_expiry(data, key.len(), compressed, Instant::now() + dur, now),
            None => Entry::new(data, key.len(), compressed, now),
        };
        let new_size = new_entry.mem_size;

        // Reserve memory BEFORE taking the per-key entry lock — eviction
        // may need to remove entries from arbitrary shards (potentially
        // including this one) and would deadlock if called under the
        // entry guard. The `set()` method makes the same trade-off.
        if !self.ensure_memory(new_size) {
            return false;
        }

        // Atomic check-and-insert. The shard write lock held by `entry()`
        // serialises concurrent set_nx calls for this key.
        match self.data.entry(key) {
            dashmap::Entry::Occupied(mut occ) => {
                if !occ.get().is_expired() {
                    // Lost the race; another writer landed first. We
                    // already ran `ensure_memory` which may have evicted
                    // unrelated keys — that's wasted work but not a
                    // correctness bug.
                    return false;
                }
                // The existing entry has expired; reclaim its bytes and
                // replace it.
                self.mem_sub(occ.get().mem_size);
                self.mem_add(new_size);
                occ.insert(new_entry);
                true
            }
            dashmap::Entry::Vacant(vac) => {
                self.mem_add(new_size);
                vac.insert(new_entry);
                true
            }
        }
    }

    /// Remove a key, returning `true` if it existed. Removes from both
    /// string and hash storage.
    ///
    /// When a [`Replicator`] is installed, the delete is delegated to it so
    /// remote copies are dropped as well. See [`Store::remove_local`] for
    /// the bypass variant.
    pub fn remove(&self, key: &str) -> bool {
        if let Some(rep) = self.active_replicator() {
            return rep.replicate_remove(key);
        }
        self.remove_local(key)
    }

    /// Local-only remove — bypasses any installed [`Replicator`]. Used by a
    /// replicator implementation to drop its own local copy.
    pub fn remove_local(&self, key: &str) -> bool {
        let string_removed = if let Some((_, old)) = self.data.remove(key) {
            self.mem_sub(old.mem_size);
            true
        } else {
            false
        };
        let hash_removed = self.hash_remove(key);
        string_removed || hash_removed
    }

    /// Set an expiry on an existing key. Returns `false` if the key doesn't exist.
    ///
    /// When a [`Replicator`] is installed, the TTL update is delegated so
    /// remote copies see the change. See [`Store::expire_local`] for the
    /// bypass variant.
    pub fn expire(&self, key: &str, ttl: Duration) -> bool {
        if let Some(rep) = self.active_replicator() {
            return rep.replicate_expire(key, ttl);
        }
        self.expire_local(key, ttl)
    }

    /// Local-only expire — bypasses any installed [`Replicator`].
    pub fn expire_local(&self, key: &str, ttl: Duration) -> bool {
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.remove_local(key);
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
                // Lazy-expiry cleanup: local only.
                self.remove_local(key);
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
        self.incr_by_with_ttl(key, delta, None)
    }

    /// Increment the value at `key` by `delta`, applying `ttl` only when the
    /// key is created by this call. Treats the stored bytes as a decimal
    /// integer string.
    ///
    /// Fixed-window semantics: when the key already exists the TTL is left
    /// untouched (the window keeps counting down toward its original expiry);
    /// when the key is absent it is created with value `delta` and the given
    /// `ttl`. Passing `ttl = None` reproduces plain [`incr_by`] behaviour
    /// (no expiry on create).
    ///
    /// This closes the fixed-window rate-limit race where a missing key
    /// created by `incr` (no TTL) could become immortal if a separate
    /// `set_nx` pre-seed lost the race or was skipped.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the stored value is not a valid integer string.
    pub fn incr_by_with_ttl(
        &self,
        key: &str,
        delta: i64,
        ttl: Option<Duration>,
    ) -> Result<i64, String> {
        // Fast path: key exists, try to update in place.
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                // Lazy-expiry cleanup: local only.
                self.remove_local(key);
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
                let new_val = current
                    .checked_add(delta)
                    .ok_or_else(|| "ERR increment or decrement would overflow".to_string())?;
                let new_bytes = new_val.to_string().into_bytes();

                // Try compression again if configured.
                let (stored_data, compressed) = if self.config.compression.algo
                    != CompressionAlgo::None
                    && new_bytes.len() >= self.config.compression.min_size
                {
                    let compressed_data = compress_value(&new_bytes, self.config.compression);
                    if compressed_data.len() < new_bytes.len() {
                        (Bytes::from(compressed_data), true)
                    } else {
                        (Bytes::from(new_bytes), false)
                    }
                } else {
                    (Bytes::from(new_bytes), false)
                };

                let old_mem = entry.mem_size;
                entry.data = stored_data;
                entry.compressed = compressed;
                entry.mem_size = Entry::new(entry.data.clone(), key.len(), compressed, 0).mem_size;
                entry.touch(self.now_nanos());
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

        // Key doesn't exist — create it with the caller's TTL (if any).
        let val_bytes = delta.to_string().into_bytes();
        self.set(key.to_string(), val_bytes, ttl);
        Ok(delta)
    }

    /// Append `value` to the existing value at `key`, or create it.
    /// Returns the new length of the value.
    pub fn append(&self, key: &str, value: &[u8]) -> usize {
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                // Lazy-expiry cleanup: local only.
                self.remove_local(key);
                // Fall through to create.
            } else {
                // Decompress if needed before appending. We need a
                // mutable owned Vec to grow, so decompress-or-copy the
                // stored bytes into a fresh Vec.
                let mut data: Vec<u8> = if entry.compressed {
                    decompress_value(&entry.data, self.config.compression.algo)
                        .map_or_else(|| entry.data.to_vec(), |b| b.to_vec())
                } else {
                    entry.data.to_vec()
                };

                data.extend_from_slice(value);
                let final_len = data.len();

                // Try compression again if configured.
                let (stored_data, compressed) = if self.config.compression.algo
                    != CompressionAlgo::None
                    && data.len() >= self.config.compression.min_size
                {
                    let compressed_data = compress_value(&data, self.config.compression);
                    if compressed_data.len() < data.len() {
                        (Bytes::from(compressed_data), true)
                    } else {
                        (Bytes::from(data), false)
                    }
                } else {
                    (Bytes::from(data), false)
                };

                let old_mem = entry.mem_size;
                entry.data = stored_data;
                entry.compressed = compressed;
                entry.mem_size = Entry::new(entry.data.clone(), key.len(), compressed, 0).mem_size;
                entry.touch(self.now_nanos());
                let new_mem = entry.mem_size;
                drop(entry);
                // Adjust memory tracking.
                if new_mem > old_mem {
                    self.mem_add(new_mem - old_mem);
                } else {
                    self.mem_sub(old_mem - new_mem);
                }
                return final_len;
            }
        }

        let len = value.len();
        self.set(key.to_string(), value.to_vec(), None);
        len
    }

    // ── Hash operations ──────────────────────────────────────────

    /// Set a field in a hash. Creates the hash if it doesn't exist.
    ///
    /// Returns `true` if the field was newly inserted, `false` if updated.
    pub fn hset(&self, key: &str, field: &str, value: Vec<u8>) -> bool {
        let field_mem = field.len() + value.len() + 64;
        let value = Bytes::from(value);
        let mut entry = self.hashes.entry(key.to_string()).or_insert_with(|| {
            self.mem_add(64); // base hash overhead
            HashEntry { fields: HashMap::new(), expires_at: None }
        });
        if entry.is_expired() {
            let old_mem = entry.mem_size();
            entry.fields.clear();
            entry.expires_at = None;
            self.mem_sub(old_mem);
        }
        let is_new = !entry.fields.contains_key(field);
        if let Some(old_val) = entry.fields.insert(field.to_string(), value) {
            // Replaced — adjust memory for the difference.
            let old_field_mem = field.len() + old_val.len() + 64;
            if field_mem > old_field_mem {
                self.mem_add(field_mem - old_field_mem);
            } else {
                self.mem_sub(old_field_mem - field_mem);
            }
        } else {
            self.mem_add(field_mem);
        }
        is_new
    }

    /// Get a field value from a hash.
    ///
    /// Returns a cheap [`Bytes`] `Arc`-clone; no memcpy of the field
    /// value.
    #[must_use]
    pub fn hget(&self, key: &str, field: &str) -> Option<Bytes> {
        let entry = self.hashes.get(key)?;
        if entry.is_expired() {
            drop(entry);
            self.hash_remove(key);
            return None;
        }
        entry.fields.get(field).cloned()
    }

    /// Delete a field from a hash.
    ///
    /// Returns `true` if the field existed and was removed.
    pub fn hdel(&self, key: &str, field: &str) -> bool {
        if let Some(mut entry) = self.hashes.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.hash_remove(key);
                return false;
            }
            if let Some(old_val) = entry.fields.remove(field) {
                let freed = field.len() + old_val.len() + 64;
                self.mem_sub(freed);
                // Remove the hash key entirely if empty.
                if entry.fields.is_empty() {
                    drop(entry);
                    self.hash_remove(key);
                }
                return true;
            }
        }
        false
    }

    /// Get all field-value pairs from a hash.
    ///
    /// Values are cheap [`Bytes`] `Arc`-clones; field names are copied
    /// (they're `String`s, no `Arc` sharing).
    #[must_use]
    pub fn hgetall(&self, key: &str) -> Vec<(String, Bytes)> {
        let Some(entry) = self.hashes.get(key) else {
            return Vec::new();
        };
        if entry.is_expired() {
            drop(entry);
            self.hash_remove(key);
            return Vec::new();
        }
        entry.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Get all field names from a hash.
    #[must_use]
    pub fn hkeys(&self, key: &str) -> Vec<String> {
        let Some(entry) = self.hashes.get(key) else {
            return Vec::new();
        };
        if entry.is_expired() {
            drop(entry);
            self.hash_remove(key);
            return Vec::new();
        }
        entry.fields.keys().cloned().collect()
    }

    /// Get all values from a hash.
    ///
    /// Returned values are cheap [`Bytes`] `Arc`-clones.
    #[must_use]
    pub fn hvals(&self, key: &str) -> Vec<Bytes> {
        let Some(entry) = self.hashes.get(key) else {
            return Vec::new();
        };
        if entry.is_expired() {
            drop(entry);
            self.hash_remove(key);
            return Vec::new();
        }
        entry.fields.values().cloned().collect()
    }

    /// Get the number of fields in a hash.
    #[must_use]
    pub fn hlen(&self, key: &str) -> usize {
        let Some(entry) = self.hashes.get(key) else {
            return 0;
        };
        if entry.is_expired() {
            drop(entry);
            self.hash_remove(key);
            return 0;
        }
        entry.fields.len()
    }

    /// Check if a field exists in a hash.
    #[must_use]
    pub fn hexists(&self, key: &str, field: &str) -> bool {
        let Some(entry) = self.hashes.get(key) else {
            return false;
        };
        if entry.is_expired() {
            drop(entry);
            self.hash_remove(key);
            return false;
        }
        entry.fields.contains_key(field)
    }

    /// Remove a hash key entirely.
    fn hash_remove(&self, key: &str) -> bool {
        if let Some((_, old)) = self.hashes.remove(key) {
            self.mem_sub(old.mem_size());
            true
        } else {
            false
        }
    }

    /// Check if a key exists as a hash.
    #[must_use]
    pub fn is_hash(&self, key: &str) -> bool {
        if let Some(entry) = self.hashes.get(key) {
            if entry.is_expired() {
                drop(entry);
                self.hash_remove(key);
                return false;
            }
            return true;
        }
        false
    }

    /// Remove all keys.
    pub fn flush(&self) {
        self.data.clear();
        self.hashes.clear();
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
            // Lazy-expiry cleanup: local only. Replicas run their own
            // expire_pass on the same TTLs.
            self.remove_local(key);
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
        // Saturating subtraction: `mem_used` is an *approximate*, unsigned
        // counter, so it must never underflow. `flush()` resets it to 0 with a
        // plain `store`, which races with concurrent `remove`/`set` on other
        // worker threads (the store is `Arc`-shared across the spawn_blocking
        // pool): a thread can pull an entry out of the map — capturing its
        // `mem_size` — just before `flush` zeroes the counter, then run its
        // `mem_sub` afterwards. A plain `fetch_sub` would wrap to ~`usize::MAX`,
        // making `ensure_memory` believe the store is permanently full
        // (rejecting every write under NoEviction, eviction-spinning under LRU)
        // until restart. Flooring at 0 turns that catastrophe into a harmless,
        // self-correcting slight undercount.
        let _ = self
            .mem_used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| Some(cur.saturating_sub(n)));
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
            // last_accessed is nanoseconds since the store anchor, read
            // via a Relaxed atomic load — LRU sampling is approximate by
            // design so a lost update just picks a slightly-wrong
            // victim, never a correctness bug.
            let mut oldest: Option<(String, u64)> = None;
            let mut count = 0;

            for entry in &self.data {
                if volatile_only && entry.value().expires_at.is_none() {
                    continue;
                }
                let ts = entry.value().last_accessed_nanos();
                match &oldest {
                    Some((_, oldest_ts)) if ts < *oldest_ts => {
                        oldest = Some((entry.key().clone(), ts));
                    }
                    None => {
                        oldest = Some((entry.key().clone(), ts));
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
                // Eviction is a local memory-pressure decision — replicas
                // manage their own memory. Do not broadcast the reap.
                self.remove_local(&key);
            } else {
                return false; // nothing to evict
            }
        }

        // Gave it a good try.
        self.mem_used.load(Ordering::Relaxed) + needed <= limit
    }

    /// Random eviction — pick a pseudo-random key and remove it.
    ///
    /// Uses time-based entropy mixed with an iteration counter to select
    /// different keys on each attempt, avoiding the deterministic "always
    /// evict the first shard entry" pattern.
    fn evict_random(&self, needed: usize) -> bool {
        let limit = self.config.memory_limit;

        for attempt in 0..100u64 {
            let current = self.mem_used.load(Ordering::Relaxed);
            if current + needed <= limit {
                return true;
            }

            let len = self.data.len();
            if len == 0 {
                return false;
            }

            // Mix time nanos with attempt counter for pseudo-random offset.
            let nanos = u64::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos(),
            );
            let seed = nanos
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(attempt.wrapping_mul(1_442_695_040_888_963_407));
            #[allow(clippy::cast_possible_truncation)]
            let skip = (seed >> 33) as usize % len;

            let key = self.data.iter().nth(skip).map(|e| e.key().clone());
            if let Some(key) = key {
                debug!(key = %key, "evicting key (random)");
                // Eviction is a local memory-pressure decision.
                self.remove_local(&key);
            } else {
                // skip went past the end — try first entry as fallback.
                let key = self.data.iter().next().map(|e| e.key().clone());
                if let Some(key) = key {
                    debug!(key = %key, "evicting key (random fallback)");
                    self.remove_local(&key);
                } else {
                    return false;
                }
            }
        }

        self.mem_used.load(Ordering::Relaxed) + needed <= limit
    }
}

/// Parse a stored byte value as a decimal integer.
fn parse_int_value(data: &[u8]) -> Result<i64, String> {
    let s = std::str::from_utf8(data)
        .map_err(|_| "ERR value is not an integer or out of range".to_string())?;
    s.parse::<i64>().map_err(|_| "ERR value is not an integer or out of range".to_string())
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
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(config.level));
            if encoder.write_all(data).is_err() {
                return data.to_vec(); // Fall back to uncompressed
            }
            match encoder.finish() {
                Ok(compressed) => compressed,
                Err(_) => data.to_vec(),
            }
        }
        CompressionAlgo::Brotli => {
            let mut output = Vec::new();
            {
                let mut encoder =
                    brotli::CompressorWriter::new(&mut output, 4096, config.level, 22);
                if encoder.write_all(data).is_err() {
                    return data.to_vec(); // Fall back to uncompressed
                }
                // CompressorWriter flushes remaining data on drop.
            }
            output
        }
        CompressionAlgo::Zstd =>
        {
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
///
/// The decoded value is wrapped in [`Bytes`] at the boundary — the
/// decoder still needs to write into a growable `Vec<u8>`, but the
/// caller (typically `Store::get`) receives the same `Bytes` handle it
/// would have gotten from the fast path, so downstream code doesn't
/// branch on the compressed/uncompressed shape.
fn decompress_value(data: &[u8], algo: CompressionAlgo) -> Option<Bytes> {
    use std::io::Read;
    match algo {
        CompressionAlgo::None => unreachable!(),
        CompressionAlgo::Gzip => {
            let decoder = flate2::read::GzDecoder::new(data);
            let mut output = Vec::new();
            match std::io::BufReader::new(decoder).read_to_end(&mut output) {
                Ok(_) => Some(Bytes::from(output)),
                Err(_) => None,
            }
        }
        CompressionAlgo::Brotli => {
            let mut decompressor = brotli::Decompressor::new(data, 4096);
            let mut output = Vec::new();
            if decompressor.read_to_end(&mut output).is_ok() {
                Some(Bytes::from(output))
            } else {
                None
            }
        }
        CompressionAlgo::Zstd => zstd::decode_all(data).ok().map(Bytes::from),
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
        assert_eq!(s.get("hello").as_deref(), Some(&b"world"[..]));
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
        assert_eq!(s.get("k").as_deref(), Some(&b"v2"[..]));
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
    fn set_nx_inserts_when_absent() {
        let s = test_store();
        assert!(s.set_nx("k".into(), b"first".to_vec(), None));
        assert_eq!(s.get("k").as_deref(), Some(&b"first"[..]));
    }

    #[test]
    fn set_nx_refuses_when_present() {
        let s = test_store();
        s.set("k".into(), b"original".to_vec(), None);
        assert!(!s.set_nx("k".into(), b"replacement".to_vec(), None));
        // Original value untouched.
        assert_eq!(s.get("k").as_deref(), Some(&b"original"[..]));
    }

    #[test]
    fn set_nx_replaces_expired_entry() {
        let s = test_store();
        // Plant an already-expired entry by going through the Entry API
        // directly so we don't have to wait real time.
        let expired = Entry::with_expiry(
            Bytes::from_static(b"stale"),
            1,
            false,
            Instant::now() - Duration::from_secs(60),
            0,
        );
        let mem_size = expired.mem_size;
        s.data.insert("k".into(), expired);
        s.mem_add(mem_size);

        // Expired counts as vacant for SETNX purposes.
        assert!(s.set_nx("k".into(), b"fresh".to_vec(), None));
        assert_eq!(s.get("k").as_deref(), Some(&b"fresh"[..]));
    }

    #[test]
    fn set_nx_with_ttl_applies_expiry() {
        let s = test_store();
        assert!(s.set_nx("k".into(), b"v".to_vec(), Some(Duration::from_secs(60))));
        let entry = s.data.get("k").unwrap();
        assert!(entry.expires_at.is_some(), "expected TTL to be set");
    }

    #[test]
    fn incr_by_with_ttl_sets_ttl_on_create_only() {
        let s = test_store();
        // First incr creates the key with the window TTL.
        assert_eq!(s.incr_by_with_ttl("win", 1, Some(Duration::from_secs(60))), Ok(1));
        let created_expiry = {
            let entry = s.data.get("win").unwrap();
            entry.expires_at.expect("expected TTL on create")
        };

        // A subsequent incr must NOT extend or reset the TTL (fixed window).
        assert_eq!(s.incr_by_with_ttl("win", 1, Some(Duration::from_secs(600))), Ok(2));
        let after_expiry = {
            let entry = s.data.get("win").unwrap();
            entry.expires_at.expect("TTL must survive subsequent incr")
        };
        assert_eq!(
            created_expiry, after_expiry,
            "TTL must be set on create and left untouched on later incrs"
        );
    }

    #[test]
    fn incr_by_with_ttl_recreates_after_expiry() {
        let s = test_store();
        // Plant an already-expired counter directly.
        let expired = Entry::with_expiry(
            Bytes::from_static(b"5"),
            3,
            false,
            Instant::now() - Duration::from_secs(60),
            0,
        );
        let mem_size = expired.mem_size;
        s.data.insert("win".into(), expired);
        s.mem_add(mem_size);

        // Incr treats the expired key as vacant: value resets to delta and a
        // fresh TTL is applied.
        assert_eq!(s.incr_by_with_ttl("win", 1, Some(Duration::from_secs(60))), Ok(1));
        let entry = s.data.get("win").unwrap();
        assert!(entry.expires_at.is_some(), "fresh window must have a TTL");
    }

    #[test]
    fn set_nx_picks_one_winner_under_concurrent_callers() {
        // The whole reason set_nx exists: 32 threads all call it on the
        // same key, exactly one must observe true. The old exists+set
        // pattern in command.rs would let multiple callers all "win"
        // under contention.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let s = test_store();
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(32);
        for i in 0..32 {
            let s = Arc::clone(&s);
            let winners = Arc::clone(&winners);
            handles.push(thread::spawn(move || {
                let payload = format!("thread-{i}").into_bytes();
                if s.set_nx("contested".into(), payload, None) {
                    winners.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::Relaxed), 1, "exactly one set_nx must win");
        // And the key holds *some* thread's payload — which one is fine.
        assert!(s.get("contested").is_some());
    }

    #[test]
    fn ttl_expiry() {
        let s = test_store();
        // Set with a TTL that has already passed.
        let entry = Entry::with_expiry(
            Bytes::from_static(b"v"),
            1,
            false,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            0,
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
        assert_eq!(s.get("k").as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn append_existing() {
        let s = test_store();
        s.set("k".into(), b"hello".to_vec(), None);
        assert_eq!(s.append("k", b" world"), 11);
        assert_eq!(s.get("k").as_deref(), Some(&b"hello world"[..]));
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
            Bytes::from_static(b"v"),
            1,
            false,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            0,
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
            compression: CompressionConfig { algo: CompressionAlgo::Gzip, level: 6, min_size: 10 },
        });
        let data = b"hello world this is a test string that should compress well";
        s.set("key".into(), data.to_vec(), None);
        let retrieved = s.get("key");
        assert_eq!(retrieved.as_deref(), Some(&data[..]));
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
        assert_eq!(retrieved.as_deref(), Some(&data[..]));
    }

    #[test]
    fn compression_zstd_round_trip() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig { algo: CompressionAlgo::Zstd, level: 6, min_size: 10 },
        });
        let data = b"hello world this is yet another test string for zstd";
        s.set("key".into(), data.to_vec(), None);
        let retrieved = s.get("key");
        assert_eq!(retrieved.as_deref(), Some(&data[..]));
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
        // Drop the guard before calling s.get() to avoid DashMap deadlock.
        let is_compressed = {
            let entry = s.data.get("key").unwrap();
            entry.compressed
        };
        assert!(!is_compressed);
        let retrieved = s.get("key");
        assert_eq!(retrieved.as_deref(), Some(&data[..]));
    }

    #[test]
    fn compression_incr_by_works() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig { algo: CompressionAlgo::Gzip, level: 6, min_size: 1 },
        });
        assert_eq!(s.incr_by("counter", 1), Ok(1));
        assert_eq!(s.incr_by("counter", 5), Ok(6));
        let retrieved = s.get("counter");
        assert_eq!(retrieved.as_deref(), Some(&b"6"[..]));
    }

    #[test]
    fn compression_append_works() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig { algo: CompressionAlgo::Zstd, level: 6, min_size: 1 },
        });
        assert_eq!(s.append("key", b"hello"), 5);
        assert_eq!(s.append("key", b" world"), 11);
        let retrieved = s.get("key");
        assert_eq!(retrieved.as_deref(), Some(&b"hello world"[..]));
    }

    // ── eviction policy parsing ─────────────────────────────────────

    #[test]
    fn eviction_policy_from_str_lossy_all_variants() {
        assert_eq!(EvictionPolicy::from_str_lossy("noeviction"), EvictionPolicy::NoEviction);
        assert_eq!(EvictionPolicy::from_str_lossy("allkeys-lru"), EvictionPolicy::AllKeysLru);
        assert_eq!(EvictionPolicy::from_str_lossy("volatile-lru"), EvictionPolicy::VolatileLru);
        assert_eq!(EvictionPolicy::from_str_lossy("allkeys-random"), EvictionPolicy::AllKeysRandom);
        // Unknown falls back to AllKeysLru.
        assert_eq!(EvictionPolicy::from_str_lossy("bogus"), EvictionPolicy::AllKeysLru);
        assert_eq!(EvictionPolicy::from_str_lossy(""), EvictionPolicy::AllKeysLru);
    }

    // ── AllKeysLru eviction ─────────────────────────────────────────

    #[test]
    fn allkeys_lru_evicts_to_make_room() {
        let s = Store::new(StoreConfig {
            memory_limit: 2048,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        // Fill with entries. Each entry ~130 bytes overhead + value.
        for i in 0..10 {
            assert!(s.set(format!("k{i}"), vec![0u8; 50], None));
        }
        // This large write should trigger eviction of some keys.
        assert!(s.set("big".into(), vec![0u8; 500], None));
        assert!(s.get("big").is_some());
        // Some original keys must have been evicted.
        let remaining: usize = (0..10).filter(|i| s.exists(&format!("k{i}"))).count();
        assert!(remaining < 10, "expected some keys evicted, {remaining}/10 remain");
    }

    #[test]
    fn allkeys_lru_evicts_oldest_accessed_key() {
        let s = Store::new(StoreConfig {
            memory_limit: 900,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        // Insert 3 keys with small sleeps to get different timestamps.
        s.set("oldest".into(), vec![1u8; 50], None);
        std::thread::sleep(Duration::from_millis(10));
        s.set("middle".into(), vec![2u8; 50], None);
        std::thread::sleep(Duration::from_millis(10));
        s.set("newest".into(), vec![3u8; 50], None);

        // Touch "oldest" to refresh its LRU timestamp.
        let _ = s.get("oldest");
        std::thread::sleep(Duration::from_millis(10));

        // Force eviction with a large write.
        assert!(s.set("trigger".into(), vec![4u8; 200], None));

        // "middle" should be evicted (least recently accessed).
        assert!(s.get("middle").is_none(), "middle should be evicted");
        assert!(s.get("oldest").is_some(), "oldest should survive (was touched)");
    }

    // ── VolatileLru eviction ────────────────────────────────────────

    #[test]
    fn volatile_lru_only_evicts_keys_with_ttl() {
        let s = Store::new(StoreConfig {
            memory_limit: 4096,
            eviction_policy: EvictionPolicy::VolatileLru,
            compression: CompressionConfig::default(),
        });
        // Persistent keys.
        for i in 0..3 {
            s.set(format!("perm{i}"), vec![0u8; 50], None);
        }
        // Volatile keys (with TTL).
        for i in 0..5 {
            s.set(format!("vol{i}"), vec![0u8; 50], Some(Duration::from_secs(3600)));
        }
        // Large write that forces eviction.
        assert!(s.set("big".into(), vec![0u8; 2000], None));
        // All persistent keys should survive.
        for i in 0..3 {
            assert!(s.exists(&format!("perm{i}")), "perm{i} should survive");
        }
    }

    #[test]
    fn volatile_lru_fails_when_only_persistent_keys() {
        let s = Store::new(StoreConfig {
            memory_limit: 500,
            eviction_policy: EvictionPolicy::VolatileLru,
            compression: CompressionConfig::default(),
        });
        s.set("perm".into(), vec![0u8; 50], None);
        // No volatile keys to evict — should fail.
        assert!(!s.set("toobig".into(), vec![0u8; 500], None));
    }

    // ── AllKeysRandom eviction ──────────────────────────────────────

    #[test]
    fn allkeys_random_evicts_to_make_room() {
        let s = Store::new(StoreConfig {
            memory_limit: 2048,
            eviction_policy: EvictionPolicy::AllKeysRandom,
            compression: CompressionConfig::default(),
        });
        for i in 0..10 {
            assert!(s.set(format!("k{i}"), vec![0u8; 50], None));
        }
        assert!(s.set("big".into(), vec![0u8; 500], None));
        assert!(s.get("big").is_some());
    }

    // ── NoEviction edge cases ───────────────────────────────────────

    #[test]
    fn noeviction_rejects_when_at_limit() {
        let s = Store::new(StoreConfig {
            memory_limit: 500,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        s.set("a".into(), vec![0u8; 50], None);
        assert!(!s.set("b".into(), vec![0u8; 500], None));
        assert!(s.get("a").is_some(), "original key should be intact");
    }

    // ── Eviction edge cases ─────────────────────────────────────────

    #[test]
    fn eviction_on_empty_store_fails() {
        let s = Store::new(StoreConfig {
            memory_limit: 100,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        // Value exceeds limit with nothing to evict.
        assert!(!s.set("huge".into(), vec![0u8; 1024], None));
    }

    #[test]
    fn unlimited_memory_accepts_any_size() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        assert!(s.set("big".into(), vec![0u8; 1_000_000], None));
        assert!(s.get("big").is_some());
    }

    #[test]
    fn eviction_frees_multiple_keys_for_large_write() {
        let s = Store::new(StoreConfig {
            memory_limit: 2048,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        for i in 0..12 {
            assert!(s.set(format!("s{i}"), vec![0u8; 30], None));
        }
        assert!(s.set("large".into(), vec![0u8; 1000], None));
        assert!(s.get("large").is_some());
        let remaining: usize = (0..12).filter(|i| s.exists(&format!("s{i}"))).count();
        assert!(remaining < 12, "multiple keys should have been evicted");
    }

    // ── Memory tracking ─────────────────────────────────────────────

    #[test]
    fn mem_used_tracks_insertions_and_removals() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        assert_eq!(s.mem_used(), 0);
        s.set("key1".into(), vec![0u8; 100], None);
        assert!(s.mem_used() > 100, "should account for overhead");
        s.remove("key1");
        assert_eq!(s.mem_used(), 0);
    }

    #[test]
    fn mem_used_adjusts_on_overwrite() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        s.set("key".into(), vec![0u8; 50], None);
        let mem1 = s.mem_used();
        s.set("key".into(), vec![0u8; 200], None);
        let mem2 = s.mem_used();
        assert!(mem2 > mem1, "memory should increase with larger value");
        s.set("key".into(), vec![0u8; 10], None);
        let mem3 = s.mem_used();
        assert!(mem3 < mem2, "memory should decrease with smaller value");
    }

    #[test]
    fn flush_resets_mem_used() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        for i in 0..10 {
            s.set(format!("k{i}"), vec![0u8; 100], None);
        }
        assert!(s.mem_used() > 0);
        s.flush();
        assert_eq!(s.mem_used(), 0);
    }

    // ── Glob matching ───────────────────────────────────────────────

    #[test]
    fn glob_match_question_mark() {
        assert!(glob_match("h?llo", "hello"));
        assert!(glob_match("h?llo", "hallo"));
        assert!(!glob_match("h?llo", "hllo"));
        assert!(!glob_match("h?llo", "heello"));
    }

    #[test]
    fn glob_match_combined_wildcards() {
        assert!(glob_match("h*o", "hello"));
        assert!(glob_match("h*o", "ho"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "abc"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn glob_match_empty_pattern() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "nonempty"));
    }

    // ── Hash operations ─────────────────────────────────────────

    #[test]
    fn hset_and_hget() {
        let s = test_store();
        assert!(s.hset("myhash", "field1", b"value1".to_vec()));
        assert_eq!(s.hget("myhash", "field1").as_deref(), Some(&b"value1"[..]));
    }

    #[test]
    fn hset_overwrite_returns_false() {
        let s = test_store();
        assert!(s.hset("myhash", "f", b"v1".to_vec()));
        assert!(!s.hset("myhash", "f", b"v2".to_vec()));
        assert_eq!(s.hget("myhash", "f").as_deref(), Some(&b"v2"[..]));
    }

    #[test]
    fn hget_missing_key() {
        let s = test_store();
        assert_eq!(s.hget("nope", "field"), None);
    }

    #[test]
    fn hget_missing_field() {
        let s = test_store();
        s.hset("myhash", "f1", b"v".to_vec());
        assert_eq!(s.hget("myhash", "f2"), None);
    }

    #[test]
    fn hdel_existing() {
        let s = test_store();
        s.hset("h", "f", b"v".to_vec());
        assert!(s.hdel("h", "f"));
        assert_eq!(s.hget("h", "f"), None);
    }

    #[test]
    fn hdel_missing() {
        let s = test_store();
        assert!(!s.hdel("h", "f"));
    }

    #[test]
    fn hgetall() {
        let s = test_store();
        s.hset("h", "a", b"1".to_vec());
        s.hset("h", "b", b"2".to_vec());
        let mut pairs = s.hgetall("h");
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            pairs,
            vec![
                ("a".to_string(), Bytes::from_static(b"1")),
                ("b".to_string(), Bytes::from_static(b"2")),
            ]
        );
    }

    #[test]
    fn hkeys_and_hvals() {
        let s = test_store();
        s.hset("h", "x", b"10".to_vec());
        s.hset("h", "y", b"20".to_vec());
        let mut keys = s.hkeys("h");
        keys.sort();
        assert_eq!(keys, vec!["x", "y"]);
        assert_eq!(s.hvals("h").len(), 2);
    }

    #[test]
    fn hlen() {
        let s = test_store();
        assert_eq!(s.hlen("h"), 0);
        s.hset("h", "a", b"1".to_vec());
        s.hset("h", "b", b"2".to_vec());
        assert_eq!(s.hlen("h"), 2);
    }

    #[test]
    fn hexists() {
        let s = test_store();
        s.hset("h", "a", b"1".to_vec());
        assert!(s.hexists("h", "a"));
        assert!(!s.hexists("h", "b"));
        assert!(!s.hexists("nope", "a"));
    }

    #[test]
    fn hash_type_detection() {
        let s = test_store();
        s.hset("h", "f", b"v".to_vec());
        assert!(s.is_hash("h"));
        s.set("str".into(), b"v".to_vec(), None);
        assert!(!s.is_hash("str"));
    }

    #[test]
    fn hash_exists_in_global_exists() {
        let s = test_store();
        s.hset("h", "f", b"v".to_vec());
        assert!(s.exists("h"));
    }

    #[test]
    fn hash_remove_via_global_remove() {
        let s = test_store();
        s.hset("h", "f", b"v".to_vec());
        assert!(s.remove("h"));
        assert!(!s.exists("h"));
    }

    #[test]
    fn hash_in_global_keys() {
        let s = test_store();
        s.set("str_key".into(), b"v".to_vec(), None);
        s.hset("hash_key", "f", b"v".to_vec());
        let mut keys = s.keys("*");
        keys.sort();
        assert_eq!(keys, vec!["hash_key", "str_key"]);
    }

    #[test]
    fn hash_in_global_len() {
        let s = test_store();
        s.set("a".into(), b"1".to_vec(), None);
        s.hset("b", "f", b"2".to_vec());
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn hdel_removes_empty_hash() {
        let s = test_store();
        s.hset("h", "only", b"v".to_vec());
        s.hdel("h", "only");
        assert!(!s.exists("h"));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn flush_clears_hashes() {
        let s = test_store();
        s.hset("h", "f", b"v".to_vec());
        s.flush();
        assert_eq!(s.hlen("h"), 0);
        assert!(!s.exists("h"));
    }

    #[test]
    fn flush_clears_mixed_ttl_keys_and_resets_memory() {
        // FLUSHDB/FLUSHALL backing: confirm a single flush nukes both
        // persistent and TTL'd string keys plus hash keys, and zeros the
        // memory accountant so subsequent writes start from a clean slate.
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        s.set("persistent".into(), b"v1".to_vec(), None);
        s.set("ttl".into(), b"v2".to_vec(), Some(Duration::from_secs(60)));
        s.set("ttl_short".into(), b"v3".to_vec(), Some(Duration::from_millis(1)));
        s.hset("h", "f1", b"hv1".to_vec());
        s.hset("h", "f2", b"hv2".to_vec());
        assert!(s.mem_used() > 0);
        assert_eq!(s.len(), 4); // 3 strings + 1 hash

        s.flush();

        assert_eq!(s.len(), 0);
        assert_eq!(s.mem_used(), 0);
        assert!(!s.exists("persistent"));
        assert!(!s.exists("ttl"));
        assert!(!s.exists("ttl_short"));
        assert!(!s.exists("h"));
        assert_eq!(s.pttl("ttl"), None);

        // Confirm the store is still usable after flush.
        assert!(s.set("after_flush".into(), b"ok".to_vec(), None));
        assert_eq!(s.get("after_flush").as_deref(), Some(&b"ok"[..]));
    }

    #[test]
    fn mem_sub_saturates_and_does_not_underflow_after_flush() {
        // Regression for the flush/remove race: `flush()` resets `mem_used`
        // to 0 while another thread may still owe a `mem_sub` for an entry it
        // pulled from the map just before the flush. With a plain `fetch_sub`
        // that stale subtraction wraps the unsigned counter to ~usize::MAX,
        // which permanently wedges `ensure_memory`. Here we reproduce the
        // outcome deterministically: zero the counter, then apply a stale sub.
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        s.set("k".into(), vec![0u8; 1024], None);
        s.flush(); // mem_used == 0, but the entry's mem_size is still "owed"
        assert_eq!(s.mem_used(), 0);

        // Simulate the late `mem_sub` from a remove that captured the entry
        // before flush ran. Must floor at 0, never wrap.
        s.mem_sub(4096);
        assert_eq!(s.mem_used(), 0, "mem_sub underflowed instead of saturating");

        // And the store must still accept writes (a wrapped counter would make
        // ensure_memory reject everything under a non-zero limit).
        let s2 = Store::new(StoreConfig {
            memory_limit: 1024 * 1024,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        s2.set("a".into(), vec![1u8; 512], None);
        s2.flush();
        s2.mem_sub(99_999); // stale over-subtraction
        assert_eq!(s2.mem_used(), 0);
        assert!(
            s2.set("b".into(), vec![2u8; 512], None),
            "write rejected — counter likely underflowed and wedged ensure_memory"
        );
    }

    // ── Replicator hook ─────────────────────────────────────────────

    /// Recorded arguments of a single [`Replicator::replicate_set`] call.
    type RecordedSet = (String, Vec<u8>, Option<Duration>);

    /// Recorded arguments of a single [`Replicator::replicate_expire`] call.
    type RecordedExpire = (String, Duration);

    /// Test replicator that records every hook invocation and always
    /// bypasses the local write (proving `set`/`remove`/`expire` actually
    /// delegate rather than double-writing).
    #[derive(Debug, Default)]
    struct RecordingReplicator {
        sets: std::sync::Mutex<Vec<RecordedSet>>,
        removes: std::sync::Mutex<Vec<String>>,
        expires: std::sync::Mutex<Vec<RecordedExpire>>,
    }

    impl Replicator for RecordingReplicator {
        fn replicate_set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
            self.sets.lock().unwrap().push((key, value, ttl));
            true
        }
        fn replicate_remove(&self, key: &str) -> bool {
            self.removes.lock().unwrap().push(key.to_string());
            true
        }
        fn replicate_expire(&self, key: &str, ttl: Duration) -> bool {
            self.expires.lock().unwrap().push((key.to_string(), ttl));
            true
        }
    }

    #[test]
    fn replicator_hook_intercepts_public_set() {
        // The whole point of the seam: an installed replicator sees every
        // public `set`, and the local map does NOT hold the value (proving
        // routing decisions are the replicator's to make).
        let s = test_store();
        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        assert!(s.set("k".into(), b"v".to_vec(), Some(Duration::from_secs(30))));

        // Hook fired with the expected args.
        let sets = rep.sets.lock().unwrap();
        assert_eq!(sets.len(), 1, "replicator must observe exactly one set");
        assert_eq!(sets[0].0, "k");
        assert_eq!(sets[0].1, b"v".to_vec());
        assert_eq!(sets[0].2, Some(Duration::from_secs(30)));
        drop(sets);

        // Local map is untouched — the replicator did not forward the
        // write, and Store::set MUST have delegated to it.
        assert_eq!(s.get("k"), None, "hooked set must not write locally");
    }

    #[test]
    fn replicator_hook_intercepts_remove_and_expire() {
        let s = test_store();
        // Seed a value via set_local so a subsequent public remove has
        // something to work against for the non-hook case; the hook itself
        // is what we're validating here (`remove` delegates).
        s.set_local("k".into(), b"v".to_vec(), None);

        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        assert!(s.remove("k"));
        assert!(s.expire("k", Duration::from_secs(60)));

        assert_eq!(rep.removes.lock().unwrap().as_slice(), &["k".to_string()]);
        let expires = rep.expires.lock().unwrap();
        assert_eq!(expires.len(), 1);
        assert_eq!(expires[0].0, "k");
        assert_eq!(expires[0].1, Duration::from_secs(60));
    }

    #[test]
    fn set_local_bypasses_replicator() {
        // A replicator implementation writes its own local copy through
        // `set_local` to avoid re-entering the hook. Prove that path
        // actually skips the hook (no infinite recursion possible).
        let s = test_store();
        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        assert!(s.set_local("k".into(), b"v".to_vec(), None));
        assert!(s.remove_local("k"));
        assert!(!s.expire_local("missing", Duration::from_secs(60)));

        assert!(rep.sets.lock().unwrap().is_empty(), "set_local must not fire the hook");
        assert!(rep.removes.lock().unwrap().is_empty(), "remove_local must not fire the hook");
        assert!(rep.expires.lock().unwrap().is_empty(), "expire_local must not fire the hook");
    }

    #[test]
    fn no_replicator_is_transparent() {
        // Baseline: on single-node (no hook installed) the behaviour is
        // identical to the pre-seam direct-write path. Guards the perf
        // contract in the docs — a single Option check, no atomics.
        let s = test_store();
        assert!(s.set("k".into(), b"v".to_vec(), None));
        assert_eq!(s.get("k").as_deref(), Some(b"v".as_slice()));
        assert!(s.expire("k", Duration::from_secs(60)));
        assert!(s.remove("k"));
        assert_eq!(s.get("k"), None);
    }

    #[test]
    fn replicator_can_be_unhooked() {
        // Passing None restores the direct-write path.
        let s = test_store();
        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));
        s.set("hooked".into(), b"v".to_vec(), None);
        assert_eq!(rep.sets.lock().unwrap().len(), 1);

        s.set_replicator(None);
        s.set("direct".into(), b"v".to_vec(), None);
        assert_eq!(
            s.get("direct").as_deref(),
            Some(b"v".as_slice()),
            "unhooked set must write locally"
        );
        assert_eq!(rep.sets.lock().unwrap().len(), 1, "replicator must not see the direct write");
    }
}

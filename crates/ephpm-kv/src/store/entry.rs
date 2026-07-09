//! KV store entry with TTL and LRU metadata.
//!
//! # Design notes
//!
//! `data` is a [`bytes::Bytes`] rather than a `Vec<u8>` so that
//! `Store::get` can hand a caller a cheap `Arc`-clone of the value
//! instead of a `memcpy` of every byte on every read. This matters for
//! hot-key reads: WordPress session/user-meta gets are ~4-16 KiB and
//! ran at up to 2x observed cost on the old `.clone()` path.
//!
//! `last_accessed` is an [`AtomicU64`] holding nanoseconds since a
//! store-level anchor `Instant` so that GET only needs a shard **read**
//! lock (`DashMap::get`) to touch the LRU timestamp; the old
//! `Instant`-typed field forced GET to take a shard **write** lock
//! (`get_mut`), serialising concurrent reads of the same hot key.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::Bytes;

/// A single value stored in the KV store.
#[derive(Debug)]
pub struct Entry {
    /// The raw value bytes (may be compressed). `Bytes` is an
    /// `Arc`-shared slice, so cloning it is one atomic increment — no
    /// data copy.
    pub data: Bytes,
    /// `true` if `data` is compressed; `false` if raw.
    pub compressed: bool,
    /// Absolute expiry time, or `None` for persistent keys.
    pub expires_at: Option<Instant>,
    /// Last access time for LRU eviction, as nanoseconds since the
    /// store-level anchor. Stored atomically so `get()` can touch it
    /// under the shard **read** lock. `Ordering::Relaxed` is sufficient:
    /// eviction only reads this to pick an approximate "oldest" key and
    /// tolerates minor reordering — it's a heuristic, not a
    /// synchronisation point.
    pub last_accessed: AtomicU64,
    /// Approximate heap size of this entry (key + value + overhead).
    pub mem_size: usize,
}

impl Entry {
    /// Create a new entry with no expiry.
    #[must_use]
    pub fn new(data: impl Into<Bytes>, key_len: usize, compressed: bool, now_nanos: u64) -> Self {
        let data = data.into();
        let mem_size = Self::estimate_size(key_len, data.len());
        Self {
            data,
            compressed,
            expires_at: None,
            last_accessed: AtomicU64::new(now_nanos),
            mem_size,
        }
    }

    /// Create a new entry with an absolute expiry.
    #[must_use]
    pub fn with_expiry(
        data: impl Into<Bytes>,
        key_len: usize,
        compressed: bool,
        expires_at: Instant,
        now_nanos: u64,
    ) -> Self {
        let data = data.into();
        let mem_size = Self::estimate_size(key_len, data.len());
        Self {
            data,
            compressed,
            expires_at: Some(expires_at),
            last_accessed: AtomicU64::new(now_nanos),
            mem_size,
        }
    }

    /// Returns `true` if this entry has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Instant::now() >= exp)
    }

    /// Touch the entry, updating its last-access nanos for LRU.
    ///
    /// Uses `Ordering::Relaxed` because the LRU eviction sampler
    /// tolerates approximate ordering; a lost write here at worst
    /// picks a slightly wrong "oldest" victim, never a correctness bug.
    pub fn touch(&self, now_nanos: u64) {
        self.last_accessed.store(now_nanos, Ordering::Relaxed);
    }

    /// Read the last-accessed nanos snapshot for LRU comparison.
    #[must_use]
    pub fn last_accessed_nanos(&self) -> u64 {
        self.last_accessed.load(Ordering::Relaxed)
    }

    /// Rough memory estimate: key string + value vec + struct overhead.
    /// Used for memory-limit enforcement, not exact accounting.
    fn estimate_size(key_len: usize, value_len: usize) -> usize {
        // key (String on heap) + value (Bytes buffer on heap) + Entry struct + DashMap overhead
        key_len + value_len + std::mem::size_of::<Self>() + 64
    }
}

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

/// Coarse-clock granularity for `Entry::touch` in nanoseconds.
///
/// LRU touches only pay the atomic store when the entry's last-access
/// timestamp is at least this stale. 100ms is comfortably below any real
/// LRU eviction horizon (seconds to minutes on the eviction sampler),
/// and small enough that "recently touched" behaviour is indistinguishable
/// from precise-clock touch under human-perceptible workloads. See
/// [`Entry::touch`] for the full rationale.
pub const LRU_TOUCH_GRANULARITY_NANOS: u64 = 100_000_000;

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
    /// # Coarse-clock optimisation
    ///
    /// The previous unconditional `store` was ~15% of the native-GET cost
    /// (~40ns of 271ns), driven by cache-line ping-pong on hot keys — every
    /// concurrent GET on the same key contended on the same atomic
    /// cache-line. The overwhelmingly common case is that the timestamp
    /// is already recent enough for LRU purposes; a fresh store adds no
    /// eviction-decision information.
    ///
    /// We now do a **relaxed load** first and only pay the store when the
    /// clock has advanced by more than [`LRU_TOUCH_GRANULARITY_NANOS`]
    /// (100ms). Loads on a shared cache line are cheap (MESI Shared state);
    /// only the rare RMW pays the cache-coherence cost. LRU semantics are
    /// preserved at 100ms granularity — coarse compared to the seconds/
    /// minutes timescales the eviction sampler operates on.
    ///
    /// Uses `Ordering::Relaxed` throughout because the LRU eviction sampler
    /// tolerates approximate ordering; a lost write here at worst picks a
    /// slightly wrong "oldest" victim, never a correctness bug.
    pub fn touch(&self, now_nanos: u64) {
        let prev = self.last_accessed.load(Ordering::Relaxed);
        // saturating_sub covers the (impossible under a monotonic anchor)
        // case where the caller passes a nanos snapshot older than what's
        // already stored.
        if now_nanos.saturating_sub(prev) >= LRU_TOUCH_GRANULARITY_NANOS {
            self.last_accessed.store(now_nanos, Ordering::Relaxed);
        }
    }

    /// Force an unconditional last-access update, bypassing the coarse-clock
    /// filter. Only used by tests that need to plant an exact timestamp;
    /// production code should always call [`touch`](Self::touch) so the hot
    /// GET path stays cheap.
    #[cfg(test)]
    pub fn touch_precise(&self, now_nanos: u64) {
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

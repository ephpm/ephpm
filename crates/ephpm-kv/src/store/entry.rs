//! KV store entry with TTL and LRU metadata.

use std::time::Instant;

/// A single value stored in the KV store.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The raw value bytes (may be compressed).
    pub data: Vec<u8>,
    /// `true` if `data` is compressed; `false` if raw.
    pub compressed: bool,
    /// Absolute expiry time, or `None` for persistent keys.
    pub expires_at: Option<Instant>,
    /// Last access time for LRU eviction.
    pub last_accessed: Instant,
    /// Approximate heap size of this entry (key + value + overhead).
    pub mem_size: usize,
}

impl Entry {
    /// Create a new entry with no expiry.
    #[must_use]
    pub fn new(data: Vec<u8>, key_len: usize, compressed: bool) -> Self {
        let mem_size = Self::estimate_size(key_len, data.len());
        Self {
            data,
            compressed,
            expires_at: None,
            last_accessed: Instant::now(),
            mem_size,
        }
    }

    /// Create a new entry with an absolute expiry.
    #[must_use]
    pub fn with_expiry(data: Vec<u8>, key_len: usize, compressed: bool, expires_at: Instant) -> Self {
        let mem_size = Self::estimate_size(key_len, data.len());
        Self {
            data,
            compressed,
            expires_at: Some(expires_at),
            last_accessed: Instant::now(),
            mem_size,
        }
    }

    /// Returns `true` if this entry has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|exp| Instant::now() >= exp)
    }

    /// Touch the entry, updating its last-access time for LRU.
    pub fn touch(&mut self) {
        self.last_accessed = Instant::now();
    }

    /// Rough memory estimate: key string + value vec + struct overhead.
    /// Used for memory-limit enforcement, not exact accounting.
    fn estimate_size(key_len: usize, value_len: usize) -> usize {
        // key (String on heap) + value (Vec on heap) + Entry struct + DashMap overhead
        key_len + value_len + std::mem::size_of::<Self>() + 64
    }
}

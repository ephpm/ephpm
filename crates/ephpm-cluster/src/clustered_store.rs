//! Clustered KV store with two-tier routing and hot key promotion.
//!
//! Wraps the local [`ephpm_kv::store::Store`] and a [`ClusterHandle`] to
//! provide transparent routing:
//!
//! - **Small values** (≤ `small_key_threshold`): stored via chitchat
//!   gossip, replicated to all nodes automatically.
//! - **Large values** (> threshold): stored in the local `Store`.
//!   When a `get()` misses locally, the TCP data plane fetches the
//!   value from the owner node (determined by hashing the key against
//!   the list of alive cluster nodes).
//!
//! ## Hot key promotion
//!
//! When enabled, large keys that are frequently fetched from remote
//! nodes are cached locally. The owner publishes a version counter
//! via chitchat; when it bumps, all caching nodes invalidate and
//! refetch on the next read.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use ephpm_config::ClusterKvConfig;
use ephpm_kv::store::Store;

use crate::ClusterHandle;

/// Key prefix for hot-key version metadata in chitchat state.
const HOT_PREFIX: &str = "hot:";

/// A clustered KV store that routes between gossip and local tiers.
pub struct ClusteredStore {
    /// Local in-memory store (DashMap-based).
    store: Arc<Store>,
    /// Cluster gossip handle.
    cluster: Arc<ClusterHandle>,
    /// Configuration for tier routing and hot key behavior.
    config: ClusterKvConfig,
    /// Per-key hit counters for hot key detection.
    hit_counters: DashMap<u64, HitCounter>,
    /// Locally cached hot key values.
    hot_cache: DashMap<u64, CachedValue>,
    /// Whether the hot key subscription is active.
    subscribed: AtomicBool,
    /// Approximate memory used by the hot cache.
    hot_cache_mem: AtomicU64,
}

/// Tracks remote fetch frequency for a single key.
struct HitCounter {
    /// Number of remote fetches in the current window.
    count: u32,
    /// When the current counting window started.
    window_start: Instant,
}

/// A locally cached copy of a hot remote key.
struct CachedValue {
    /// The cached value bytes.
    data: Vec<u8>,
    /// When this entry was cached.
    cached_at: Instant,
    /// The version we cached (matches the gossip-published version).
    version: u64,
    /// The original key string (for logging/debugging).
    key: String,
}

impl ClusteredStore {
    /// Create a new clustered store.
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        cluster: Arc<ClusterHandle>,
        config: ClusterKvConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            cluster,
            config,
            hit_counters: DashMap::new(),
            hot_cache: DashMap::new(),
            subscribed: AtomicBool::new(false),
            hot_cache_mem: AtomicU64::new(0),
        })
    }

    /// Initialize the hot key invalidation subscription.
    ///
    /// Must be called once after construction. Subscribes to chitchat
    /// events for hot-key version changes so local caches are
    /// invalidated promptly.
    pub async fn init_hot_key_watcher(self: &Arc<Self>) {
        if !self.config.hot_key_cache
            || self.subscribed.swap(true, Ordering::SeqCst)
        {
            return;
        }

        let this = Arc::clone(self);
        self.cluster
            .subscribe_hot_key_versions(move |key_hash_str, version_str| {
                let key_hash: u64 = match key_hash_str.parse() {
                    Ok(h) => h,
                    Err(_) => return,
                };
                let new_version: u64 = match version_str.parse() {
                    Ok(v) => v,
                    Err(_) => return,
                };

                // If we have a cached copy at an older version, evict it.
                if let Some(entry) = this.hot_cache.get(&key_hash) {
                    if entry.version < new_version {
                        let mem = entry.data.len() + entry.key.len() + 64;
                        drop(entry);
                        this.hot_cache.remove(&key_hash);
                        this.hot_cache_mem
                            .fetch_sub(mem as u64, Ordering::Relaxed);
                        tracing::debug!(
                            key_hash,
                            old_version = new_version - 1,
                            new_version,
                            "hot key invalidated via gossip",
                        );
                    }
                }
            })
            .await;
    }

    /// Get a value, routing through the appropriate tier.
    pub async fn get(&self, key: &str) -> Option<Vec<u8>> {
        // 1. Check gossip tier (small keys, always local).
        if let Some(value) = self.cluster.gossip_get(key).await {
            return Some(value);
        }

        // 2. Check local store (we own this key, or it's not clustered).
        if let Some(value) = self.store.get(key) {
            return Some(value);
        }

        // 3. Check hot key cache.
        if self.config.hot_key_cache {
            let key_hash = hash_key(key);
            if let Some(cached) = self.hot_cache.get(&key_hash) {
                let ttl = Duration::from_secs(self.config.hot_key_local_ttl_secs);
                if cached.cached_at.elapsed() < ttl && cached.key == key {
                    return Some(cached.data.clone());
                }
                // Expired — remove it.
                let mem = cached.data.len() + cached.key.len() + 64;
                drop(cached);
                self.hot_cache.remove(&key_hash);
                self.hot_cache_mem
                    .fetch_sub(mem as u64, Ordering::Relaxed);
            }
        }

        // 4. TCP fetch from the owner node via the data plane.
        if let Some(owner_addr) = self.resolve_owner_data_addr(key).await {
            match crate::kv_data_plane::fetch_remote(owner_addr, key).await {
                Ok(Some(value)) => {
                    self.track_remote_fetch(key, &value);
                    return Some(value);
                }
                Ok(None) => {
                    tracing::debug!(key, %owner_addr, "key not found on owner node");
                }
                Err(e) => {
                    tracing::debug!(key, %owner_addr, %e, "TCP data plane fetch failed");
                }
            }
        }

        None
    }

    /// Set a value, routing through the appropriate tier.
    ///
    /// Returns `true` on success, `false` if the local store rejected the
    /// write (e.g., memory limit with `NoEviction` policy).
    pub async fn set(
        &self,
        key: String,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> bool {
        if value.len() <= self.config.small_key_threshold {
            // Gossip tier: replicate to all nodes.
            self.cluster.gossip_set(&key, &value, ttl).await;

            // Also bump the hot key version if this key was hot, so
            // remote caches invalidate.
            if self.config.hot_key_cache {
                self.maybe_bump_hot_version(&key).await;
            }

            true
        } else {
            // Local store (TCP tier will route to owner in Phase 7).
            let ok = self.store.set(key.clone(), value, ttl);

            // Bump hot key version for remote invalidation.
            if ok && self.config.hot_key_cache {
                self.maybe_bump_hot_version(&key).await;
            }

            ok
        }
    }

    /// Delete a key from whichever tier owns it.
    pub async fn remove(&self, key: &str) -> bool {
        // Try gossip tier first.
        let gossip_deleted = self.cluster.gossip_del(key).await;

        // Also remove from local store.
        let local_deleted = self.store.remove(key);

        // Evict from hot cache.
        if self.config.hot_key_cache {
            let key_hash = hash_key(key);
            if let Some((_, entry)) = self.hot_cache.remove(&key_hash) {
                let mem = entry.data.len() + entry.key.len() + 64;
                self.hot_cache_mem
                    .fetch_sub(mem as u64, Ordering::Relaxed);
            }
        }

        gossip_deleted || local_deleted
    }

    /// Check if a key exists in any tier.
    pub async fn exists(&self, key: &str) -> bool {
        self.cluster.gossip_exists(key).await || self.store.exists(key)
    }

    /// Get the remaining TTL of a key in milliseconds.
    ///
    /// Returns `Some(-1)` for keys with no TTL, `Some(-2)` for missing
    /// keys, or `Some(ms)` for the remaining time.
    pub async fn pttl(&self, key: &str) -> Option<i64> {
        // Check gossip tier.
        if let Some(ms) = self.cluster.gossip_pttl(key).await {
            return Some(i64::try_from(ms).unwrap_or(i64::MAX));
        }
        // Check local store.
        self.store.pttl(key)
    }

    /// Access the underlying local store directly.
    ///
    /// Useful for operations that don't need cluster routing (e.g.,
    /// `expire_pass`, `flush`, `len`, `mem_used`).
    #[must_use]
    pub fn local_store(&self) -> &Store {
        &self.store
    }

    /// Access the underlying cluster handle.
    #[must_use]
    pub fn cluster(&self) -> &ClusterHandle {
        &self.cluster
    }

    /// Determine the TCP data plane address for the node that owns this key.
    ///
    /// Uses a simple hash-based owner selection: hash the key, pick a
    /// live node by index. Returns `None` if the owner is this node
    /// (already checked local store) or no remote nodes are alive.
    async fn resolve_owner_data_addr(&self, key: &str) -> Option<SocketAddr> {
        let nodes = self.cluster.nodes().await;
        let alive: Vec<_> = nodes
            .iter()
            .filter(|n| n.state == crate::NodeState::Alive)
            .collect();

        if alive.len() <= 1 {
            return None; // Only this node is alive.
        }

        let key_hash = hash_key(key);
        #[allow(clippy::cast_possible_truncation)]
        let idx = (key_hash as usize) % alive.len();
        let owner = &alive[idx];

        // If we own it, no remote fetch needed.
        if owner.id == self.cluster.self_node().id {
            return None;
        }

        // Derive data plane address from the node's gossip address IP
        // and the configured data port.
        owner
            .gossip_addr
            .parse::<SocketAddr>()
            .ok()
            .map(|gossip| SocketAddr::new(gossip.ip(), self.config.data_port))
    }

    /// Record a remote fetch and potentially promote a key to the hot
    /// cache.
    ///
    /// Called when a TCP fetch completes (Phase 7). For now, exposed
    /// for testing.
    pub fn track_remote_fetch(&self, key: &str, value: &[u8]) {
        if !self.config.hot_key_cache {
            return;
        }

        let key_hash = hash_key(key);
        let window = Duration::from_secs(self.config.hot_key_window_secs);
        let now = Instant::now();

        let should_promote = {
            let mut counter = self
                .hit_counters
                .entry(key_hash)
                .or_insert_with(|| HitCounter {
                    count: 0,
                    window_start: now,
                });

            // Reset window if expired.
            if counter.window_start.elapsed() > window {
                counter.count = 0;
                counter.window_start = now;
            }

            counter.count += 1;
            counter.count >= self.config.hot_key_threshold
        };

        if should_promote {
            self.promote_to_hot_cache(key, key_hash, value);
        }
    }

    /// Promote a value into the local hot key cache.
    fn promote_to_hot_cache(&self, key: &str, key_hash: u64, value: &[u8]) {
        let max_mem = match parse_memory_size(&self.config.hot_key_max_memory) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e, "invalid hot_key_max_memory, defaulting to 64MB");
                64 * 1024 * 1024
            }
        };
        let entry_mem = (value.len() + key.len() + 64) as u64;

        // Don't exceed memory budget.
        if self.hot_cache_mem.load(Ordering::Relaxed) + entry_mem > max_mem {
            return;
        }

        let old = self.hot_cache.insert(
            key_hash,
            CachedValue {
                data: value.to_vec(),
                cached_at: Instant::now(),
                version: 0, // Will be set by gossip version tracking.
                key: key.to_string(),
            },
        );

        // Adjust memory accounting.
        if let Some(old_entry) = old {
            let old_mem = (old_entry.data.len() + old_entry.key.len() + 64) as u64;
            self.hot_cache_mem
                .fetch_sub(old_mem, Ordering::Relaxed);
        }
        self.hot_cache_mem
            .fetch_add(entry_mem, Ordering::Relaxed);

        tracing::debug!(
            key,
            key_hash,
            value_len = value.len(),
            "promoted to hot key cache",
        );
    }

    /// If a key is tracked as hot, bump its version in chitchat so
    /// remote caches invalidate.
    async fn maybe_bump_hot_version(&self, key: &str) {
        let key_hash = hash_key(key);
        // Only bump if we've seen this key in the hit counters (it's hot
        // somewhere) or it's in our own hot cache.
        let is_tracked = self.hit_counters.contains_key(&key_hash)
            || self.hot_cache.contains_key(&key_hash);
        if !is_tracked {
            return;
        }

        // Read current version, increment, publish.
        let version_key = format!("{key_hash}");
        let current = self
            .cluster
            .gossip_get(&format!("{HOT_PREFIX}{version_key}"))
            .await
            .and_then(|v| String::from_utf8(v).ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let new_version = current + 1;
        self.cluster
            .gossip_set(
                &format!("{HOT_PREFIX}{version_key}"),
                new_version.to_string().as_bytes(),
                None,
            )
            .await;

        tracing::debug!(
            key,
            key_hash,
            new_version,
            "bumped hot key version via gossip",
        );
    }

    /// Periodic maintenance: evict expired hot cache entries and stale
    /// hit counters.
    pub fn hot_cache_cleanup(&self) {
        let ttl = Duration::from_secs(self.config.hot_key_local_ttl_secs);
        let window = Duration::from_secs(self.config.hot_key_window_secs);

        // Evict expired hot cache entries.
        self.hot_cache.retain(|_, entry| {
            if entry.cached_at.elapsed() >= ttl {
                let mem = (entry.data.len() + entry.key.len() + 64) as u64;
                self.hot_cache_mem
                    .fetch_sub(mem, Ordering::Relaxed);
                false
            } else {
                true
            }
        });

        // Evict stale hit counters (windows that have expired).
        self.hit_counters
            .retain(|_, counter| counter.window_start.elapsed() < window * 2);
    }

    /// Current hot cache memory usage in bytes.
    #[must_use]
    pub fn hot_cache_mem_used(&self) -> u64 {
        self.hot_cache_mem.load(Ordering::Relaxed)
    }

    /// Number of entries in the hot key cache.
    #[must_use]
    pub fn hot_cache_len(&self) -> usize {
        self.hot_cache.len()
    }
}

/// Subscribe to hot-key version changes on the `ClusterHandle`.
impl ClusterHandle {
    /// Subscribe to hot-key version bumps published via gossip.
    ///
    /// The callback receives `(key_hash_str, version_str)` for each
    /// change to a `hot:*` key in any node's chitchat state.
    pub async fn subscribe_hot_key_versions<F>(&self, callback: F)
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        let chitchat = self.handle.chitchat();
        let guard = chitchat.lock().await;
        guard
            .subscribe_event(HOT_PREFIX, move |event| {
                callback(event.key, event.value);
            })
            .forever();
    }
}

/// FNV-1a-style hash for key → u64 mapping.
fn hash_key(key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// Parse a memory size string like `"64MB"` to bytes.
///
/// Supported units (case-insensitive): `B`, `KB`/`K`, `MB`/`M`, `GB`/`G`.
/// Bare numbers without a unit suffix default to bytes.
///
/// # Errors
///
/// Returns an error for unrecognized unit suffixes.
fn parse_memory_size(s: &str) -> Result<u64, ParseMemoryError> {
    let s = s.trim();
    let (num, unit) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((s, ""), |i| s.split_at(i));

    let base: f64 = num
        .parse()
        .map_err(|_| ParseMemoryError::InvalidNumber(s.to_string()))?;

    let multiplier = match unit.trim().to_uppercase().as_str() {
        "B" | "" => 1.0,
        "KB" | "K" => 1024.0,
        "MB" | "M" => 1024.0 * 1024.0,
        "GB" | "G" => 1024.0 * 1024.0 * 1024.0,
        other => return Err(ParseMemoryError::UnknownUnit(other.to_string())),
    };

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok((base * multiplier) as u64)
}

/// Errors from [`parse_memory_size`].
#[derive(Debug, thiserror::Error)]
enum ParseMemoryError {
    /// The numeric portion could not be parsed.
    #[error("invalid memory size number: {0}")]
    InvalidNumber(String),
    /// An unrecognized unit suffix was provided.
    #[error("unknown memory size unit \"{0}\" (expected B, KB, MB, or GB)")]
    UnknownUnit(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_memory_sizes() {
        assert_eq!(parse_memory_size("64MB").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_memory_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("512KB").unwrap(), 512 * 1024);
        assert_eq!(parse_memory_size("1024B").unwrap(), 1024);
        assert_eq!(parse_memory_size("256").unwrap(), 256);
    }

    #[test]
    fn parse_memory_size_unknown_unit_errors() {
        assert!(parse_memory_size("64TB").is_err());
        assert!(parse_memory_size("10PB").is_err());
    }

    #[test]
    fn parse_memory_size_case_insensitive() {
        assert_eq!(parse_memory_size("64mb").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_memory_size("1gb").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_size("512kb").unwrap(), 512 * 1024);
        assert_eq!(parse_memory_size("100M").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_memory_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn hash_key_deterministic() {
        let h1 = hash_key("session:abc123");
        let h2 = hash_key("session:abc123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_key_different_keys() {
        let h1 = hash_key("session:abc");
        let h2 = hash_key("session:xyz");
        assert_ne!(h1, h2);
    }
}

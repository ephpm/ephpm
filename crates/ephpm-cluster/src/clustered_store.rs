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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use ephpm_config::ClusterKvConfig;
use ephpm_kv::store::Store;

use crate::ClusterHandle;
use crate::node::NodeInfo;
use crate::secure_transport::ClusterCipher;

/// Key prefix for hot-key version metadata in chitchat state.
const HOT_PREFIX: &str = "hot:";

/// How large-value writes propagate to replica nodes.
///
/// # Consistency guarantee
///
/// Replication is **write-time only** — there is no active anti-entropy
/// or rebalancing. See [`ClusteredStore::set`] for the exact per-mode
/// guarantee and the membership-change caveat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplicationMode {
    /// Return as soon as the primary/local copy is written; other
    /// replicas are updated in the background (fire-and-forget).
    Async,
    /// Wait for every reachable replica to acknowledge before
    /// returning; an unreachable replica is logged, not fatal.
    Sync,
}

impl ReplicationMode {
    /// Parse the `[cluster.kv] replication_mode` string.
    ///
    /// Anything other than `"sync"` (case-insensitive) is treated as
    /// [`ReplicationMode::Async`], matching the config default.
    fn from_config(mode: &str) -> Self {
        if mode.eq_ignore_ascii_case("sync") { Self::Sync } else { Self::Async }
    }
}

/// A clustered KV store that routes between gossip and local tiers.
pub struct ClusteredStore {
    /// Local in-memory store (DashMap-based).
    store: Arc<Store>,
    /// Cluster gossip handle.
    cluster: Arc<ClusterHandle>,
    /// Configuration for tier routing and hot key behavior.
    config: ClusterKvConfig,
    /// Cipher for TCP data plane traffic (`None` = plaintext).
    cipher: Option<Arc<ClusterCipher>>,
    /// Per-key hit counters for hot key detection.
    hit_counters: DashMap<u64, HitCounter>,
    /// Locally cached hot key values.
    hot_cache: DashMap<u64, CachedValue>,
    /// Whether the hot key subscription is active.
    subscribed: AtomicBool,
    /// Approximate memory used by the hot cache.
    hot_cache_mem: AtomicU64,
    /// Count of replica writes that failed to reach a peer (data plane
    /// error or rejection). Observability only — a failed replica write
    /// never fails the client set (see [`ClusteredStore::set`]). Wrapped
    /// in an `Arc` so fire-and-forget async replica tasks can record
    /// their failures too.
    replica_write_failures: Arc<AtomicU64>,
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
    ///
    /// When `cipher` is `Some` (derive it with
    /// [`ClusterCipher::for_kv_data_plane`] from `[cluster] secret`),
    /// all TCP data plane traffic to remote nodes is sealed; the remote
    /// listeners must be started with the same cipher.
    #[must_use]
    pub fn new(
        store: Arc<Store>,
        cluster: Arc<ClusterHandle>,
        config: ClusterKvConfig,
        cipher: Option<Arc<ClusterCipher>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            cluster,
            config,
            cipher,
            hit_counters: DashMap::new(),
            hot_cache: DashMap::new(),
            subscribed: AtomicBool::new(false),
            hot_cache_mem: AtomicU64::new(0),
            replica_write_failures: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Initialize the hot key invalidation subscription.
    ///
    /// Must be called once after construction. Subscribes to chitchat
    /// events for hot-key version changes so local caches are
    /// invalidated promptly.
    pub async fn init_hot_key_watcher(self: &Arc<Self>) {
        if !self.config.hot_key_cache || self.subscribed.swap(true, Ordering::SeqCst) {
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
                        this.hot_cache_mem.fetch_sub(mem as u64, Ordering::Relaxed);
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
                self.hot_cache_mem.fetch_sub(mem as u64, Ordering::Relaxed);
            }
        }

        // 4. TCP fetch from the replica set via the data plane. Try the
        //    primary owner first, then fall back to the other replicas in
        //    ring order — this is what lets a large value survive the
        //    loss of its owner (up to `replication_factor - 1` failures).
        let self_id = self.cluster.self_node().id;
        for node in self.replica_nodes(key).await {
            // Skip ourselves: if we had the value it would have been
            // served by the local-store check above.
            if node.id == self_id {
                continue;
            }
            let Some(addr) = node_data_addr(&node, self.config.data_port) else {
                continue;
            };
            match crate::kv_data_plane::fetch_remote(addr, key, self.cipher.as_deref()).await {
                Ok(Some(value)) => {
                    self.track_remote_fetch(key, &value);
                    return Some(value);
                }
                Ok(None) => {
                    tracing::debug!(key, replica = %node.id, %addr, "key not found on replica");
                }
                Err(e) => {
                    tracing::debug!(key, replica = %node.id, %addr, %e, "replica fetch failed");
                }
            }
        }

        None
    }

    /// Set a value, routing through the appropriate tier.
    ///
    /// Returns `true` on success, `false` if the primary write failed
    /// (the local store rejected it, or the primary owner rejected /
    /// was unreachable and no local fallback succeeded).
    ///
    /// # Large-value replication
    ///
    /// Large values are written to the **replica set**: the primary
    /// owner (`hash(key) % alive_nodes`) plus the next
    /// `replication_factor - 1` distinct nodes on the sorted alive-node
    /// ring (wrapping around; clamped to the number of alive nodes). The
    /// client-visible result reflects only the **primary** write; replica
    /// propagation follows the configured [`ReplicationMode`]:
    ///
    /// - **async** (default): the primary write is awaited, then replica
    ///   writes are spawned fire-and-forget. Failures are counted
    ///   ([`ClusteredStore::replica_write_failures`]) and logged at
    ///   `warn`, but never fail or delay the client set.
    /// - **sync**: replica writes are awaited best-effort. The guarantee
    ///   is *"every reachable replica has acknowledged when `set`
    ///   returns"*. A replica that is down or rejects the write is
    ///   logged and counted but does **not** fail the set — this is not
    ///   a quorum/consensus protocol, and no rollback is performed. It
    ///   trades the async latency win for read-your-writes durability
    ///   against reachable peers.
    ///
    /// ## Membership-change caveat (v1)
    ///
    /// Replication happens only at write time. There is no active
    /// anti-entropy or rebalancing: a node that was down (or not yet in
    /// the replica set) when a key was written will **not** hold that
    /// key until the key is rewritten or fetched-through from a node
    /// that has it. Likewise, when membership changes the replica set
    /// for existing keys is not recomputed retroactively. This is an
    /// honest v1 — durable-through-rebalance replication is future work.
    pub async fn set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        if value.len() <= self.config.small_key_threshold {
            // Gossip tier: replicate to all nodes.
            self.cluster.gossip_set(&key, &value, ttl).await;

            // Also bump the hot key version if this key was hot, so
            // remote caches invalidate.
            if self.config.hot_key_cache {
                self.maybe_bump_hot_version(&key).await;
            }

            return true;
        }

        // Large value — write to the replica set (primary + N-1 peers).
        let ok = self.set_large(&key, &value, ttl).await;

        // Bump hot key version for remote invalidation.
        if ok && self.config.hot_key_cache {
            self.maybe_bump_hot_version(&key).await;
        }

        ok
    }

    /// Write a large value to its replica set. Returns whether the
    /// primary write succeeded (the client-visible result). See
    /// [`ClusteredStore::set`] for the replication semantics.
    async fn set_large(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> bool {
        let self_id = self.cluster.self_node().id;
        let replicas = self.replica_nodes(key).await;

        // No cluster peers (single node, or only self alive): store
        // locally and we're done.
        if replicas.is_empty() {
            return self.store.set(key.to_string(), value.to_vec(), ttl);
        }

        // The primary is the first replica; the rest are secondaries.
        // Write the primary copy first — its result is what the client
        // sees. Secondaries follow per the replication mode.
        let (primary, secondaries) = replicas.split_first().expect("replicas is non-empty");
        let primary_ok = self.write_one_replica(primary, &self_id, key, value, ttl).await;
        if !primary_ok {
            // The primary write failed. Fall back to a local copy so the
            // value is not lost outright, mirroring the pre-replication
            // behaviour. Do not attempt secondaries in this case.
            if primary.id != self_id {
                return self.store.set(key.to_string(), value.to_vec(), ttl);
            }
            return false;
        }

        let mode = ReplicationMode::from_config(&self.config.replication_mode);
        match mode {
            ReplicationMode::Async => {
                // Fire-and-forget: spawn each secondary write and return.
                for node in secondaries {
                    if node.id == self_id {
                        // Local replica — write inline (cheap, no task).
                        if !self.store.set(key.to_string(), value.to_vec(), ttl) {
                            self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(key, "local replica write rejected (memory limit?)");
                        }
                        continue;
                    }
                    let Some(addr) = node_data_addr(node, self.config.data_port) else {
                        continue;
                    };
                    let cipher = self.cipher.clone();
                    let key = key.to_string();
                    let value = value.to_vec();
                    let node_id = node.id.clone();
                    let failures = Arc::clone(&self.replica_write_failures);
                    tokio::spawn(async move {
                        match crate::kv_data_plane::store_remote(
                            addr,
                            &key,
                            &value,
                            cipher.as_deref(),
                        )
                        .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                failures.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    key,
                                    replica = %node_id,
                                    %addr,
                                    "async replica rejected large-value write"
                                );
                            }
                            Err(e) => {
                                failures.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    key,
                                    replica = %node_id,
                                    %addr,
                                    %e,
                                    "async replica write failed"
                                );
                            }
                        }
                    });
                }
            }
            ReplicationMode::Sync => {
                // Best-effort: await every reachable secondary. Failures
                // are logged/counted but do not fail the client write.
                for node in secondaries {
                    let ok = self.write_one_replica(node, &self_id, key, value, ttl).await;
                    if !ok {
                        self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        true
    }

    /// Write one copy of a large value to a single replica node.
    ///
    /// Writes locally when `node` is this node, otherwise via the TCP
    /// data plane. Returns whether the write was accepted (a data plane
    /// error is logged and reported as `false`). The caller decides
    /// whether a `false` result bumps the replica-failure counter — the
    /// primary's failure is handled by the [`set_large`](Self::set_large)
    /// fallback, not counted as a replica failure.
    async fn write_one_replica(
        &self,
        node: &NodeInfo,
        self_id: &str,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> bool {
        if node.id == self_id {
            return self.store.set(key.to_string(), value.to_vec(), ttl);
        }
        let Some(addr) = node_data_addr(node, self.config.data_port) else {
            tracing::warn!(key, replica = %node.id, "replica has no parseable data address");
            return false;
        };
        match crate::kv_data_plane::store_remote(addr, key, value, self.cipher.as_deref()).await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(key, replica = %node.id, %addr, %e, "replica write failed");
                false
            }
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
                self.hot_cache_mem.fetch_sub(mem as u64, Ordering::Relaxed);
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

    /// Compute the replica set for `key` from live cluster membership.
    ///
    /// The set is the primary owner (`hash(key) % alive_nodes`) followed
    /// by the next `replication_factor - 1` distinct alive nodes on the
    /// sorted ring, wrapping around. The returned vector is ordered
    /// primary-first (the read/write fallback order). Returns an empty
    /// vector when only this node is alive (no clustering to do).
    async fn replica_nodes(&self, key: &str) -> Vec<NodeInfo> {
        let nodes = self.cluster.nodes().await;
        let mut alive: Vec<NodeInfo> =
            nodes.into_iter().filter(|n| n.state == crate::NodeState::Alive).collect();
        // `ClusterHandle::nodes` already sorts by id, but sort defensively
        // so replica selection is stable regardless of caller.
        alive.sort_by(|a, b| a.id.cmp(&b.id));

        if alive.len() <= 1 {
            return Vec::new(); // Only this node is alive.
        }

        replica_nodes_for(&alive, hash_key(key), self.config.replication_factor)
    }

    /// Number of replica writes that failed to reach or be accepted by a
    /// peer since startup. A failed replica write never fails the client
    /// `set` — this counter is for observability only.
    #[must_use]
    pub fn replica_write_failures(&self) -> u64 {
        self.replica_write_failures.load(Ordering::Relaxed)
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
                .or_insert_with(|| HitCounter { count: 0, window_start: now });

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
            self.hot_cache_mem.fetch_sub(old_mem, Ordering::Relaxed);
        }
        self.hot_cache_mem.fetch_add(entry_mem, Ordering::Relaxed);

        tracing::debug!(key, key_hash, value_len = value.len(), "promoted to hot key cache",);
    }

    /// If a key is tracked as hot, bump its version in chitchat so
    /// remote caches invalidate.
    async fn maybe_bump_hot_version(&self, key: &str) {
        let key_hash = hash_key(key);
        // Only bump if we've seen this key in the hit counters (it's hot
        // somewhere) or it's in our own hot cache.
        let is_tracked =
            self.hit_counters.contains_key(&key_hash) || self.hot_cache.contains_key(&key_hash);
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

        tracing::debug!(key, key_hash, new_version, "bumped hot key version via gossip",);
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
                self.hot_cache_mem.fetch_sub(mem, Ordering::Relaxed);
                false
            } else {
                true
            }
        });

        // Evict stale hit counters (windows that have expired).
        self.hit_counters.retain(|_, counter| counter.window_start.elapsed() < window * 2);
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

/// Select the replica set for a key from a sorted alive-node list.
///
/// The primary owner is `alive[key_hash % alive.len()]`; the replica set
/// is the primary plus the next `factor - 1` distinct nodes clockwise on
/// the ring (wrapping around). `factor` is clamped to `alive.len()`, so a
/// replication factor larger than the cluster keeps one copy per node
/// (never an error). The result is primary-first and contains no
/// duplicates.
///
/// This is a pure function of `(alive, key_hash, factor)` so it is
/// directly unit-testable with a fixed node list. `alive` MUST be sorted
/// by a stable key (node id) by the caller for selection to be
/// membership-stable.
fn replica_nodes_for(alive: &[NodeInfo], key_hash: u64, factor: usize) -> Vec<NodeInfo> {
    if alive.is_empty() {
        return Vec::new();
    }
    let n = alive.len();
    // Clamp the factor to the cluster size and to at least one copy.
    let want = factor.clamp(1, n);
    #[allow(clippy::cast_possible_truncation)]
    let start = (key_hash as usize) % n;
    // Walk `want` distinct nodes clockwise from the primary. Because we
    // step over distinct indices `[start, start+1, ...] mod n`, the nodes
    // are inherently distinct — no dedup needed.
    (0..want).map(|offset| alive[(start + offset) % n].clone()).collect()
}

/// Derive a node's TCP data plane address from its gossip address and the
/// configured data port. Returns `None` if the gossip address does not
/// parse as a socket address.
fn node_data_addr(node: &NodeInfo, data_port: u16) -> Option<SocketAddr> {
    node.gossip_addr
        .parse::<SocketAddr>()
        .ok()
        .map(|gossip| SocketAddr::new(gossip.ip(), data_port))
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
    let (num, unit) =
        s.find(|c: char| !c.is_ascii_digit() && c != '.').map_or((s, ""), |i| s.split_at(i));

    let base: f64 = num.parse().map_err(|_| ParseMemoryError::InvalidNumber(s.to_string()))?;

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

    #[test]
    fn hash_key_distribution_reasonable() {
        // Generate 1000 keys and check they don't all collide.
        let mut hashes = std::collections::HashSet::new();
        for i in 0..1000 {
            hashes.insert(hash_key(&format!("key:{i}")));
        }
        // With a good hash, we expect ~1000 unique hashes.
        assert!(
            hashes.len() > 900,
            "hash distribution too clustered: {} unique out of 1000",
            hashes.len()
        );
    }

    #[test]
    fn hash_key_empty_string() {
        // Should not panic.
        let _ = hash_key("");
    }

    #[test]
    fn parse_memory_size_with_decimals() {
        assert_eq!(parse_memory_size("1.5GB").unwrap(), 1_610_612_736);
        assert_eq!(parse_memory_size("0.5MB").unwrap(), 512 * 1024);
    }

    #[test]
    fn parse_memory_size_zero() {
        assert_eq!(parse_memory_size("0").unwrap(), 0);
        assert_eq!(parse_memory_size("0MB").unwrap(), 0);
    }

    #[test]
    fn parse_memory_size_whitespace() {
        assert_eq!(parse_memory_size("  64MB  ").unwrap(), 64 * 1024 * 1024);
    }

    // -----------------------------------------------------------------
    // Replica set selection
    // -----------------------------------------------------------------

    /// Build a sorted 5-node fixture: `n0`..`n4`.
    fn fixture_nodes() -> Vec<NodeInfo> {
        (0..5)
            .map(|i| NodeInfo {
                id: format!("n{i}"),
                gossip_addr: format!("127.0.0.1:{}", 7946 + i),
                state: crate::NodeState::Alive,
            })
            .collect()
    }

    /// The primary is `alive[key_hash % len]`, regardless of factor.
    #[allow(clippy::cast_possible_truncation)]
    fn primary_index(key_hash: u64, len: usize) -> usize {
        (key_hash as usize) % len
    }

    #[test]
    fn replica_set_factor_two_picks_primary_plus_next() {
        let nodes = fixture_nodes();
        // key_hash 7 % 5 = 2 → primary n2, then n3.
        let set = replica_nodes_for(&nodes, 7, 2);
        let ids: Vec<_> = set.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["n2", "n3"]);
    }

    #[test]
    fn replica_set_wraps_around_ring() {
        let nodes = fixture_nodes();
        // key_hash 4 % 5 = 4 → primary n4, then wrap to n0, n1.
        let set = replica_nodes_for(&nodes, 4, 3);
        let ids: Vec<_> = set.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["n4", "n0", "n1"]);
    }

    #[test]
    fn replica_set_clamps_to_alive_count() {
        let nodes = fixture_nodes();
        // Factor 9 on a 5-node cluster keeps 5 distinct copies, not error.
        let set = replica_nodes_for(&nodes, 0, 9);
        assert_eq!(set.len(), 5, "factor is clamped to the number of alive nodes");
        // All distinct.
        let mut ids: Vec<_> = set.iter().map(|n| n.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 5, "replica set must contain no duplicates");
    }

    #[test]
    fn replica_set_factor_one_is_owner_only() {
        let nodes = fixture_nodes();
        for key_hash in [0u64, 1, 2, 3, 4, 99, 12_345] {
            let set = replica_nodes_for(&nodes, key_hash, 1);
            assert_eq!(set.len(), 1);
            assert_eq!(set[0].id, nodes[primary_index(key_hash, 5)].id);
        }
    }

    #[test]
    fn replica_set_zero_factor_is_treated_as_one() {
        let nodes = fixture_nodes();
        // A misconfigured factor of 0 must still keep the primary copy.
        let set = replica_nodes_for(&nodes, 3, 0);
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].id, "n3");
    }

    #[test]
    fn replica_set_empty_node_list() {
        assert!(replica_nodes_for(&[], 42, 2).is_empty());
    }

    #[test]
    fn replica_set_all_entries_distinct() {
        let nodes = fixture_nodes();
        // Exercise every start index at the max factor; each result must
        // be duplicate-free and full-length.
        for key_hash in 0..5u64 {
            let set = replica_nodes_for(&nodes, key_hash, 5);
            let mut ids: Vec<_> = set.iter().map(|n| n.id.clone()).collect();
            assert_eq!(ids.len(), 5);
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), 5, "start={key_hash}: replicas must be distinct");
        }
    }

    #[test]
    fn replica_set_stable_when_unrelated_node_leaves() {
        // Selection must be stable: dropping a node that is NOT in a
        // key's replica set must not change that set's members. key_hash
        // 0 % 5 = 0 → {n0, n1} with factor 2. Removing n3 (not in the
        // set) keeps the same primary and secondary.
        let full = fixture_nodes();
        let before = replica_nodes_for(&full, 0, 2);
        let before_ids: Vec<_> = before.iter().map(|n| n.id.clone()).collect();
        assert_eq!(before_ids, vec!["n0", "n1"]);

        let reduced: Vec<NodeInfo> = full.into_iter().filter(|n| n.id != "n3").collect();
        let after = replica_nodes_for(&reduced, 0, 2);
        let after_ids: Vec<_> = after.iter().map(|n| n.id.clone()).collect();
        assert_eq!(after_ids, before_ids, "removing an unrelated node must not shift the set");
    }

    #[test]
    fn node_data_addr_uses_configured_port() {
        let node = NodeInfo {
            id: "n0".to_string(),
            gossip_addr: "10.0.0.5:7946".to_string(),
            state: crate::NodeState::Alive,
        };
        let addr = node_data_addr(&node, 7947).unwrap();
        assert_eq!(addr.to_string(), "10.0.0.5:7947");
    }

    #[test]
    fn node_data_addr_rejects_unparseable_gossip_addr() {
        let node = NodeInfo {
            id: "n0".to_string(),
            gossip_addr: "not-an-addr".to_string(),
            state: crate::NodeState::Alive,
        };
        assert!(node_data_addr(&node, 7947).is_none());
    }

    #[test]
    fn replication_mode_parsing() {
        assert_eq!(ReplicationMode::from_config("sync"), ReplicationMode::Sync);
        assert_eq!(ReplicationMode::from_config("SYNC"), ReplicationMode::Sync);
        assert_eq!(ReplicationMode::from_config("async"), ReplicationMode::Async);
        // Unknown / default falls back to async.
        assert_eq!(ReplicationMode::from_config("whatever"), ReplicationMode::Async);
        assert_eq!(ReplicationMode::from_config(""), ReplicationMode::Async);
    }
}

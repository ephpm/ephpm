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
use ephpm_kv::store::{Routed, SetNxOutcome, Store};

use crate::ClusterHandle;
use crate::gossip_kv::{Stamp, expiry_ms_for};
use crate::kv_data_plane::DataPlaneClient;
use crate::node::NodeInfo;
use crate::secure_transport::ClusterCipher;

/// The stamp and absolute expiry fixed for one replicated write.
///
/// Taken **once**, by the synchronous replication hook before the local copy
/// is written, and carried unchanged onto every transport — the gossip
/// payload, the applied map, and (for large values) the data plane. A
/// second stamp taken later in a spawned task is what made concurrent writes
/// to one key diverge for good (stress finding F-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteStamp {
    /// Position in the cluster-wide last-writer-wins order.
    pub stamp: Stamp,
    /// Epoch-millisecond expiry carried on the gossip wire; `0` = no TTL.
    pub expiry_ms: u64,
}

/// Key prefix for hot-key version metadata in chitchat state.
const HOT_PREFIX: &str = "hot:";

/// How long a non-owner waits for the owner to decide a routed `SETNX` /
/// compare-and-delete before treating it as unreachable (and writing nothing).
///
/// The call blocks a PHP worker or RESP connection, so it is kept well below
/// any request timeout. An owner that is merely slow — not dead — is still
/// answered "not acquired", which a lock caller retries; a dead owner stops
/// being the owner once gossip declares it dead.
pub const CONDITIONAL_ROUTE_TIMEOUT: Duration = Duration::from_secs(2);

/// Run `fut` to completion from a **synchronous** caller on `handle`'s runtime.
///
/// [`ephpm_kv::store::Replicator`]'s routing hooks are synchronous (RESP
/// dispatch, the PHP bridge, middleware), but owner routing is a network round
/// trip. Three caller shapes exist:
///
/// - a thread outside any runtime (the PHP execution pools): `block_on` the
///   runtime handle directly;
/// - a multi-thread runtime worker (RESP connections, ACME, middleware):
///   `block_in_place` hands this worker's other tasks to a fresh worker while
///   the round trip runs, so the runtime keeps making progress;
/// - a current-thread runtime: blocking would deadlock the only worker, so
///   this returns `None` and the caller fails closed. Production runs the
///   multi-thread runtime; this case exists for tests.
///
/// A panic while blocking (the runtime is shutting down under a PHP thread
/// that is still serving a request) also returns `None` rather than unwinding
/// into the `extern "C"` caller, which would abort the process.
fn block_on_cluster<F: std::future::Future>(
    handle: &tokio::runtime::Handle,
    fut: F,
) -> Option<F::Output> {
    // `catch_unwind`: the callers include `extern "C"` PHP bridge functions on
    // ePHPm-owned PHP threads, which are not joined before the runtime drops
    // at shutdown. `block_on` + `tokio::time::timeout` on a runtime that is
    // shutting down panics, and a panic unwinding out of an `extern "C"` fn
    // aborts the process. Treat it as "owner unreachable" (fail closed).
    match tokio::runtime::Handle::try_current() {
        Err(_) => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.block_on(fut))).ok()
        }
        Ok(current) if current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tokio::task::block_in_place(|| handle.block_on(fut))
            }))
            .ok()
        }
        Ok(_) => {
            tracing::warn!(
                "KV conditional write called from a current-thread tokio runtime; cannot route \
                 it to the key's owner without deadlocking, so it fails closed"
            );
            None
        }
    }
}

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
    /// [`ReplicationMode::Async`], matching the config default. Under `ephpm
    /// serve`, `Config::validate` in `ephpm-config` rejects anything but
    /// `async`/`sync` before this runs; paths that skip validation (e.g.
    /// `ephpm dev`) still get this lenient fallback.
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
    /// Pooled, timeout-bounded client for peers' TCP data planes (sealed
    /// with the cluster cipher when one is configured).
    client: Arc<DataPlaneClient>,
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

/// Approximate heap footprint accounted for one hot-cache entry.
fn hot_entry_bytes(entry: &CachedValue) -> u64 {
    (entry.data.len() + entry.key.len() + 64) as u64
}

/// Evict `key_hash` from the hot cache, debiting the memory counter by the
/// footprint of the entry *this caller actually removed*.
///
/// The accounting must hang off the value returned by `remove`, never off a
/// value read beforehand. `DashMap::remove` returns `Some` to exactly one
/// caller, so this is race-free; debiting unconditionally is not. Two readers
/// hitting the same expired entry both used to subtract, and `hot_cache_mem`
/// is a `u64` — it wraps to ~`u64::MAX`, the budget check in
/// [`ClusteredStore::maybe_promote_hot_key`] is then always over, and hot-key
/// promotion is silently dead for the rest of the process's life while
/// [`ClusteredStore::hot_cache_mem_used`] reports garbage.
///
/// The caller must not be holding a `DashMap` guard for `key_hash`: `remove`
/// takes the shard write lock.
fn evict_hot_entry(cache: &DashMap<u64, CachedValue>, mem: &AtomicU64, key_hash: u64) {
    if let Some((_, entry)) = cache.remove(&key_hash) {
        mem.fetch_sub(hot_entry_bytes(&entry), Ordering::Relaxed);
    }
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
            client: Arc::new(DataPlaneClient::new(cipher)),
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
                // Read the stale version out and release the shard read guard
                // before `evict_hot_entry` takes the write lock.
                let stale_version = this
                    .hot_cache
                    .get(&key_hash)
                    .and_then(|entry| (entry.version < new_version).then_some(entry.version));
                if let Some(old_version) = stale_version {
                    evict_hot_entry(&this.hot_cache, &this.hot_cache_mem, key_hash);
                    tracing::debug!(
                        key_hash,
                        old_version,
                        new_version,
                        "hot key invalidated via gossip",
                    );
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
        // `Store::get` now returns `Bytes` (Arc-cloned, no memcpy); the
        // clustered API still returns `Vec<u8>` for callers that hold it
        // across await points, so we materialise here at the API
        // boundary. The internal read path stays copy-free.
        if let Some(value) = self.store.get(key) {
            return Some(value.to_vec());
        }

        // 3. Check hot key cache.
        if self.config.hot_key_cache {
            let key_hash = hash_key(key);
            if let Some(cached) = self.hot_cache.get(&key_hash) {
                let ttl = Duration::from_secs(self.config.hot_key_local_ttl_secs);
                if cached.cached_at.elapsed() < ttl && cached.key == key {
                    return Some(cached.data.clone());
                }
                // Expired (or a hash collision with another key) — evict it.
                // Drop the shard read guard first: `evict_hot_entry` takes the
                // write lock, and it debits the counter only for the entry it
                // actually removed, so two readers racing here cannot
                // double-subtract.
                drop(cached);
                evict_hot_entry(&self.hot_cache, &self.hot_cache_mem, key_hash);
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
            match self.client.fetch(addr, key).await {
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

    /// Fix the stamp and absolute expiry for a write about to be made with
    /// `ttl`. The sync replication hooks call this before touching the local
    /// store, then hand the result to the `*_at` variants below.
    #[must_use]
    pub fn new_write(&self, ttl: Option<Duration>) -> WriteStamp {
        WriteStamp { stamp: self.cluster.stamp(), expiry_ms: expiry_ms_for(ttl) }
    }

    /// Set a value, routing through the appropriate tier, stamping the write
    /// now. See [`set_at`](Self::set_at).
    pub async fn set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        let w = self.new_write(ttl);
        self.set_at(key, value, ttl, w).await
    }

    /// Set a value, routing through the appropriate tier, with a stamp the
    /// caller already fixed.
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
    pub async fn set_at(
        &self,
        key: String,
        value: Vec<u8>,
        ttl: Option<Duration>,
        w: WriteStamp,
    ) -> bool {
        if value.len() <= self.config.small_key_threshold {
            // Gossip tier: replicate to all nodes.
            self.cluster.gossip_set_stamped(&key, &value, w.expiry_ms, w.stamp).await;

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

    /// Fan out an **already-applied local value** to the rest of the cluster,
    /// stamping it now. See [`publish_at`](Self::publish_at).
    pub async fn publish(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> bool {
        let w = self.new_write(ttl);
        self.publish_at(key, value, ttl, w).await
    }

    /// Fan out an **already-applied local value** to the rest of the cluster
    /// with a stamp the caller already fixed.
    ///
    /// Same tier routing as [`set`](Self::set) — gossip for small values,
    /// replica-set writes for large ones — with one deliberate difference: it
    /// never writes the value into this node's local store. The caller (a
    /// read-modify-write op on [`Store`]) applied it under the key's shard lock
    /// already, and re-writing it here would let an older publish overwrite a
    /// newer local total, losing an update on the origin node. See
    /// [`ephpm_kv::store::Replicator::replicate_published`].
    pub async fn publish_at(
        &self,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
        w: WriteStamp,
    ) -> bool {
        if value.len() <= self.config.small_key_threshold {
            self.cluster.gossip_set_stamped(key, value, w.expiry_ms, w.stamp).await;
            if self.config.hot_key_cache {
                self.maybe_bump_hot_version(key).await;
            }
            return true;
        }
        let ok = self.set_large_routed(key, &self.store, key, value, ttl, LocalCopy::Skip).await;
        if ok && self.config.hot_key_cache {
            self.maybe_bump_hot_version(key).await;
        }
        ok
    }

    /// Write a value that **every** node must hold a local copy of,
    /// regardless of its size.
    ///
    /// Small values already behave this way — the gossip tier fans out to the
    /// whole cluster — so this differs from [`set`](Self::set) only above
    /// `small_key_threshold`, where `set` shards to `replication_factor`
    /// nodes and this writes to every alive node instead.
    ///
    /// # Why a separate tier rather than a bigger threshold
    ///
    /// Raising `small_key_threshold` past the value's size is not an
    /// equivalent fix: the gossip tier is chitchat over **UDP**, so a payload
    /// that does not fit a datagram cannot ride it at all. The size threshold
    /// exists to keep multi-kilobyte values off gossip, and it must keep
    /// doing that. What was wrong is the assumption that "too big for gossip"
    /// implies "shard it" — for cluster-wide state such as an ACME
    /// certificate it implies the opposite.
    ///
    /// Unlike the sharded path, peer writes are **awaited**: a broadcast key
    /// is written rarely (certificate issuance and renewal) and the caller
    /// wants cluster-wide read-your-writes far more than it wants the write
    /// to return quickly. A peer that is unreachable is logged and counted
    /// ([`ClusteredStore::replica_write_failures`]) but does not fail the
    /// write, matching the rest of this store: this is not a consensus
    /// protocol.
    ///
    /// Returns whether **this node's** copy was written — the same
    /// client-visible contract [`set`](Self::set) has.
    ///
    /// ## What this does not do
    ///
    /// Fan-out reaches the nodes that are alive *at write time*. A node that
    /// joins later, or is down and comes back, does not receive the value
    /// retroactively — there is no anti-entropy here any more than there is
    /// on the sharded path. Readers that must tolerate that should use
    /// [`get`](Self::get), whose miss path fetches from a peer.
    pub async fn broadcast(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        let w = self.new_write(ttl);
        self.broadcast_at(key, value, ttl, w).await
    }

    /// [`broadcast`](Self::broadcast) with a stamp the caller already fixed.
    pub async fn broadcast_at(
        &self,
        key: String,
        value: Vec<u8>,
        ttl: Option<Duration>,
        w: WriteStamp,
    ) -> bool {
        if value.len() <= self.config.small_key_threshold {
            // Gossip already means "every node"; nothing to widen.
            self.cluster.gossip_set_stamped(&key, &value, w.expiry_ms, w.stamp).await;
            if self.config.hot_key_cache {
                self.maybe_bump_hot_version(&key).await;
            }
            return true;
        }

        let ok = self.broadcast_large(&key, &value, ttl).await;
        if ok && self.config.hot_key_cache {
            self.maybe_bump_hot_version(&key).await;
        }
        ok
    }

    /// [`publish`](Self::publish) for a **per-site** key: the site-namespaced
    /// transport key goes on the wire, and — as with the global path — this
    /// node's own copy is left exactly as the mutation left it.
    pub async fn publish_for_site(
        &self,
        site: &str,
        local: &Store,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
        w: WriteStamp,
    ) -> bool {
        let transport = crate::site_namespace::encode(site, key);
        if value.len() <= self.config.small_key_threshold {
            self.cluster.gossip_set_stamped(&transport, value, w.expiry_ms, w.stamp).await;
            return true;
        }
        self.set_large_routed(&transport, local, key, value, ttl, LocalCopy::Skip).await
    }

    /// Replicate a **per-site** (multi-tenant) write.
    ///
    /// Identical routing to [`set`](Self::set), except that what goes on the
    /// wire is the site-namespaced *transport* key
    /// (`site_namespace::encode(site, key)`) while the local copy is written
    /// into that site's own [`Store`] under the bare key. The receiving node
    /// decodes the envelope and applies the write into the same site's store,
    /// so a vhost's keys replicate without ever entering the global keyspace or
    /// another tenant's.
    ///
    /// `local` is the site's store — the one whose `set_local` also bumps the
    /// key's watch slot, keeping `ephpm_kv_wait` working for replicated writes.
    ///
    /// The small/large tier decision compares `value.len()` against
    /// `small_key_threshold` exactly as the global path does; the envelope adds
    /// key bytes only, so the payload budget is unchanged.
    pub async fn set_for_site(
        &self,
        site: &str,
        local: &Store,
        key: String,
        value: Vec<u8>,
        ttl: Option<Duration>,
        w: WriteStamp,
    ) -> bool {
        let transport = crate::site_namespace::encode(site, &key);
        if value.len() <= self.config.small_key_threshold {
            // Gossip tier: replicated to all nodes, applied per-site on arrival.
            tracing::trace!(
                site,
                key,
                bytes = value.len(),
                tier = "gossip",
                "per-site KV: publishing"
            );
            self.cluster.gossip_set_stamped(&transport, &value, w.expiry_ms, w.stamp).await;
            return true;
        }
        // Large value: replica-set write over the data plane. The transport key
        // is what peers store and route on; our own copy is the bare key in the
        // site's store.
        tracing::trace!(
            site,
            key,
            bytes = value.len(),
            tier = "data-plane",
            "per-site KV: publishing"
        );
        let ok =
            self.set_large_routed(&transport, local, &key, &value, ttl, LocalCopy::Write).await;
        if !ok {
            // A large-tier publish that fails leaves this vhost's key on this
            // node only. Never silent — that is exactly how the INCR gap
            // survived a full deploy cycle.
            tracing::warn!(
                site,
                key,
                bytes = value.len(),
                "per-site KV: large-value publish FAILED — this key is now node-local and will \
                 not be visible on peers until it is rewritten"
            );
        }
        ok
    }

    /// Delete a per-site key: gossip tombstone under the namespaced transport
    /// key, plus a local drop from the site's own store.
    pub async fn remove_for_site(
        &self,
        site: &str,
        local: &Store,
        key: &str,
        stamp: Stamp,
    ) -> bool {
        let transport = crate::site_namespace::encode(site, key);
        let gossip_deleted = self.cluster.gossip_del_stamped(&transport, stamp).await;
        let local_deleted = local.remove_local(key);
        gossip_deleted || local_deleted
    }

    /// Per-site publish-only tombstone — the multi-tenant mirror of
    /// [`gossip_tombstone`](Self::gossip_tombstone). Gossips the
    /// site-namespaced delete without removing the site store's local copy, so
    /// a value recreated between `del_if_eq`'s removal and this broadcast
    /// survives on this node.
    pub async fn gossip_tombstone_for_site(&self, site: &str, key: &str, stamp: Stamp) -> bool {
        let transport = crate::site_namespace::encode(site, key);
        self.cluster.gossip_del_stamped(&transport, stamp).await
    }

    /// Re-broadcast a per-site key's current value with a new TTL, so peers
    /// pick up the new expiry by the same last-writer-wins rule the global
    /// path uses. Small-key tier only, matching the global `replicate_expire`.
    pub async fn expire_for_site(&self, site: &str, key: &str, value: Vec<u8>, w: WriteStamp) {
        if value.len() <= self.config.small_key_threshold {
            let transport = crate::site_namespace::encode(site, key);
            self.cluster.gossip_set_stamped(&transport, &value, w.expiry_ms, w.stamp).await;
        }
    }

    /// Write a large value to every alive node. See
    /// [`broadcast`](Self::broadcast).
    async fn broadcast_large(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> bool {
        // This node's own copy first: it must be able to serve the value out
        // of its local store even if every peer write below fails.
        let local_ok = self.store.set_local(key.to_string(), value.to_vec(), ttl);

        let self_id = self.cluster.self_node().id;
        let mut delivered = 0usize;
        let mut peers = 0usize;
        for node in self.alive_nodes().await {
            if node.id == self_id {
                continue;
            }
            peers += 1;
            let Some(addr) = node_data_addr(&node, self.config.data_port) else {
                self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(key, peer = %node.id, "broadcast peer has no parseable data address");
                continue;
            };
            match self.client.store(addr, key, value).await {
                Ok(true) => delivered += 1,
                Ok(false) => {
                    self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(key, peer = %node.id, %addr, "broadcast peer rejected the write");
                }
                Err(e) => {
                    self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(key, peer = %node.id, %addr, %e, "broadcast peer write failed");
                }
            }
        }

        if delivered < peers {
            // Never silent: a partially-delivered broadcast means some nodes
            // cannot read a value they are supposed to hold locally.
            tracing::warn!(
                key,
                bytes = value.len(),
                delivered,
                peers,
                "broadcast reached only some peers; the rest must fetch through on read"
            );
        } else {
            tracing::debug!(key, bytes = value.len(), peers, "broadcast written to every peer");
        }

        local_ok
    }

    /// Write a large value to its replica set. Returns whether the
    /// primary write succeeded (the client-visible result). See
    /// [`ClusteredStore::set`] for the replication semantics.
    async fn set_large(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> bool {
        // The global path's transport key IS the key, and its local target is
        // the process-wide store — so this is byte-for-byte the old behaviour.
        self.set_large_routed(key, &self.store, key, value, ttl, LocalCopy::Write).await
    }

    /// [`set_large`](Self::set_large) with the wire key and the local target
    /// decoupled, so the per-site path can put a namespaced key on the wire
    /// while writing the bare key into that site's store. The global path
    /// passes `transport_key == local_key` and the process-wide store.
    ///
    /// `local_copy` decides whether this node's own copy is (re)written. Every
    /// `set` path passes [`LocalCopy::Write`] — the value has to land somewhere
    /// locally. The publish paths pass [`LocalCopy::Skip`]: the mutation already
    /// wrote it under the key's shard lock, and rewriting it from a spawned
    /// task could regress a newer concurrent mutation.
    #[allow(clippy::too_many_arguments)]
    async fn set_large_routed(
        &self,
        transport_key: &str,
        local: &Store,
        local_key: &str,
        value: &[u8],
        ttl: Option<Duration>,
        local_copy: LocalCopy,
    ) -> bool {
        let key = transport_key;
        let self_id = self.cluster.self_node().id;
        let replicas = self.replica_nodes(key).await;

        // No cluster peers (single node, or only self alive): store
        // locally and we're done.
        if replicas.is_empty() {
            return local_copy.write(local, local_key, value, ttl);
        }

        // The primary is the first replica; the rest are secondaries.
        // Write the primary copy first — its result is what the client
        // sees. Secondaries follow per the replication mode.
        let (primary, secondaries) = replicas.split_first().expect("replicas is non-empty");
        let primary_ok = self
            .write_one_replica(primary, &self_id, key, local, local_key, value, ttl, local_copy)
            .await;
        if !primary_ok {
            // The primary write failed. Fall back to a local copy so the
            // value is not lost outright, mirroring the pre-replication
            // behaviour. Do not attempt secondaries in this case.
            if primary.id != self_id {
                return local_copy.write(local, local_key, value, ttl);
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
                        if !local_copy.write(local, local_key, value, ttl) {
                            self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(key, "local replica write rejected (memory limit?)");
                        }
                        continue;
                    }
                    let Some(addr) = node_data_addr(node, self.config.data_port) else {
                        continue;
                    };
                    let client = Arc::clone(&self.client);
                    let key = key.to_string();
                    let value = value.to_vec();
                    let node_id = node.id.clone();
                    let failures = Arc::clone(&self.replica_write_failures);
                    tokio::spawn(async move {
                        match client.store(addr, &key, &value).await {
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
                    let ok = self
                        .write_one_replica(
                            node, &self_id, key, local, local_key, value, ttl, local_copy,
                        )
                        .await;
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
    #[allow(clippy::too_many_arguments)]
    async fn write_one_replica(
        &self,
        node: &NodeInfo,
        self_id: &str,
        key: &str,
        local: &Store,
        local_key: &str,
        value: &[u8],
        ttl: Option<Duration>,
        local_copy: LocalCopy,
    ) -> bool {
        if node.id == self_id {
            return local_copy.write(local, local_key, value, ttl);
        }
        let Some(addr) = node_data_addr(node, self.config.data_port) else {
            tracing::warn!(key, replica = %node.id, "replica has no parseable data address");
            return false;
        };
        match self.client.store(addr, key, value).await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(key, replica = %node.id, %addr, %e, "replica write failed");
                false
            }
        }
    }

    /// Delete a key from whichever tier owns it, stamping the delete now.
    pub async fn remove(&self, key: &str) -> bool {
        let stamp = self.cluster.stamp();
        self.remove_at(key, stamp).await
    }

    /// Delete a key from whichever tier owns it with a stamp the caller
    /// already fixed.
    pub async fn remove_at(&self, key: &str, stamp: Stamp) -> bool {
        // Try gossip tier first.
        let gossip_deleted = self.cluster.gossip_del_stamped(key, stamp).await;

        // Also remove from local store. `remove_local` skips the Replicator
        // hook so we don't re-enter ourselves when this ClusteredStore is
        // installed as its own local Store's replicator.
        let local_deleted = self.store.remove_local(key);

        // Evict from hot cache.
        if self.config.hot_key_cache {
            evict_hot_entry(&self.hot_cache, &self.hot_cache_mem, hash_key(key));
        }

        gossip_deleted || local_deleted
    }

    /// Broadcast a delete **tombstone only** — fan the removal out on the
    /// gossip tier without touching this node's local materialized copy.
    ///
    /// This is the transport for
    /// [`ephpm_kv::store::Replicator::replicate_remove_published`], emitted by
    /// [`Store::del_if_eq`](ephpm_kv::store::Store::del_if_eq) after its
    /// compare-and-delete has already removed the matching entry locally under
    /// the key's shard lock. Unlike [`remove`](Self::remove) it must NOT call
    /// `remove_local`: a concurrent writer may have recreated the key with a
    /// different value in the gap between that local removal and this call, and
    /// re-removing here would destroy it — reopening the delete-after-recreate
    /// race the compare-and-delete exists to close. Peers converge by the
    /// gossip tier's usual last-writer-wins rule.
    pub async fn gossip_tombstone(&self, key: &str, stamp: Stamp) -> bool {
        let deleted = self.cluster.gossip_del_stamped(key, stamp).await;
        if self.config.hot_key_cache {
            evict_hot_entry(&self.hot_cache, &self.hot_cache_mem, hash_key(key));
        }
        deleted
    }

    /// The node that arbitrates conditional writes (`SETNX`, compare-and-delete)
    /// on `transport_key`, or `None` when this node is the arbiter.
    ///
    /// The arbiter is the key's **primary owner** — the same
    /// `hash(key) % sorted alive nodes` choice the large-value tier uses — so
    /// every node that agrees on membership names the same single node, and the
    /// one shard lock on that node serialises every caller in the cluster.
    /// `None` also covers a cluster of one.
    pub async fn conditional_owner(&self, transport_key: &str) -> Option<NodeInfo> {
        let primary = self.replica_nodes(transport_key).await.into_iter().next()?;
        (primary.id != self.cluster.self_node().id).then_some(primary)
    }

    /// Route a `SETNX` on `transport_key` to its arbiter.
    ///
    /// Returns [`Routed::Local`] when this node is the arbiter (the caller then
    /// decides on its own map), the owner's answer when it is another node, or
    /// [`Routed::OwnerUnreachable`] when the owner did not answer within
    /// [`CONDITIONAL_ROUTE_TIMEOUT`] — in which case the outcome is unknown: a
    /// request that reached the owner before the timeout may have been applied.
    pub async fn route_set_nx(
        &self,
        transport_key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Routed<SetNxOutcome> {
        let Some(owner) = self.conditional_owner(transport_key).await else {
            return Routed::Local;
        };
        let Some(addr) = node_data_addr(&owner, self.config.data_port) else {
            tracing::warn!(key = transport_key, owner = %owner.id, "SETNX owner has no data address");
            return Routed::OwnerUnreachable;
        };
        let call = self.client.set_nx(addr, transport_key, value, ttl);
        match tokio::time::timeout(CONDITIONAL_ROUTE_TIMEOUT, call).await {
            Ok(Ok(outcome)) => {
                tracing::trace!(key = transport_key, owner = %owner.id, ?outcome, "SETNX routed");
                Routed::Owner(outcome)
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    key = transport_key, owner = %owner.id, %addr, %e,
                    "SETNX owner unreachable; not acquired (fail closed)"
                );
                Routed::OwnerUnreachable
            }
            Err(_) => {
                tracing::warn!(
                    key = transport_key, owner = %owner.id, %addr,
                    "SETNX owner timed out; not acquired (fail closed)"
                );
                Routed::OwnerUnreachable
            }
        }
    }

    /// Route a compare-and-delete on `transport_key` to the same arbiter
    /// [`route_set_nx`](Self::route_set_nx) uses. Same contract.
    pub async fn route_del_if_eq(&self, transport_key: &str, token: &[u8]) -> Routed<bool> {
        let Some(owner) = self.conditional_owner(transport_key).await else {
            return Routed::Local;
        };
        let Some(addr) = node_data_addr(&owner, self.config.data_port) else {
            tracing::warn!(key = transport_key, owner = %owner.id, "DELIFEQ owner has no data address");
            return Routed::OwnerUnreachable;
        };
        let call = self.client.del_if_eq(addr, transport_key, token);
        match tokio::time::timeout(CONDITIONAL_ROUTE_TIMEOUT, call).await {
            Ok(Ok(deleted)) => Routed::Owner(deleted),
            Ok(Err(e)) => {
                tracing::warn!(
                    key = transport_key, owner = %owner.id, %addr, %e,
                    "DELIFEQ owner unreachable; nothing deleted (fail closed)"
                );
                Routed::OwnerUnreachable
            }
            Err(_) => {
                tracing::warn!(
                    key = transport_key, owner = %owner.id, %addr,
                    "DELIFEQ owner timed out; nothing deleted (fail closed)"
                );
                Routed::OwnerUnreachable
            }
        }
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
        let alive = self.alive_nodes().await;

        if alive.len() <= 1 {
            return Vec::new(); // Only this node is alive.
        }

        replica_nodes_for(&alive, hash_key(key), self.config.replication_factor)
    }

    /// Every alive cluster node, including this one, sorted by id.
    ///
    /// `ClusterHandle::nodes` already sorts by id, but this sorts defensively
    /// so replica selection is stable regardless of caller.
    async fn alive_nodes(&self) -> Vec<NodeInfo> {
        let mut alive: Vec<NodeInfo> = self
            .cluster
            .nodes()
            .await
            .into_iter()
            .filter(|n| n.state == crate::NodeState::Alive)
            .collect();
        alive.sort_by(|a, b| a.id.cmp(&b.id));
        alive
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

        // Don't exceed memory budget. `saturating_add` so a bad accounting
        // state can never turn this check into an overflow panic in debug
        // builds.
        if self.hot_cache_mem.load(Ordering::Relaxed).saturating_add(entry_mem) > max_mem {
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

        // Adjust memory accounting. `insert` returns the displaced value to
        // exactly one caller, so debiting off `old` is race-free for the same
        // reason `evict_hot_entry` is.
        if let Some(old_entry) = old {
            self.hot_cache_mem.fetch_sub(hot_entry_bytes(&old_entry), Ordering::Relaxed);
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

        // Evict expired hot cache entries. `retain` visits each entry once
        // under its shard lock, so the debit here happens exactly once per
        // removal — same invariant `evict_hot_entry` enforces.
        self.hot_cache.retain(|_, entry| {
            if entry.cached_at.elapsed() >= ttl {
                self.hot_cache_mem.fetch_sub(hot_entry_bytes(entry), Ordering::Relaxed);
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

// ─────────────────────────────────────────────────────────────────────
// Sync bridge to `ephpm_kv::store::Replicator`
// ─────────────────────────────────────────────────────────────────────

/// Adapter that implements the synchronous
/// [`ephpm_kv::store::Replicator`] trait on top of the async
/// [`ClusteredStore`] API.
///
/// The RESP command dispatcher and the PHP `kv_bridge` FFI callbacks call
/// `Store::set` / `remove` / `expire` from **non-async** contexts, but
/// `ClusteredStore`'s routing is async (chitchat writes, data-plane RPCs).
/// This adapter bridges the two by pinning a tokio [`Handle`] at
/// construction time and calling
/// [`Handle::spawn`] for the async work.
///
/// ## Semantics
///
/// - **Small keys (≤ `small_key_threshold`)** — gossip fan-out is spawned
///   fire-and-forget. The hook returns `true` immediately (the gossip tier
///   never rejects a write locally). Peers converge in gossip-time.
/// - **Large keys** — a bounded-inline primary write happens synchronously
///   via `Handle::block_on` **only** when we are NOT already inside a
///   tokio runtime. Inside a runtime we cannot `block_on` (that panics),
///   so we spawn and return `true`. This matches the semantics operators
///   already accept from the `replication_mode = "async"` path: replicas
///   are best-effort, the client sees success as long as the local write
///   would have succeeded.
/// - **Remove / expire** — always spawned fire-and-forget. The return
///   value reflects only the local action.
///
/// This is a v1 sync-bridge. A future revision may collapse the runtime
/// gap by making the RESP dispatcher itself async and threading a routing
/// trait through it; that is a larger refactor and out of scope here.
pub struct KvReplicator {
    inner: Arc<ClusteredStore>,
    /// The tokio runtime that owns the cluster gossip + data plane tasks.
    /// Captured at construction so sync callers on any thread can spawn
    /// async replication work.
    handle: tokio::runtime::Handle,
    /// Shared with [`start_gossip_applier`]. When we materialize a local
    /// copy synchronously we also record the write's stamp here so a
    /// slower echo of that same write cannot clobber a newer overwrite.
    applied: AppliedWriteMap,
}

impl std::fmt::Debug for KvReplicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvReplicator").field("clustered_store", &"<ClusteredStore>").finish()
    }
}

impl KvReplicator {
    /// Wrap a [`ClusteredStore`] as a sync [`ephpm_kv::store::Replicator`]
    /// bound to the given tokio runtime `handle`. Typically
    /// [`tokio::runtime::Handle::current`] captured from the server's
    /// tokio context at startup.
    ///
    /// `applied` MUST be the same handle passed to
    /// [`start_gossip_applier`] on this node — sharing it is what makes
    /// local writes and remote applies participate in the same
    /// last-arrival-wins ordering.
    #[must_use]
    pub fn new(
        inner: Arc<ClusteredStore>,
        handle: tokio::runtime::Handle,
        applied: AppliedWriteMap,
    ) -> Arc<Self> {
        Arc::new(Self { inner, handle, applied })
    }
}

impl ephpm_kv::store::Replicator for KvReplicator {
    fn replicate_set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        // Small values: materialize the LOCAL copy synchronously first.
        // The gossip tier lives in chitchat state, invisible to raw-store
        // readers (RESP GET, PHP native functions, the OPcache watcher) —
        // without this, the origin node itself could not read back its own
        // write. Peers materialize via the gossip applier
        // (start_gossip_applier).
        // ONE stamp per write, taken before anything is written, and carried
        // unchanged into the applied map and the gossip payload — so the
        // origin's own record and what peers see are the same point in the
        // order. (Stamping again inside the spawned task is what let two
        // nodes each win on the other, F-7.)
        let w = self.inner.new_write(ttl);
        if value.len() <= self.inner.config.small_key_threshold {
            // Record OUR own write in the applied map and materialize it in
            // one step under the key's slot (`apply_if_newer`), so a slower
            // echo of this write from gossip, a concurrent local writer or a
            // peer's write racing this one cannot leave the local copy and
            // the recorded stamp disagreeing.
            let store = &self.inner.store;
            let fresh = apply_if_newer(&self.applied, &key, w.stamp, || {
                store.set_local(key.clone(), value.clone(), ttl);
            });
            if !fresh {
                // A newer write for this key already landed here: last
                // writer wins, and this one is neither kept nor published.
                return true;
            }
        }
        // Fire-and-forget: hand the routed write off to the runtime and
        // return success. The clustered set() itself decides gossip vs
        // local + replica fan-out and does all local writes via
        // Store::set_local, so no recursion is possible.
        let inner = Arc::clone(&self.inner);
        self.handle.spawn(async move {
            let ok = inner.set_at(key.clone(), value, ttl, w).await;
            if !ok {
                tracing::warn!(key, "clustered set returned false (primary rejected)");
            }
        });
        true
    }

    fn replicate_broadcast(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        // Materialize this node's own copy synchronously at *every* size, not
        // just below the gossip threshold as `replicate_set` does. A
        // broadcast key's contract is "readable from every node's raw local
        // Store", and the node that wrote it is one of those nodes — the
        // caller (ACME certificate issuance) reads its own write back.
        let w = self.inner.new_write(ttl);
        // Same last-writer-wins bookkeeping as `replicate_set`, at every
        // size: the record and the local write are one step.
        let store = &self.inner.store;
        let mut local_ok = true;
        let fresh = apply_if_newer(&self.applied, &key, w.stamp, || {
            local_ok = store.set_local(key.clone(), value.clone(), ttl);
        });
        if !fresh {
            return true;
        }

        let inner = Arc::clone(&self.inner);
        self.handle.spawn(async move {
            if !inner.broadcast_at(key.clone(), value, ttl, w).await {
                tracing::warn!(key, "clustered broadcast returned false (local write rejected)");
            }
        });
        local_ok
    }

    fn fetch_cluster<'a>(
        &'a self,
        key: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + 'a>> {
        // `ClusteredStore::get` is the cluster-aware read: gossip tier, local
        // store, hot cache, then a data-plane fetch from the key's replica
        // set. That last step is what a node which missed the write needs.
        Box::pin(async move { self.inner.get(key).await })
    }

    fn route_set_nx(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Routed<SetNxOutcome> {
        // The global keyspace's transport key IS the key.
        block_on_cluster(&self.handle, self.inner.route_set_nx(key, value, ttl))
            .unwrap_or(Routed::OwnerUnreachable)
    }

    fn route_del_if_eq(&self, key: &str, token: &[u8]) -> Routed<bool> {
        block_on_cluster(&self.handle, self.inner.route_del_if_eq(key, token))
            .unwrap_or(Routed::OwnerUnreachable)
    }

    fn replicate_remove(&self, key: &str) -> bool {
        // Drop the local materialized copy synchronously (mirror of the
        // set path above), then propagate the removal via a gossip
        // tombstone. Record our own stamp in the applied map so a
        // slow echo of THIS tombstone through gossip does not clobber a
        // follow-up SET we later accept locally.
        let stamp = self.inner.cluster.stamp();
        let store = &self.inner.store;
        let fresh = apply_if_newer(&self.applied, key, stamp, || {
            store.remove_local(key);
        });
        if !fresh {
            // A newer write already landed: it wins over this delete.
            return true;
        }

        let inner = Arc::clone(&self.inner);
        let key = key.to_string();
        self.handle.spawn(async move {
            let _ = inner.remove_at(&key, stamp).await;
        });
        // We cannot know synchronously whether the key existed on any
        // tier; return `true` optimistically (RESP DEL semantics: the
        // caller uses this as an "attempted" signal, not a guarantee).
        true
    }

    fn replicate_published(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) {
        // PUBLISH ONLY — no `set_local`. The read-modify-write op that called
        // this already applied `value` under the key's shard lock; writing it
        // back here would let an older increment's publish overwrite a newer
        // local total (the publishes are spawned, so they race). See
        // `Replicator::replicate_published`.
        //
        // Recording our own stamp is still required: it is what stops the
        // gossip echo of THIS publish from clobbering a newer local value that
        // lands before the echo arrives.
        let w = self.inner.new_write(ttl);
        if value.len() <= self.inner.config.small_key_threshold
            && !apply_if_newer(&self.applied, &key, w.stamp, || {})
        {
            // Overtaken by a newer write for this key: not published.
            return;
        }
        let inner = Arc::clone(&self.inner);
        self.handle.spawn(async move {
            if !inner.publish_at(&key, &value, ttl, w).await {
                tracing::warn!(key, "clustered publish returned false (primary rejected)");
            }
        });
    }

    fn replicate_remove_published(&self, key: String) {
        // PUBLISH ONLY — no local removal. `Store::del_if_eq` already removed
        // the matching entry under its shard lock; a re-removal here (whether
        // synchronous or via `inner.remove`, which also drops the local copy)
        // would destroy a value recreated between that removal and this
        // broadcast. Record our own stamp first so a slow gossip echo of
        // THIS tombstone cannot clobber a follow-up SET we later accept
        // locally, then fan the tombstone out on the gossip tier only.
        let stamp = self.inner.cluster.stamp();
        if !apply_if_newer(&self.applied, &key, stamp, || {}) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        self.handle.spawn(async move {
            inner.gossip_tombstone(&key, stamp).await;
        });
    }

    fn replicate_expire(&self, key: &str, ttl: Duration) -> bool {
        // Update the local copy synchronously so this node's readers see
        // the new expiry immediately, then re-broadcast the (unchanged)
        // value with the new TTL through the gossip tier. Peers apply the
        // event by the same last-writer-wins rule the SET applier uses,
        // so a TTL refresh (extend) propagates just like a shorten — the
        // origin's newer stamp wins, and the peer takes whatever
        // expiry the origin last stamped. That's what session refresh
        // needs; a shorter-wins rule would break lazy_write.
        let w = self.inner.new_write(Some(ttl));
        // Record OUR own stamp and apply the new expiry in one step, so a
        // slower echo of the same expiry update from gossip (or a racing
        // newer write) cannot leave the local copy and the record at odds.
        let store = self.inner.local_store();
        let mut ok = false;
        let fresh = apply_if_newer(&self.applied, key, w.stamp, || {
            ok = store.expire_local(key, ttl);
        });
        if !fresh {
            // A newer write for this key already landed; it wins.
            return true;
        }
        if !ok {
            tracing::debug!(key, "expire: no local copy to re-broadcast (may exist only on peers)");
            return false;
        }
        // Read the current bytes so we can re-emit them with the new TTL.
        // `Store::get` returns Bytes cheaply (Arc-cloned).
        let Some(bytes) = store.get(key) else {
            // Racy edge: the entry expired between expire_local and get.
            // Nothing to broadcast, but the local update did succeed.
            return true;
        };

        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        let value = bytes.to_vec();
        let threshold = inner.config.small_key_threshold;
        self.handle.spawn(async move {
            // Small-key tier only: the gossip TTL update is what carries
            // the new expiry across the cluster. Large values live only on
            // their replica set; a TTL update for a large value updates
            // this node's copy (already done above) but does not fan out
            // to peer replicas — the roadmap's OP_EXPIRE data-plane frame
            // is out of scope for this change. Session blobs are almost
            // always well below `small_key_threshold`, so refresh works
            // for the primary consumer.
            if value.len() <= threshold {
                inner.cluster().gossip_set_stamped(&key_owned, &value, w.expiry_ms, w.stamp).await;
            }
        });
        true
    }
}

/// A [`Replicator`](ephpm_kv::store::Replicator) for **one vhost's** KV store
/// in multi-tenant clustered mode.
///
/// Installed on each per-site [`Store`] as it is created (see
/// `MultiTenantStore::set_replicator_factory`). It mirrors [`KvReplicator`]'s
/// semantics exactly, with one difference: everything it puts on the wire is
/// namespaced with this site's key (`site_namespace::encode`), and every local
/// materialization targets **this site's** store rather than the process-wide
/// one. That is what makes a vhost's writes replicate cluster-wide while
/// staying physically inside that vhost's keyspace on every node.
///
/// Isolation follows from two facts: the transport key names the site, and the
/// apply side routes strictly on that name — so a site's key can only ever land
/// in the same site's store on a peer, never in the global store and never in
/// another tenant's.
///
/// # Not implemented here: `replicate_broadcast` / `fetch_cluster`
///
/// This impl deliberately inherits the trait defaults for both, because the only
/// callers of [`Store::set_broadcast`](ephpm_kv::store::Store::set_broadcast)
/// and [`Store::get_cluster`](ephpm_kv::store::Store::get_cluster) are the ACME
/// certificate and account paths, which run against the **process-wide** store
/// (whose replicator is [`KvReplicator`], and which does override both). No
/// per-site store reaches them.
///
/// That is a fact about today's callers, not a property of this type, so it is
/// recorded rather than assumed. This replicator *does* shard large values
/// across a subset of the cluster (`set_for_site` → `set_large_routed`), which
/// is exactly the condition under which `replicate_broadcast`'s contract says an
/// implementation MUST override it. The inherited default would silently deliver
/// a cert-sized per-site value to `replication_factor` nodes instead of all of
/// them. **Before any per-site caller of `set_broadcast`/`get_cluster` is added,
/// both methods must be implemented here** over a site-namespaced transport key.
pub struct SiteKvReplicator {
    inner: Arc<ClusteredStore>,
    /// This vhost's own store — the local target for materialization, and the
    /// store whose watch slots must be bumped so `ephpm_kv_wait` sees
    /// replicated writes.
    local: Arc<Store>,
    /// The site key that namespaces this store's keys on the wire.
    site: Arc<str>,
    handle: tokio::runtime::Handle,
    /// Shared with the applier, keyed by **transport** key so two sites using
    /// the same bare key never share an ordering slot.
    applied: AppliedWriteMap,
}

impl std::fmt::Debug for SiteKvReplicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteKvReplicator").field("site", &self.site).finish_non_exhaustive()
    }
}

impl SiteKvReplicator {
    /// Wrap the node's [`ClusteredStore`] as the replicator for `site`'s store.
    ///
    /// `applied` MUST be the same map the applier uses, so origin writes and
    /// remote applies share one last-arrival-wins ordering.
    #[must_use]
    pub fn new(
        inner: Arc<ClusteredStore>,
        local: Arc<Store>,
        site: &str,
        handle: tokio::runtime::Handle,
        applied: AppliedWriteMap,
    ) -> Arc<Self> {
        Arc::new(Self { inner, local, site: Arc::from(site), handle, applied })
    }
}

impl ephpm_kv::store::Replicator for SiteKvReplicator {
    fn replicate_set(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        // Materialize into THIS SITE's store synchronously (mirrors the global
        // replicator): the gossip tier lives in chitchat state, invisible to
        // the raw-store readers this vhost's PHP and RESP clients use.
        let w = self.inner.new_write(ttl);
        if value.len() <= self.inner.config.small_key_threshold {
            // Keyed by the TRANSPORT key so site A's "foo" and site B's "foo"
            // occupy different ordering slots; record + local write are one
            // step (see `apply_if_newer`).
            let transport = crate::site_namespace::encode(&self.site, &key);
            let local = &self.local;
            let fresh = apply_if_newer(&self.applied, &transport, w.stamp, || {
                local.set_local(key.clone(), value.clone(), ttl);
            });
            if !fresh {
                return true;
            }
        }
        let inner = Arc::clone(&self.inner);
        let local = Arc::clone(&self.local);
        let site = Arc::clone(&self.site);
        self.handle.spawn(async move {
            let ok = inner.set_for_site(&site, &local, key.clone(), value, ttl, w).await;
            if !ok {
                tracing::warn!(
                    %site,
                    key,
                    "per-site KV: clustered set returned false — the write did not reach peers"
                );
            }
        });
        true
    }

    fn replicate_remove(&self, key: &str) -> bool {
        let stamp = self.inner.cluster.stamp();
        let transport = crate::site_namespace::encode(&self.site, key);
        let local = &self.local;
        let fresh = apply_if_newer(&self.applied, &transport, stamp, || {
            local.remove_local(key);
        });
        if !fresh {
            return true;
        }

        let inner = Arc::clone(&self.inner);
        let local = Arc::clone(&self.local);
        let site = Arc::clone(&self.site);
        let key = key.to_string();
        self.handle.spawn(async move {
            let _ = inner.remove_for_site(&site, &local, &key, stamp).await;
        });
        true
    }

    fn replicate_published(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) {
        // PUBLISH ONLY — mirrors the global replicator: the site store already
        // holds this value (applied under its shard lock), so writing it back
        // would risk regressing a newer concurrent increment on this node. Only
        // the cluster-wide fan-out is left to do.
        let w = self.inner.new_write(ttl);
        if value.len() <= self.inner.config.small_key_threshold
            && !apply_if_newer(
                &self.applied,
                &crate::site_namespace::encode(&self.site, &key),
                w.stamp,
                || {},
            )
        {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let local = Arc::clone(&self.local);
        let site = Arc::clone(&self.site);
        self.handle.spawn(async move {
            if !inner.publish_for_site(&site, &local, &key, &value, ttl, w).await {
                tracing::warn!(
                    %site,
                    key,
                    "per-site KV: clustered publish returned false — the value did not reach peers"
                );
            }
        });
    }

    fn replicate_remove_published(&self, key: String) {
        // PUBLISH ONLY — mirrors the global replicator: no local removal, so a
        // value recreated on this node between `del_if_eq`'s removal and this
        // broadcast survives. Record the stamp keyed by the TRANSPORT key,
        // then gossip the per-site tombstone only.
        let stamp = self.inner.cluster.stamp();
        if !apply_if_newer(
            &self.applied,
            &crate::site_namespace::encode(&self.site, &key),
            stamp,
            || {},
        ) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let site = Arc::clone(&self.site);
        self.handle.spawn(async move {
            inner.gossip_tombstone_for_site(&site, &key, stamp).await;
        });
    }

    fn replicate_expire(&self, key: &str, ttl: Duration) -> bool {
        // Same shape as the global path: update locally, then re-broadcast the
        // unchanged value with the new expiry so peers converge on it.
        let w = self.inner.new_write(Some(ttl));
        let transport = crate::site_namespace::encode(&self.site, key);
        let local = &self.local;
        let mut ok = false;
        let fresh = apply_if_newer(&self.applied, &transport, w.stamp, || {
            ok = local.expire_local(key, ttl);
        });
        if !fresh {
            return true;
        }
        if !ok {
            return false;
        }
        let Some(bytes) = self.local.get(key) else {
            // Raced with expiry between the update and the read — the local
            // update did happen, there is just nothing left to broadcast.
            return true;
        };

        let inner = Arc::clone(&self.inner);
        let site = Arc::clone(&self.site);
        let key = key.to_string();
        let value = bytes.to_vec();
        self.handle.spawn(async move {
            inner.expire_for_site(&site, &key, value, w).await;
        });
        true
    }

    fn route_set_nx(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Routed<SetNxOutcome> {
        // Route on the TRANSPORT key: it is both what picks the owner (so two
        // sites' same-named locks are arbitrated independently) and what the
        // owner's data plane decodes to land the decision in this site's store.
        let transport = crate::site_namespace::encode(&self.site, key);
        block_on_cluster(&self.handle, self.inner.route_set_nx(&transport, value, ttl))
            .unwrap_or(Routed::OwnerUnreachable)
    }

    fn route_del_if_eq(&self, key: &str, token: &[u8]) -> Routed<bool> {
        let transport = crate::site_namespace::encode(&self.site, key);
        block_on_cluster(&self.handle, self.inner.route_del_if_eq(&transport, token))
            .unwrap_or(Routed::OwnerUnreachable)
    }
}

/// Per-key last-applied [`Stamp`] shared between the gossip applier and
/// the origin-side [`KvReplicator::replicate_set`]. Keeps the local
/// materialized copy last-writer-wins: a slow gossip echo of an older
/// write can no longer clobber a newer write we already applied
/// locally, and two nodes writing one key in the same millisecond settle
/// on the same winner everywhere (the stamp's node tag breaks the tie).
///
/// v1 legacy entries decode as `write = 0` and always apply on first
/// arrival (the map has no entry yet); a second stale echo with the same
/// stamp is skipped, which is the correct behavior.
pub type AppliedWriteMap = Arc<DashMap<String, Stamp>>;

/// Construct a fresh [`AppliedWriteMap`] for use by both the applier and
/// [`KvReplicator`]. Callers wire the SAME handle through both so origin
/// writes and remote applies participate in the same ordering.
#[must_use]
pub fn new_applied_write_map() -> AppliedWriteMap {
    Arc::new(DashMap::new())
}

/// Materialize gossip-tier small values into the local raw [`Store`].
///
/// The gossip tier lives in chitchat node state — invisible to the raw
/// `Store` that the RESP dispatcher, PHP native functions, and the OPcache
/// watcher read. This applier subscribes to gossip KV changes and writes
/// every REMOTE small-value update into the local store via `set_local`,
/// making gossip a replication transport with per-node materialization.
/// Origin nodes materialize synchronously in
/// [`KvReplicator::replicate_set`]; their own events are skipped here.
///
/// **Last-writer-wins ordering.** The applier keeps a per-key
/// last-applied [`Stamp`] in `applied` and skips any incoming event whose
/// stamp is not strictly greater. The origin also records its own stamp
/// in the same map — the very stamp it put on the wire — so a delayed echo
/// of the origin's own write cannot clobber a newer local overwrite, and
/// two nodes writing one key in the same millisecond settle on the same
/// winner on every node (the stamp's node tag breaks the tie). Pre-stamp
/// peers send `write = 0`, which always applies on first sight.
///
/// **Tombstones.** A remote delete rides the same subscription as a
/// tombstone-marker payload (see [`crate::gossip_kv`]). When the
/// tombstone's stamp beats the last-applied write for its key, the
/// applier calls `remove_local` — dropping both the gossip-materialized
/// copy and any locally-held data-plane replica of the same key. That
/// unifies delete propagation for the two tiers without a separate
/// data-plane OP_DELETE frame: every node subscribes to gossip and
/// removes on tombstone regardless of which tier originally held the
/// value.
///
/// **TTL updates.** An EXPIRE on the origin re-emits the current value
/// with the new `expiry_ms` and a fresh stamp. Peers apply the
/// event verbatim by the same last-writer-wins rule — so a session TTL
/// refresh (which *extends* the expiry) replicates correctly, not just a
/// shortening. Write-time ordering already serializes intent; the peer
/// takes whatever expiry the origin last stamped.
pub async fn start_gossip_applier(
    cluster: &ClusterHandle,
    store: Arc<Store>,
    applied: AppliedWriteMap,
) {
    start_gossip_applier_multi_tenant(cluster, store, None, applied).await;
}

/// [`start_gossip_applier`] with per-vhost routing.
///
/// When `multi_tenant` is `Some`, an incoming key that carries the per-site
/// envelope (see [`crate::site_namespace`]) is applied into **that site's**
/// store under the bare key; every other key is applied into the global store
/// exactly as before. That is the receive half of per-vhost KV replication: a
/// tenant's write made on any node materializes into the same tenant's
/// keyspace on every other node — and, because it lands via `set_local`, bumps
/// that key's watch slot so `ephpm_kv_wait` wakes for replicated writes too.
///
/// A site the receiving node has never served yet is **created on arrival**
/// (`get_site_store` is get-or-create), so a peer's write does not have to wait
/// for a local request to that vhost. The store is created through the
/// multi-tenant handle, so it picks up the same replicator factory and config a
/// locally-created one would.
///
/// Passing `None` reproduces the previous single-keyspace behaviour byte for
/// byte — which is what a non-multi-tenant node uses.
pub async fn start_gossip_applier_multi_tenant(
    cluster: &ClusterHandle,
    store: Arc<Store>,
    multi_tenant: Option<ephpm_kv::multi_tenant::MultiTenantStore>,
    applied: AppliedWriteMap,
) {
    let self_id = cluster.self_node().id.clone();
    cluster
        .subscribe_kv_changes(move |event| {
            if event.node().node_id == self_id {
                return;
            }
            let transport_key = event.key();
            let stamp = event.stamp();

            // Route: per-site envelope → that site's store; anything else →
            // the global store. An envelope arriving on a node that is not
            // multi-tenant has nowhere to go, so it is dropped with a warning
            // rather than being flattened into the global keyspace.
            let (target, key, site) =
                match (&multi_tenant, crate::site_namespace::decode(transport_key)) {
                    // Enveloped key on a multi-tenant node → that vhost's store.
                    (Some(mt), Some((site, key))) => (mt.get_site_store(site), key, Some(site)),
                    // Enveloped key with nowhere to go: drop it loudly rather
                    // than flattening a tenant's key into the global keyspace.
                    (None, Some(_)) => {
                        tracing::warn!(
                            key = transport_key,
                            from = %event.node().node_id,
                            "received a per-site KV replication event but this node is not \
                             multi-tenant ([server] sites_dir unset); dropping it rather than \
                             flattening a tenant's key into the global keyspace"
                        );
                        return;
                    }
                    // Leading SEP but undecodable: a malformed or hostile
                    // envelope. Fail CLOSED — flattening it into the shared
                    // global keyspace would take a key that claims to be a
                    // tenant's and make it visible to every tenant.
                    (_, None) if crate::site_namespace::is_enveloped(transport_key) => {
                        tracing::warn!(
                            key = transport_key,
                            from = %event.node().node_id,
                            "received a KV replication event whose per-site envelope does not \
                             decode to a valid site key; dropping it rather than flattening it \
                             into the global keyspace"
                        );
                        return;
                    }
                    // Plain global key — unchanged behaviour.
                    (_, None) => (Arc::clone(&store), transport_key, None),
                };

            // Ordering is tracked per TRANSPORT key, so two sites sharing a
            // bare key name never contend for one slot. The record and the
            // local write are one step under that slot (`apply_if_newer`).
            let is_set = matches!(event, crate::gossip_kv::KvChangeEvent::Set { .. });
            let fresh = apply_if_newer(&applied, transport_key, stamp, || match event {
                crate::gossip_kv::KvChangeEvent::Set { value, ttl, .. } => {
                    target.set_local(key.to_string(), value.to_vec(), ttl);
                }
                crate::gossip_kv::KvChangeEvent::Tombstone { .. } => {
                    target.remove_local(key);
                }
            });
            if fresh {
                tracing::trace!(
                    key,
                    site,
                    from = %event.node().node_id,
                    ?stamp,
                    is_set,
                    "gossip KV change materialized locally"
                );
            } else {
                tracing::trace!(
                    key = transport_key,
                    from = %event.node().node_id,
                    ?stamp,
                    "gossip KV change skipped as stale"
                );
            }
        })
        .await;
}

/// Pure last-writer-wins gate for the gossip applier.
///
/// Returns `true` (and records `stamp`) when this incoming write is
/// strictly newer than the last one we materialized for `key`; returns
/// `false` when it is stale (stamp <= last-applied) so a slow echo
/// cannot clobber a newer write we already accepted. Because a [`Stamp`]
/// carries the origin's node tag, two different nodes' writes are never
/// equal: a same-millisecond tie is decided by the tag, identically on
/// every node, instead of being treated as stale on both. Legacy v1
/// payloads (`write = 0`) apply on first sight — the map has no entry
/// yet — and subsequent v1 echoes for the same key are skipped, which is
/// the desired behavior.
///
/// The check and the record are one atomic step on the key's map entry, so
/// two events for one key racing through here cannot both pass. Production
/// paths use [`apply_if_newer`] directly so the local write happens under
/// the same step; this is the record-only form the ordering tests exercise.
#[cfg(test)]
fn should_apply(applied: &AppliedWriteMap, key: &str, stamp: Stamp) -> bool {
    apply_if_newer(applied, key, stamp, || {})
}

/// The one way a value lands on this node: compare `stamp` against the last
/// applied write for `key`, and — while still holding that key's slot in the
/// applied map — record it and run `write` (the local `set_local` /
/// `remove_local` / `expire_local`). Returns whether `write` ran.
///
/// Every path that materializes a value locally goes through here: the
/// replication hooks for this node's own writes, the gossip applier for
/// peers' writes, and the verify pass. Holding the slot across the local
/// write is what makes the order total: two concurrent hooks for one key
/// take their stamps in one order but would otherwise reach the store in
/// another, and a hook racing the applier for the same key could leave the
/// local copy holding the older value while the applied map (and the
/// cluster) agree on the newer — a divergence no later event repairs. The
/// local write is a few microseconds under a per-shard lock of the applied
/// map, taken before (never after) any store lock, so the lock order is the
/// same everywhere.
pub(crate) fn apply_if_newer(
    applied: &AppliedWriteMap,
    key: &str,
    stamp: Stamp,
    write: impl FnOnce(),
) -> bool {
    let guard = match applied.entry(key.to_string()) {
        dashmap::Entry::Occupied(mut prev) => {
            if stamp <= *prev.get() {
                return false;
            }
            *prev.get_mut() = stamp;
            prev.into_ref()
        }
        dashmap::Entry::Vacant(slot) => slot.insert(stamp),
    };
    // `guard` keeps the key's slot locked while the local write happens.
    write();
    drop(guard);
    true
}

/// FNV-1a-style hash for key → u64 mapping.
fn hash_key(key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// Whether a large-value fan-out may (re)write this node's own copy.
///
/// A `set` must: the value has to land somewhere locally. A **publish** must
/// not: the read-modify-write op that produced the value already wrote it under
/// the key's shard lock, and publishes are spawned, so rewriting from one could
/// overwrite a newer concurrent mutation on this node — the lost-update bug the
/// publish-only path exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalCopy {
    /// Write the value into the local target store.
    Write,
    /// Leave the local store alone; only fan out to peers.
    Skip,
}

impl LocalCopy {
    /// Perform (or skip) the local write, reporting success either way — a
    /// skipped write is not a failure.
    fn write(self, local: &Store, key: &str, value: &[u8], ttl: Option<Duration>) -> bool {
        match self {
            Self::Write => local.set_local(key.to_string(), value.to_vec(), ttl),
            Self::Skip => true,
        }
    }
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

/// Derive a node's TCP data plane address: its gossip IP with the data port
/// it advertises via gossip, or — for a node that advertises none (one
/// predating the advertisement) — this node's own configured `data_port`,
/// which is what every node assumed before. Returns `None` if the gossip
/// address does not parse as a socket address.
fn node_data_addr(node: &NodeInfo, local_data_port: u16) -> Option<SocketAddr> {
    node.gossip_addr
        .parse::<SocketAddr>()
        .ok()
        .map(|gossip| SocketAddr::new(gossip.ip(), node.data_port.unwrap_or(local_data_port)))
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

    /// A PHP thread routing a conditional write while the runtime is shutting
    /// down must fail closed, not panic: the caller is an `extern "C"` bridge
    /// fn, where an unwinding panic aborts the process.
    #[test]
    fn block_on_cluster_after_runtime_shutdown_fails_closed() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        drop(rt);
        let out = std::thread::spawn(move || {
            // Lazily built like the real `route_*` async fns, so the timer is
            // touched only once polled on the (shut-down) runtime.
            block_on_cluster(&handle, async {
                tokio::time::timeout(Duration::from_millis(50), std::future::pending::<()>()).await
            })
        })
        .join()
        .expect("block_on_cluster must not panic out of the calling thread");
        assert!(out.is_none(), "a shut-down runtime is treated as unreachable");
    }

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

    // ── hot cache memory accounting ──────────────────────────────────

    fn hot_entry(len: usize) -> CachedValue {
        CachedValue {
            data: vec![0u8; len],
            cached_at: Instant::now(),
            version: 1,
            key: "hot:key".to_string(),
        }
    }

    /// Two readers racing on the same expired entry both used to
    /// `fetch_sub` its footprint. `hot_cache_mem` is a `u64`, so the second
    /// subtraction wrapped it to ~`u64::MAX`, the budget check in
    /// `maybe_promote_hot_key` was over forever after, and hot-key promotion
    /// went silently dead for the rest of the process's life.
    #[test]
    fn concurrent_eviction_debits_the_counter_exactly_once() {
        let cache: DashMap<u64, CachedValue> = DashMap::new();
        let mem = AtomicU64::new(0);

        let entry = hot_entry(1024);
        let bytes = hot_entry_bytes(&entry);
        cache.insert(7, entry);
        mem.store(bytes, Ordering::Relaxed);

        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| evict_hot_entry(&cache, &mem, 7));
            }
        });

        assert!(cache.is_empty(), "the entry must be gone");
        assert_eq!(
            mem.load(Ordering::Relaxed),
            0,
            "exactly one caller may debit the counter; a second subtraction wraps the u64"
        );
    }

    #[test]
    fn evicting_a_missing_key_leaves_the_counter_alone() {
        let cache: DashMap<u64, CachedValue> = DashMap::new();
        let mem = AtomicU64::new(4096);
        evict_hot_entry(&cache, &mem, 99);
        assert_eq!(mem.load(Ordering::Relaxed), 4096);
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
            .map(|i| {
                NodeInfo::new(
                    format!("n{i}"),
                    format!("127.0.0.1:{}", 7946 + i),
                    crate::NodeState::Alive,
                )
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

    /// A peer that advertises no data port (an older node) is dialled on
    /// this node's own `data_port`, exactly as before the advertisement.
    #[test]
    fn node_data_addr_falls_back_to_local_port() {
        let node = NodeInfo::new("n0", "10.0.0.5:7946", crate::NodeState::Alive);
        let addr = node_data_addr(&node, 7947).unwrap();
        assert_eq!(addr.to_string(), "10.0.0.5:7947");
    }

    /// A peer that advertises its data port is dialled there, not on this
    /// node's port — heterogeneous `data_port`s used to send "remote" writes
    /// to the wrong listener.
    #[test]
    fn node_data_addr_prefers_advertised_port() {
        let mut node = NodeInfo::new("n0", "10.0.0.5:7946", crate::NodeState::Alive);
        node.data_port = Some(19402);
        let addr = node_data_addr(&node, 7947).unwrap();
        assert_eq!(addr.to_string(), "10.0.0.5:19402");
    }

    #[test]
    fn node_data_addr_rejects_unparseable_gossip_addr() {
        let node = NodeInfo::new("n0", "not-an-addr", crate::NodeState::Alive);
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

    // -----------------------------------------------------------------
    // Last-arrival-wins ordering (issue #150.1)
    // -----------------------------------------------------------------

    fn at(write: u64, node: &str) -> Stamp {
        Stamp::new(write, node)
    }

    #[test]
    fn should_apply_first_write_lands() {
        let applied = new_applied_write_map();
        assert!(should_apply(&applied, "k", at(1000, "a")));
        assert_eq!(*applied.get("k").unwrap(), at(1000, "a"));
    }

    #[test]
    fn should_apply_newer_write_lands() {
        let applied = new_applied_write_map();
        assert!(should_apply(&applied, "k", at(1000, "a")));
        assert!(should_apply(&applied, "k", at(2000, "b")));
        assert_eq!(*applied.get("k").unwrap(), at(2000, "b"));
    }

    #[test]
    fn should_apply_stale_write_is_skipped() {
        let applied = new_applied_write_map();
        assert!(should_apply(&applied, "k", at(2000, "a")));
        // Delayed echo of an older write must NOT clobber the newer one.
        assert!(!should_apply(&applied, "k", at(1000, "b")));
        // The identical stamp (an echo of the same write) is stale.
        assert!(!should_apply(&applied, "k", at(2000, "a")));
        assert_eq!(*applied.get("k").unwrap(), at(2000, "a"));
    }

    /// Two nodes writing one key in the same millisecond: the node tag
    /// decides, the same way on both nodes, so they converge instead of
    /// each treating the other's write as "stale" (F-7).
    #[test]
    fn should_apply_breaks_same_millisecond_ties_by_node_tag() {
        let a = at(7000, "node-a");
        let b = at(7000, "node-b");
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };

        // Node that wrote `lo` first, then sees `hi`: applies it.
        let applied = new_applied_write_map();
        applied.insert("k".into(), lo);
        assert!(should_apply(&applied, "k", hi));
        assert_eq!(*applied.get("k").unwrap(), hi);

        // Node that wrote `hi` first, then sees `lo`: keeps its own.
        let applied = new_applied_write_map();
        applied.insert("k".into(), hi);
        assert!(!should_apply(&applied, "k", lo));
        assert_eq!(*applied.get("k").unwrap(), hi);
    }

    #[test]
    fn should_apply_legacy_zero_write_lands_once() {
        // Pre-stamp peers emit write = 0. The first echo applies
        // (map has no entry), the second is treated as stale — matches
        // the desired "apply on first sight, deduplicate re-sends"
        // behavior during rolling upgrades.
        let applied = new_applied_write_map();
        assert!(should_apply(&applied, "legacy-key", at(0, "old")));
        assert!(!should_apply(&applied, "legacy-key", at(0, "old")));
    }

    #[test]
    fn should_apply_origin_recorded_write_blocks_stale_echo() {
        // Simulates KvReplicator::replicate_set recording its own write
        // in the applied map, then a slow echo of the SAME write coming
        // back through gossip. The echo must be skipped.
        let applied = new_applied_write_map();
        let origin = at(5000, "self");
        applied.insert("origin-key".into(), origin);
        assert!(!should_apply(&applied, "origin-key", origin));
        // A newer overwrite (say a follow-up SET) should still apply.
        assert!(should_apply(&applied, "origin-key", at(5001, "self")));
    }

    /// The hook's stamp and the gossip payload's stamp are one and the same
    /// object, so what the origin records is exactly what peers compare
    /// against.
    #[test]
    fn write_stamp_fixes_expiry_and_stamp_together() {
        let clock = crate::gossip_kv::StampClock::new("n");
        let w = WriteStamp {
            stamp: clock.next(),
            expiry_ms: crate::gossip_kv::expiry_ms_for(Some(Duration::from_secs(30))),
        };
        assert!(w.expiry_ms > 0);
        let later = WriteStamp { stamp: clock.next(), expiry_ms: 0 };
        assert!(later.stamp > w.stamp);
    }
}

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

use dashmap::{DashMap, DashSet};
use ephpm_config::ClusterKvConfig;
use ephpm_kv::store::{
    ColdRead, HashFieldState, Routed, SetIfEqOutcome, SetNxOutcome, SetTtl, Store,
};

use crate::ClusterHandle;
use crate::gossip_kv::{Stamp, expiry_ms_for};
use crate::kv_data_plane::{DataPlaneClient, StampedValue};
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

/// Total budget for a read-through fetch of a large value from its replica
/// set on behalf of a reader that blocks a request thread
/// ([`ClusteredStore::fetch_through`]), and for one cold read
/// ([`ClusteredStore::cold_fetch`]).
pub const FETCH_THROUGH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long after start the catch-up gate waits for peers to show up before
/// it can conclude there are none: the failure detector needs two heartbeats
/// (~2 s) to call a peer alive, and a node with no alive peer after this is
/// the first node of its cluster (or a standalone one) with nothing to catch
/// up on.
pub const CATCH_UP_JOIN_WAIT: Duration = Duration::from_secs(5);

/// How often the catch-up gate re-reads membership and the peers' marks.
const CATCH_UP_POLL: Duration = Duration::from_millis(250);

/// How often the gate logs that it is still waiting.
const CATCH_UP_WARN_EVERY: Duration = Duration::from_secs(60);

/// The longest the gate stays cold. Past this it opens with a `WARN` and
/// the node serves its own store even though gossip never proved it caught
/// up — the state it would have been in from the first second without the
/// gate. This bounds a stuck catch-up (a reset storm: a peer's state too big
/// to stream within chitchat's 60 s tombstone grace, see #631) and any bug
/// in the gate itself to a latency cost, never a permanent one.
pub const CATCH_UP_CAP: Duration = Duration::from_secs(15 * 60);

/// Gauge: `1` while this node serves small-tier reads through its peers
/// because its gossip catch-up is not complete, `0` otherwise.
pub const METRIC_CATCH_UP_COLD: &str = "ephpm_kv_catch_up_cold";

/// Counter of reads answered through peers while cold, by `outcome`
/// (`found`, `absent`, `unavailable`).
pub const METRIC_COLD_READS: &str = "ephpm_kv_cold_reads_total";

/// What [`ClusteredStore::cold_fetch`] found across the alive peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdFetch {
    /// The newest copy any peer holds (by stamp; an unstamped copy loses to
    /// a stamped one).
    Found(Vec<u8>),
    /// At least one peer answered, and none holds the key.
    Absent,
    /// Every peer failed or timed out.
    Unavailable,
    /// No alive peer to ask.
    NoPeers,
}

/// Counters of the catch-up gate ([`ClusteredStore::catch_up_stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUpStats {
    /// Whether reads are currently served through peers.
    pub cold: bool,
    /// Cold reads that a peer answered with a value.
    pub found: u64,
    /// Cold reads no peer had the key for.
    pub absent: u64,
    /// Cold reads no peer could be reached for (served as a miss).
    pub unavailable: u64,
}

/// The catch-up gate's shared state: owned by the [`ClusteredStore`] and the
/// gate task, which holds nothing else of the store so a handle shutdown is
/// unaffected (the task exits when the gate opens).
struct CatchUpGate {
    /// `true` from [`ClusteredStore::init_catch_up_gate`] until the gate
    /// opens. The one load on every `get_through`.
    cold: AtomicBool,
    /// Set by the first `init_catch_up_gate`; later calls are no-ops.
    armed: AtomicBool,
    /// When the gate was armed: until [`CATCH_UP_JOIN_WAIT`] has passed, "no
    /// alive peer" means "none detected yet", not "none exists".
    armed_at: std::sync::OnceLock<Instant>,
    /// Alive peers (this node excluded), refreshed each poll while cold, so
    /// a cold read never takes the chitchat lock.
    peers: parking_lot::RwLock<Vec<NodeInfo>>,
    found: AtomicU64,
    absent: AtomicU64,
    unavailable: AtomicU64,
}

impl CatchUpGate {
    fn new() -> Self {
        Self {
            cold: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            armed_at: std::sync::OnceLock::new(),
            peers: parking_lot::RwLock::new(Vec::new()),
            found: AtomicU64::new(0),
            absent: AtomicU64::new(0),
            unavailable: AtomicU64::new(0),
        }
    }

    fn open(&self) {
        self.cold.store(false, Ordering::SeqCst);
        metrics::gauge!(METRIC_CATCH_UP_COLD).set(0.0);
    }

    /// Whether the gate is still inside its join wait, when an empty peer
    /// list says nothing about whether peers exist.
    fn within_join_wait(&self) -> bool {
        self.armed_at.get().is_some_and(|t| t.elapsed() < CATCH_UP_JOIN_WAIT)
    }
}

/// Large-value writes waiting for a data-plane worker before new ones are
/// shed. Each entry holds one value copy; with the default 512-byte gossip
/// threshold and typical cache values this is tens of megabytes at most.
pub const LARGE_QUEUE_CAP: usize = 4_096;

/// Data-plane workers draining the large-write queue: the bound on
/// concurrent replica writes a node makes (times the replica set size).
pub const LARGE_WORKERS: usize = 16;

/// One large-value write queued for the data-plane workers.
///
/// Two stamps travel with it: `w.stamp` on the value itself (every replica
/// applies the value only if it is strictly newer than what it holds), and
/// `marker`, stamped strictly *before* it, which is queued as a gossip
/// tombstone so every node that is **not** a replica drops whatever older
/// copy it held — a previous small value (stress finding F-10), or an
/// earlier large write it originated — and fetches through to the owners on
/// its next read. A replica orders the marker below the value, so it keeps
/// the value whichever of the two arrives first.
struct LargeWrite {
    transport_key: String,
    value: Vec<u8>,
    ttl: Option<Duration>,
    w: WriteStamp,
    marker: Option<Stamp>,
    scope: FanOut,
}

/// Which nodes a large value is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FanOut {
    /// The key's replica set (`replication_factor` nodes).
    ReplicaSet,
    /// Every alive node ([`ClusteredStore::broadcast`]).
    Everyone,
}

/// Outcome of one large-value fan-out.
#[derive(Debug, Default, Clone, Copy)]
struct Delivery {
    delivered: usize,
    failed: usize,
}

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
    /// The bounded queue feeding the data-plane workers; started on first
    /// use by a replication hook (see [`ClusteredStore::enqueue_large`]).
    large_tx: std::sync::OnceLock<tokio::sync::mpsc::Sender<LargeWrite>>,
    /// Large writes shed because that queue was full.
    large_shed: AtomicU64,
    /// The catch-up gate (see [`ClusteredStore::init_catch_up_gate`]).
    catch_up: Arc<CatchUpGate>,
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
            large_tx: std::sync::OnceLock::new(),
            large_shed: AtomicU64::new(0),
            catch_up: Arc::new(CatchUpGate::new()),
        })
    }

    /// Arm the catch-up gate: serve `get_through` reads from peers until this
    /// node's gossip catch-up is complete.
    ///
    /// A node that (re)joins a cluster streams each peer's gossip state per
    /// peer, in version order, one MTU per gossip exchange — tens of seconds
    /// to minutes for a large state. Everything it materializes meanwhile is
    /// "current as of that peer's cursor": a copy of a key that another peer
    /// has already superseded can land first and be served as a local hit for
    /// as long as the newer version sits further up some peer's backlog. The
    /// live lab measured that as stale reads on a restarted node for minutes
    /// after every restart (#631).
    ///
    /// While the gate is **cold**, [`Replicator::cold_read`] answers each
    /// read by asking every alive peer for its copy *and its stamp*
    /// ([`ClusteredStore::cold_fetch`]) and serving the newest — the
    /// freshness of the best-informed peer, one data-plane round trip per
    /// read. Keys this node wrote itself since it started are served locally
    /// (read-your-writes). The gate opens when every alive peer's post-join
    /// catch-up mark has arrived ([`crate::node::KV_MARK_KEY`]), i.e. the
    /// local store holds every entry each peer had, or after
    /// [`CATCH_UP_CAP`]. A node with no alive peer after
    /// [`CATCH_UP_JOIN_WAIT`] opens at once. Idempotent; called once at
    /// startup by the server before it serves requests.
    ///
    /// [`Replicator::cold_read`]: ephpm_kv::store::Replicator::cold_read
    pub async fn init_catch_up_gate(self: &Arc<Self>) {
        if self.catch_up.armed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.catch_up.armed_at.set(Instant::now());
        self.catch_up.cold.store(true, Ordering::SeqCst);
        metrics::gauge!(METRIC_CATCH_UP_COLD).set(1.0);
        tracing::info!(
            "gossip KV catch-up gate armed: reads are served through peers until every \
             peer's state has arrived"
        );
        tokio::spawn(run_catch_up_gate(
            Arc::clone(&self.catch_up),
            self.cluster.handle.chitchat(),
            self.cluster.self_node().id,
            self.cluster.started_ms(),
        ));
    }

    /// Whether the catch-up gate is cold (reads are served through peers).
    #[must_use]
    pub fn is_cold(&self) -> bool {
        self.catch_up.cold.load(Ordering::Relaxed)
    }

    /// Counters of the catch-up gate.
    #[must_use]
    pub fn catch_up_stats(&self) -> CatchUpStats {
        CatchUpStats {
            cold: self.is_cold(),
            found: self.catch_up.found.load(Ordering::Relaxed),
            absent: self.catch_up.absent.load(Ordering::Relaxed),
            unavailable: self.catch_up.unavailable.load(Ordering::Relaxed),
        }
    }

    /// Whether `transport_key` was last written **by this node, in this
    /// incarnation** according to `applied`: a cold read serves such a key
    /// locally (read-your-writes), every other key through peers.
    ///
    /// The applied map is per process, so an entry carrying this node's tag is
    /// one of this process's writes; the stamp time is checked too so a
    /// transport that re-delivers an earlier incarnation's writes under their
    /// original stamps could never pass as one of ours.
    fn written_here_since_start(&self, applied: &AppliedWriteMap, transport_key: &str) -> bool {
        applied.get(transport_key).is_some_and(|stamp| {
            stamp.node_tag == self.cluster.node_tag()
                && stamp.epoch_ms() >= self.cluster.started_ms()
        })
    }

    /// The decision behind [`Replicator::cold_read`] for a transport key:
    /// `Warm` when the gate is open or the key is this node's own write, else
    /// the peers' answer.
    ///
    /// [`Replicator::cold_read`]: ephpm_kv::store::Replicator::cold_read
    fn cold_read(
        &self,
        handle: &tokio::runtime::Handle,
        applied: &AppliedWriteMap,
        transport_key: &str,
    ) -> ColdRead {
        if !self.is_cold() || self.written_here_since_start(applied, transport_key) {
            return ColdRead::Warm;
        }
        match block_on_cluster(handle, self.cold_fetch(transport_key)) {
            Some(ColdFetch::Found(value)) => ColdRead::Found(value),
            Some(ColdFetch::Absent | ColdFetch::Unavailable) => ColdRead::Absent,
            // No alive peer yet, inside the join wait: the failure detector
            // takes a couple of heartbeats to call a peer alive, and gossip
            // has usually delivered a first batch of entries by then — the
            // very copies this gate exists not to serve. A miss, not a hit.
            Some(ColdFetch::NoPeers) if self.catch_up.within_join_wait() => {
                self.catch_up.absent.fetch_add(1, Ordering::Relaxed);
                metrics::counter!(METRIC_COLD_READS, "outcome" => "absent").increment(1);
                ColdRead::Absent
            }
            // Nobody to ask after the join wait (the gate is about to open),
            // or no runtime to ask from: the local store is the best this
            // node has.
            Some(ColdFetch::NoPeers) | None => ColdRead::Warm,
        }
    }

    /// Ask every alive peer for `transport_key` with the stamp it holds, and
    /// keep the newest.
    ///
    /// Each peer is asked within [`FETCH_THROUGH_TIMEOUT`], all concurrently,
    /// so the call costs one round trip. A peer predating the stamped read is
    /// asked again with a plain GET (its copy then counts as unstamped, which
    /// loses to any stamped copy). The result is never cached: it carries no
    /// TTL, and the gossip applier is materializing the same keys meanwhile.
    pub async fn cold_fetch(&self, transport_key: &str) -> ColdFetch {
        let peers = self.catch_up.peers.read().clone();
        // Only peers whose data port has arrived in gossip: `node_data_addr`
        // falls back to this node's own port for a peer without one, which on
        // a cold node is a round trip to its own empty store.
        let targets: Vec<(String, SocketAddr)> = peers
            .iter()
            .filter(|n| n.data_port.is_some())
            .filter_map(|n| node_data_addr(n, self.config.data_port).map(|a| (n.id.clone(), a)))
            .collect();
        if targets.is_empty() {
            return ColdFetch::NoPeers;
        }
        let asks = targets.iter().map(|(id, addr)| async move {
            let answer = tokio::time::timeout(FETCH_THROUGH_TIMEOUT, async {
                match self.client.fetch_stamped(*addr, transport_key).await {
                    Ok(found) => Ok(found),
                    Err(e) => {
                        tracing::debug!(key = transport_key, peer = %id, %e, "stamped read failed; plain GET");
                        self.client
                            .fetch(*addr, transport_key)
                            .await
                            .map(|v| v.map(|value| StampedValue { value, stamp: None }))
                    }
                }
            })
            .await;
            match answer {
                Ok(Ok(found)) => Some(found),
                Ok(Err(e)) => {
                    tracing::debug!(key = transport_key, peer = %id, %e, "cold read failed");
                    None
                }
                Err(_) => {
                    tracing::debug!(key = transport_key, peer = %id, "cold read timed out");
                    None
                }
            }
        });
        let answers = futures::future::join_all(asks).await;
        let answered = answers.iter().filter(|a| a.is_some()).count();
        let best = answers.into_iter().flatten().flatten().max_by(|a, b| a.stamp.cmp(&b.stamp));
        let outcome = match best {
            Some(found) => {
                self.catch_up.found.fetch_add(1, Ordering::Relaxed);
                metrics::counter!(METRIC_COLD_READS, "outcome" => "found").increment(1);
                return ColdFetch::Found(found.value);
            }
            None if answered > 0 => {
                self.catch_up.absent.fetch_add(1, Ordering::Relaxed);
                ColdFetch::Absent
            }
            None => {
                self.catch_up.unavailable.fetch_add(1, Ordering::Relaxed);
                ColdFetch::Unavailable
            }
        };
        let label = if outcome == ColdFetch::Absent { "absent" } else { "unavailable" };
        metrics::counter!(METRIC_COLD_READS, "outcome" => label).increment(1);
        outcome
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
    /// store.
    #[must_use]
    pub fn new_write(&self, ttl: Option<Duration>) -> WriteStamp {
        WriteStamp { stamp: self.cluster.stamp(), expiry_ms: expiry_ms_for(ttl) }
    }

    /// Fix the stamps for a **large** write: the gossip marker's stamp and,
    /// strictly after it, the value's. See [`LargeWrite`] for why there are
    /// two.
    #[must_use]
    pub fn new_large_write(&self, ttl: Option<Duration>) -> (Stamp, WriteStamp) {
        let marker = self.cluster.stamp();
        (marker, self.new_write(ttl))
    }

    /// Set a value, routing through the appropriate tier.
    ///
    /// Returns whether this node's own copy was written. Small values are
    /// queued for gossip; large values are written locally and then fanned
    /// out to the key's replica set over the data plane (awaited).
    ///
    /// # Large-value replication
    ///
    /// A large value is written to this node's local store **always** —
    /// whether or not this node is in the key's replica set — so the writing
    /// node reads its own write back, and to the **replica set**: the primary
    /// owner (`hash(key) % alive_nodes`) plus the next `replication_factor - 1`
    /// distinct nodes on the sorted alive-node ring (wrapping around; clamped
    /// to the number of alive nodes). Each copy carries the value's TTL and
    /// stamp. A gossip tombstone stamped just *before* the value accompanies
    /// every large write, so a node outside the replica set that still holds
    /// an older copy (a previous small value, or an earlier large write it
    /// originated) drops it and fetches through to the owners on its next
    /// read; replicas order the marker below the value they received. This
    /// is what fixed stress findings F-8 (large values unreadable on a third
    /// of the nodes), F-9 (replica copies never expired) and F-10 (a stale
    /// small copy served forever after a small→large overwrite).
    ///
    /// A replica that is down or rejects the write is logged and counted
    /// ([`ClusteredStore::replica_write_failures`]) but does **not** fail the
    /// set — this is not a quorum/consensus protocol, and no rollback is
    /// performed. The replication hooks decide whether a client waits for the
    /// fan-out ([`ReplicationMode`]); this method always awaits it.
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
            let w = self.new_write(ttl);
            // Gossip tier: replicate to all nodes.
            self.cluster.gossip_set_queued(&key, &value, w.expiry_ms, w.stamp);

            // Also bump the hot key version if this key was hot, so
            // remote caches invalidate.
            if self.config.hot_key_cache {
                self.maybe_bump_hot_version(&key).await;
            }

            return true;
        }

        let (marker, w) = self.new_large_write(ttl);
        let local_ok = self.store.set_local(key.clone(), value.clone(), ttl);
        self.fan_out_large(&key, &value, ttl, w, Some(marker), FanOut::ReplicaSet).await;

        // Bump hot key version for remote invalidation.
        if local_ok && self.config.hot_key_cache {
            self.maybe_bump_hot_version(&key).await;
        }

        local_ok
    }

    /// Fan out an **already-applied local value** to the rest of the cluster.
    ///
    /// Same tier routing as [`set`](Self::set) — gossip for small values,
    /// replica-set writes for large ones — with one deliberate difference: it
    /// never writes the value into this node's local store. The caller (a
    /// read-modify-write op on [`Store`]) applied it under the key's shard lock
    /// already, and re-writing it here would let an older publish overwrite a
    /// newer local total, losing an update on the origin node. See
    /// [`ephpm_kv::store::Replicator::replicate_published`].
    pub async fn publish(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> bool {
        if value.len() <= self.config.small_key_threshold {
            let w = self.new_write(ttl);
            self.cluster.gossip_set_queued(key, value, w.expiry_ms, w.stamp);
            if self.config.hot_key_cache {
                self.maybe_bump_hot_version(key).await;
            }
            return true;
        }
        let (marker, w) = self.new_large_write(ttl);
        self.fan_out_large(key, value, ttl, w, Some(marker), FanOut::ReplicaSet).await;
        if self.config.hot_key_cache {
            self.maybe_bump_hot_version(key).await;
        }
        true
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
    /// Peer writes are **awaited**: a broadcast key is written rarely
    /// (certificate issuance and renewal) and the caller wants cluster-wide
    /// read-your-writes far more than it wants the write to return quickly.
    /// A peer that is unreachable is logged and counted
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
        if value.len() <= self.config.small_key_threshold {
            // Gossip already means "every node"; nothing to widen.
            self.cluster.gossip_set_queued(&key, &value, w.expiry_ms, w.stamp);
            if self.config.hot_key_cache {
                self.maybe_bump_hot_version(&key).await;
            }
            return true;
        }

        // This node's own copy first: it must be able to serve the value out
        // of its local store even if every peer write below fails.
        let local_ok = self.store.set_local(key.clone(), value.clone(), ttl);
        self.fan_out_large(&key, &value, ttl, w, None, FanOut::Everyone).await;
        if local_ok && self.config.hot_key_cache {
            self.maybe_bump_hot_version(&key).await;
        }
        local_ok
    }

    /// [`publish`](Self::publish) for a **per-site** key: the site-namespaced
    /// transport key goes on the wire, and — as with the global path — this
    /// node's own copy is left exactly as the mutation left it.
    pub async fn publish_for_site(
        &self,
        site: &str,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> bool {
        let transport = crate::site_namespace::encode(site, key);
        if value.len() <= self.config.small_key_threshold {
            let w = self.new_write(ttl);
            self.cluster.gossip_set_queued(&transport, value, w.expiry_ms, w.stamp);
            return true;
        }
        let (marker, w) = self.new_large_write(ttl);
        self.fan_out_large(&transport, value, ttl, w, Some(marker), FanOut::ReplicaSet).await;
        true
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
    ) -> bool {
        let transport = crate::site_namespace::encode(site, &key);
        if value.len() <= self.config.small_key_threshold {
            let w = self.new_write(ttl);
            // Gossip tier: replicated to all nodes, applied per-site on arrival.
            tracing::trace!(
                site,
                key,
                bytes = value.len(),
                tier = "gossip",
                "per-site KV: publishing"
            );
            self.cluster.gossip_set_queued(&transport, &value, w.expiry_ms, w.stamp);
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
        let (marker, w) = self.new_large_write(ttl);
        let local_ok = local.set_local(key.clone(), value.clone(), ttl);
        let delivered =
            self.fan_out_large(&transport, &value, ttl, w, Some(marker), FanOut::ReplicaSet).await;
        if delivered.failed > 0 {
            // A large-tier publish that fails leaves this vhost's key on fewer
            // nodes than intended. Never silent — that is exactly how the INCR
            // gap survived a full deploy cycle.
            tracing::warn!(
                site,
                key,
                bytes = value.len(),
                failed = delivered.failed,
                "per-site KV: large-value publish did not reach every replica; readers on \
                 those nodes fetch through to the ones that have it"
            );
        }
        local_ok
    }

    /// Delete a per-site key: gossip tombstone under the namespaced transport
    /// key, plus a local drop from the site's own store.
    pub fn remove_for_site(&self, site: &str, local: &Store, key: &str, stamp: Stamp) -> bool {
        let transport = crate::site_namespace::encode(site, key);
        let gossip_deleted = self.cluster.gossip_del_queued(&transport, stamp);
        let local_deleted = local.remove_local(key);
        gossip_deleted || local_deleted
    }

    /// Per-site publish-only tombstone — the multi-tenant mirror of
    /// [`gossip_tombstone`](Self::gossip_tombstone). Gossips the
    /// site-namespaced delete without removing the site store's local copy, so
    /// a value recreated between `del_if_eq`'s removal and this broadcast
    /// survives on this node.
    pub fn gossip_tombstone_for_site(&self, site: &str, key: &str, stamp: Stamp) -> bool {
        let transport = crate::site_namespace::encode(site, key);
        self.cluster.gossip_del_queued(&transport, stamp)
    }

    /// Fetch `transport_key` from the key's replica set for a reader on this
    /// node that has no local copy — the bounded, synchronous-safe read-through
    /// behind [`ephpm_kv::store::Store::get_through`].
    ///
    /// Costs nothing when this node is itself one of the key's replicas (a
    /// miss here means the key does not exist, modulo in-flight replication)
    /// or when it is the only node alive. Otherwise the owners are asked in
    /// ring order within one [`FETCH_THROUGH_TIMEOUT`] budget, and a value
    /// found counts toward hot-key promotion exactly like a fetch from
    /// [`get`](Self::get). The value is not materialized locally beyond that
    /// cache: it arrives with no TTL.
    pub async fn fetch_through(&self, transport_key: &str) -> Option<Vec<u8>> {
        if self.config.hot_key_cache {
            let key_hash = hash_key(transport_key);
            if let Some(cached) = self.hot_cache.get(&key_hash) {
                let ttl = Duration::from_secs(self.config.hot_key_local_ttl_secs);
                if cached.cached_at.elapsed() < ttl && cached.key == transport_key {
                    return Some(cached.data.clone());
                }
                drop(cached);
                evict_hot_entry(&self.hot_cache, &self.hot_cache_mem, key_hash);
            }
        }

        let alive = self.alive_nodes().await;
        if alive.len() <= 1 {
            return None;
        }
        let self_id = self.cluster.self_node().id;
        let replicas =
            replica_nodes_for(&alive, hash_key(transport_key), self.config.replication_factor);
        if replicas.iter().any(|n| n.id == self_id) {
            // We are a replica and we have no copy: the key does not exist.
            return None;
        }

        let deadline = tokio::time::Instant::now() + FETCH_THROUGH_TIMEOUT;
        for node in &replicas {
            let Some(addr) = node_data_addr(node, self.config.data_port) else { continue };
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if budget.is_zero() {
                break;
            }
            match tokio::time::timeout(budget, self.client.fetch(addr, transport_key)).await {
                Ok(Ok(Some(value))) => {
                    self.track_remote_fetch(transport_key, &value);
                    return Some(value);
                }
                Ok(Ok(None)) => {
                    tracing::trace!(key = transport_key, replica = %node.id, "not on replica");
                }
                Ok(Err(e)) => {
                    tracing::debug!(key = transport_key, replica = %node.id, %addr, %e, "fetch-through failed");
                }
                Err(_) => {
                    tracing::debug!(key = transport_key, replica = %node.id, %addr, "fetch-through timed out");
                    break;
                }
            }
        }
        None
    }

    /// Fan a large value out over the data plane and await every target.
    ///
    /// `marker`, when given, is queued as a gossip tombstone **before** the
    /// data-plane writes: it carries a stamp strictly below the value's, so a
    /// replica that receives the value first keeps it (the marker is older),
    /// a replica that receives the marker first drops its old copy and then
    /// takes the value, and every other node drops whatever older copy it
    /// held and fetches through on its next read.
    async fn fan_out_large(
        &self,
        transport_key: &str,
        value: &[u8],
        ttl: Option<Duration>,
        w: WriteStamp,
        marker: Option<Stamp>,
        scope: FanOut,
    ) -> Delivery {
        debug_assert!(marker.is_none_or(|m| m < w.stamp), "marker must order below the value");
        if let Some(marker) = marker {
            self.cluster.gossip_del_queued(transport_key, marker);
        }

        let self_id = self.cluster.self_node().id;
        let targets: Vec<NodeInfo> = match scope {
            FanOut::ReplicaSet => self.replica_nodes(transport_key).await,
            FanOut::Everyone => self.alive_nodes().await,
        }
        .into_iter()
        .filter(|n| n.id != self_id)
        .collect();

        let mut delivery = Delivery::default();
        for node in &targets {
            let Some(addr) = node_data_addr(node, self.config.data_port) else {
                delivery.failed += 1;
                self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(key = transport_key, peer = %node.id, "peer has no parseable data address");
                continue;
            };
            match self.client.store_stamped(addr, transport_key, value, ttl, w.stamp).await {
                Ok(true) => delivery.delivered += 1,
                Ok(false) => {
                    delivery.failed += 1;
                    self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(key = transport_key, peer = %node.id, %addr, "peer rejected large-value write");
                }
                Err(e) => {
                    delivery.failed += 1;
                    self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(key = transport_key, peer = %node.id, %addr, %e, "large-value write failed");
                }
            }
        }
        if delivery.failed > 0 && scope == FanOut::Everyone {
            // Never silent: a partially-delivered broadcast means some nodes
            // cannot read a value they are supposed to hold locally.
            tracing::warn!(
                key = transport_key,
                bytes = value.len(),
                delivered = delivery.delivered,
                peers = targets.len(),
                "broadcast reached only some peers; the rest must fetch through on read"
            );
        }
        delivery
    }

    /// Queue a large write for the bounded worker pool, starting the pool on
    /// `handle` the first time. Returns `false` when the queue is full and
    /// the write was shed: this node's copy was already written by the
    /// caller, the value just does not reach its replicas until it is
    /// written again (counted, rate-limited WARN).
    fn enqueue_large(self: &Arc<Self>, handle: &tokio::runtime::Handle, job: LargeWrite) -> bool {
        let tx = self.large_tx.get_or_init(|| {
            let (tx, rx) = tokio::sync::mpsc::channel::<LargeWrite>(LARGE_QUEUE_CAP);
            let rx = Arc::new(tokio::sync::Mutex::new(rx));
            for _ in 0..LARGE_WORKERS {
                let rx = Arc::clone(&rx);
                // Weak, not Arc: the workers must not keep the store (and
                // through it the cluster handle) alive. Dropping the store
                // drops the sender, `recv` yields `None`, and they exit.
                let store = Arc::downgrade(self);
                handle.spawn(async move {
                    loop {
                        let job = rx.lock().await.recv().await;
                        let Some(job) = job else { break };
                        let Some(store) = store.upgrade() else { break };
                        store.run_large(job).await;
                    }
                });
            }
            tx
        });
        match tx.try_send(job) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(job)) => {
                self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
                let shed = self.large_shed.fetch_add(1, Ordering::Relaxed) + 1;
                if shed.is_multiple_of(1000) || shed == 1 {
                    tracing::warn!(
                        key = %job.transport_key,
                        queue = LARGE_QUEUE_CAP,
                        shed_total = shed,
                        "large-value replication shed: the data-plane queue is full (the local \
                         write succeeded; this key reaches its replicas when written again)"
                    );
                }
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Execute one queued large write.
    async fn run_large(&self, job: LargeWrite) {
        self.fan_out_large(&job.transport_key, &job.value, job.ttl, job.w, job.marker, job.scope)
            .await;
    }

    /// Large writes shed because the data-plane queue was full.
    #[must_use]
    pub fn large_writes_shed(&self) -> u64 {
        self.large_shed.load(Ordering::Relaxed)
    }

    /// Delete a key from whichever tier owns it, stamping the delete now.
    pub fn remove(&self, key: &str) -> bool {
        let stamp = self.cluster.stamp();
        self.remove_at(key, stamp)
    }

    /// Delete a key from whichever tier owns it with a stamp the caller
    /// already fixed. Synchronous: the gossip tombstone is queued, the local
    /// copy dropped and any hot-cache entry evicted without touching the
    /// runtime, so the replication hooks call it directly.
    pub fn remove_at(&self, key: &str, stamp: Stamp) -> bool {
        // Try gossip tier first.
        let gossip_deleted = self.cluster.gossip_del_queued(key, stamp);

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
    pub fn gossip_tombstone(&self, key: &str, stamp: Stamp) -> bool {
        let deleted = self.cluster.gossip_del_queued(key, stamp);
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

    /// Route a compare-and-set (`SET ... IFEQ`) on `transport_key` to the same
    /// arbiter [`route_set_nx`](Self::route_set_nx) uses. Same contract: an
    /// owner predating the op drops the connection, which reads as
    /// [`Routed::OwnerUnreachable`] — the write fails loudly rather than being
    /// decided on a possibly stale local copy.
    pub async fn route_set_if_eq(
        &self,
        transport_key: &str,
        expected: &[u8],
        value: &[u8],
        ttl: SetTtl,
    ) -> Routed<SetIfEqOutcome> {
        let Some(owner) = self.conditional_owner(transport_key).await else {
            return Routed::Local;
        };
        let Some(addr) = node_data_addr(&owner, self.config.data_port) else {
            tracing::warn!(key = transport_key, owner = %owner.id, "SET IFEQ owner has no data address");
            return Routed::OwnerUnreachable;
        };
        let call = self.client.set_if_eq(addr, transport_key, expected, value, ttl);
        match tokio::time::timeout(CONDITIONAL_ROUTE_TIMEOUT, call).await {
            Ok(Ok(outcome)) => Routed::Owner(outcome),
            Ok(Err(e)) => {
                tracing::warn!(
                    key = transport_key, owner = %owner.id, %addr, %e,
                    "SET IFEQ owner unreachable; nothing written (fail closed)"
                );
                Routed::OwnerUnreachable
            }
            Err(_) => {
                tracing::warn!(
                    key = transport_key, owner = %owner.id, %addr,
                    "SET IFEQ owner timed out; nothing written (fail closed)"
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
        self.cluster.gossip_set(
            &format!("{HOT_PREFIX}{version_key}"),
            new_version.to_string().as_bytes(),
            None,
        );

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

/// The catch-up gate task: polls membership and the peers' marks until every
/// alive peer is caught up (then opens the gate and exits), or the cap
/// passes (opens with a warning).
///
/// Holds only the chitchat mutex and the gate — never the `ClusterHandle` or
/// the `ClusteredStore` — so shutting either down is unaffected.
async fn run_catch_up_gate(
    gate: Arc<CatchUpGate>,
    chitchat: Arc<tokio::sync::Mutex<chitchat::Chitchat>>,
    self_id: String,
    started_ms: u64,
) {
    let started = Instant::now();
    let mut next_warn = started + CATCH_UP_WARN_EVERY;
    loop {
        tokio::time::sleep(CATCH_UP_POLL).await;
        let (status, peers) = {
            let guard = chitchat.lock().await;
            (
                crate::node::catch_up_status_locked(&guard, &self_id, started_ms),
                crate::node::alive_peers_locked(&guard, &self_id),
            )
        };
        *gate.peers.write() = peers;
        let elapsed = started.elapsed();
        if elapsed >= CATCH_UP_JOIN_WAIT && status.complete() {
            gate.open();
            tracing::info!(
                elapsed_ms = elapsed.as_millis(),
                peers = status.peers.len(),
                "gossip KV catch-up complete: every peer's state has arrived; serving reads locally"
            );
            return;
        }
        if elapsed >= CATCH_UP_CAP {
            gate.open();
            tracing::warn!(
                elapsed_ms = elapsed.as_millis(),
                pending = ?status.pending(),
                "gossip KV catch-up did not complete within the cap; serving reads locally \
                 anyway — small-tier values on this node may be stale until gossip converges \
                 (peers whose state is too large to stream within chitchat's tombstone grace \
                 never converge; see #631)"
            );
            return;
        }
        if Instant::now() >= next_warn {
            next_warn = Instant::now() + CATCH_UP_WARN_EVERY;
            tracing::warn!(
                elapsed_ms = elapsed.as_millis(),
                pending = ?status.pending(),
                "gossip KV catch-up still in progress; reads are served through peers (one \
                 data-plane round trip each)"
            );
        }
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
        let store = &self.inner.store;
        if value.len() <= self.inner.config.small_key_threshold {
            // Record OUR own write in the applied map and materialize it in
            // one step under the key's slot (`apply_if_newer`), so a slower
            // echo of this write from gossip, a concurrent local writer or a
            // peer's write racing this one cannot leave the local copy and
            // the recorded stamp disagreeing.
            let w = self.inner.new_write(ttl);
            let verdict = apply_if_newer(&self.applied, &key, w.stamp, || {
                store.set_local(key.clone(), value.clone(), ttl);
            });
            if verdict == Apply::Skip {
                // A newer write for this key already landed here: last
                // writer wins, and this one is neither kept nor published.
                return true;
            }
            // Queue the gossip publish synchronously: no task, no copy
            // parked on the chitchat mutex (F-6). The writer drains it.
            self.inner.cluster.gossip_set_queued(&key, &value, w.expiry_ms, w.stamp);
            return true;
        }
        // Large value: this node's own copy is written here, always — so the
        // writing node reads its own write back whether or not it is in the
        // replica set (F-8) — and the replica fan-out goes through the
        // bounded data-plane queue (or is awaited in `sync` mode). Same
        // one-step record + local write as the small path.
        let (marker, w) = self.inner.new_large_write(ttl);
        let mut local_ok = true;
        let verdict = apply_if_newer(&self.applied, &key, w.stamp, || {
            local_ok = store.set_local(key.clone(), value.clone(), ttl);
        });
        if verdict == Apply::Skip {
            return true;
        }
        self.dispatch_large(LargeWrite {
            transport_key: key,
            value,
            ttl,
            w,
            marker: Some(marker),
            scope: FanOut::ReplicaSet,
        });
        local_ok
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
        let verdict = apply_if_newer(&self.applied, &key, w.stamp, || {
            local_ok = store.set_local(key.clone(), value.clone(), ttl);
        });
        if verdict == Apply::Skip {
            return true;
        }
        if value.len() <= self.inner.config.small_key_threshold {
            // Gossip already means "every node", so queue it and return.
            self.inner.cluster.gossip_set_queued(&key, &value, w.expiry_ms, w.stamp);
            return local_ok;
        }
        self.dispatch_large(LargeWrite {
            transport_key: key,
            value,
            ttl,
            w,
            marker: None,
            scope: FanOut::Everyone,
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

    fn fetch_through(&self, key: &str) -> Option<Vec<u8>> {
        // The global keyspace's transport key IS the key. Bounded by
        // FETCH_THROUGH_TIMEOUT inside; `None` on a shut-down runtime.
        block_on_cluster(&self.handle, self.inner.fetch_through(key)).flatten()
    }

    fn cold_read(&self, key: &str) -> ColdRead {
        // The global keyspace's transport key IS the key.
        self.inner.cold_read(&self.handle, &self.applied, key)
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

    fn route_set_if_eq(
        &self,
        key: &str,
        expected: &[u8],
        value: &[u8],
        ttl: SetTtl,
    ) -> Routed<SetIfEqOutcome> {
        block_on_cluster(&self.handle, self.inner.route_set_if_eq(key, expected, value, ttl))
            .unwrap_or(Routed::OwnerUnreachable)
    }

    fn replicate_hash_field(
        &self,
        key: &str,
        field: &str,
        apply: &mut dyn FnMut() -> Option<HashFieldState>,
    ) -> bool {
        let transport = crate::hash_field::encode_key(key, field);
        publish_hash_field(&self.inner, &self.applied, &transport, apply)
    }

    fn hash_field_value_limit(&self) -> Option<usize> {
        Some(hash_field_value_limit(&self.inner))
    }

    fn replicate_remove(&self, key: &str) -> bool {
        // Drop the local materialized copy synchronously (mirror of the
        // set path above), then propagate the removal via a gossip
        // tombstone. Record our own stamp in the applied map so a
        // slow echo of THIS tombstone through gossip does not clobber a
        // follow-up SET we later accept locally.
        let stamp = self.inner.cluster.stamp();
        let store = &self.inner.store;
        let verdict = apply_if_newer(&self.applied, key, stamp, || {
            store.remove_local(key);
        });
        if verdict == Apply::Skip {
            // A newer write already landed: it wins over this delete.
            return true;
        }
        // Local copy is gone; queue the tombstone and evict any hot-cache
        // entry (the local removal above is idempotent).
        self.inner.remove_at(key, stamp);
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
        if value.len() <= self.inner.config.small_key_threshold {
            let w = self.inner.new_write(ttl);
            if apply_if_newer(&self.applied, &key, w.stamp, || {}) == Apply::Skip {
                // Overtaken by a newer write for this key: not published.
                return;
            }
            self.inner.cluster.gossip_set_queued(&key, &value, w.expiry_ms, w.stamp);
            return;
        }
        let (marker, w) = self.inner.new_large_write(ttl);
        if apply_if_newer(&self.applied, &key, w.stamp, || {}) == Apply::Skip {
            return;
        }
        self.dispatch_large(LargeWrite {
            transport_key: key,
            value,
            ttl,
            w,
            marker: Some(marker),
            scope: FanOut::ReplicaSet,
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
        if apply_if_newer(&self.applied, &key, stamp, || {}) == Apply::Skip {
            return;
        }
        self.inner.gossip_tombstone(&key, stamp);
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
        let verdict = apply_if_newer(&self.applied, key, w.stamp, || {
            ok = store.expire_local(key, ttl);
        });
        if verdict == Apply::Skip {
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

        if bytes.len() <= self.inner.config.small_key_threshold {
            self.inner.cluster.gossip_set_queued(key, &bytes, w.expiry_ms, w.stamp);
        } else {
            // A large value's TTL change is re-sent to its replicas as a
            // stamped SET with the new TTL — one path for "value and expiry
            // on the replica set", and it repairs a replica that missed the
            // value (F-11: this used to be node-local). No marker: nothing
            // outside the replica set holds a copy to invalidate.
            self.dispatch_large(LargeWrite {
                transport_key: key.to_string(),
                value: bytes.to_vec(),
                ttl: Some(ttl),
                w,
                marker: None,
                scope: FanOut::ReplicaSet,
            });
        }
        true
    }
}

impl KvReplicator {
    /// Hand a large write to the data-plane workers, or — in
    /// `replication_mode = "sync"` — run it to completion here so the client
    /// is answered only once every reachable replica has acknowledged.
    fn dispatch_large(&self, job: LargeWrite) {
        dispatch_large(&self.inner, &self.handle, job);
    }
}

/// Shared by both replicators: queue a large write, or in `sync` mode await
/// its fan-out from the synchronous hook via [`block_on_cluster`].
fn dispatch_large(inner: &Arc<ClusteredStore>, handle: &tokio::runtime::Handle, job: LargeWrite) {
    if ReplicationMode::from_config(&inner.config.replication_mode) == ReplicationMode::Sync
        && can_block_here()
    {
        // Answer the client only once the fan-out is done. `None` (the
        // runtime is shutting down) leaves the local copy, already written,
        // as the only one.
        let _ = block_on_cluster(handle, inner.run_large(job));
        return;
    }
    // Async mode — or a current-thread runtime, where blocking would deadlock
    // the only worker: the queue delivers the same writes, just not before
    // the client is answered.
    inner.enqueue_large(handle, job);
}

/// Whether [`block_on_cluster`] can run a future from this thread: off any
/// runtime, or on a multi-thread runtime worker.
fn can_block_here() -> bool {
    match tokio::runtime::Handle::try_current() {
        Err(_) => true,
        Ok(current) => current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread,
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
        // Keyed by the TRANSPORT key so site A's "foo" and site B's "foo"
        // occupy different ordering slots.
        // Record + local write are one step (see `apply_if_newer`).
        let transport = crate::site_namespace::encode(&self.site, &key);
        let local = &self.local;
        if value.len() <= self.inner.config.small_key_threshold {
            let w = self.inner.new_write(ttl);
            let verdict = apply_if_newer(&self.applied, &transport, w.stamp, || {
                local.set_local(key.clone(), value.clone(), ttl);
            });
            if verdict == Apply::Skip {
                return true;
            }
            self.inner.cluster.gossip_set_queued(&transport, &value, w.expiry_ms, w.stamp);
            return true;
        }
        // Large value: this site's own copy under the bare key, the fan-out
        // under the transport key (see `KvReplicator::replicate_set`).
        let (marker, w) = self.inner.new_large_write(ttl);
        let mut local_ok = true;
        let verdict = apply_if_newer(&self.applied, &transport, w.stamp, || {
            local_ok = local.set_local(key.clone(), value.clone(), ttl);
        });
        if verdict == Apply::Skip {
            return true;
        }
        dispatch_large(
            &self.inner,
            &self.handle,
            LargeWrite {
                transport_key: transport,
                value,
                ttl,
                w,
                marker: Some(marker),
                scope: FanOut::ReplicaSet,
            },
        );
        local_ok
    }

    fn replicate_remove(&self, key: &str) -> bool {
        let stamp = self.inner.cluster.stamp();
        let transport = crate::site_namespace::encode(&self.site, key);
        let local = &self.local;
        let verdict = apply_if_newer(&self.applied, &transport, stamp, || {
            local.remove_local(key);
        });
        if verdict == Apply::Skip {
            return true;
        }
        self.inner.cluster.gossip_del_queued(&transport, stamp);
        true
    }

    fn replicate_published(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) {
        // PUBLISH ONLY — mirrors the global replicator: the site store already
        // holds this value (applied under its shard lock), so writing it back
        // would risk regressing a newer concurrent increment on this node. Only
        // the cluster-wide fan-out is left to do.
        let transport = crate::site_namespace::encode(&self.site, &key);
        if value.len() <= self.inner.config.small_key_threshold {
            let w = self.inner.new_write(ttl);
            if apply_if_newer(&self.applied, &transport, w.stamp, || {}) == Apply::Skip {
                return;
            }
            self.inner.cluster.gossip_set_queued(&transport, &value, w.expiry_ms, w.stamp);
            return;
        }
        let (marker, w) = self.inner.new_large_write(ttl);
        if apply_if_newer(&self.applied, &transport, w.stamp, || {}) == Apply::Skip {
            return;
        }
        dispatch_large(
            &self.inner,
            &self.handle,
            LargeWrite {
                transport_key: transport,
                value,
                ttl,
                w,
                marker: Some(marker),
                scope: FanOut::ReplicaSet,
            },
        );
    }

    fn fetch_through(&self, key: &str) -> Option<Vec<u8>> {
        let transport = crate::site_namespace::encode(&self.site, key);
        block_on_cluster(&self.handle, self.inner.fetch_through(&transport)).flatten()
    }

    fn cold_read(&self, key: &str) -> ColdRead {
        // Peers resolve the enveloped key into this site's store on their
        // side, exactly as a fetch-through does.
        let transport = crate::site_namespace::encode(&self.site, key);
        self.inner.cold_read(&self.handle, &self.applied, &transport)
    }

    fn replicate_remove_published(&self, key: String) {
        // PUBLISH ONLY — mirrors the global replicator: no local removal, so a
        // value recreated on this node between `del_if_eq`'s removal and this
        // broadcast survives. Record the stamp keyed by the TRANSPORT key,
        // then gossip the per-site tombstone only.
        let stamp = self.inner.cluster.stamp();
        let transport = crate::site_namespace::encode(&self.site, &key);
        if apply_if_newer(&self.applied, &transport, stamp, || {}) == Apply::Skip {
            return;
        }
        self.inner.cluster.gossip_del_queued(&transport, stamp);
    }

    fn replicate_expire(&self, key: &str, ttl: Duration) -> bool {
        // Same shape as the global path: update locally, then re-broadcast the
        // unchanged value with the new expiry so peers converge on it.
        let w = self.inner.new_write(Some(ttl));
        let transport = crate::site_namespace::encode(&self.site, key);
        let local = &self.local;
        let mut ok = false;
        let verdict = apply_if_newer(&self.applied, &transport, w.stamp, || {
            ok = local.expire_local(key, ttl);
        });
        if verdict == Apply::Skip {
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
        // Small-key tier only, matching the global `replicate_expire`.
        if bytes.len() <= self.inner.config.small_key_threshold {
            self.inner.cluster.gossip_set_queued(&transport, &bytes, w.expiry_ms, w.stamp);
        } else {
            // Re-send to the replica set with the new TTL, as the global path does.
            dispatch_large(
                &self.inner,
                &self.handle,
                LargeWrite {
                    transport_key: transport,
                    value: bytes.to_vec(),
                    ttl: Some(ttl),
                    w,
                    marker: None,
                    scope: FanOut::ReplicaSet,
                },
            );
        }
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

    fn route_set_if_eq(
        &self,
        key: &str,
        expected: &[u8],
        value: &[u8],
        ttl: SetTtl,
    ) -> Routed<SetIfEqOutcome> {
        let transport = crate::site_namespace::encode(&self.site, key);
        block_on_cluster(&self.handle, self.inner.route_set_if_eq(&transport, expected, value, ttl))
            .unwrap_or(Routed::OwnerUnreachable)
    }

    fn replicate_hash_field(
        &self,
        key: &str,
        field: &str,
        apply: &mut dyn FnMut() -> Option<HashFieldState>,
    ) -> bool {
        // The site envelope is outermost, so the receiver routes to this
        // site's store first and then decodes the field inside it.
        let transport =
            crate::site_namespace::encode(&self.site, &crate::hash_field::encode_key(key, field));
        publish_hash_field(&self.inner, &self.applied, &transport, apply)
    }

    fn hash_field_value_limit(&self) -> Option<usize> {
        Some(hash_field_value_limit(&self.inner))
    }
}

/// The largest hash-field value that fits one gossip item: the gossip tier's
/// payload budget less the field header.
fn hash_field_value_limit(inner: &ClusteredStore) -> usize {
    inner.config.small_key_threshold.saturating_sub(crate::hash_field::HEADER_LEN)
}

/// Shared body of both replicators' `replicate_hash_field`: order the field
/// write under `transport`'s slot, run the local mutation there, and gossip
/// the field's resulting state (or a tombstone) with the same stamp.
///
/// A mutation that changed nothing leaves no trace in the applied map
/// ([`apply_if_newer_cond`]), so it cannot make this node refuse a peer's
/// genuinely newer write to the field.
fn publish_hash_field(
    inner: &ClusteredStore,
    applied: &AppliedWriteMap,
    transport: &str,
    apply: &mut dyn FnMut() -> Option<HashFieldState>,
) -> bool {
    let stamp = inner.cluster.stamp();
    let mut ran = false;
    let mut published = None;
    apply_if_newer_cond(applied, transport, stamp, || {
        ran = true;
        published = apply();
        published.is_some()
    });
    let Some(state) = published else {
        return ran;
    };
    match state.value {
        Some(value) => {
            let (payload, expiry_ms) =
                crate::hash_field::encode_value(&value, state.field_ttl, state.hash_ttl);
            inner.cluster.gossip_set_queued(transport, &payload, expiry_ms, stamp);
        }
        None => {
            inner.cluster.gossip_del_queued(transport, stamp);
        }
    }
    true
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
    let verify: VerifySet = Arc::new(DashSet::new());

    // Bound the applied map and repair keys applied without an ordering
    // record. Lives as long as the runtime; it holds only the chitchat
    // mutex and the writer, never the ClusterHandle, so a handle shutdown
    // is unaffected.
    tokio::spawn(applied_maintenance(
        cluster.handle.chitchat(),
        Arc::clone(&cluster.writer),
        Arc::clone(&store),
        multi_tenant.clone(),
        Arc::clone(&applied),
        Arc::clone(&verify),
    ));

    let retire = Arc::clone(&cluster.writer);
    cluster
        .subscribe_kv_changes(move |event| {
            if event.node().node_id == self_id {
                return;
            }
            let transport_key = event.key();
            let stamp = event.stamp();
            // This node may hold its own, older copy of the key in its
            // gossip state; once a peer's write is the newest one that copy
            // is dead weight that keeps a later tombstone from ever being
            // collectable. The writer deletes it if it really is older.
            retire
                .enqueue_retire(&format!("{}{transport_key}", crate::gossip_kv::KV_PREFIX), stamp);
            let Some((target, key, site)) =
                route_target(multi_tenant.as_ref(), &store, transport_key, &event.node().node_id)
            else {
                return;
            };

            // Ordering is tracked per TRANSPORT key, so two sites sharing a
            // bare key name never contend for one slot. The record and the
            // local write are one step under that slot (`apply_if_newer`).
            let is_set = matches!(event, crate::gossip_kv::KvChangeEvent::Set { .. });
            let verdict = apply_if_newer(&applied, transport_key, stamp, || match event {
                crate::gossip_kv::KvChangeEvent::Set { value, ttl, .. } => {
                    materialize(&target, &key, Some((value, ttl)));
                }
                crate::gossip_kv::KvChangeEvent::Tombstone { .. } => {
                    materialize(&target, &key, None);
                }
            });
            match verdict {
                Apply::Skip => {
                    tracing::trace!(
                        key = transport_key,
                        from = %event.node().node_id,
                        ?stamp,
                        "gossip KV change skipped as stale"
                    );
                }
                Apply::Newer => {
                    tracing::trace!(
                        key,
                        site,
                        from = %event.node().node_id,
                        ?stamp,
                        is_set,
                        "gossip KV change materialized locally"
                    );
                }
                // No ordering record for this key (never seen, or pruned):
                // applied now, and the verifier confirms it against every
                // node's state — a chitchat reset re-firing an old entry is
                // the one way a pruned key could otherwise regress.
                Apply::FirstSeen => {
                    if verify.len() < VERIFY_CAP {
                        verify.insert(transport_key.to_string());
                    }
                }
            }
        })
        .await;
}

/// Route a transport key to the local store it materializes in and the bare
/// key within it: per-site envelope → that site's store; anything else → the
/// global store. An envelope arriving on a node that is not multi-tenant, or
/// one that does not decode, has nowhere to go and is dropped with a warning
/// rather than flattened into the global keyspace.
fn route_target(
    multi_tenant: Option<&ephpm_kv::multi_tenant::MultiTenantStore>,
    store: &Arc<Store>,
    transport_key: &str,
    from: &str,
) -> Option<(Arc<Store>, String, Option<String>)> {
    match (multi_tenant, crate::site_namespace::decode(transport_key)) {
        // Enveloped key on a multi-tenant node → that vhost's store.
        (Some(mt), Some((site, key))) => {
            Some((mt.get_site_store(site), key.to_string(), Some(site.to_string())))
        }
        // Enveloped key with nowhere to go: drop it loudly rather
        // than flattening a tenant's key into the global keyspace.
        (None, Some(_)) => {
            tracing::warn!(
                key = transport_key,
                from,
                "received a per-site KV replication event but this node is not \
                 multi-tenant ([server] sites_dir unset); dropping it rather than \
                 flattening a tenant's key into the global keyspace"
            );
            None
        }
        // Leading SEP but undecodable: a malformed or hostile envelope. Fail
        // CLOSED — flattening it into the shared global keyspace would take a
        // key that claims to be a tenant's and make it visible to every
        // tenant.
        (_, None) if crate::site_namespace::is_enveloped(transport_key) => {
            tracing::warn!(
                key = transport_key,
                from,
                "received a KV replication event whose per-site envelope does not \
                 decode to a valid site key; dropping it rather than flattening it \
                 into the global keyspace"
            );
            None
        }
        // Plain global key — unchanged behaviour.
        (_, None) => Some((Arc::clone(store), transport_key.to_string(), None)),
    }
}

/// Keys applied without an ordering record, awaiting verification against
/// the full gossip state. Coalescing and capped at [`VERIFY_CAP`].
type VerifySet = Arc<DashSet<String>>;

/// Keys the verify set holds before further first-seen keys go unverified
/// (they are still applied; only the repair pass is skipped).
const VERIFY_CAP: usize = 65_536;

/// Entries older than this are pruned from the applied map; an event for
/// such a key applies on first sight and is then verified.
pub const APPLIED_HORIZON: Duration = Duration::from_secs(600);

/// Hard cap on applied-map entries; when the horizon prune leaves more, the
/// horizon is halved until it fits.
pub const APPLIED_MAX_ENTRIES: usize = 1_000_000;

/// How often the verify set is drained.
const VERIFY_INTERVAL: Duration = Duration::from_secs(1);

/// How often the applied map is pruned.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// The maintenance loop behind [`start_gossip_applier_multi_tenant`]: drains
/// the verify set every second and prunes the applied map every minute.
async fn applied_maintenance(
    chitchat: Arc<tokio::sync::Mutex<chitchat::Chitchat>>,
    writer: Arc<crate::gossip_writer::GossipWriter>,
    store: Arc<Store>,
    multi_tenant: Option<ephpm_kv::multi_tenant::MultiTenantStore>,
    applied: AppliedWriteMap,
    verify: VerifySet,
) {
    let mut next_prune = Instant::now() + PRUNE_INTERVAL;
    loop {
        tokio::time::sleep(VERIFY_INTERVAL).await;
        let keys: Vec<String> = verify.iter().map(|k| k.clone()).collect();
        if !keys.is_empty() {
            for key in &keys {
                verify.remove(key);
            }
            let repaired = {
                let mut guard = chitchat.lock().await;
                verify_keys(&mut guard, &writer, &store, multi_tenant.as_ref(), &applied, &keys)
            };
            if repaired > 0 {
                tracing::debug!(checked = keys.len(), repaired, "gossip KV verify pass");
            }
        }
        if Instant::now() >= next_prune {
            next_prune = Instant::now() + PRUNE_INTERVAL;
            let pruned = prune_applied(&applied, APPLIED_HORIZON, APPLIED_MAX_ENTRIES);
            if pruned > 0 {
                tracing::debug!(pruned, remaining = applied.len(), "applied-write map pruned");
            }
        }
    }
}

/// Re-derive the winner for each key from every node's state plus this
/// node's queue, and re-apply it locally when it beats what the applied map
/// records. Returns how many keys were repaired.
fn verify_keys(
    guard: &mut chitchat::Chitchat,
    writer: &crate::gossip_writer::GossipWriter,
    store: &Arc<Store>,
    multi_tenant: Option<&ephpm_kv::multi_tenant::MultiTenantStore>,
    applied: &AppliedWriteMap,
    keys: &[String],
) -> usize {
    let mut repaired = 0;
    for transport_key in keys {
        let chitchat_key = format!("{}{transport_key}", crate::gossip_kv::KV_PREFIX);
        let Some(winner) = crate::gossip_kv::resolve_locked(guard, writer, &chitchat_key) else {
            continue;
        };
        let Some((target, key, _site)) = route_target(multi_tenant, store, transport_key, "verify")
        else {
            continue;
        };
        let verdict = apply_if_newer(applied, transport_key, winner.stamp, || match winner.value {
            Some((bytes, expiry_ms)) => {
                let ttl = crate::gossip_kv::ttl_from_expiry(expiry_ms);
                materialize(&target, &key, Some((&bytes, ttl)));
            }
            None => materialize(&target, &key, None),
        });
        // `Skip`: local already reflects the winner (or something newer).
        if verdict != Apply::Skip {
            repaired += 1;
        }
    }
    repaired
}

/// Drop applied-map entries whose stamp is older than `horizon`; if more than
/// `max_entries` remain, halve the horizon and repeat. Returns how many were
/// removed.
fn prune_applied(applied: &AppliedWriteMap, horizon: Duration, max_entries: usize) -> usize {
    let now_ms = crate::gossip_kv::now_epoch_ms();
    let before = applied.len();
    let mut horizon_ms = u64::try_from(horizon.as_millis()).unwrap_or(u64::MAX);
    loop {
        let cutoff = now_ms.saturating_sub(horizon_ms);
        applied.retain(|_, stamp| stamp.epoch_ms() >= cutoff);
        if applied.len() <= max_entries || horizon_ms == 0 {
            break;
        }
        horizon_ms /= 2;
    }
    before.saturating_sub(applied.len())
}

/// The applier's verdict on an incoming write for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Apply {
    /// Not strictly newer than the last applied write: skip it.
    Skip,
    /// Newer than the recorded write: apply it.
    Newer,
    /// No record for this key: apply it, and verify it against the full
    /// gossip state afterwards.
    FirstSeen,
}

/// Pure last-writer-wins gate for the gossip applier.
///
/// Records `stamp` and answers [`Apply::Newer`] when this incoming write is
/// strictly newer than the last one we materialized for `key`, or
/// [`Apply::FirstSeen`] when there is no record (never seen, or pruned);
/// answers [`Apply::Skip`] when it is stale (stamp <= last-applied) so a
/// slow echo cannot clobber a newer write we already accepted. Because a
/// [`Stamp`] carries the origin's node tag, two different nodes' writes are
/// never equal: a same-millisecond tie is decided by the tag, identically on
/// every node, instead of being treated as stale on both. Legacy v1 payloads
/// (`write = 0`) apply on first sight and subsequent v1 echoes for the same
/// key are skipped, which is the desired behavior.
///
/// The check and the record are one atomic step on the key's map entry, so
/// two events for one key racing through here cannot both pass. Production
/// paths use [`apply_if_newer`] so the local write happens under the same
/// step; this is the record-only form the ordering tests exercise.
#[cfg(test)]
fn should_apply(applied: &AppliedWriteMap, key: &str, stamp: Stamp) -> Apply {
    apply_if_newer(applied, key, stamp, || {})
}

/// The one way a value lands on this node: compare `stamp` against the last
/// applied write for `key`, and — while still holding that key's slot in the
/// applied map — record it and run `write` (the local `set_local` /
/// `remove_local` / `expire_local`). `write` runs only for
/// [`Apply::Newer`] and [`Apply::FirstSeen`].
///
/// Every path that materializes a value locally goes through here: the
/// replication hooks for this node's own writes, the gossip applier for
/// peers' writes, the verify pass. Holding the slot across the local write
/// is what makes the order total: two concurrent hooks for one key take
/// their stamps in one order but would otherwise reach the store in
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
) -> Apply {
    let (guard, verdict) = match applied.entry(key.to_string()) {
        dashmap::Entry::Occupied(mut prev) => {
            if stamp <= *prev.get() {
                return Apply::Skip;
            }
            *prev.get_mut() = stamp;
            (prev.into_ref(), Apply::Newer)
        }
        dashmap::Entry::Vacant(slot) => (slot.insert(stamp), Apply::FirstSeen),
    };
    // `guard` keeps the key's slot locked while the local write happens.
    write();
    drop(guard);
    verdict
}

/// [`apply_if_newer`] for a write that may turn out to change nothing.
///
/// `write` runs under the key's slot exactly as in `apply_if_newer` and
/// reports whether it changed anything. When it did not (an `HSETNX` that
/// lost, an `HDEL` of an absent field), the slot is restored to what it held
/// before — the previous stamp, or no record at all — so a no-op leaves no
/// trace in the ordering. Recording the stamp of a write that published
/// nothing would make this node refuse a peer's write to the same key that
/// every other node accepts. Returns [`Apply::Skip`] for a no-op as for a
/// stale write.
pub(crate) fn apply_if_newer_cond(
    applied: &AppliedWriteMap,
    key: &str,
    stamp: Stamp,
    write: impl FnOnce() -> bool,
) -> Apply {
    match applied.entry(key.to_string()) {
        dashmap::Entry::Occupied(mut prev) => {
            let previous = *prev.get();
            if stamp <= previous {
                return Apply::Skip;
            }
            *prev.get_mut() = stamp;
            // `prev` keeps the slot locked across the write.
            if write() {
                Apply::Newer
            } else {
                *prev.get_mut() = previous;
                Apply::Skip
            }
        }
        dashmap::Entry::Vacant(slot) => {
            let entry = slot.insert_entry(stamp);
            if write() {
                Apply::FirstSeen
            } else {
                entry.remove();
                Apply::Skip
            }
        }
    }
}

/// Land one replicated item in `target` under the bare `key`: a hash-field
/// item ([`crate::hash_field`]) is applied to its hash, anything else is a
/// string key. `value` is `None` for a delete. Local only.
fn materialize(target: &Store, key: &str, value: Option<(&[u8], Option<Duration>)>) {
    if let Some((hash_key, field)) = crate::hash_field::decode_key(key) {
        let state = match value {
            Some((payload, _)) => match crate::hash_field::decode_value(payload) {
                Some(state) => state,
                None => {
                    tracing::warn!(
                        key = hash_key,
                        field,
                        "dropping a replicated hash field this node cannot decode (a newer \
                         format from a peer running a later version?)"
                    );
                    return;
                }
            },
            None => HashFieldState::deleted(),
        };
        target.apply_hash_field_local(hash_key, field, state);
        return;
    }
    match value {
        Some((bytes, ttl)) => {
            target.set_local(key.to_string(), bytes.to_vec(), ttl);
        }
        None => {
            target.remove_local(key);
        }
    }
}

/// FNV-1a-style hash for key → u64 mapping.
pub(crate) fn hash_key(key: &str) -> u64 {
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
pub(crate) fn replica_nodes_for(alive: &[NodeInfo], key_hash: u64, factor: usize) -> Vec<NodeInfo> {
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
pub(crate) fn node_data_addr(node: &NodeInfo, local_data_port: u16) -> Option<SocketAddr> {
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
        assert_eq!(should_apply(&applied, "k", at(1000, "a")), Apply::FirstSeen);
        assert_eq!(*applied.get("k").unwrap(), at(1000, "a"));
    }

    #[test]
    fn should_apply_newer_write_lands() {
        let applied = new_applied_write_map();
        assert_eq!(should_apply(&applied, "k", at(1000, "a")), Apply::FirstSeen);
        assert_eq!(should_apply(&applied, "k", at(2000, "b")), Apply::Newer);
        assert_eq!(*applied.get("k").unwrap(), at(2000, "b"));
    }

    #[test]
    fn should_apply_stale_write_is_skipped() {
        let applied = new_applied_write_map();
        assert_eq!(should_apply(&applied, "k", at(2000, "a")), Apply::FirstSeen);
        // Delayed echo of an older write must NOT clobber the newer one.
        assert_eq!(should_apply(&applied, "k", at(1000, "b")), Apply::Skip);
        // The identical stamp (an echo of the same write) is stale.
        assert_eq!(should_apply(&applied, "k", at(2000, "a")), Apply::Skip);
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
        assert_eq!(should_apply(&applied, "k", hi), Apply::Newer);
        assert_eq!(*applied.get("k").unwrap(), hi);

        // Node that wrote `hi` first, then sees `lo`: keeps its own.
        let applied = new_applied_write_map();
        applied.insert("k".into(), hi);
        assert_eq!(should_apply(&applied, "k", lo), Apply::Skip);
        assert_eq!(*applied.get("k").unwrap(), hi);
    }

    #[test]
    fn should_apply_legacy_zero_write_lands_once() {
        // Pre-stamp peers emit write = 0. The first echo applies
        // (map has no entry), the second is treated as stale — matches
        // the desired "apply on first sight, deduplicate re-sends"
        // behavior during rolling upgrades.
        let applied = new_applied_write_map();
        assert_eq!(should_apply(&applied, "legacy-key", at(0, "old")), Apply::FirstSeen);
        assert_eq!(should_apply(&applied, "legacy-key", at(0, "old")), Apply::Skip);
    }

    #[test]
    fn should_apply_origin_recorded_write_blocks_stale_echo() {
        // Simulates KvReplicator::replicate_set recording its own write
        // in the applied map, then a slow echo of the SAME write coming
        // back through gossip. The echo must be skipped.
        let applied = new_applied_write_map();
        let origin = at(5000, "self");
        applied.insert("origin-key".into(), origin);
        assert_eq!(should_apply(&applied, "origin-key", origin), Apply::Skip);
        // A newer overwrite (say a follow-up SET) should still apply.
        assert_eq!(should_apply(&applied, "origin-key", at(5001, "self")), Apply::Newer);
    }

    // ── applied-map bounding (F-6) ───────────────────────────────────

    fn stamp_at_ms(ms: u64) -> Stamp {
        Stamp::new(ms << crate::gossip_kv::SEQ_BITS, "n")
    }

    /// Entries older than the horizon go; recent ones stay.
    #[test]
    fn prune_applied_drops_entries_past_the_horizon() {
        let applied = new_applied_write_map();
        let now = crate::gossip_kv::now_epoch_ms();
        applied.insert("old".into(), stamp_at_ms(now - 3_600_000));
        applied.insert("recent".into(), stamp_at_ms(now - 1_000));
        applied.insert("legacy".into(), at(0, "old-peer"));
        let pruned = prune_applied(&applied, APPLIED_HORIZON, APPLIED_MAX_ENTRIES);
        assert_eq!(pruned, 2, "the hour-old entry and the legacy zero stamp");
        assert!(applied.contains_key("recent"));
        assert!(!applied.contains_key("old"));
    }

    /// Past the entry cap the horizon shrinks until the map fits, so the
    /// map is bounded by count as well as by age.
    #[test]
    fn prune_applied_halves_the_horizon_until_under_the_cap() {
        let applied = new_applied_write_map();
        let now = crate::gossip_kv::now_epoch_ms();
        // 1000 entries spread evenly over the last 10 minutes.
        for i in 0..1000u64 {
            applied.insert(format!("k{i}"), stamp_at_ms(now - i * 600));
        }
        let pruned = prune_applied(&applied, Duration::from_secs(600), 100);
        assert!(pruned >= 900, "pruned {pruned}");
        assert!(applied.len() <= 100, "{} entries remain", applied.len());
        // The survivors are the newest ones.
        assert!(applied.contains_key("k0"));
        assert!(!applied.contains_key("k999"));
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

    // ── Hash-field replication ─────────────────────────────────────────

    /// A write that changed nothing must not leave its stamp behind, or a
    /// peer's genuinely newer write could be refused on this node alone.
    #[test]
    fn a_no_op_conditional_write_leaves_no_ordering_trace() {
        let applied = new_applied_write_map();
        assert_eq!(apply_if_newer_cond(&applied, "k", at(10, "a"), || false), Apply::Skip);
        assert!(!applied.contains_key("k"), "a first-seen no-op must not create a record");

        assert_eq!(apply_if_newer_cond(&applied, "k", at(20, "a"), || true), Apply::FirstSeen);
        assert_eq!(apply_if_newer_cond(&applied, "k", at(30, "a"), || false), Apply::Skip);
        assert_eq!(*applied.get("k").unwrap(), at(20, "a"), "the earlier stamp is restored");
        // A peer write between the two is still accepted.
        assert_eq!(should_apply(&applied, "k", at(25, "b")), Apply::Newer);
        assert_eq!(apply_if_newer_cond(&applied, "k", at(5, "a"), || true), Apply::Skip);
    }

    /// Replicated hash-field items land in the hash (string keys stay string
    /// keys), a tombstone deletes the field, and a per-site envelope routes
    /// the field into that site's store.
    #[test]
    fn hash_field_items_materialize_into_the_right_hash() {
        let global = Store::new(ephpm_kv::store::StoreConfig::default());
        let mt = ephpm_kv::multi_tenant::MultiTenantStore::new(
            Arc::clone(&global),
            ephpm_kv::store::StoreConfig::default(),
        );
        let (payload, _) = crate::hash_field::encode_value(b"v", None, None);
        let item = crate::hash_field::encode_key("h", "f");

        let (target, key, _) = route_target(Some(&mt), &global, &item, "peer").unwrap();
        materialize(&target, &key, Some((&payload, None)));
        assert_eq!(global.hget("h", "f").as_deref(), Some(&b"v"[..]));
        assert!(!global.is_hash(&item), "not stored under its encoded name");

        let site_item = crate::site_namespace::encode("alice.test", &item);
        let (target, key, site) = route_target(Some(&mt), &global, &site_item, "peer").unwrap();
        assert_eq!(site.as_deref(), Some("alice.test"));
        materialize(&target, &key, Some((&payload, None)));
        assert_eq!(mt.get_site_store("alice.test").hget("h", "f").as_deref(), Some(&b"v"[..]));

        materialize(&global, &item, None);
        assert!(!global.exists("h"), "the tombstone deleted the only field");
        assert_eq!(mt.get_site_store("alice.test").hlen("h"), 1, "other tenants untouched");

        // A plain key is still a string.
        materialize(&global, "plain", Some((b"x", None)));
        assert_eq!(global.get("plain").as_deref(), Some(&b"x"[..]));
    }
}

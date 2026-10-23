//! In-memory KV store engine.
//!
//! Backed by [`dashmap::DashMap`] for lock-free concurrent reads and
//! fine-grained write locking. Supports TTL expiry, LRU eviction, and
//! approximate memory tracking.

mod entry;
mod glob;
mod hash;
mod scan;
mod strings;
mod watch;

use std::hash::BuildHasher;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
pub use entry::Entry;
use hash::HashEntry;
pub use hash::{
    HASH_NOT_FLOAT, HASH_NOT_INTEGER, HashWriteError, INCR_NAN_OR_INF, INCR_OVERFLOW, field_code,
    format_float, parse_float,
};
pub use scan::{KeyKind, ScanPage};
pub use strings::{GetExpiry, SetIfEqOutcome, SetTtl};
use tracing::{debug, trace};
use watch::WatchSlot;

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
    /// Parse from the config string. Falls back to `AllKeysLru` on unknown
    /// values — it has no error channel, so it cannot reject a typo.
    ///
    /// On the `[kv] eviction_policy` path an unknown value never reaches
    /// here: `Config::validate` in `ephpm-config` rejects anything outside
    /// its `KV_EVICTION_POLICIES` list at startup. Keep that list in sync
    /// with the arms below.
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

/// Redis-compatible out-of-memory error message, surfaced on the wire as
/// `-OOM …`.
///
/// Returned when a write is refused under the [`EvictionPolicy::NoEviction`]
/// policy with the store already over its `memory_limit`. Matching Redis'
/// wording verbatim means client libraries that special-case `-OOM` (Predis,
/// phpredis, and the PSR cache wrappers built on them) behave against ePHPm's
/// KV exactly as they do against Redis — the write is reported as failed
/// rather than silently answered `+OK` (#477).
pub const OOM_ERROR: &str = "OOM command not allowed when used memory > 'maxmemory'";

/// Outcome of a memory-guarded conditional write (`SET … NX` / `SETNX`).
///
/// The plain write path ([`Store::set`]) needs no such type: it returns
/// `false` for exactly one reason — an out-of-memory refusal — so a caller can
/// map `false` straight to [`OOM_ERROR`]. A conditional write has *two*
/// distinct not-stored outcomes, and the RESP layer must tell them apart: a
/// failed `NX` precondition is a normal `nil`/`:0` reply, whereas an
/// out-of-memory refusal is `-OOM`. Collapsing them into one `bool` is what let
/// a memory-refused `SETNX` answer as if the key merely already existed (#477).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetNxOutcome {
    /// The value was inserted (the key was absent or held an expired entry).
    Inserted,
    /// A live entry already exists at this key; nothing was written.
    Exists,
    /// Refused: the `NoEviction` policy is active and the store is over its
    /// `memory_limit`. Surfaced on the wire as [`OOM_ERROR`].
    OutOfMemory,
    /// Clustered only: the node that arbitrates this key (its owner — see
    /// [`Replicator::route_set_nx`]) could not be reached or did not answer in
    /// time. Nothing was written on this node, but the outcome on the owner is
    /// **unknown**: a request that timed out may still have been applied there.
    /// Surfaced on the wire as [`OWNER_UNREACHABLE_ERROR`].
    ///
    /// This is a deliberate fail-closed answer. Falling back to a node-local
    /// check would let two nodes both acquire the same lock — the bug owner
    /// routing exists to close — so an unreachable owner means "not acquired",
    /// never "acquired locally".
    OwnerUnreachable,
}

/// Error text for an `APPEND` refused because its key is a large value held
/// only by other cluster nodes (see [`Store::try_append`]).
pub const APPEND_HELD_ELSEWHERE_ERROR: &str = "ERR APPEND to a large value this node does not \
     hold is not supported in a cluster (it would replace the value cluster-wide): run it on a \
     node that holds the key, or rewrite it with SET";

/// Why [`Store::try_append`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendError {
    /// The memory budget refused the write (Redis `-OOM`).
    OutOfMemory,
    /// Clustered only: `key` is a large value held by other nodes and not
    /// this one. See [`Store::try_append`].
    HeldElsewhere,
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OutOfMemory => OOM_ERROR,
            Self::HeldElsewhere => APPEND_HELD_ELSEWHERE_ERROR,
        })
    }
}

impl std::error::Error for AppendError {}

/// Warn once per process that an `APPEND` was refused because its key lives
/// on other nodes, then log further refusals at DEBUG. The PHP bridge only
/// sees `false`, so the operator needs one clear line saying why.
fn log_append_held_elsewhere(key: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        debug!(key, "APPEND refused: large value held only by other nodes");
        return;
    }
    tracing::warn!(
        key,
        "APPEND refused: the key is a large value held only by other cluster nodes, and \
         creating it from the appended bytes alone would replace that value everywhere. RESP \
         clients get an error and ephpm_kv_append() returns false. Further refusals are logged \
         at DEBUG."
    );
}

/// Redis Cluster-style error returned when a conditional write (`SETNX`,
/// `SET … NX`, `DELIFEQ`) cannot reach the cluster node that owns its key.
///
/// The `CLUSTERDOWN` prefix is what Redis Cluster uses for "the cluster
/// cannot serve this right now", so a client that retries on it does the
/// right thing here too: the owner is recomputed from live membership on every
/// attempt, and a dead owner stops being the owner once gossip notices.
pub const OWNER_UNREACHABLE_ERROR: &str =
    "CLUSTERDOWN the cluster node that owns this key is unreachable; the outcome is unknown";

/// The cluster node that arbitrates a conditional write could not be reached.
///
/// Returned by [`Store::try_del_if_eq`]; the `SETNX` side reports the same
/// condition as [`SetNxOutcome::OwnerUnreachable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerUnreachable;

impl std::fmt::Display for OwnerUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(OWNER_UNREACHABLE_ERROR)
    }
}

impl std::error::Error for OwnerUnreachable {}

/// What a [`Replicator`] answers when asked to intercept a read before the
/// local map is consulted ([`Replicator::cold_read`]).
///
/// A node that has just (re)joined a cluster holds whatever its gossip
/// catch-up has delivered so far, in per-peer version order — a copy of a key
/// that is older than one a peer has already acknowledged, with nothing local
/// to say so. Until the catch-up is complete the replicator answers reads from
/// peers instead; afterwards it steps aside and the local map is authoritative
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdRead {
    /// Not intercepting: serve the local map (then fetch through on a miss).
    Warm,
    /// A peer's current value; serve it instead of anything held locally.
    Found(Vec<u8>),
    /// No peer holds the key (or none could be asked): serve a miss rather
    /// than a local copy of unknown freshness.
    Absent,
}

/// Where a [`Replicator`] says a conditional write must be decided.
///
/// `SETNX` and compare-and-delete are only atomic where they are *decided*.
/// Each node's map is atomic under its own shard lock, but a cluster has one
/// map per node, so deciding locally lets two nodes both see "absent" and both
/// acquire. A clustered replicator therefore names a single arbiter per key and
/// either lets this node decide (it is the arbiter) or forwards the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed<T> {
    /// Decide on this node's local map — this node is the key's arbiter, or
    /// there is no cluster behind the replicator.
    Local,
    /// The owner decided; this is its answer. The owner has already applied
    /// and published the effect, so the caller must not apply it again.
    Owner(T),
    /// The owner could not be reached, or did not answer in time. Nothing was
    /// applied on this node; whether the owner applied it is **unknown** (a
    /// request that timed out may still have landed).
    OwnerUnreachable,
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
    ///
    /// Under `ephpm serve`, `Config::validate` in `ephpm-config` rejects any
    /// `[kv] compression` value outside its `KV_COMPRESSION_ALGOS` list before
    /// this runs (paths that skip validation, e.g. `ephpm dev`, still get the
    /// lenient fallback). Keep that list in sync with the arms below.
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

/// The replicable state of one hash field after a mutation — what a
/// [`Replicator`] fans out for it, and what [`Store::apply_hash_field_local`]
/// applies on a peer.
///
/// Durations are **remaining** lifetimes measured when the state was read; a
/// transport converts them to absolute times for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashFieldState {
    /// The field's value, or `None` when the field no longer exists (deleted,
    /// expired, or the whole hash is gone) — a tombstone.
    pub value: Option<Vec<u8>>,
    /// The field's own remaining TTL (`HEXPIRE`), if any.
    pub field_ttl: Option<Duration>,
    /// The hash key's remaining TTL (`EXPIRE` on the hash), if any. Carried on
    /// every field so a peer learns the key expiry whichever field reaches it
    /// first.
    pub hash_ttl: Option<Duration>,
}

impl HashFieldState {
    /// A tombstone: the field does not exist.
    #[must_use]
    pub fn deleted() -> Self {
        Self { value: None, field_ttl: None, hash_ttl: None }
    }
}

/// The `NX`/`XX`/`GT`/`LT` condition of an expire command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExpireCondition {
    /// No condition: always set.
    #[default]
    Always,
    /// Only when there is no expiry yet.
    Nx,
    /// Only when there already is an expiry.
    Xx,
    /// Only when the new expiry is later than the current one. No expiry
    /// counts as infinitely late, so `GT` never applies to a persistent item.
    Gt,
    /// Only when the new expiry is earlier than the current one. No expiry
    /// counts as infinitely late, so `LT` always applies to a persistent item.
    Lt,
}

impl ExpireCondition {
    /// Whether moving from `current` to `new` expiry is allowed (`None` =
    /// no expiry / unrepresentably far).
    #[must_use]
    pub fn allows(self, current: Option<Instant>, new: Option<Instant>) -> bool {
        match self {
            Self::Always => true,
            Self::Nx => current.is_none(),
            Self::Xx => current.is_some(),
            Self::Gt => match (current, new) {
                (None, _) => false,
                (Some(_), None) => true,
                (Some(c), Some(n)) => n > c,
            },
            Self::Lt => match (current, new) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(c), Some(n)) => n < c,
            },
        }
    }
}

/// Current wall-clock time in Unix milliseconds.
#[must_use]
pub fn unix_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Translate a store deadline into Unix milliseconds (for `EXPIRETIME` and
/// friends). Rounded to the nearest millisecond, so a deadline set from a
/// whole Unix time (`EXPIREAT`) reads back as that time; a deadline in the
/// past maps below now.
#[must_use]
pub fn instant_to_unix_ms(at: Instant) -> i64 {
    let rounded_ms = |d: Duration| i64::try_from((d.as_nanos() + 500_000) / 1_000_000);
    let now = Instant::now();
    let now_ms = unix_ms_now();
    if at >= now {
        now_ms.saturating_add(rounded_ms(at.duration_since(now)).unwrap_or(i64::MAX))
    } else {
        now_ms.saturating_sub(rounded_ms(now.duration_since(at)).unwrap_or(i64::MAX))
    }
}

/// Drop a removed value **off the calling thread** when freeing it is
/// expensive (a hash with many fields), the way Redis' lazyfree does for
/// `UNLINK` (and, since Valkey 8, for `DEL`).
///
/// Freeing a hash walks every field allocation; for a million fields that is
/// tens of milliseconds a PHP request or a RESP connection would otherwise
/// spend inside `DEL`. Small values are dropped inline — handing them to
/// another thread costs more than freeing them.
pub(crate) fn lazy_drop<T: LazyFree>(value: T) {
    if value.free_cost() < LAZYFREE_THRESHOLD {
        return;
    }
    static FREER: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Box<dyn Send>>>> =
        std::sync::OnceLock::new();
    let tx = FREER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn Send>>();
        let spawned = std::thread::Builder::new()
            .name("ephpm-kv-lazyfree".into())
            .spawn(move || while rx.recv().is_ok() {});
        if spawned.is_err() {
            tracing::debug!("lazyfree thread could not be spawned; freeing inline");
        }
        std::sync::Mutex::new(tx)
    });
    let boxed: Box<dyn Send> = Box::new(value);
    // A poisoned lock or a dead thread hands the value back: free it inline.
    if let Ok(tx) = tx.lock()
        && let Err(back) = tx.send(boxed)
    {
        drop(back);
    }
}

/// Number of allocations above which [`lazy_drop`] frees off-thread (Redis'
/// `LAZYFREE_THRESHOLD`).
const LAZYFREE_THRESHOLD: usize = 64;

/// What it costs to free a value, in allocations.
pub(crate) trait LazyFree: Send + 'static {
    /// Approximate number of allocations dropping this frees.
    fn free_cost(&self) -> usize;
}

impl LazyFree for HashEntry {
    fn free_cost(&self) -> usize {
        self.fields.len()
    }
}

impl LazyFree for hash::HashField {
    fn free_cost(&self) -> usize {
        1
    }
}

/// Keyspace hit/miss counters for `INFO stats`.
///
/// Striped over cache-line-padded slots, one picked per thread, so the hot
/// read path pays a relaxed add on a line no other thread writes — a single
/// shared counter would put every `GET` in the process on one contended cache
/// line.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    hits: [PaddedCounter; STAT_STRIPES],
    misses: [PaddedCounter; STAT_STRIPES],
}

/// Stripes per counter.
const STAT_STRIPES: usize = 16;

/// One counter on its own cache line.
#[derive(Debug, Default)]
#[repr(align(64))]
struct PaddedCounter(AtomicU64);

impl Stats {
    /// This thread's stripe.
    fn stripe() -> usize {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        thread_local! {
            static SLOT: usize = NEXT.fetch_add(1, Ordering::Relaxed) % STAT_STRIPES;
        }
        SLOT.try_with(|s| *s).unwrap_or(0)
    }

    pub(crate) fn hit(&self) {
        self.hits[Self::stripe()].0.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn miss(&self) {
        self.misses[Self::stripe()].0.fetch_add(1, Ordering::Relaxed);
    }

    fn sum(counters: &[PaddedCounter]) -> u64 {
        counters.iter().map(|c| c.0.load(Ordering::Relaxed)).fold(0, u64::wrapping_add)
    }
}

/// One string entry copied out of the store for the snapshot writer
/// (`crate::snapshot`). `data` is the bytes **as stored** — possibly
/// compressed, which `compressed` records — so writing a snapshot never
/// decodes a value.
#[derive(Debug, Clone)]
pub(crate) struct StringRecord {
    pub key: String,
    pub data: Bytes,
    pub compressed: bool,
    pub expires_at: Option<Instant>,
}

/// One field of a [`HashRecord`]. `expires_at` is the field's own expiry,
/// independent of the hash's: the snapshot format carries it so a per-field
/// TTL (`HEXPIRE`) survives a restart once the store tracks one.
#[derive(Debug, Clone)]
pub(crate) struct HashFieldRecord {
    pub name: String,
    pub value: Bytes,
    pub expires_at: Option<Instant>,
}

/// One hash entry copied out of the store for the snapshot writer.
#[derive(Debug, Clone)]
pub(crate) struct HashRecord {
    pub key: String,
    pub fields: Vec<HashFieldRecord>,
    pub expires_at: Option<Instant>,
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

    /// **Publish only** — broadcast an already-applied local value to peers.
    ///
    /// Called by the read-modify-write ops (`incr`, `set_nx`, `append`,
    /// `persist`) after they have mutated the local map. Unlike
    /// [`replicate_set`](Self::replicate_set), the local copy is **already
    /// correct**, so an implementation MUST NOT write `value` back into the
    /// local store.
    ///
    /// # Why that is a hard requirement
    ///
    /// `value` is the result of *one* increment, captured under that
    /// increment's shard lock. Publishing is asynchronous, so two concurrent
    /// increments publish `n` and `n+1` in an unspecified order. A local
    /// write-back would therefore let the older publish overwrite the newer
    /// local value and lose an update on the origin node — precisely the
    /// per-node atomicity `Store::incr_by_with_ttl` takes the shard lock to
    /// guarantee. Fanning the value out to *peers* is last-arrival-wins by
    /// design (see [`Store::replicate_published_value`]); regressing the
    /// origin's own copy is not.
    ///
    /// Required rather than defaulted to `replicate_set` on purpose: a default
    /// would silently reintroduce the write-back in every new implementation.
    fn replicate_published(&self, key: String, value: Vec<u8>, ttl: Option<Duration>);

    /// **Publish only** — broadcast a delete that has *already* been applied to
    /// the local map, dropping the key on every peer.
    ///
    /// Called by [`Store::del_if_eq`] after its compare-and-delete has fired
    /// under the key's shard lock. It is the delete-side mirror of
    /// [`replicate_published`](Self::replicate_published): the local copy is
    /// **already gone**, so an implementation MUST NOT call
    /// [`Store::remove_local`] (or any other local removal) from here.
    ///
    /// # Why the local re-removal is forbidden
    ///
    /// [`replicate_remove`](Self::replicate_remove) exists to service a plain
    /// `DEL`, whose contract is "make this key not exist" — so it *does* drop
    /// the local copy before fanning out. A conditional delete is different:
    /// its contract is "delete this key **only while it still equals my
    /// token**". After `del_if_eq` has removed the matching entry, a concurrent
    /// writer may immediately recreate the key with a *different* value (a fresh
    /// lock acquisition, say). Re-removing locally here would destroy that new
    /// value — reintroducing the exact delete-after-recreate race the primitive
    /// exists to close. Broadcasting a tombstone without touching the local map
    /// preserves the recreated value locally while letting peers converge by the
    /// same last-arrival-wins rule as every other write in this tier.
    ///
    /// Required rather than defaulted on purpose, for the same reason
    /// [`replicate_published`](Self::replicate_published) is: a default that
    /// delegated to `replicate_remove` would silently reintroduce that
    /// local-re-removal in every new implementation, and a no-op default would
    /// silently drop the tombstone — leaving a released lock alive on every peer
    /// until its TTL, an invisible divergence.
    fn replicate_remove_published(&self, key: String);

    /// Handle a public [`Store::set_broadcast`]: put this value on **every**
    /// node, whatever its size.
    ///
    /// A replicator that shards large values across a subset of the cluster
    /// MUST override this — that is the whole point of the call. The default
    /// delegates to [`replicate_set`](Self::replicate_set), which is correct
    /// only for a replicator that already reaches every node (and for the
    /// test doubles in this crate).
    fn replicate_broadcast(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        self.replicate_set(key, value, ttl)
    }

    /// Fetch `key` from the rest of the cluster, for a node that has no local
    /// copy. Returns `None` when the key is nowhere to be found, and for any
    /// replicator with no cluster behind it.
    ///
    /// Boxed rather than `async fn` so the trait stays object-safe: it is
    /// stored as `Arc<dyn Replicator>`.
    fn fetch_cluster<'a>(
        &'a self,
        key: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + 'a>> {
        let _ = key;
        Box::pin(std::future::ready(None))
    }

    /// Fetch `key` from the rest of the cluster **synchronously**, for a
    /// reader on this node that found no local copy — the RESP `GET` and PHP
    /// `ephpm_kv_get` path (see [`Store::get_through`]).
    ///
    /// A clustered implementation must bound this: it blocks a request
    /// thread. The default is `None`, correct for a replicator with no
    /// cluster behind it and for the test doubles in this crate.
    fn fetch_through(&self, key: &str) -> Option<Vec<u8>> {
        let _ = key;
        None
    }

    /// Intercept a [`Store::get_through`] **before** the local map is read.
    ///
    /// The default answers [`ColdRead::Warm`] — never intercept — which is
    /// correct for a replicator with no cluster behind it and for the test
    /// doubles in this crate. A clustered implementation answers from peers
    /// while this node is still catching up on the gossip tier after a
    /// (re)start, because a local hit in that window can be a copy that a
    /// peer has already superseded (see [`ColdRead`]). It is a **blocking**
    /// call on the caller's thread when it goes to the network, bounded by the
    /// implementation exactly like `fetch_through`.
    fn cold_read(&self, key: &str) -> ColdRead {
        let _ = key;
        ColdRead::Warm
    }

    /// Decide where a `SETNX` on `key` is arbitrated.
    ///
    /// Called by [`Store::set_nx_outcome`] **before** it touches the local map.
    /// A clustered implementation must pick exactly one arbiter per key (the
    /// same node for every caller, given the same membership view) and, when
    /// that is another node, perform the conditional insert *there* and return
    /// [`Routed::Owner`] with its answer. Returning [`Routed::Local`] makes
    /// this node decide on its own map.
    ///
    /// The default is [`Routed::Local`], which is correct for a replicator
    /// with no cluster behind it (and for the test doubles in this crate). A
    /// replicator that spans several nodes MUST override this — otherwise every
    /// node decides `SETNX` on its own copy and two nodes can both acquire the
    /// same lock.
    ///
    /// This is a **blocking** call: it may wait on a network round trip to the
    /// owner. Callers are the RESP dispatcher, the PHP bridge and middleware —
    /// all synchronous — so an async implementation must bridge accordingly.
    fn route_set_nx(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Routed<SetNxOutcome> {
        let _ = (key, value, ttl);
        Routed::Local
    }

    /// Decide where a compare-and-delete on `key` is arbitrated — the release
    /// side of [`route_set_nx`](Self::route_set_nx), with the same contract.
    ///
    /// It must route to the **same** arbiter as `route_set_nx`: a release
    /// decided against a different node's copy can match a token that the
    /// arbiter has already replaced, and delete a lock someone else now holds.
    fn route_del_if_eq(&self, key: &str, token: &[u8]) -> Routed<bool> {
        let _ = (key, token);
        Routed::Local
    }

    /// Decide where a compare-and-set (`SET key value IFEQ expected`) on `key`
    /// is arbitrated — the same arbiter as [`route_set_nx`](Self::route_set_nx)
    /// and [`route_del_if_eq`](Self::route_del_if_eq), with the same contract.
    /// A clustered implementation MUST override this for the same reason.
    fn route_set_if_eq(
        &self,
        key: &str,
        expected: &[u8],
        value: &[u8],
        ttl: SetTtl,
    ) -> Routed<SetIfEqOutcome> {
        let _ = (key, expected, value, ttl);
        Routed::Local
    }

    /// Run **one hash-field mutation** as a replicated write and publish the
    /// field's resulting state.
    ///
    /// `apply` performs the local mutation under the store's own locks and
    /// returns the field's resulting [`HashFieldState`], or `None` when it
    /// changed nothing (an `HSETNX` that lost, an `HDEL` of an absent field) —
    /// in which case nothing may be published *and* the write must leave no
    /// trace in the replicator's ordering, or a peer's genuinely newer write
    /// could be refused here.
    ///
    /// An implementation orders writes **per field** (`key` + `field`), takes
    /// that position *before* calling `apply`, and calls `apply` at most once,
    /// while holding the position — exactly what
    /// [`replicate_set`](Self::replicate_set) does for a string key — so the
    /// local copy and what peers receive are the same point in that order. It
    /// may decline to call `apply` at all when a newer write for the field has
    /// already landed (last-writer-wins: this write lost).
    ///
    /// Returns whether `apply` ran.
    ///
    /// Required rather than defaulted, like the other publish hooks: a default
    /// that ran `apply` without publishing would silently leave hashes
    /// node-local in every new implementation — which is precisely how hashes
    /// went unreplicated before this hook existed.
    fn replicate_hash_field(
        &self,
        key: &str,
        field: &str,
        apply: &mut dyn FnMut() -> Option<HashFieldState>,
    ) -> bool;

    /// The largest value a hash field may hold and still replicate, or `None`
    /// for no limit. Writes of larger values are refused with
    /// [`HashWriteError::TooLargeForCluster`] rather than stored node-locally.
    fn hash_field_value_limit(&self) -> Option<usize> {
        None
    }
}

/// Thread-safe in-memory KV store.
pub struct Store {
    /// The main data map (string values).
    data: DashMap<String, Entry>,
    /// Hash values stored separately to avoid Entry enum complexity.
    hashes: DashMap<String, HashEntry>,
    /// Hashes with something that expires — the key itself or a field —
    /// mapped to (no later than) the earliest such deadline. The hash
    /// counterpart of `ttl_keys`, sampled by active expiry. A hint, like
    /// `ttl_keys`: an early one costs one wasted look, and the look corrects
    /// it.
    hash_ttl_keys: DashMap<String, Instant>,
    /// Where active expiry stopped in `hash_ttl_keys`.
    hash_expire_cursor: AtomicU64,
    /// Keyspace hit/miss counters (`INFO stats`).
    stats: Stats,
    /// Keys whose entries currently carry an expiry, mapped to that expiry.
    ///
    /// # Why a side index
    ///
    /// Active expiry ([`active_expire_cycle`](Self::active_expire_cycle))
    /// samples this index instead of the `data` map, so its cost does not
    /// depend on how many persistent keys sit alongside the TTL'd ones (the
    /// session / rate-limit / cache-key pattern: a small TTL'd subset next to a
    /// much larger persistent set). Holding the deadline here lets a sample
    /// decide "expired?" without touching `data` at all; only the expired
    /// candidates cost a map lookup.
    ///
    /// Maintained by every write path through [`ttl_index_put`] and
    /// [`ttl_index_drop`]: `set_local`, `set_nx_outcome_local`,
    /// `expire_local`, `persist`, `remove_local`, `del_if_eq_local`,
    /// `incr_by_with_ttl` for create-with-ttl, the reap paths, and `flush`. It
    /// is a **hint**: the authority on expiry is still `Entry::expires_at`, and
    /// a reap only removes an entry whose `expires_at` equals the hint's
    /// deadline. Removals are likewise conditional on the deadline, so a
    /// remove racing a re-write cannot delete the *new* entry's hint.
    ///
    /// [`ttl_index_put`]: Self::ttl_index_put
    /// [`ttl_index_drop`]: Self::ttl_index_drop
    ttl_keys: DashMap<String, Instant>,
    /// Where active expiry stopped in `ttl_keys` (a packed `MapCursor`). The
    /// next round continues from here, so every TTL hint is reached within one
    /// sweep however the work is split across cycles and writes.
    expire_cursor: AtomicU64,
    /// State of the splitmix64 generator that picks random sample positions
    /// for eviction. Not cryptographic; it only spreads samples over the map.
    rng: AtomicU64,
    /// The approximated-LRU eviction pool: the best victims seen by recent
    /// samples, oldest first (see `Store::next_victim`).
    eviction_pool: parking_lot::Mutex<Vec<Victim>>,
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
    /// Per-key watch slots backing [`Store::wait_for_change`]
    /// (`ephpm_kv_wait()` from PHP). Created lazily on first wait, never
    /// reclaimed — see the [`watch`] module docs for the design and the
    /// zero-cost-when-unused invariant.
    watchers: DashMap<String, Arc<WatchSlot>>,
    /// Number of live entries in `watchers`. Write paths check this with
    /// a single atomic load before doing any watch-related work, so
    /// deployments that never call `wait_for_change` pay exactly one
    /// uncontended `Acquire` load per write — nothing else.
    watch_slots: AtomicUsize,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("data_len", &self.data.len())
            .field("hashes_len", &self.hashes.len())
            .field("hash_ttl_keys_len", &self.hash_ttl_keys.len())
            .field(
                "hash_expire_cursor",
                &MapCursor::unpack(self.hash_expire_cursor.load(Ordering::Relaxed)),
            )
            .field("stats", &self.stats)
            .field("ttl_keys_len", &self.ttl_keys.len())
            .field("expire_cursor", &MapCursor::unpack(self.expire_cursor.load(Ordering::Relaxed)))
            .field("eviction_pool_len", &self.eviction_pool.try_lock().map(|p| p.len()))
            .field("rng", &self.rng.load(Ordering::Relaxed))
            .field("mem_used", &self.mem_used.load(Ordering::Relaxed))
            .field("config", &self.config)
            .field("replicator_installed", &self.replicator.read().is_ok_and(|g| g.is_some()))
            .field("anchor", &self.anchor)
            .field("watchers_len", &self.watchers.len())
            .field("watch_slots", &self.watch_slots.load(Ordering::Relaxed))
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
            hash_ttl_keys: DashMap::new(),
            hash_expire_cursor: AtomicU64::new(0),
            stats: Stats::default(),
            ttl_keys: DashMap::new(),
            expire_cursor: AtomicU64::new(0),
            rng: AtomicU64::new(rng_seed()),
            eviction_pool: parking_lot::Mutex::new(Vec::with_capacity(EVICTION_POOL_SIZE)),
            mem_used: AtomicUsize::new(0),
            config,
            replicator: std::sync::RwLock::new(None),
            anchor: Instant::now(),
            watchers: DashMap::new(),
            watch_slots: AtomicUsize::new(0),
        })
    }

    /// The [`StoreConfig`] this store was built with.
    ///
    /// Memory limit, eviction policy, and compression are fixed at
    /// construction, so this is read-only. Exposed so a caller that hands a
    /// config template to a
    /// [`MultiTenantStore`](crate::multi_tenant::MultiTenantStore) can check
    /// that the per-site stores actually inherited it.
    #[must_use]
    pub fn config(&self) -> &StoreConfig {
        &self.config
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

    /// Whether a [`Replicator`] is installed, without cloning the `Arc`.
    ///
    /// Used by the read-modify-write ops to decide whether it is worth
    /// retaining a copy of the value they are about to publish: on a
    /// standalone node there is no replicator and no copy is made.
    fn has_replicator(&self) -> bool {
        self.replicator.read().is_ok_and(|g| g.is_some())
    }

    /// Publish a read-modify-write op's **own result** through the installed
    /// [`Replicator`], after it has already applied it locally.
    ///
    /// `value` and `expires_at` MUST be the values the caller captured while
    /// still holding the key's shard lock — never re-read afterwards. Re-reading
    /// is what made concurrent same-node `incr`s publish each other's totals and
    /// (via the replicator's local write-back) lose an update on the origin
    /// node. The lock-scoped capture makes each call publish exactly the state
    /// *it* produced, and [`Replicator::replicate_published`] is publish-only so
    /// an out-of-order publish cannot rewrite the local copy.
    ///
    /// # Why this exists
    ///
    /// [`Store::set`] / [`Store::remove`] / [`Store::expire`] can hand the
    /// caller's intent straight to the replicator, because the intent *is* the
    /// new state. The read-modify-write ops cannot: `incr`, `set_nx` and
    /// `append` only know the resulting value *after* they have taken the shard
    /// lock and applied it. Before this helper they simply skipped replication
    /// entirely — a mutation that changed the local map and was never broadcast.
    ///
    /// That was a **silent divergence bug on every keyspace, global included**:
    /// an `INCR`ed counter (`ephpm_kv_incr`) advanced on the writing node and
    /// stayed frozen on every peer forever, with no warning logged. It is what
    /// made per-vhost `switchboard:gen` read `1` on node-1 and `0` on nodes 2/3
    /// indefinitely.
    ///
    /// # Semantics, honestly stated
    ///
    /// The result is replicated as a **SET of the resulting value**, not as the
    /// operation. Replication therefore stays last-arrival-wins, exactly like
    /// every other write in this tier — it is *not* a CRDT counter and *not* a
    /// distributed lock:
    ///
    /// * Two nodes incrementing the same counter concurrently can lose an
    ///   update (both publish their own total; the later arrival wins).
    /// * [`Store::set_nx`] and [`Store::del_if_eq`] are **not** decided here:
    ///   a clustered replicator routes them to the key's single owner first
    ///   ([`Replicator::route_set_nx`]), and this publish is the owner fanning
    ///   its decision out. See [`Store::set_nx_outcome`] for the guarantee and
    ///   its membership-change limits.
    ///
    /// The counter half is strictly better than the previous behaviour (no
    /// replication at all, so peers never converged) and is documented rather
    /// than implied. True cross-node atomic counters need a consensus tier,
    /// which this KV deliberately is not.
    fn replicate_published_value(&self, key: &str, value: Vec<u8>, expires_at: Option<Instant>) {
        let Some(rep) = self.active_replicator() else {
            return;
        };
        // Carry the captured expiry across as a remaining lifetime so peers land
        // on the same deadline rather than resurrecting the key as immortal —
        // e.g. a fixed-window rate-limit counter incremented in the last
        // instant of its window must not become permanently immortal on every
        // node. A deadline already reached is not broadcast at all; the peers'
        // own lazy expiry converges them.
        let ttl = match expires_at {
            None => None,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    tracing::debug!(key, "replicate: key at or past its expiry; not broadcasting");
                    return;
                }
                Some(remaining)
            }
        };
        tracing::trace!(key, bytes = value.len(), has_ttl = ttl.is_some(), "replicate: publishing");
        rep.replicate_published(key.to_string(), value, ttl);
    }

    /// Current time as nanoseconds since the store anchor. Used for
    /// `AtomicU64`-based LRU timestamps on entries.
    #[inline]
    fn now_nanos(&self) -> u64 {
        // Saturate at u64::MAX rather than panicking on the (unreachable
        // in practice) overflow after 584 years of uptime.
        u64::try_from(self.anchor.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Turn a value into the bytes the store keeps: compressed when
    /// compression is configured, the value is at least `min_size`, and
    /// compressing actually makes it smaller. Returns `(bytes, compressed)`.
    ///
    /// The stored buffer is always trimmed to its length. `Bytes::from(Vec)`
    /// keeps the vector's whole allocation, so a value built in a growable
    /// buffer — every compressor's output, an `APPEND` result — used to hold
    /// its spare capacity for as long as it was stored, uncounted: gzip output
    /// held ~2x the bytes the accounting charged.
    fn encode_value(&self, value: Vec<u8>) -> (Bytes, bool) {
        if self.config.compression.algo != CompressionAlgo::None
            && value.len() >= self.config.compression.min_size
        {
            let compressed = compress_value(&value, self.config.compression);
            if compressed.len() < value.len() {
                return (exact_bytes(compressed), true);
            }
        }
        (exact_bytes(value), false)
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
            let observed = entry.expires_at;
            drop(entry);
            // Lazy-expiry cleanup: local only. Replicas expire the key on
            // their own timelines; no need to broadcast the reap.
            //
            // Compare-and-remove, not an unconditional reap: only drop the
            // entry we actually observed expire. A `set` that landed in the
            // gap after we dropped the read guard carries a different
            // `expires_at` and must survive — otherwise the reader destroys a
            // fresh value the writer just stored (#478).
            if let Some(exp) = observed {
                self.reap_if_expired(key, exp);
            }
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
                let observed = entry.expires_at;
                drop(entry);
                // Lazy-expiry cleanup: local only, compare-and-remove so a
                // value written after we observed the expiry is not reaped by
                // this reader (#478).
                if let Some(exp) = observed {
                    self.reap_if_expired(key, exp);
                }
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
    ///
    /// A hash key reports its whole-key expiry (field expiries are read with
    /// [`hpttl`](Self::hpttl)).
    #[must_use]
    pub fn pttl(&self, key: &str) -> Option<i64> {
        let Some(entry) = self.data.get(key) else {
            let at = self.hash_key_expiry(key)?;
            return Some(at.map_or(-1, |at| {
                let remaining = at.saturating_duration_since(Instant::now());
                i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX)
            }));
        };
        if entry.is_expired() {
            let observed = entry.expires_at;
            drop(entry);
            // Lazy-expiry cleanup: local only, compare-and-remove so a value
            // written after we observed the expiry is not reaped here (#478).
            if let Some(exp) = observed {
                self.reap_if_expired(key, exp);
            }
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

    /// Collect keys matching a Redis glob pattern (`*`, `?`, `[...]`,
    /// `[^...]`, ranges, `\` escapes — byte-for-byte Redis `stringmatchlen`
    /// semantics, documented in the private `store::glob` module). The literal pattern `*` lists every key, including an
    /// empty one, as Redis' `KEYS *` does. Returns both string and hash keys.
    ///
    /// Matching is `O(len(key) × len(pattern))` worst case per key and runs
    /// with no lock held; each shard's read lock is held only while its live
    /// keys are copied out (see `collect_keys`).
    #[must_use]
    pub fn keys(&self, pattern: &str) -> Vec<String> {
        let pattern = (pattern != "*").then_some(pattern.as_bytes());
        let mut result = Vec::new();
        let now = Instant::now();
        collect_keys(&self.data, |e| !e.is_expired(), pattern, &mut result);
        collect_keys(&self.hashes, |h| h.is_live(now), pattern, &mut result);
        result
    }

    /// The keyspace counters `INFO` reports: `(keys, keys_with_expiry,
    /// keyspace_hits, keyspace_misses)`. Key counts include expired keys not
    /// yet reaped, as `DBSIZE` does.
    #[must_use]
    pub fn keyspace_info(&self) -> (usize, usize, u64, u64) {
        (
            self.len(),
            self.ttl_keys.len() + self.hash_ttl_keys.len(),
            Stats::sum(&self.stats.hits),
            Stats::sum(&self.stats.misses),
        )
    }

    // ── Key watches (blocking wait) ──────────────────────────────

    /// Block until `key`'s watch version exceeds `last_version`, or
    /// `timeout` elapses. This is the engine behind PHP's
    /// `ephpm_kv_wait()`.
    ///
    /// Returns `Some((new_version, value))` when the version advanced
    /// (`value` is `None` if the key is absent/deleted at wakeup time),
    /// or `None` on timeout.
    ///
    /// The first call for a key creates its watch slot at version `1`,
    /// so `wait_for_change(key, 0, _)` returns immediately with the
    /// current value — the race-free "register + snapshot" step. See the
    /// [`watch`] module docs for the full protocol and versioning rules.
    ///
    /// # Blocking
    ///
    /// Parks the calling OS thread. Callers are PHP execution threads
    /// (per-request pool / worker pool) — never call this from an async
    /// task.
    ///
    /// # Clustering
    ///
    /// Wakeups fire when the **local** copy of the key is written
    /// (`set_local` / `remove_local` / in-place mutations), so on a
    /// clustered store a waiter observes changes as they land on its own
    /// node.
    #[must_use]
    pub fn wait_for_change(
        &self,
        key: &str,
        last_version: u64,
        timeout: Duration,
    ) -> Option<(u64, Option<Bytes>)> {
        let slot = self.watch_slot(key);
        let version = slot.wait_past(last_version, timeout)?;
        Some((version, self.get(key)))
    }

    /// Number of live watch slots (keys that have ever been waited on).
    #[must_use]
    pub fn watch_slot_count(&self) -> usize {
        self.watch_slots.load(Ordering::Acquire)
    }

    /// Get or lazily create the watch slot for `key`.
    fn watch_slot(&self, key: &str) -> Arc<WatchSlot> {
        if let Some(slot) = self.watchers.get(key) {
            return Arc::clone(&slot);
        }
        match self.watchers.entry(key.to_string()) {
            dashmap::Entry::Occupied(occ) => Arc::clone(occ.get()),
            dashmap::Entry::Vacant(vac) => {
                let slot = Arc::new(WatchSlot::new());
                // Publish the count BEFORE the slot becomes visible in the
                // map (we hold the shard write lock here): a writer that
                // sees the slot must also see a non-zero count, so it never
                // skips a notify for a visible slot. The reverse (count
                // visible, slot not yet) only makes a writer do one wasted
                // lookup.
                self.watch_slots.fetch_add(1, Ordering::Release);
                vac.insert(Arc::clone(&slot));
                slot
            }
        }
    }

    /// Look up the watch slot for `key`, gated by the fast-path count
    /// check. Returns `None` — at the cost of a single `Acquire` load —
    /// when no key has ever been waited on.
    ///
    /// Write paths that consume the key by value call this *before* the
    /// map insert (borrowing the key) and bump the returned slot *after*
    /// the write is visible, so a woken waiter always reads the new value.
    #[inline]
    fn watch_slot_if_any(&self, key: &str) -> Option<Arc<WatchSlot>> {
        if self.watch_slots.load(Ordering::Acquire) == 0 {
            return None;
        }
        self.watchers.get(key).map(|slot| Arc::clone(&slot))
    }

    /// Bump the watch version for `key` if it is being watched. Called by
    /// write paths that still hold the key by reference after the write.
    #[inline]
    fn notify_write(&self, key: &str) {
        if let Some(slot) = self.watch_slot_if_any(key) {
            slot.bump();
        }
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

    /// Write a value that **every** node must hold a local copy of,
    /// regardless of its size.
    ///
    /// # Why this is not just `set`
    ///
    /// [`Store::set`] hands the write to the [`Replicator`], which routes by
    /// size: small values ride gossip (all nodes), large ones are *sharded* —
    /// stored on `replication_factor` nodes and fetched through from an owner
    /// on a miss. That is the right trade for cache and session data, and the
    /// wrong one for cluster-wide state that every node must be able to read
    /// out of its own local map with no round trip and no dependency on an
    /// owner still being alive. An ACME certificate is the motivating case:
    /// every node terminates TLS with it, and it is comfortably larger than
    /// `small_key_threshold`, so plain `set` left it on a subset of the
    /// cluster where the other nodes could never see it.
    ///
    /// Sharding is a capacity optimisation. Broadcast keys opt out of it, so
    /// this is for a small, bounded set of keys — not a general-purpose write
    /// path.
    ///
    /// Falls back to a local write when no replicator is installed, which is
    /// already "every node" on a standalone node.
    pub fn set_broadcast(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        if let Some(rep) = self.active_replicator() {
            return rep.replicate_broadcast(key, value, ttl);
        }
        self.set_local(key, value, ttl)
    }

    /// Read a key, falling back to the rest of the cluster when this node has
    /// no local copy.
    ///
    /// [`Store::get`] is local-only by design — it is on the hot path for
    /// every RESP `GET` and every PHP `ephpm_kv_get`. This is the deliberate
    /// slow path for the handful of callers that must not miss a value merely
    /// because *this* node has not got it yet: a node that joined after the
    /// write, or was down when it happened.
    ///
    /// The fetched value is **not** cached locally: it arrives with no TTL
    /// information, so caching it would silently turn an expiring key into a
    /// permanent one. Callers here poll on a timer, so a round trip per poll
    /// is the cheaper mistake.
    pub async fn get_cluster(&self, key: &str) -> Option<Bytes> {
        if let Some(local) = self.get(key) {
            return Some(local);
        }
        let replicator = self.active_replicator()?;
        let value = replicator.fetch_cluster(key).await?;
        Some(Bytes::from(value))
    }

    /// Read a key, fetching it from the cluster when this node has no local
    /// copy — the **synchronous** read path for RESP `GET`/`MGET` and PHP
    /// `ephpm_kv_get`.
    ///
    /// Identical to [`Store::get`] on a standalone node. In a cluster a large
    /// value lives on `replication_factor` nodes only, so a reader elsewhere
    /// used to see `nil` for a key that exists (stress finding F-8); this asks
    /// the installed [`Replicator`] to fetch it. The replicator bounds the
    /// wait and skips the network when this node is one of the key's replicas
    /// (a miss there means the key does not exist). The fetched value is not
    /// cached here — it carries no TTL — beyond whatever the replicator's own
    /// hot-key cache keeps.
    ///
    /// The replicator is consulted **first** ([`Replicator::cold_read`]): a
    /// node still catching up on the gossip tier after a (re)start answers
    /// from its peers instead of its own map, because a local hit there can be
    /// a copy a peer has already superseded. Once caught up that call is one
    /// atomic load and the local map is served as before. Standalone nodes
    /// have no replicator and skip it entirely.
    ///
    /// Counts the lookup as a keyspace hit or miss (`INFO stats`), whichever
    /// path answered it.
    #[must_use]
    pub fn get_through(&self, key: &str) -> Option<Bytes> {
        let found = self.get_through_uncounted(key);
        if found.is_some() {
            self.stats.hit();
        } else {
            self.stats.miss();
        }
        found
    }

    /// [`get_through`](Self::get_through) without the hit/miss accounting.
    fn get_through_uncounted(&self, key: &str) -> Option<Bytes> {
        let replicator = self.active_replicator();
        if let Some(rep) = &replicator {
            match rep.cold_read(key) {
                ColdRead::Warm => {}
                ColdRead::Found(value) => return Some(Bytes::from(value)),
                ColdRead::Absent => return None,
            }
        }
        if let Some(local) = self.get(key) {
            return Some(local);
        }
        replicator?.fetch_through(key).map(Bytes::from)
    }

    /// Local-only write — bypasses any installed [`Replicator`]. Used by a
    /// replicator implementation to write its own local copy without
    /// re-entering itself. All other callers should use [`Store::set`].
    ///
    /// # An overwrite is atomic — the key is never absent
    ///
    /// The replacement goes through a single [`DashMap::insert`], which swaps
    /// the value under the shard's own write lock and hands back what was
    /// there. A concurrent `get` therefore reads either the old value or the
    /// new one, never `None`.
    ///
    /// This used to reclaim the old entry's bytes by `remove`-ing it *before*
    /// the insert, leaving a window — widened by the memory check and the
    /// TTL-index maintenance that sat inside it — in which the key genuinely
    /// did not exist. A read landing in that window was a spurious miss on a
    /// key nobody had deleted: a stampede on a cache key, a logged-out user on
    /// a session key, a `WAIT` observer woken to nothing (#452).
    ///
    /// # Memory accounting
    ///
    /// Because the old entry now stays in the map across the budget check,
    /// `mem_used` already counts it, so only the *delta* is reserved. The
    /// predicate is arithmetically identical to the remove-first form
    /// (`current - old + new <= limit`), so the eviction threshold has not
    /// moved. Reconciliation afterwards uses the entry `insert` actually
    /// returned rather than the peeked size: a writer that lands in between
    /// changes what is replaced, and crediting the peek would leak or
    /// double-count those bytes.
    ///
    /// # An over-budget write keeps the old value
    ///
    /// When the budget refuses the write, whatever is already stored is left
    /// alone and `false` is returned. This matches Redis under `noeviction`,
    /// where an OOM-rejected `SET` does not delete the existing key. In the
    /// remove-first form the removal had already happened by the time the
    /// check failed, so a rejected write silently *destroyed* the value it was
    /// merely failing to replace — data loss from a write that reported
    /// failure.
    pub fn set_local(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        // Try compression if configured and size is above threshold.
        let (data, compressed) = self.encode_value(value);

        let now = self.now_nanos();
        let entry = match ttl {
            Some(dur) => Entry::with_expiry(data, key.len(), compressed, Instant::now() + dur, now),
            None => Entry::new(data, key.len(), compressed, now),
        };

        let new_size = entry.mem_size;
        let deadline = entry.expires_at;

        // PEEK the bytes already charged for this key — deliberately not a
        // remove. The old entry has to stay visible for the whole
        // check-and-insert (see the doc comment), and `mem_used` already
        // counts it, so only the growth needs reserving. The peek can race
        // with another writer; the reconcile after the insert is what makes
        // the accounting correct, not this number.
        let (old_size, old_had_ttl) =
            self.data.get(&key).map_or((0, false), |old| (old.mem_size, old.expires_at.is_some()));
        // A TTL'd write onto a key without one also adds a TTL-index hint.
        let index_growth =
            if deadline.is_some() && !old_had_ttl { ttl_index_cost(&key) } else { 0 };

        // Reserve the extra bytes with NO shard guard held: `ensure_memory` ->
        // `evict` takes write locks on arbitrary shards (possibly this
        // one) and would deadlock under a guard. `set_nx` and
        // `incr_by_with_ttl` make the same trade for the same reason.
        if !self.reserve(&key, (new_size + index_growth).saturating_sub(old_size)) {
            // Leave the existing entry — and its TTL hint — untouched.
            return false;
        }

        // Maintain the TTL side index: record the new deadline on a TTL'd
        // write, drop the hint on the TTL-less overwrite of a TTL'd key.
        //
        // This runs before the insert (it borrows `key`, which the insert
        // consumes), so there is a moment where the index disagrees with the
        // entry. That is not the same class of bug: `ttl_keys` is only a
        // *hint*, and active expiry reaps an entry only when its `expires_at`
        // equals the hint's deadline, so a hint that is briefly ahead of the
        // entry can at worst delay a proactive reap — it can never make a live
        // key read as missing.
        self.ttl_index_put(&key, deadline);
        // Snapshot the watch slot BEFORE `key` moves into the insert; bump
        // AFTER the insert so a woken waiter always reads the new value.
        // One Acquire load when nothing is watched (the common case).
        let watch = self.watch_slot_if_any(&key);
        // The atomic replace. `insert` swaps the value under the shard write
        // lock, so the key is continuously present to concurrent readers, and
        // returns the entry it displaced so its bytes can be reclaimed.
        let previous = self.data.insert(key, entry);
        // Reconcile from what was ACTUALLY replaced, not from `old_size`.
        // Add before subtract: the transient error is then an over-count,
        // which only makes the budget momentarily stricter, rather than an
        // under-count that could let the store overshoot its limit.
        self.mem_add(new_size);
        if let Some(old) = previous {
            self.mem_sub(old.mem_size);
        }
        if let Some(slot) = watch {
            slot.bump();
        }
        true
    }

    /// Reserve `needed` more bytes for a write to `key`, reclaiming expired
    /// keys and evicting per the policy as required (see
    /// [`ensure_memory`](Self::ensure_memory)). Logs the refusal when it fails.
    ///
    /// Every write path that grows the store goes through here, so none of
    /// them can bypass `memory_limit`. Must be called with **no shard guard
    /// held**: reclaiming and evicting lock arbitrary shards.
    fn reserve(&self, key: &str, needed: usize) -> bool {
        if self.ensure_memory(needed) {
            return true;
        }
        self.log_write_rejected(key, needed);
        false
    }

    /// Report a write refused by the memory budget: `warn!` for the first one
    /// in the process, `debug!` for every one after.
    ///
    /// The RESP layer surfaces a refusal to the client as Redis `-OOM` (#477).
    /// This line stays because not every caller reads the return value — the
    /// in-process `ephpm_kv_*` bridge hands PHP a `0`/`false` a caller may
    /// ignore, and an operator watching the log still wants one clear signal
    /// that the store is parked at its limit.
    ///
    /// It also has to go quiet: a store sitting at the limit refuses *every*
    /// write, and a `warn!` per write would bury the log in a message whose
    /// content never changes.
    fn log_write_rejected(&self, key: &str, needed: usize) {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let mem_used = self.mem_used();
        let limit = self.config.memory_limit;
        if WARNED.swap(true, Ordering::Relaxed) {
            debug!(key, needed, mem_used, limit, "write refused: over the memory limit");
            return;
        }
        tracing::warn!(
            key,
            needed,
            mem_used,
            limit,
            policy = ?self.config.eviction_policy,
            "write refused: the KV store is over its memory limit and could not free enough space \
             (expired keys were reclaimed first). The existing value at this key is left intact; a \
             RESP client is answered -OOM and a PHP ephpm_kv_* call reports failure. Raise `[kv] \
             memory_limit` or choose an evicting policy. Further refusals are logged at DEBUG."
        );
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
    ///
    /// This is a thin wrapper over [`set_nx_outcome`](Self::set_nx_outcome),
    /// which distinguishes the "key already exists" refusal from an
    /// out-of-memory refusal — a distinction the RESP layer needs but this
    /// boolean cannot carry. Callers that only need "did it store?" (the
    /// distributed-lock / idempotency-key use cases) keep this signature.
    ///
    /// In a cluster this is decided by the key's owner node; `false` also
    /// covers "the owner was unreachable" (see
    /// [`set_nx_outcome`](Self::set_nx_outcome)).
    pub fn set_nx(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) -> bool {
        matches!(self.set_nx_outcome(key, value, ttl), SetNxOutcome::Inserted)
    }

    /// Atomically set a key only if it doesn't already exist, reporting *why*
    /// the write was or was not stored.
    ///
    /// Same atomicity and memory semantics as [`set_nx`](Self::set_nx) — the
    /// existence check and the insert happen under the same per-key write lock
    /// — but returns a [`SetNxOutcome`] so the RESP layer can answer a normal
    /// `nil`/`:0` when the key already exists and Redis' `-OOM` when the write
    /// was refused for memory. The two used to be indistinguishable, so a
    /// memory-refused `SETNX` looked identical to "key already present" (#477).
    ///
    /// # Clustering: one arbiter per key
    ///
    /// When a [`Replicator`] is installed, the decision is handed to
    /// [`Replicator::route_set_nx`] first. A clustered replicator names the
    /// key's owner node and, when that is not this node, performs the insert
    /// *there* — so every node's `SETNX` on a key is serialised by the same
    /// shard lock on the same node, and two nodes can no longer both acquire
    /// it. Before this, each node checked its own copy and the result was
    /// last-arrival-wins across the cluster.
    ///
    /// The guarantee is exactly as strong as agreement on who the owner is.
    /// Ownership is computed from each node's own view of live membership, so
    /// it holds while membership is stable and can briefly fail across a
    /// membership change (a join, a leave, or the seconds it takes gossip to
    /// declare a node dead), during which two nodes can name different owners.
    /// A new owner also only knows about a lock its predecessor granted once
    /// that grant has replicated to it. This is single-arbiter routing, not
    /// consensus; the replicator's own docs give the precise window.
    ///
    /// An unreachable owner returns [`SetNxOutcome::OwnerUnreachable`] and
    /// writes nothing (fail closed). On success the value reaches *this* node's
    /// local map through normal replication, not synchronously.
    pub fn set_nx_outcome(
        &self,
        key: String,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> SetNxOutcome {
        if let Some(rep) = self.active_replicator() {
            match rep.route_set_nx(&key, &value, ttl) {
                Routed::Local => {}
                // The owner applied and published the effect itself. Mirroring
                // it into this node's map here would race the owner's own
                // replication of a *later* decision on the same key (a release
                // and re-acquire landing first) and leave a stale grant behind,
                // so this node learns the value the same way every peer does.
                Routed::Owner(outcome) => return outcome,
                Routed::OwnerUnreachable => return SetNxOutcome::OwnerUnreachable,
            }
        }
        self.set_nx_outcome_local(key, value, ttl)
    }

    /// [`set_nx_outcome`](Self::set_nx_outcome) decided on **this node's** map,
    /// bypassing [`Replicator::route_set_nx`].
    ///
    /// This is what the owner runs when it serves a `SETNX` routed to it by a
    /// peer — re-routing there could bounce the request between two nodes that
    /// momentarily disagree about ownership. The winning value is still
    /// published through the installed replicator. Everything else should call
    /// [`set_nx_outcome`](Self::set_nx_outcome).
    pub fn set_nx_outcome_local(
        &self,
        key: String,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> SetNxOutcome {
        // Fast path: peek without taking the per-key write lock. If the
        // key is already present and live we can bail before triggering
        // any eviction work. The TOCTOU window between this peek and the
        // entry() lock below is fine — the locked check below catches
        // it; the peek just saves an unnecessary `ensure_memory` call
        // for the common "already taken" case.
        if let Some(existing) = self.data.get(&key)
            && !existing.is_expired()
        {
            return SetNxOutcome::Exists;
        }

        // Keep the *logical* bytes for the publish below, before compression
        // consumes `value`. Only when a replicator is installed — a standalone
        // node must not pay a copy per `SETNX`.
        let published_value = self.has_replicator().then(|| value.clone());

        // Build the candidate entry first so we can compute its size
        // before reserving memory.
        let (data, compressed) = self.encode_value(value);

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
        let index = if new_entry.expires_at.is_some() { ttl_index_cost(&key) } else { 0 };
        if !self.reserve(&key, new_size + index) {
            return SetNxOutcome::OutOfMemory;
        }

        let published_expiry = new_entry.expires_at;
        // Atomic check-and-insert. The shard write lock held by `entry()`
        // serialises concurrent set_nx calls for this key.
        let key_ref = match self.data.entry(key) {
            dashmap::Entry::Occupied(mut occ) => {
                if !occ.get().is_expired() {
                    // Lost the race; another writer landed first. We
                    // already ran `ensure_memory` which may have evicted
                    // unrelated keys — that's wasted work but not a
                    // correctness bug.
                    return SetNxOutcome::Exists;
                }
                // The existing entry has expired; reclaim its bytes and
                // replace it.
                self.mem_sub(occ.get().mem_size);
                self.mem_add(new_size);
                let k = occ.key().clone();
                occ.insert(new_entry);
                k
            }
            dashmap::Entry::Vacant(vac) => {
                self.mem_add(new_size);
                let k = vac.key().clone();
                vac.insert(new_entry);
                k
            }
        };
        self.ttl_index_put(&key_ref, published_expiry);
        // Wake any waiters — the insert is visible at this point.
        self.notify_write(&key_ref);
        // Broadcast the winning value. This path already *has* the exact
        // bytes and TTL it inserted (they are the caller's arguments), so
        // there is nothing to re-read. In a cluster this node is the key's
        // owner (or the caller routed here deliberately), so this publish is
        // the arbiter announcing its decision.
        if let Some(value) = published_value {
            self.replicate_published_value(&key_ref, value, published_expiry);
        }
        SetNxOutcome::Inserted
    }

    /// Atomically delete a key **only if** its current value equals `token`
    /// (compare-and-delete). Returns `true` if the key was deleted by this
    /// call, `false` otherwise.
    ///
    /// This is the delete-side companion to [`set_nx`](Self::set_nx): together
    /// they make a KV lock *safe to release*. A holder acquires the lock with
    /// `set_nx(key, my_token, ttl)` and releases it with `del_if_eq(key,
    /// my_token)`. Because the release only fires while the stored value is
    /// still the holder's own token, a holder that overran its TTL — and whose
    /// lock was therefore taken over by someone else — can no longer delete the
    /// **new** owner's lock. A plain [`remove`](Self::remove) release cannot
    /// make that distinction and will happily unlock a lock it no longer owns.
    ///
    /// # Return semantics (precise)
    ///
    /// * key present and its value equals `token` → the entry is removed;
    ///   returns `true`.
    /// * key present but its value differs from `token` → **no-op**; returns
    ///   `false` (someone else holds it now).
    /// * key absent, or present but expired → **no-op**; returns `false` (an
    ///   expired entry is treated as absent and is left for the normal
    ///   lazy/proactive reap, exactly as [`get`](Self::get) treats it).
    ///
    /// The comparison is byte-exact against the **logical** (post-decompression)
    /// value, so it is agnostic to whether the entry happens to be stored
    /// compressed.
    ///
    /// # Atomicity
    ///
    /// The compare and the delete happen under the **same** per-key shard write
    /// lock via [`DashMap::remove_if`], whose predicate is evaluated while the
    /// shard is exclusively locked. A concurrent [`set`](Self::set) /
    /// [`set_nx`](Self::set_nx) on this key is serialised against it: it either
    /// lands *before* (the predicate then sees the new value and declines) or
    /// *after* (we have already removed). There is no window in which a value
    /// this call did not observe gets deleted — the property `remove` cannot
    /// offer.
    ///
    /// # Clustering
    ///
    /// Like [`set_nx_outcome`](Self::set_nx_outcome), the compare-and-delete is
    /// decided by the key's **owner** when a clustered [`Replicator`] is
    /// installed ([`Replicator::route_del_if_eq`]) — the same node that decides
    /// `SETNX`, so a release is compared against the arbiter's copy and not
    /// against a possibly-stale local one. (A stale local copy was exactly how
    /// a holder whose lock had expired and been re-granted elsewhere could still
    /// "match" its old token and broadcast a delete of the new holder's lock.)
    /// The same membership-change limits apply.
    ///
    /// The owner fans the deletion out via
    /// [`Replicator::replicate_remove_published`] — a *publish-only* tombstone
    /// that does **not** re-remove the owner's local copy, so a value recreated
    /// there between the removal and the broadcast survives.
    ///
    /// An unreachable owner deletes nothing and returns `false`; use
    /// [`try_del_if_eq`](Self::try_del_if_eq) to tell that apart from a token
    /// mismatch.
    pub fn del_if_eq(&self, key: &str, token: &[u8]) -> bool {
        self.try_del_if_eq(key, token).unwrap_or(false)
    }

    /// [`del_if_eq`](Self::del_if_eq), distinguishing an unreachable owner
    /// (`Err`) from "nothing matched" (`Ok(false)`).
    ///
    /// # Errors
    ///
    /// Returns [`OwnerUnreachable`] when a clustered replicator routed the
    /// compare-and-delete to an owner node that could not be reached. Nothing
    /// was deleted anywhere in that case.
    pub fn try_del_if_eq(&self, key: &str, token: &[u8]) -> Result<bool, OwnerUnreachable> {
        if let Some(rep) = self.active_replicator() {
            match rep.route_del_if_eq(key, token) {
                Routed::Local => {}
                // As with SETNX: the owner removed and published; this node's
                // copy goes away when the owner's tombstone arrives.
                Routed::Owner(deleted) => return Ok(deleted),
                Routed::OwnerUnreachable => return Err(OwnerUnreachable),
            }
        }
        Ok(self.del_if_eq_local(key, token))
    }

    /// [`del_if_eq`](Self::del_if_eq) decided on **this node's** map, bypassing
    /// [`Replicator::route_del_if_eq`] — what the owner runs when serving a
    /// routed release. The deletion is still published through the installed
    /// replicator.
    pub fn del_if_eq_local(&self, key: &str, token: &[u8]) -> bool {
        // Atomic compare-and-delete on the LOCAL map. `remove_if` evaluates the
        // predicate under the shard write lock and only removes when it returns
        // true, so the compare and the delete are one indivisible step.
        let removed = self.data.remove_if(key, |_, entry| {
            // An expired entry is logically absent: decline, and leave it for
            // the normal reap rather than counting it as a match.
            if entry.is_expired() {
                return false;
            }
            if entry.compressed {
                // Compare against the decompressed logical value. A decode
                // failure (corrupt/foreign payload) is treated as "does not
                // match" — never delete on an ambiguous compare.
                decompress_value(&entry.data, self.config.compression.algo)
                    .is_some_and(|plain| plain.as_ref() == token)
            } else {
                entry.data.as_ref() == token
            }
        });
        let Some((_, old)) = removed else {
            return false;
        };
        // Reconcile the same bookkeeping `remove_local` maintains: memory
        // accounting, the TTL side index, and watch wakeups (the key is now
        // absent, so a `wait_for_change` observer must be woken).
        self.mem_sub(old.mem_size);
        self.ttl_index_drop(key, old.expires_at);
        self.notify_write(key);
        // Clustered: broadcast a publish-only tombstone. The compare already
        // happened locally under the shard lock; peers drop the key by
        // last-arrival-wins. Crucially this must NOT re-remove locally (see
        // `Replicator::replicate_remove_published`) — a value recreated here
        // between the removal above and this call has to survive on this node.
        if let Some(rep) = self.active_replicator() {
            rep.replicate_remove_published(key.to_string());
        }
        true
    }

    /// Remove a key, returning `true` if it existed. Removes from both
    /// string and hash storage.
    ///
    /// When a [`Replicator`] is installed, the delete is delegated to it so
    /// remote copies are dropped as well. See [`Store::remove_local`] for
    /// the bypass variant.
    ///
    /// # Hashes in a cluster
    ///
    /// Peers drop a whole hash when the key's tombstone reaches them, but the
    /// replication tier also holds each field as its own replicated item, and
    /// those would otherwise be re-delivered (to a node that joins later, or
    /// by a repair pass) and resurrect the hash. So deleting a hash also
    /// publishes a tombstone for every field this node knows. A field written
    /// concurrently on another node that had not reached this one yet is not
    /// covered and survives the delete — per-field last-writer-wins, the same
    /// window every write in this tier has.
    pub fn remove(&self, key: &str) -> bool {
        if let Some(rep) = self.active_replicator() {
            let fields = self.hash_field_names(key);
            let removed = rep.replicate_remove(key);
            for field in &fields {
                let mut tombstone = || Some(HashFieldState::deleted());
                rep.replicate_hash_field(key, field, &mut tombstone);
            }
            return removed;
        }
        self.remove_local(key)
    }

    /// Local-only remove — bypasses any installed [`Replicator`]. Used by a
    /// replicator implementation to drop its own local copy.
    pub fn remove_local(&self, key: &str) -> bool {
        let string_removed = if let Some((_, old)) = self.data.remove(key) {
            self.mem_sub(old.mem_size);
            // Keep the TTL index proportional to live TTL'd keys. Conditional
            // on the removed entry's deadline, so a concurrent re-write's
            // fresh hint survives.
            self.ttl_index_drop(key, old.expires_at);
            true
        } else {
            false
        };
        let hash_removed = self.hash_remove(key);
        let removed = string_removed || hash_removed;
        if removed {
            // A delete is a state change: wake waiters so they observe the
            // key as absent (covers explicit del, lazy-expiry reaps, and
            // LRU eviction — all funnel through remove_local).
            self.notify_write(key);
        }
        removed
    }

    /// Compare-and-remove an entry that a read path just observed expired.
    ///
    /// The lazy-expiry reap on `get`/`exists`/`pttl` used to call
    /// [`remove_local`](Self::remove_local) unconditionally after dropping the
    /// read guard. A `set` that landed in the gap between the observation and
    /// the removal was then destroyed by the *reader* — the writer's success
    /// silently undone. That is the stampede-refresh pattern exactly (write a
    /// fresh value as the old one expires), so the window is aimed at, not
    /// incidental (#478).
    ///
    /// The removal is now conditional on the entry still carrying the exact
    /// `expires_at` the caller observed. Every TTL'd write builds a *fresh*
    /// [`Instant`] (`Instant::now() + ttl`), and a TTL-less write stores
    /// `None`, so a value written after the observation never compares equal —
    /// it is preserved. A wholly new entry likewise does not match. The
    /// observed instant is therefore a stable identity for "the entry I saw
    /// expire", and equality alone suffices: it was already in the past when
    /// observed, so a matching entry is still expired.
    ///
    /// Returns `true` if this call removed the entry.
    ///
    /// # Locking
    ///
    /// [`DashMap::remove_if`] evaluates the predicate under the shard write
    /// lock and hands back the displaced entry with the lock already released.
    /// The predicate touches no other lock, and the reconciliation
    /// (`mem_sub` / `ttl_keys` / `notify_write`) runs after the guard is
    /// dropped. So — unlike the eviction path — this never holds a shard guard
    /// across [`ensure_memory`](Self::ensure_memory); the #476 deadlock
    /// constraint does not apply here and is not reintroduced.
    fn reap_if_expired(&self, key: &str, observed: Instant) -> bool {
        let removed = self.data.remove_if(key, |_, entry| entry.expires_at == Some(observed));
        // Drop the hint for the entry we observed — whether we reaped it or a
        // concurrent remove got there first. A hint carrying a different
        // deadline belongs to a newer write and is kept.
        self.ttl_index_drop(key, Some(observed));
        if let Some((_, old)) = removed {
            self.mem_sub(old.mem_size);
            // A reap is a state change (the key becomes absent) — wake waiters,
            // matching remove_local's contract.
            self.notify_write(key);
            true
        } else {
            false
        }
    }

    /// Record `key`'s expiry in the TTL index, or drop its hint when the new
    /// value has none. Called by write paths that have just stored (or are
    /// about to store) an entry with expiry `deadline`.
    ///
    /// The index holds its own copy of the key, so a hint is charged to
    /// `mem_used` ([`ttl_index_cost`]) when it is created and credited back
    /// when it is removed. Uncounted, it was ~25% of a TTL'd store's real
    /// footprint.
    fn ttl_index_put(&self, key: &str, deadline: Option<Instant>) {
        match deadline {
            Some(at) => {
                if let Some(mut slot) = self.ttl_keys.get_mut(key) {
                    *slot = at;
                } else if self.ttl_keys.insert(key.to_string(), at).is_none() {
                    self.mem_add(ttl_index_cost(key));
                }
            }
            None => {
                if self.ttl_keys.remove(key).is_some() {
                    self.mem_sub(ttl_index_cost(key));
                }
            }
        }
    }

    /// Drop `key`'s TTL hint, but only if it still records `deadline` — the
    /// expiry of the entry the caller just removed. A hint with any other
    /// deadline was written by a newer write and stays. `None` (the removed
    /// entry had no expiry) has no hint to drop.
    fn ttl_index_drop(&self, key: &str, deadline: Option<Instant>) {
        if let Some(at) = deadline
            && self.ttl_keys.remove_if(key, |_, hint| *hint == at).is_some()
        {
            self.mem_sub(ttl_index_cost(key));
        }
    }

    /// Set an expiry on an existing key. Returns `false` if the key doesn't exist.
    ///
    /// When a [`Replicator`] is installed, the TTL update is delegated so
    /// remote copies see the change. See [`Store::expire_local`] for the
    /// bypass variant.
    ///
    /// Applies to a hash key too (its whole-key expiry); every field is then
    /// re-published so peers carry the new key expiry.
    pub fn expire(&self, key: &str, ttl: Duration) -> bool {
        let string = if let Some(rep) = self.active_replicator() {
            rep.replicate_expire(key, ttl)
        } else {
            self.expire_local(key, ttl)
        };
        let hash = self.hash_set_key_expiry(key, Instant::now().checked_add(ttl));
        string || hash
    }

    /// Local-only expire of a **string** key — bypasses any installed
    /// [`Replicator`]. (A hash's key expiry replicates with its fields; see
    /// [`expire`](Self::expire).)
    pub fn expire_local(&self, key: &str, ttl: Duration) -> bool {
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.remove_local(key);
                return false;
            }
            let deadline = Instant::now() + ttl;
            entry.expires_at = Some(deadline);
            drop(entry);
            // Track the new deadline in the TTL side index so active expiry
            // can find this key without scanning the whole map.
            self.ttl_index_put(key, Some(deadline));
            true
        } else {
            false
        }
    }

    /// Remove the expiry from a key (make it persistent). Returns `true` if
    /// an expiry was removed — `false` if the key does not exist or had none.
    /// Applies to a hash key's whole-key expiry too.
    pub fn persist(&self, key: &str) -> bool {
        let string = self.persist_string(key);
        let hash = self.hash_set_key_expiry(key, None);
        string || hash
    }

    /// [`persist`](Self::persist) for the string keyspace only.
    pub(crate) fn persist_string(&self, key: &str) -> bool {
        if let Some(mut entry) = self.data.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                // Lazy-expiry cleanup: local only.
                self.remove_local(key);
                return false;
            }
            let had_ttl = entry.expires_at.is_some();
            entry.expires_at = None;
            // Capture the logical value under the entry guard, so what is
            // published is the state THIS call made permanent rather than
            // whatever a concurrent writer left behind afterwards.
            let published_value = (had_ttl && self.has_replicator()).then(|| {
                if entry.compressed {
                    decompress_value(&entry.data, self.config.compression.algo)
                        .map_or_else(|| entry.data.to_vec(), |b| b.to_vec())
                } else {
                    entry.data.to_vec()
                }
            });
            drop(entry);
            if had_ttl {
                // Key no longer has a TTL — drop it from the side index so
                // active expiry doesn't waste a sample on it.
                self.ttl_index_put(key, None);
                // Re-broadcast the value with no expiry, so peers drop their
                // TTL too instead of expiring a key this node made permanent.
                if let Some(value) = published_value {
                    self.replicate_published_value(key, value, None);
                }
            }
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
        // The whole read-modify-write MUST happen under one shard write lock,
        // otherwise two concurrent increments on a missing key both observe it
        // absent and both create it with value `delta`, losing an update (the
        // `concurrent_kv_increments_are_consistent` e2e regression: 20 parallel
        // `incr` on a freshly-deleted key returned 18). `data.entry()` holds the
        // shard write lock across the check-and-create, serialising callers for
        // this key the same way `set_nx` does.
        //
        // Eviction must NOT run while that lock is held: `ensure_memory` ->
        // `evict_lru` locks arbitrary shards (possibly this one) and would
        // deadlock. So, mirroring `set_nx`, peek first and reserve memory up
        // front only when the key looks absent/expired (the create case). The
        // peek can race, but the entry lock below is authoritative: a wasted
        // `ensure_memory` on the rare lost race is not a correctness bug.
        let needs_create = self.data.get(key).is_none_or(|existing| existing.is_expired());
        if needs_create {
            // A freshly-created counter is a single small integer string;
            // reserve for it before taking the entry lock.
            let create_size =
                Entry::new(Bytes::from(delta.to_string().into_bytes()), key.len(), false, 0)
                    .mem_size;
            let index = if ttl.is_some() { ttl_index_cost(key) } else { 0 };
            if !self.reserve(key, create_size + index) {
                // Redis-compatible `-OOM` so INCR fails the same way SET does
                // when the store is parked at its limit (#477).
                return Err(OOM_ERROR.to_string());
            }
        }

        let publishing = self.has_replicator();
        let now = self.now_nanos();
        // Filled under the shard lock with (logical bytes, expiry) of the value
        // THIS increment produced. Publishing a re-read instead is what let two
        // concurrent increments broadcast each other's totals.
        let mut published: Option<(Vec<u8>, Option<Instant>)> = None;
        let result = match self.data.entry(key.to_string()) {
            dashmap::Entry::Occupied(mut occ) if !occ.get().is_expired() => {
                // Update path: parse the live value and store `current + delta`.
                let entry = occ.get();
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
                let (stored_data, compressed) = self.encode_value(new_bytes);

                let entry = occ.get_mut();
                let old_mem = entry.mem_size;
                entry.data = stored_data;
                entry.compressed = compressed;
                entry.mem_size = Entry::new(entry.data.clone(), key.len(), compressed, 0).mem_size;
                entry.touch(now);
                if publishing {
                    // Still under the shard lock: this is unambiguously *our*
                    // total. An `incr` never changes the expiry of a live key.
                    published = Some((new_val.to_string().into_bytes(), entry.expires_at));
                }
                let new_mem = entry.mem_size;
                // Adjust memory tracking (delta of the digit-string length).
                if new_mem > old_mem {
                    self.mem_add(new_mem - old_mem);
                } else {
                    self.mem_sub(old_mem - new_mem);
                }
                new_val
            }
            entry => {
                // Create path: key vacant or holding an expired entry. Replace
                // it with a fresh counter at value `delta` and the caller's TTL.
                let val_bytes = delta.to_string().into_bytes();
                let new_entry = match ttl {
                    Some(dur) => Entry::with_expiry(
                        Bytes::from(val_bytes),
                        key.len(),
                        false,
                        Instant::now() + dur,
                        now,
                    ),
                    None => Entry::new(Bytes::from(val_bytes), key.len(), false, now),
                };
                let new_size = new_entry.mem_size;
                let deadline = new_entry.expires_at;
                if publishing {
                    published = Some((delta.to_string().into_bytes(), new_entry.expires_at));
                }
                match entry {
                    dashmap::Entry::Occupied(mut occ) => {
                        // Expired entry: reclaim its bytes, then replace.
                        self.mem_sub(occ.get().mem_size);
                        self.mem_add(new_size);
                        occ.insert(new_entry);
                    }
                    dashmap::Entry::Vacant(vac) => {
                        self.mem_add(new_size);
                        vac.insert(new_entry);
                    }
                }
                self.ttl_index_put(key, deadline);
                delta
            }
        };

        // Wake any versioned waiters now that the write is visible. Done after
        // the entry guard has been dropped by the `match` arms above.
        self.notify_write(key);
        // Broadcast the new counter value. Without this an INCR advanced only
        // on the writing node and every peer stayed frozen forever — silently.
        if let Some((value, expires_at)) = published {
            self.replicate_published_value(key, value, expires_at);
        }
        Ok(result)
    }

    /// Append `value` to the existing value at `key`, or create it.
    ///
    /// Returns `Some(new_length)` on success. Returns `None` when the memory
    /// budget refused the write — the create of an absent key, or the growth
    /// of an existing one — and the RESP layer maps that to Redis `-OOM`
    /// instead of falsely answering with a length (#477). A refused append
    /// leaves the existing value untouched.
    ///
    /// In a cluster, an append that would *create* the key first checks that
    /// it is not a large value held only by other nodes; such an append is
    /// refused (`None`) — see [`try_append`](Self::try_append) for the reason.
    pub fn append(&self, key: &str, value: &[u8]) -> Option<usize> {
        self.try_append(key, value).ok()
    }

    /// [`append`](Self::append), telling the two refusals apart.
    ///
    /// # Errors
    ///
    /// [`AppendError::OutOfMemory`] when the memory budget refuses the write,
    /// and, in a cluster, [`AppendError::HeldElsewhere`] when this node has no
    /// copy of `key` but its replica set does (a value above
    /// `small_key_threshold` lives on `replication_factor` nodes only).
    /// Creating the key from the appended bytes alone there would replace the
    /// real value cluster-wide — data loss — and reading it through first
    /// would drop its TTL (a read-through carries none), so the append is
    /// refused instead. Run it on a node that holds the key, or rewrite the
    /// value with `SET`.
    pub fn try_append(&self, key: &str, value: &[u8]) -> Result<usize, AppendError> {
        self.append_inner(key, value)
    }

    fn append_inner(&self, key: &str, value: &[u8]) -> Result<usize, AppendError> {
        // Make room for the growth BEFORE taking the entry guard: reclaiming
        // and evicting lock arbitrary shards, possibly this key's. An append to
        // a live key used to grow it in place with no budget check at all, so
        // a loop of APPENDs could take the store arbitrarily far past
        // `memory_limit`. The appended length is the exact growth of an
        // uncompressed value and an over-estimate for a compressed one; the
        // bytes actually charged are the exact delta computed under the guard.
        let live = self.data.get(key).is_some_and(|e| !e.is_expired());
        if live && !self.reserve(key, value.len()) {
            return Err(AppendError::OutOfMemory);
        }
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
                // Capture the appended-to bytes under the entry guard — the
                // same lock-scoped capture `incr` uses, for the same reason.
                let published_value = if self.has_replicator() { Some(data.clone()) } else { None };
                let published_expiry = entry.expires_at;

                // Try compression again if configured.
                let (stored_data, compressed) = self.encode_value(data);

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
                // In-place append — wake waiters (the create path below
                // notifies via set → set_local).
                self.notify_write(key);
                // ...and broadcast the appended-to value; the create path
                // below replicates via `set`.
                if let Some(value) = published_value {
                    self.replicate_published_value(key, value, published_expiry);
                }
                return Ok(final_len);
            }
        }

        // No local copy. In a cluster that is not proof of absence: a large
        // value lives on its replica set only. `fetch_through` costs nothing
        // when this node is itself a replica (a miss there is authoritative)
        // and otherwise asks the replicas, bounded.
        if let Some(rep) = self.active_replicator()
            && rep.fetch_through(key).is_some()
        {
            log_append_held_elsewhere(key);
            return Err(AppendError::HeldElsewhere);
        }

        let len = value.len();
        if self.set(key.to_string(), value.to_vec(), None) {
            Ok(len)
        } else {
            // Create refused for memory — report it rather than claiming a
            // length for a value that was never stored.
            Err(AppendError::OutOfMemory)
        }
    }

    /// Remove all keys.
    /// **Node-local by design.** `flush` is an administrative reset of *this*
    /// node's map; it deliberately does not broadcast, because replicating it
    /// would let one node erase the whole cluster's data with a single call
    /// (and there is no way to scope or undo that). Peers keep their copies,
    /// and a subsequent replicated write to any flushed key re-materializes it
    /// here. Flush every node explicitly if a cluster-wide reset is intended.
    pub fn flush(&self) {
        self.data.clear();
        self.hashes.clear();
        self.hash_ttl_keys.clear();
        self.ttl_keys.clear();
        self.mem_used.store(0, Ordering::Relaxed);
        // A flush changes every key — wake all waiters so they observe the
        // now-empty store. Watch slots themselves survive the flush (their
        // versions must stay monotonic for the process lifetime; see the
        // `watch` module docs).
        if self.watch_slots.load(Ordering::Acquire) != 0 {
            for slot in &self.watchers {
                slot.value().bump();
            }
        }
    }

    /// A live string value, decoded, with its remaining TTL — both read
    /// under one shard read guard, so the pair is consistent. `None` when
    /// absent or expired. Does not touch the LRU clock.
    #[must_use]
    pub fn get_with_ttl(&self, key: &str) -> Option<(Bytes, Option<Duration>)> {
        let entry = self.data.get(key)?;
        let now = Instant::now();
        let ttl = match entry.expires_at {
            None => None,
            Some(at) => {
                let remaining = at.saturating_duration_since(now);
                if remaining.is_zero() {
                    return None;
                }
                Some(remaining)
            }
        };
        let value = if entry.compressed {
            decompress_value(&entry.data, self.config.compression.algo)?
        } else {
            entry.data.clone()
        };
        Some((value, ttl))
    }

    /// Visit every live string entry with its **decoded** value and
    /// remaining TTL, one shard at a time (see
    /// [`for_each_string_shard`](Self::for_each_string_shard) for the locking
    /// contract: no shard lock is held while `visit` runs).
    ///
    /// Entries whose decoded value is shorter than `min_len` bytes are
    /// skipped **before** decoding when stored raw; a stored-compressed value
    /// is always decoded, since its decoded length is unknown until then.
    /// This is the cluster's "large value" walk (`peer handoff`): with the
    /// default compression floor (1 KiB) above the default large-value
    /// threshold (512 B), a compressed entry is large by construction.
    pub fn scan_strings_decoded(
        &self,
        min_len: usize,
        mut visit: impl FnMut(&str, Bytes, Option<Duration>),
    ) {
        let algo = self.config.compression.algo;
        self.for_each_string_shard(|batch| {
            let now = Instant::now();
            for rec in batch {
                let ttl = match rec.expires_at {
                    None => None,
                    Some(at) => {
                        let remaining = at.saturating_duration_since(now);
                        if remaining.is_zero() {
                            continue;
                        }
                        Some(remaining)
                    }
                };
                let value = if rec.compressed {
                    match decompress_value(&rec.data, algo) {
                        Some(v) => v,
                        None => continue,
                    }
                } else {
                    rec.data
                };
                if value.len() < min_len {
                    continue;
                }
                visit(&rec.key, value, ttl);
            }
        });
    }

    // ── Snapshot support (crate::snapshot) ───────────────────────

    /// Visit every live string entry, one shard at a time.
    ///
    /// Each shard is copied out under its read lock into a batch of
    /// [`StringRecord`]s — cheap, since a record holds an `Arc`-clone of the
    /// stored bytes, not a copy — and `visit` runs **after** that lock is
    /// released, so however long serialising a batch takes, no writer is
    /// blocked by it. Memory is bounded by one shard's worth of records at a
    /// time, never the whole map. Expired entries are skipped.
    // The `raw-api` shard iterator is the only `unsafe` here; justified by the
    // SAFETY comment on the loop (buckets are read under the shard's own lock).
    #[allow(unsafe_code)]
    pub(crate) fn for_each_string_shard(&self, mut visit: impl FnMut(Vec<StringRecord>)) {
        let now = Instant::now();
        for shard in self.data.shards() {
            let batch: Vec<StringRecord> = {
                let guard = shard.read();
                let mut batch = Vec::with_capacity(guard.len());
                // SAFETY: `bucket.as_ref()` dereferences a live bucket into a
                // shared reference. The bucket comes from this shard's own
                // `iter()`, and `guard` holds the shard's read lock for the
                // whole loop, so no writer can rehash or remove entries
                // meanwhile. The references do not escape the block: only
                // owned clones do.
                for bucket in unsafe { guard.iter() } {
                    let (key, slot) = unsafe { bucket.as_ref() };
                    let entry = slot.get();
                    if entry.expires_at.is_some_and(|exp| exp <= now) {
                        continue;
                    }
                    batch.push(StringRecord {
                        key: key.clone(),
                        data: entry.data.clone(),
                        compressed: entry.compressed,
                        expires_at: entry.expires_at,
                    });
                }
                batch
            };
            if !batch.is_empty() {
                visit(batch);
            }
        }
    }

    /// Visit every live hash entry, one shard at a time — the hash-map twin of
    /// [`for_each_string_shard`](Self::for_each_string_shard), with the same
    /// locking contract.
    #[allow(unsafe_code)]
    pub(crate) fn for_each_hash_shard(&self, mut visit: impl FnMut(Vec<HashRecord>)) {
        let now = Instant::now();
        for shard in self.hashes.shards() {
            let batch: Vec<HashRecord> = {
                let guard = shard.read();
                let mut batch = Vec::with_capacity(guard.len());
                // SAFETY: as in `for_each_string_shard` — the shard read lock
                // is held for the loop and only owned clones leave it.
                for bucket in unsafe { guard.iter() } {
                    let (key, slot) = unsafe { bucket.as_ref() };
                    let entry = slot.get();
                    if !entry.is_live(now) {
                        continue;
                    }
                    // Live fields only, each with its own expiry (HEXPIRE).
                    let fields = entry
                        .live_parts(now)
                        .into_iter()
                        .map(|(name, value, expires_at)| HashFieldRecord {
                            name,
                            value,
                            expires_at,
                        })
                        .collect();
                    batch.push(HashRecord {
                        key: key.clone(),
                        fields,
                        expires_at: entry.expires_at,
                    });
                }
                batch
            };
            if !batch.is_empty() {
                visit(batch);
            }
        }
    }

    /// Put a string entry back exactly as a snapshot recorded it: `data` is
    /// stored as-is with its `compressed` flag, so a value the store had
    /// compressed is **not** decoded and re-encoded on the way in.
    ///
    /// Node-local by design (a restore is this node rewarming its own cache,
    /// not a write the cluster should see) and subject to the memory budget
    /// like any other write: under `noeviction` a full store refuses the
    /// restore and `false` is returned. The caller guarantees that
    /// `compressed` entries were produced by this store's configured
    /// algorithm; a snapshot taken under a different one must decode first and
    /// come in through [`set_local`](Self::set_local).
    pub(crate) fn restore_string(
        &self,
        key: String,
        data: Bytes,
        compressed: bool,
        expires_at: Option<Instant>,
    ) -> bool {
        let now = self.now_nanos();
        let entry = match expires_at {
            Some(at) => Entry::with_expiry(data, key.len(), compressed, at, now),
            None => Entry::new(data, key.len(), compressed, now),
        };
        let new_size = entry.mem_size;
        let deadline = entry.expires_at;
        let (old_size, old_had_ttl) =
            self.data.get(&key).map_or((0, false), |old| (old.mem_size, old.expires_at.is_some()));
        let index_growth =
            if deadline.is_some() && !old_had_ttl { ttl_index_cost(&key) } else { 0 };
        if !self.reserve(&key, (new_size + index_growth).saturating_sub(old_size)) {
            return false;
        }
        self.ttl_index_put(&key, deadline);
        let watch = self.watch_slot_if_any(&key);
        let previous = self.data.insert(key, entry);
        self.mem_add(new_size);
        if let Some(old) = previous {
            self.mem_sub(old.mem_size);
        }
        if let Some(slot) = watch {
            slot.bump();
        }
        true
    }

    /// Put a hash back as a snapshot recorded it (see
    /// [`restore_string`](Self::restore_string) for the contract). Replaces
    /// any hash already at `key`. Fields whose own expiry has passed are
    /// dropped; a hash left with no live field is not stored.
    pub(crate) fn restore_hash(
        &self,
        key: String,
        fields: Vec<HashFieldRecord>,
        expires_at: Option<Instant>,
    ) -> bool {
        let now = Instant::now();
        let Some(entry) = HashEntry::from_parts(
            &key,
            fields.into_iter().map(|f| (f.name, f.value, f.expires_at)),
            expires_at,
            now,
            self.now_nanos(),
        ) else {
            return false;
        };
        let new_size = entry.mem;
        let deadline = entry.earliest_deadline();
        let old_size = self.hashes.get(&key).map_or(0, |old| old.mem);
        if !self.reserve(&key, new_size.saturating_sub(old_size)) {
            return false;
        }
        let watch = self.watch_slot_if_any(&key);
        let previous = self.hashes.insert(key.clone(), entry);
        self.mem_add(new_size);
        if let Some(old) = previous {
            self.mem_sub(old.mem);
            lazy_drop(old);
        }
        // The hash TTL index drives active expiry of the key and its fields.
        self.hash_index_restore(&key, deadline);
        if let Some(slot) = watch {
            slot.bump();
        }
        true
    }

    // ── Background maintenance ───────────────────────────────────

    /// Run one adaptive active-expiry cycle (Redis' `activeExpireCycle`):
    /// examine the next [`EXPIRE_SAMPLE_KEYS`] keys that carry a TTL, reap the
    /// expired ones, and repeat while more than
    /// [`EXPIRE_ACCEPTABLE_STALE_PERCENT`]% of a round was expired — until
    /// `budget` has elapsed. Returns the number of keys reaped.
    ///
    /// At least one round always runs, whatever the budget, so a caller
    /// driving many stores from one budget still makes progress on each.
    ///
    /// # Why a cursor, and why it adapts
    ///
    /// The previous `expire_pass(100)` walked the TTL index from its first
    /// shard and stopped after 100 *visited* hints, once a second. That capped
    /// reclamation at 100 keys/s, and once the front of the index held live
    /// long-TTL keys it never reached an expired key again. Rounds now continue
    /// from a saved cursor (`expire_cursor`), so every hint is examined within
    /// one sweep of the index however the work is split up. Repeating while
    /// the expired share is high makes the reap rate follow the expiry rate;
    /// the threshold ends a cycle once further work would mostly find live
    /// keys, so a store full of long-TTL keys costs one small round per cycle.
    ///
    /// Expired keys that are not yet reaped are never readable (every read
    /// path checks expiry), and a write that needs the memory they hold
    /// reclaims them first (see [`ensure_memory`](Self::ensure_memory)).
    ///
    /// Reaps are local only, like lazy expiry: replicas expire keys on their
    /// own clocks.
    pub fn active_expire_cycle(&self, budget: Duration) -> usize {
        let reaped = self.active_expire_rounds(budget);
        // Expiry is what empties a store, so this is where its tables give
        // memory back (see `shrink_sparse_tables`).
        self.shrink_sparse_tables();
        reaped
    }

    /// Release the slot memory of hash tables that are now mostly empty.
    ///
    /// A `DashMap` shard never shrinks by itself: once a burst of 1.3M short
    /// TTL keys had expired, the store held 0 accounted bytes but ~190 MB of
    /// empty table (the data map plus the TTL index), which no later write
    /// could reuse for anything but more keys. Per-entry slot cost is part of
    /// what [`Entry`]'s size estimate charges, so leaving the slots allocated
    /// after the entries are gone is memory the accounting has already given
    /// back. Shrinking a shard once it is at least
    /// [`SHRINK_SPARSENESS`]-times oversized keeps the two in step.
    ///
    /// A shrink rehashes the shard, so the expiry cursor's position in it no
    /// longer means "where the sweep stopped"; that sweep resumes from an
    /// arbitrary point and the next full sweep covers whatever it skipped.
    /// Shrinks are rare (a shard must have lost three quarters of its slots),
    /// so this costs little.
    ///
    /// Returns the number of shards shrunk.
    pub fn shrink_sparse_tables(&self) -> usize {
        shrink_sparse_shards(&self.data)
            + shrink_sparse_shards(&self.ttl_keys)
            + shrink_sparse_shards(&self.hashes)
            + shrink_sparse_shards(&self.hash_ttl_keys)
    }

    /// The adaptive rounds of [`active_expire_cycle`](Self::active_expire_cycle).
    fn active_expire_rounds(&self, budget: Duration) -> usize {
        let start = Instant::now();
        let mut reaped = 0;
        loop {
            let round = self.expire_round();
            reaped += round.expired;
            if !round.worth_continuing() || start.elapsed() >= budget {
                break;
            }
        }
        if reaped > 0 {
            trace!(reaped, "active expiry reaped keys");
        }
        reaped
    }

    /// Examine the next [`EXPIRE_SAMPLE_KEYS`] TTL hints from the expiry
    /// cursor and reap every expired one.
    fn expire_round(&self) -> SampleRound {
        let now = Instant::now();
        let mut expired: Vec<(String, Instant)> = Vec::new();
        let from = MapCursor::unpack(self.expire_cursor.load(Ordering::Relaxed));
        // The scan only reads the TTL index (under its shard read lock) and
        // copies out the expired candidates; the reaps run after the lock is
        // released, because they take `data` and `ttl_keys` write locks.
        let (sampled, next) = scan_map(&self.ttl_keys, from, EXPIRE_SAMPLE_KEYS, |k, at| {
            if *at <= now {
                expired.push((k.clone(), *at));
            }
        });
        // Concurrent rounds (the expiry thread and a write reclaiming memory)
        // may overwrite each other's cursor; that only re-examines or skips a
        // few hints, which a later sweep covers.
        self.expire_cursor.store(next.pack(), Ordering::Relaxed);
        let expired_count = expired.len();
        for (key, at) in expired {
            // Compare-and-remove: only the entry whose expiry the hint
            // recorded. A re-write since the hint was read carries a fresh
            // deadline and survives (#478). A stale hint is pruned either way.
            self.reap_if_expired(&key, at);
        }
        let hashes = self.hash_expire_round(now);
        SampleRound { sampled: sampled + hashes.sampled, expired: expired_count + hashes.expired }
    }

    /// The hash half of [`expire_round`](Self::expire_round): examine the next
    /// [`EXPIRE_SAMPLE_KEYS`] hints of `hash_ttl_keys` and reap whatever has
    /// expired in those hashes — whole hashes past their key expiry, and
    /// expired fields of the rest. A hint that turns out early (a field's TTL
    /// was extended or removed) is corrected to the hash's real earliest
    /// deadline by the reap.
    fn hash_expire_round(&self, now: Instant) -> SampleRound {
        if self.hash_ttl_keys.is_empty() {
            return SampleRound { sampled: 0, expired: 0 };
        }
        let mut due: Vec<String> = Vec::new();
        let from = MapCursor::unpack(self.hash_expire_cursor.load(Ordering::Relaxed));
        let (sampled, next) = scan_map(&self.hash_ttl_keys, from, EXPIRE_SAMPLE_KEYS, |k, at| {
            if *at <= now {
                due.push(k.clone());
            }
        });
        self.hash_expire_cursor.store(next.pack(), Ordering::Relaxed);
        let expired = due.into_iter().filter(|key| self.reap_hash(key)).count();
        SampleRound { sampled, expired }
    }

    /// Reclaim expired keys until `needed` more bytes fit under the limit,
    /// working the way [`active_expire_cycle`](Self::active_expire_cycle)
    /// does. Stops once a round is mostly live — so a store that is genuinely
    /// full of live data pays one small round per refused write, not a scan —
    /// or after [`OOM_RECLAIM_BUDGET`]. The cursor carries over, so successive
    /// writes continue the sweep rather than re-examining the same hints.
    fn reclaim_expired_for(&self, needed: usize) {
        let start = Instant::now();
        loop {
            let round = self.expire_round();
            if self.fits(needed)
                || !round.worth_continuing()
                || start.elapsed() >= OOM_RECLAIM_BUDGET
            {
                return;
            }
        }
    }

    // ── Memory management ────────────────────────────────────────

    fn mem_add(&self, n: usize) {
        self.mem_used.fetch_add(n, Ordering::Relaxed);
    }

    fn mem_sub(&self, n: usize) {
        // Saturating subtraction: `mem_used` is an *approximate*, unsigned
        // counter, so it must never underflow. `flush()` resets it to 0 with a
        // plain `store`, which races with concurrent `remove`/`set` on other
        // worker threads (the store is `Arc`-shared across every PHP
        // execution thread): a thread can pull an entry out of the map — capturing its
        // `mem_size` — just before `flush` zeroes the counter, then run its
        // `mem_sub` afterwards. A plain `fetch_sub` would wrap to ~`usize::MAX`,
        // making `ensure_memory` believe the store is permanently full
        // (rejecting every write under NoEviction, eviction-spinning under LRU)
        // until restart. Flooring at 0 turns that catastrophe into a harmless,
        // self-correcting slight undercount.
        #[allow(
            deprecated,
            reason = "`try_update` (the rename) postdates our MSRV 1.88; switch when MSRV allows"
        )]
        let _ = self
            .mem_used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| Some(cur.saturating_sub(n)));
    }

    /// Whether `needed` more bytes fit under the memory limit right now.
    fn fits(&self, needed: usize) -> bool {
        let limit = self.config.memory_limit;
        limit == 0 || self.mem_used.load(Ordering::Relaxed).saturating_add(needed) <= limit
    }

    /// Ensure there is room for `needed` bytes. Returns `false` when there is
    /// not and none could be made.
    ///
    /// Order of resort:
    /// 1. **Expired keys.** Their bytes still count against the limit until
    ///    they are reaped, so before refusing a write or evicting live data
    ///    the store reclaims expired keys (see
    ///    [`reclaim_expired_for`](Self::reclaim_expired_for)). Under
    ///    `noeviction` this used to be skipped: a store whose keys had all
    ///    expired refused writes as out of memory while holding no live data.
    /// 2. **Eviction**, per the configured policy. `noeviction` refuses.
    fn ensure_memory(&self, needed: usize) -> bool {
        if self.fits(needed) {
            return true;
        }

        self.reclaim_expired_for(needed);
        if self.fits(needed) {
            return true;
        }

        match self.config.eviction_policy {
            EvictionPolicy::NoEviction => false,
            EvictionPolicy::AllKeysLru
            | EvictionPolicy::VolatileLru
            | EvictionPolicy::AllKeysRandom => self.evict(needed),
        }
    }

    /// Evict keys, per the configured policy, until `needed` more bytes fit.
    ///
    /// Returns `false` only when that is impossible: `needed` alone exceeds
    /// the limit, no eligible key is left (e.g. `volatile-lru` over a store
    /// with no TTL'd keys), or concurrent writers are filling the store as
    /// fast as this write empties it (see below).
    ///
    /// # Bounded by work, not by a victim count or the clock
    ///
    /// This used to give up after 100 victims. With the store full of small
    /// keys, 100 victims free ~25 KB, so every larger write was refused under
    /// the *evicting* policies (155,921 refused writes under the default
    /// `allkeys-lru` in the stress run, and 69% of 64 KB writes at a 64 MB
    /// limit). Each victim costs `O(sample)` — see
    /// [`next_victim`](Self::next_victim).
    ///
    /// The cap then became a 50 ms wall-clock budget, which still refused
    /// writes eviction could have satisfied: whether a write fit depended on
    /// how often its thread was descheduled, not on the store. A 256 KiB
    /// write into a full 1 MiB store was refused ~50% of the time on a busy
    /// machine (the PHP worker pool keeps every core busy by design), and a
    /// value needing more victims than 50 ms of eviction can reach — tens of
    /// megabytes among small keys — was refused even on an idle one.
    ///
    /// The only thing that can keep a single write evicting forever is other
    /// writers taking the freed room as fast as it is freed, so that is what
    /// is bounded: once this call has itself freed the starting shortfall
    /// plus `needed` again (at least [`EVICTION_MIN_RETRY_BYTES`]) and the
    /// write still does not fit, it is refused. Without concurrent writers
    /// the write fits as soon as the shortfall is freed, so this bound never
    /// refuses a write that is alone on the store. A
    /// run of [`EVICTION_MAX_MISSES`] consecutive candidates that were already
    /// gone (deleted between sampling and eviction) is refused too.
    fn evict(&self, needed: usize) -> bool {
        let limit = self.config.memory_limit;
        if needed > limit {
            return false; // no amount of eviction makes this fit
        }
        // What a lone writer must free; anything beyond it went to others.
        let shortfall = self.mem_used().saturating_add(needed).saturating_sub(limit);
        let give_up_after = shortfall.saturating_add(needed).max(EVICTION_MIN_RETRY_BYTES);
        let mut freed = 0usize;
        let mut evicted = 0usize;
        let mut misses = 0usize;
        let fitted = loop {
            if self.fits(needed) {
                break true;
            }
            if freed >= give_up_after || misses >= EVICTION_MAX_MISSES {
                break false; // others are consuming the room as it is freed
            }
            let Some(victim) = self.next_victim() else {
                break false; // nothing eligible left
            };
            debug!(key = %victim.key, hash = victim.hash, "evicting key");
            // Eviction is a local memory-pressure decision — replicas manage
            // their own memory — so it is never broadcast.
            match self.evict_victim(&victim) {
                Some(bytes) => {
                    freed = freed.saturating_add(bytes);
                    evicted += 1;
                    misses = 0;
                }
                None => misses += 1,
            }
            #[cfg(test)]
            if let Some(hook) = tests::AFTER_EVICTION_STEP.get() {
                hook(self);
            }
        };
        trace!(evicted, freed, needed, fitted, "eviction pass");
        fitted
    }

    /// Pick the next eviction victim for the configured policy, or `None`
    /// when no eligible key exists.
    ///
    /// * `allkeys-lru` / `volatile-lru`: Redis' approximated LRU with an
    ///   **eviction pool**. Each call samples [`EVICTION_SAMPLES`] keys from a
    ///   random position — string keys and hashes for `allkeys-lru`, only
    ///   TTL'd keys (drawn from the TTL index, not the whole map) for
    ///   `volatile-lru` — merges them into a pool of the
    ///   [`EVICTION_POOL_SIZE`] best candidates kept across calls, and takes
    ///   the oldest. The pool carries good candidates over from earlier
    ///   samples, so successive victims approximate a true LRU order far
    ///   better than "oldest of one sample". An expired key is always the
    ///   best candidate.
    /// * `allkeys-random`: one key from a random position — a string key, or a
    ///   hash once no string keys remain.
    ///
    /// Every policy costs `O(sample)` per victim. The old LRU sampler skipped
    /// TTL-less keys *without counting them*, so `volatile-lru` over a store of
    /// mostly persistent keys walked the whole map on every write (~3 ms per
    /// write); the old random picker did `data.iter().nth(random)`, O(n) per
    /// victim (filling a 64 MB store was 20x slower than LRU).
    fn next_victim(&self) -> Option<Victim> {
        match self.config.eviction_policy {
            EvictionPolicy::NoEviction => None,
            EvictionPolicy::AllKeysRandom => self.random_victim(),
            EvictionPolicy::AllKeysLru => self.lru_victim(false),
            EvictionPolicy::VolatileLru => self.lru_victim(true),
        }
    }

    /// A random position in a map's bucket space, from the store's generator.
    fn random_cursor(&self) -> MapCursor {
        let r = self.next_rand();
        #[allow(clippy::cast_possible_truncation, reason = "only the low bits pick a position")]
        MapCursor { shard: (r >> 48) as usize, bucket: r as usize }
    }

    /// `allkeys-random`: the key at a random position.
    fn random_victim(&self) -> Option<Victim> {
        let mut found = None;
        scan_map(&self.data, self.random_cursor(), 1, |k, _| {
            found = Some(Victim { key: k.clone(), hash: false, idle_rank: 0 });
        });
        if found.is_none() {
            scan_map(&self.hashes, self.random_cursor(), 1, |k, _| {
                found = Some(Victim { key: k.clone(), hash: true, idle_rank: 0 });
            });
        }
        found
    }

    /// LRU victim through the eviction pool (see [`next_victim`]).
    ///
    /// [`next_victim`]: Self::next_victim
    fn lru_victim(&self, volatile_only: bool) -> Option<Victim> {
        let mut fresh: Vec<Victim> = Vec::with_capacity(EVICTION_SAMPLES + EVICTION_HASH_SAMPLES);
        if volatile_only {
            // Draw from the TTL index, so persistent keys cost nothing however
            // many there are. The deadline in the hint says whether the key
            // has already expired; otherwise its idle time decides.
            let now = Instant::now();
            let mut picked: Vec<(String, Instant)> = Vec::with_capacity(EVICTION_SAMPLES);
            scan_map(&self.ttl_keys, self.random_cursor(), EVICTION_SAMPLES, |k, at| {
                picked.push((k.clone(), *at));
            });
            for (key, at) in picked {
                let rank = if at <= now {
                    0
                } else if let Some(entry) = self.data.get(&key) {
                    entry.last_accessed_nanos().saturating_add(1)
                } else {
                    continue; // stale hint; active expiry prunes it
                };
                fresh.push(Victim { key, hash: false, idle_rank: rank });
            }
        } else {
            scan_map(&self.data, self.random_cursor(), EVICTION_SAMPLES, |k, e| {
                let rank =
                    if e.is_expired() { 0 } else { e.last_accessed_nanos().saturating_add(1) };
                fresh.push(Victim { key: k.clone(), hash: false, idle_rank: rank });
            });
            // Hashes are keys too: under `allkeys-lru` they used to be
            // invisible to eviction, so a store filled by HSET could only
            // refuse. A smaller sample keeps the common no-hash case cheap.
            scan_map(&self.hashes, self.random_cursor(), EVICTION_HASH_SAMPLES, |k, h| {
                let rank = h.last_accessed.load(Ordering::Relaxed).saturating_add(1);
                fresh.push(Victim { key: k.clone(), hash: true, idle_rank: rank });
            });
        }

        let mut pool = self.eviction_pool.lock();
        for victim in fresh {
            match pool.iter_mut().find(|p| p.key == victim.key && p.hash == victim.hash) {
                // Re-sampled: refresh its rank (it may have been touched).
                Some(existing) => existing.idle_rank = victim.idle_rank,
                None => pool.push(victim),
            }
        }
        // Oldest (lowest rank) first; keep only the best candidates.
        pool.sort_unstable_by_key(|v| v.idle_rank);
        pool.truncate(EVICTION_POOL_SIZE);
        if pool.is_empty() { None } else { Some(pool.remove(0)) }
    }

    /// Remove an eviction victim. Removes only the map the victim came from:
    /// a string key and a hash may share a name, and evicting one must not
    /// take the other. Returns the bytes the entry was charged, or `None` if
    /// nothing was removed (a pooled candidate may have been deleted since it
    /// was sampled).
    fn evict_victim(&self, victim: &Victim) -> Option<usize> {
        let freed = if victim.hash {
            self.hashes.remove(&victim.key).map(|(_, old)| {
                let size = old.mem;
                self.mem_sub(size);
                self.hash_index_drop(&victim.key);
                lazy_drop(old);
                size
            })
        } else {
            self.data.remove(&victim.key).map(|(_, old)| {
                self.mem_sub(old.mem_size);
                self.ttl_index_drop(&victim.key, old.expires_at);
                old.mem_size
            })
        };
        if freed.is_some() {
            // Waiters observe an evicted key as deleted, as for any removal.
            self.notify_write(&victim.key);
        }
        freed
    }

    /// Next value from the store's splitmix64 generator.
    fn next_rand(&self) -> u64 {
        let mut z = self.rng.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// TTL hints examined per active-expiry round (Redis'
/// `ACTIVE_EXPIRE_CYCLE_KEYS_PER_LOOP`).
pub const EXPIRE_SAMPLE_KEYS: usize = 20;

/// An active-expiry cycle keeps going while more than this percentage of a
/// round was expired (Redis 6+ default, `active-expire-effort 1`).
pub const EXPIRE_ACCEPTABLE_STALE_PERCENT: usize = 10;

/// Most time a single write spends reclaiming expired keys before it falls
/// back to eviction or refusal.
const OOM_RECLAIM_BUDGET: Duration = Duration::from_millis(2);

/// Floor on the bytes one write may evict before concluding that concurrent
/// writers are taking the room as fast as it is freed (see `Store::evict`).
/// Without it a small write that loses one race for a freed slot would be
/// refused after a single victim.
const EVICTION_MIN_RETRY_BYTES: usize = 64 * 1024;

/// Consecutive eviction candidates that were already gone when the evictor
/// reached them, after which the write is refused rather than resampled
/// forever.
const EVICTION_MAX_MISSES: usize = 1_024;

/// String keys sampled per LRU eviction step (Redis' `maxmemory-samples` is
/// 5; more suits our coarse, 100 ms LRU clock, where ties are common).
const EVICTION_SAMPLES: usize = 16;

/// Hashes sampled per `allkeys-lru` eviction step, alongside the string keys.
const EVICTION_HASH_SAMPLES: usize = 4;

/// Best candidates the LRU eviction pool keeps across steps (Redis'
/// `EVPOOL_SIZE`).
const EVICTION_POOL_SIZE: usize = 16;

/// An eviction candidate.
#[derive(Debug, Clone)]
struct Victim {
    key: String,
    /// `true` for a hash, `false` for a string key.
    hash: bool,
    /// Lower is a better victim: `0` for an already-expired key, otherwise
    /// last access (nanos since the store anchor) plus one.
    idle_rank: u64,
}

/// Seed for a store's sampling generator: wall-clock nanos mixed with a
/// process-wide counter, so stores built in the same nanosecond still differ.
fn rng_seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0xA076_1D64_78BD_642F)
}

/// What one active-expiry round saw.
#[derive(Debug, Clone, Copy)]
struct SampleRound {
    /// TTL hints examined.
    sampled: usize,
    /// Of those, the ones already past their deadline (reaped, or found
    /// already gone).
    expired: usize,
}

impl SampleRound {
    /// Whether another round is likely to pay: the index is not empty and
    /// more than [`EXPIRE_ACCEPTABLE_STALE_PERCENT`]% of this round was
    /// expired.
    fn worth_continuing(self) -> bool {
        self.sampled > 0 && self.expired * 100 > self.sampled * EXPIRE_ACCEPTABLE_STALE_PERCENT
    }
}

/// A position in a `DashMap`'s bucket space: a shard index and a bucket index
/// inside that shard. Kept across calls by active expiry so successive cycles
/// continue a sweep instead of restarting it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MapCursor {
    shard: usize,
    bucket: usize,
}

impl MapCursor {
    /// Pack into one `u64` (shard in the top 16 bits) so the cursor can live
    /// in an atomic. DashMap never has anywhere near 2^16 shards.
    fn pack(self) -> u64 {
        ((self.shard as u64) << 48) | (self.bucket as u64 & ((1 << 48) - 1))
    }

    #[allow(clippy::cast_possible_truncation, reason = "inverse of `pack`")]
    fn unpack(v: u64) -> Self {
        Self { shard: (v >> 48) as usize, bucket: (v & ((1 << 48) - 1)) as usize }
    }
}

/// Visit up to `want` entries of `map` in bucket order, starting at `from`,
/// calling `visit` for each. Returns how many were visited and the position
/// just past the last one, from which a later call continues.
///
/// This is the shape of Redis' `dictScan`-driven active expiry: a cursor walks
/// the table a little at a time, so every entry is reached within one full
/// sweep however the work is split up — unlike restarting at the front each
/// time, which never reaches whatever sits behind a long run of entries that
/// are not eligible. The cost of a call is `O(want)` occupied buckets plus the
/// empty buckets stepped over. A bucket index past the end of a shard's table
/// (the table was resized since the cursor was saved) wraps into range.
///
/// Each shard's read lock is held only while that shard is being walked;
/// `visit` must not touch `map`. Every bucket is visited at most once per
/// call, so on a map with fewer than `want` entries this returns all of them.
// The `raw-api` shard table is the only `unsafe` here; justified by the SAFETY
// comments on each use (buckets are read under the shard's own read lock).
#[allow(unsafe_code)]
fn scan_map<V>(
    map: &DashMap<String, V>,
    from: MapCursor,
    want: usize,
    mut visit: impl FnMut(&String, &V),
) -> (usize, MapCursor) {
    let shards = map.shards();
    let shard_count = shards.len();
    if shard_count == 0 || want == 0 {
        return (0, from);
    }
    let first = from.shard % shard_count;
    let mut shard = first;
    let start = from.bucket;
    let mut seen = 0;
    // `shard_count + 1` passes: the first shard from `start` to its end, every
    // other shard whole, then the first shard again from 0 up to `start`.
    for pass in 0..=shard_count {
        let guard = shards[shard].read();
        if !guard.is_empty() {
            // `buckets()` is a power of two for an allocated table.
            let buckets = guard.buckets();
            let (lo, hi) = if pass == 0 {
                (start % buckets, buckets)
            } else if pass == shard_count {
                (0, (start % buckets).min(buckets))
            } else {
                (0, buckets)
            };
            for idx in lo..hi {
                // SAFETY: `idx < buckets()`, the precondition of
                // `is_bucket_full`; the table is allocated (it is non-empty).
                if unsafe { guard.is_bucket_full(idx) } {
                    // SAFETY: the bucket is in range and full, so it holds an
                    // initialised element; `guard` holds this shard's read
                    // lock for the whole loop, so no writer can move or drop
                    // it while the reference lives. The reference does not
                    // escape `visit`.
                    let (key, slot) = unsafe { guard.bucket(idx).as_ref() };
                    visit(key, slot.get());
                    seen += 1;
                    if seen >= want {
                        // Resume just past this bucket — or, when it was the
                        // shard's last, at the start of the next shard. A
                        // cursor of `buckets` would wrap to 0 on the next call
                        // (`start % buckets`) and walk this shard a second
                        // time, so a sweep could take more rounds than the
                        // index has hints / round.
                        let next = if idx + 1 < buckets {
                            MapCursor { shard, bucket: idx + 1 }
                        } else {
                            MapCursor { shard: (shard + 1) % shard_count, bucket: 0 }
                        };
                        return (seen, next);
                    }
                }
            }
        }
        drop(guard);
        if pass == shard_count {
            break;
        }
        shard = (shard + 1) % shard_count;
    }
    // The whole map was walked; the next sweep may start anywhere.
    (seen, MapCursor { shard: first, bucket: start })
}

/// Parse a stored byte value as a decimal integer.
fn parse_int_value(data: &[u8]) -> Result<i64, String> {
    let s = std::str::from_utf8(data)
        .map_err(|_| "ERR value is not an integer or out of range".to_string())?;
    s.parse::<i64>().map_err(|_| "ERR value is not an integer or out of range".to_string())
}

/// Append the keys of `map` that are live (per `is_live`) and match
/// `pattern` (`None` = every key) to `out`.
///
/// Walks the map one shard at a time. Under each shard's read lock it only
/// clones the live keys; the lock is released **before** any glob matching
/// runs. A writer to a shard therefore waits at most for one shard's key
/// copy, never for pattern evaluation — the old `data.iter().filter(glob)`
/// held a shard's read lock for the whole match, so one slow `KEYS` blocked
/// every `SET` to that shard (and with it the tokio worker running it).
// The `raw-api` shard iterator is the only `unsafe` here; justified by the
// SAFETY comment on the loop (buckets are read under the shard's own lock).
#[allow(unsafe_code)]
fn collect_keys<V>(
    map: &DashMap<String, V>,
    is_live: impl Fn(&V) -> bool,
    pattern: Option<&[u8]>,
    out: &mut Vec<String>,
) {
    let mut batch: Vec<String> = Vec::new();
    for shard in map.shards() {
        {
            let guard = shard.read();
            // SAFETY: `bucket.as_ref()` dereferences a live bucket into a
            // shared reference. The bucket comes from this shard's own
            // `iter()`, and `guard` holds the shard's read lock for the whole
            // loop, so no writer can rehash or remove entries meanwhile. The
            // references do not escape the block: only owned clones do.
            for bucket in unsafe { guard.iter() } {
                let (key, slot) = unsafe { bucket.as_ref() };
                if is_live(slot.get()) {
                    batch.push(key.clone());
                }
            }
        }
        match pattern {
            None => out.append(&mut batch),
            Some(p) => out.extend(batch.drain(..).filter(|k| glob::glob_match(p, k.as_bytes()))),
        }
    }
}

/// A shard's table is only shrunk once it has at least this many slots…
const SHRINK_MIN_CAPACITY: usize = 64;

/// …and holds fewer than one entry per this many slots. The gap between this
/// and the table's own growth threshold keeps a store whose size oscillates
/// from shrinking and regrowing every cycle.
const SHRINK_SPARSENESS: usize = 4;

/// Shrink every shard of `map` whose table is mostly empty. Returns the
/// number of shards shrunk.
///
/// Each shard is checked under its read lock and only write-locked when it
/// will actually be shrunk; the rehash then costs `O(entries in that shard)`.
fn shrink_sparse_shards<V>(map: &DashMap<String, V>) -> usize {
    let hasher = map.hasher();
    let sparse = |capacity: usize, len: usize| {
        capacity >= SHRINK_MIN_CAPACITY && capacity > len.saturating_mul(SHRINK_SPARSENESS)
    };
    let mut shrunk = 0;
    for shard in map.shards() {
        {
            let guard = shard.read();
            if !sparse(guard.capacity(), guard.len()) {
                continue;
            }
        }
        let mut guard = shard.write();
        let len = guard.len();
        if sparse(guard.capacity(), len) {
            // The same hash DashMap uses for both shard selection and the
            // in-shard table (`hash_usize`), so every entry keeps its slot
            // reachable after the rehash.
            guard.shrink_to(len, |(key, _)| hasher.hash_one(key));
            shrunk += 1;
        }
    }
    shrunk
}

/// Bytes charged to `memory_limit` for one TTL-index hint on `key`: the key's
/// own copy plus the map slot (`String` + `Instant`, sized up for the table's
/// load factor — measured at ~54 bytes beyond the key in
/// `tests/memory_accounting.rs`).
const fn ttl_index_cost(key: &str) -> usize {
    key.len() + TTL_INDEX_SLOT_OVERHEAD
}

/// Per-hint overhead of the TTL index beyond the key bytes; see
/// [`ttl_index_cost`].
const TTL_INDEX_SLOT_OVERHEAD: usize = 56;

/// Wrap a buffer as stored bytes holding no spare capacity (see
/// [`Store::encode_value`]). A no-op copy-free conversion when the vector is
/// already exact, which is the common case for values from RESP and PHP.
fn exact_bytes(mut v: Vec<u8>) -> Bytes {
    v.shrink_to_fit();
    Bytes::from(v)
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
pub(crate) fn decompress_value(data: &[u8], algo: CompressionAlgo) -> Option<Bytes> {
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
    use std::cell::Cell;

    use super::*;

    thread_local! {
        /// Called by `Store::evict` after every eviction step on this thread,
        /// so a test can make eviction slow or race it deterministically.
        pub(super) static AFTER_EVICTION_STEP: Cell<Option<fn(&Store)>> =
            const { Cell::new(None) };
    }

    /// Installs an [`AFTER_EVICTION_STEP`] hook for the life of the guard.
    struct EvictionHook;

    impl EvictionHook {
        fn install(hook: fn(&Store)) -> Self {
            AFTER_EVICTION_STEP.set(Some(hook));
            Self
        }
    }

    impl Drop for EvictionHook {
        fn drop(&mut self) {
            AFTER_EVICTION_STEP.set(None);
        }
    }

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

    // ── del_if_eq (compare-and-delete) ──────────────────────────────

    #[test]
    fn del_if_eq_deletes_on_exact_match() {
        let s = test_store();
        s.set("lock".into(), b"token-A".to_vec(), None);
        assert!(s.del_if_eq("lock", b"token-A"), "matching token must delete");
        assert_eq!(s.get("lock"), None, "key must be gone after a matching delete");
    }

    #[test]
    fn del_if_eq_is_noop_on_mismatch() {
        let s = test_store();
        s.set("lock".into(), b"token-A".to_vec(), None);
        assert!(!s.del_if_eq("lock", b"token-B"), "a different token must not delete");
        assert_eq!(
            s.get("lock").as_deref(),
            Some(&b"token-A"[..]),
            "the value must be untouched on a mismatch"
        );
    }

    #[test]
    fn del_if_eq_is_noop_on_absent_key() {
        let s = test_store();
        assert!(!s.del_if_eq("missing", b"whatever"), "an absent key must return false");
    }

    #[test]
    fn del_if_eq_treats_expired_as_absent() {
        let s = test_store();
        // Insert an already-expired entry directly, then try to CAD it: the
        // value bytes match, but an expired entry is logically absent, so this
        // must be a no-op (and must NOT report a delete).
        let entry = Entry::with_expiry(
            Bytes::from_static(b"tok"),
            3,
            false,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            0,
        );
        s.data.insert("lock".into(), entry);
        assert!(!s.del_if_eq("lock", b"tok"), "expired entry must be treated as absent");
    }

    #[test]
    fn del_if_eq_release_after_takeover_cannot_unlock_new_owner() {
        // The motivating safety property. Holder A took the lock, overran its
        // TTL, and holder B has since acquired it (value now B's token). A's
        // release must NOT delete B's lock — this is exactly what a plain
        // `remove()` release gets wrong.
        let s = test_store();
        s.set("lock".into(), b"holder-A".to_vec(), None); // A holds it
        s.set("lock".into(), b"holder-B".to_vec(), None); // TTL lapsed → B took over
        assert!(!s.del_if_eq("lock", b"holder-A"), "A must not unlock B's lock");
        assert_eq!(
            s.get("lock").as_deref(),
            Some(&b"holder-B"[..]),
            "B's lock must survive A's stale release"
        );
        // B releasing with its own token works.
        assert!(s.del_if_eq("lock", b"holder-B"));
        assert_eq!(s.get("lock"), None);
    }

    #[test]
    fn del_if_eq_updates_memory_accounting() {
        let s = test_store();
        let baseline = s.mem_used();
        s.set("lock".into(), b"token".to_vec(), None);
        assert!(s.mem_used() > baseline, "storing a value must charge memory");
        assert!(s.del_if_eq("lock", b"token"));
        assert_eq!(s.mem_used(), baseline, "a matching delete must reclaim its bytes");
    }

    #[test]
    fn del_if_eq_matches_logical_value_when_compressed() {
        // With compression on and a low threshold, a large value is stored
        // compressed. The compare must be against the decompressed logical
        // bytes, so a matching token still deletes and a mismatch does not.
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig { algo: CompressionAlgo::Zstd, level: 6, min_size: 1 },
        });
        let token = vec![b'z'; 4096]; // very compressible, well over min_size
        s.set("lock".into(), token.clone(), None);
        assert!(!s.del_if_eq("lock", b"z"), "a prefix must not match the logical value");
        assert!(s.del_if_eq("lock", &token), "the exact logical value must match and delete");
        assert_eq!(s.get("lock"), None);
    }

    #[test]
    fn del_if_eq_wakes_watchers() {
        // A CAD that fires is a state change (key becomes absent), so a
        // `wait_for_change` observer must be woken — same contract as remove.
        let s = test_store();
        s.set("lock".into(), b"token".to_vec(), None);
        // Register + snapshot to learn the current version.
        let (ver, _) = s.wait_for_change("lock", 0, Duration::from_millis(0)).unwrap();
        assert!(s.del_if_eq("lock", b"token"));
        let (new_ver, val) =
            s.wait_for_change("lock", ver, Duration::from_millis(0)).expect("version must advance");
        assert!(new_ver > ver, "delete must bump the watch version");
        assert_eq!(val, None, "the woken observer must see the key as absent");
    }

    #[test]
    fn del_if_eq_only_one_racing_releaser_wins() {
        // 32 threads race to release the same lock with the same token; exactly
        // one CAD may observe true. This is the mirror of the set_nx one-winner
        // property, and it is what makes double-release harmless.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let s = test_store();
        s.set("lock".into(), b"tok".to_vec(), None);
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(32);
        for _ in 0..32 {
            let s = Arc::clone(&s);
            let winners = Arc::clone(&winners);
            handles.push(thread::spawn(move || {
                if s.del_if_eq("lock", b"tok") {
                    winners.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::Relaxed), 1, "exactly one releaser may win");
        assert_eq!(s.get("lock"), None);
    }

    #[test]
    fn del_if_eq_broadcasts_publish_only_tombstone_when_replicated() {
        // In clustered mode a fired CAD must fan a tombstone to peers via the
        // publish-only hook — and must NOT go through `replicate_remove` (which
        // re-removes locally). A mismatch must broadcast nothing.
        let s = test_store();
        s.set_local("lock".into(), b"tok".to_vec(), None);
        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        // Mismatch: no delete, no broadcast.
        assert!(!s.del_if_eq("lock", b"other"));
        assert!(rep.removes_published.lock().unwrap().is_empty(), "mismatch must not broadcast");

        // Match: exactly one publish-only tombstone, and never the plain
        // remove hook (that path would re-remove locally and reopen the
        // delete-after-recreate race).
        assert!(s.del_if_eq("lock", b"tok"));
        assert_eq!(rep.removes_published.lock().unwrap().as_slice(), &["lock".to_string()]);
        assert!(
            rep.removes.lock().unwrap().is_empty(),
            "CAD must use the publish-only tombstone, not replicate_remove"
        );
    }

    // ── Owner-routed conditional writes ─────────────────────────────────

    /// Stand-in for a clustered replicator: every conditional write is
    /// arbitrated by one shared `owner` store (or the owner is "down"). Plain
    /// writes are recorded and dropped, like [`RecordingReplicator`].
    #[derive(Debug)]
    struct OwnerRoutedReplicator {
        owner: Option<Arc<Store>>,
        routed_set_nx: AtomicUsize,
    }

    impl OwnerRoutedReplicator {
        fn to(owner: &Arc<Store>) -> Arc<Self> {
            Arc::new(Self { owner: Some(Arc::clone(owner)), routed_set_nx: AtomicUsize::new(0) })
        }
        fn unreachable() -> Arc<Self> {
            Arc::new(Self { owner: None, routed_set_nx: AtomicUsize::new(0) })
        }
    }

    impl Replicator for OwnerRoutedReplicator {
        fn replicate_set(&self, _: String, _: Vec<u8>, _: Option<Duration>) -> bool {
            true
        }
        fn replicate_remove(&self, _: &str) -> bool {
            true
        }
        fn replicate_expire(&self, _: &str, _: Duration) -> bool {
            true
        }
        fn replicate_published(&self, _: String, _: Vec<u8>, _: Option<Duration>) {}
        fn replicate_hash_field(
            &self,
            _key: &str,
            _field: &str,
            apply: &mut dyn FnMut() -> Option<crate::store::HashFieldState>,
        ) -> bool {
            let _ = apply();
            true
        }
        fn replicate_remove_published(&self, _: String) {}
        fn route_set_nx(
            &self,
            key: &str,
            value: &[u8],
            ttl: Option<Duration>,
        ) -> Routed<SetNxOutcome> {
            self.routed_set_nx.fetch_add(1, Ordering::Relaxed);
            match &self.owner {
                Some(owner) => {
                    Routed::Owner(owner.set_nx_outcome_local(key.to_string(), value.to_vec(), ttl))
                }
                None => Routed::OwnerUnreachable,
            }
        }
        fn route_del_if_eq(&self, key: &str, token: &[u8]) -> Routed<bool> {
            match &self.owner {
                Some(owner) => Routed::Owner(owner.del_if_eq_local(key, token)),
                None => Routed::OwnerUnreachable,
            }
        }
    }

    /// The cluster bug, at the store seam: two nodes each SETNX the same key.
    /// Deciding on each node's own map, both won. Routed to one arbiter,
    /// exactly one does — under real thread contention.
    #[test]
    fn routed_set_nx_has_one_winner_across_nodes() {
        let owner = test_store();
        let nodes: Vec<Arc<Store>> = (0..4)
            .map(|_| {
                let s = test_store();
                s.set_replicator(Some(OwnerRoutedReplicator::to(&owner) as Arc<dyn Replicator>));
                s
            })
            .collect();

        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for (n, node) in nodes.iter().enumerate() {
            for t in 0..8 {
                let node = Arc::clone(node);
                let winners = Arc::clone(&winners);
                handles.push(std::thread::spawn(move || {
                    let token = format!("node{n}-t{t}").into_bytes();
                    if node.set_nx("lock".into(), token, None) {
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::Relaxed), 1, "exactly one node may acquire the lock");
        assert!(owner.get("lock").is_some(), "the owner holds the winning grant");
        for node in &nodes {
            assert_eq!(node.get("lock"), None, "a routed grant is not applied on the caller");
        }
    }

    /// The stale-release half: a holder whose lock expired and was re-granted
    /// to someone else still has its OLD token in its local copy. Compared
    /// locally that matched and broadcast a delete of the new holder's lock;
    /// compared on the owner it does not.
    #[test]
    fn routed_del_if_eq_compares_against_the_owner_not_a_stale_local_copy() {
        let owner = test_store();
        let node = test_store();
        node.set_local("lock".into(), b"old-holder".to_vec(), None);
        node.set_replicator(Some(OwnerRoutedReplicator::to(&owner) as Arc<dyn Replicator>));
        owner.set_local("lock".into(), b"new-holder".to_vec(), None);

        assert_eq!(node.try_del_if_eq("lock", b"old-holder"), Ok(false));
        assert_eq!(owner.get("lock").as_deref(), Some(&b"new-holder"[..]), "new lock survives");

        assert_eq!(node.try_del_if_eq("lock", b"new-holder"), Ok(true));
        assert_eq!(owner.get("lock"), None);
    }

    /// An unreachable owner fails closed: not acquired, not released, and the
    /// caller's own map is left exactly as it was.
    #[test]
    fn unreachable_owner_fails_closed() {
        let node = test_store();
        node.set_local("held".into(), b"tok".to_vec(), None);
        node.set_replicator(Some(OwnerRoutedReplicator::unreachable() as Arc<dyn Replicator>));

        assert_eq!(
            node.set_nx_outcome("free".into(), b"v".to_vec(), None),
            SetNxOutcome::OwnerUnreachable
        );
        assert!(!node.set_nx("free".into(), b"v".to_vec(), None));
        assert_eq!(node.get("free"), None, "nothing may be acquired locally as a fallback");

        assert_eq!(node.try_del_if_eq("held", b"tok"), Err(OwnerUnreachable));
        assert!(!node.del_if_eq("held", b"tok"));
        assert_eq!(node.get("held").as_deref(), Some(&b"tok"[..]), "nothing released locally");
    }

    /// The `*_local` variants are what the owner runs: they must never consult
    /// the router (that is what keeps two nodes that disagree about ownership
    /// from bouncing a request between them).
    #[test]
    fn local_variants_bypass_routing() {
        let node = test_store();
        let rep = OwnerRoutedReplicator::unreachable();
        node.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        assert_eq!(
            node.set_nx_outcome_local("k".into(), b"v".to_vec(), None),
            SetNxOutcome::Inserted
        );
        assert!(node.del_if_eq_local("k", b"v"));
        assert_eq!(rep.routed_set_nx.load(Ordering::Relaxed), 0);
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

    /// Regression for the `incr_by` create-path race: N threads incrementing a
    /// missing key concurrently must land exactly N, with no lost updates.
    /// Before the `data.entry()` rewrite, two threads that both observed the
    /// key absent would each create it at value 1, dropping an increment (the
    /// `concurrent_kv_increments_are_consistent` e2e failure: 20 -> 18).
    #[test]
    fn incr_by_concurrent_create_has_no_lost_updates() {
        // Repeat: the race only fires when threads collide on the create
        // window, which is timing-dependent, so a single pass could get lucky.
        for _ in 0..50 {
            let s = test_store();
            const N: i64 = 32;
            let mut handles = Vec::with_capacity(N as usize);
            for _ in 0..N {
                let s = Arc::clone(&s);
                handles.push(std::thread::spawn(move || {
                    s.incr_by("ctr", 1).expect("incr must succeed");
                }));
            }
            for h in handles {
                h.join().expect("increment thread panicked");
            }
            let final_val = s.get("ctr").expect("counter must exist");
            assert_eq!(
                final_val.as_ref(),
                N.to_string().as_bytes(),
                "concurrent incr lost updates: expected {N}"
            );
        }
    }
    #[test]
    fn append_new_key() {
        let s = test_store();
        assert_eq!(s.append("k", b"hello"), Some(5));
        assert_eq!(s.get("k").as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn append_existing() {
        let s = test_store();
        s.set("k".into(), b"hello".to_vec(), None);
        assert_eq!(s.append("k", b" world"), Some(11));
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
    fn keys_star_lists_the_empty_key() {
        // Redis' `KEYS *` short-circuits to "all keys", so an empty key is
        // listed even though no glob matches the empty string.
        let s = test_store();
        s.set(String::new(), b"a".to_vec(), None);
        s.set("x".into(), b"b".to_vec(), None);
        let mut keys = s.keys("*");
        keys.sort();
        assert_eq!(keys, vec!["", "x"]);
        assert_eq!(s.keys("?"), vec!["x"]);
    }

    #[test]
    fn keys_includes_hash_keys_and_skips_expired() {
        let s = test_store();
        s.set("str:1".into(), b"a".to_vec(), None);
        s.set("str:gone".into(), b"a".to_vec(), Some(Duration::from_millis(1)));
        let _ = s.hset("hash:1", "f", b"v".to_vec());
        std::thread::sleep(Duration::from_millis(10));
        let mut keys = s.keys("*:*");
        keys.sort();
        assert_eq!(keys, vec!["hash:1", "str:1"]);
    }

    /// Regression for KV stress report F-0: one `KEYS` with a pathological
    /// pattern against a ~60-byte key used to run >30 s while holding a
    /// shard read lock. It must now finish in well under a second, and
    /// writers to the same store must not be blocked behind it.
    #[test]
    fn keys_pathological_pattern_is_fast_over_many_keys() {
        let s = test_store();
        for i in 0..2_000 {
            s.set(format!("{}{i}", "a".repeat(60)), b"v".to_vec(), None);
        }
        let start = Instant::now();
        assert_eq!(s.keys("*a*a*a*a*a*a*a*a*a*a*a*a*b"), Vec::<String>::new());
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(2), "KEYS took {elapsed:?}");
    }

    /// Plant an entry that expired a minute ago — and its TTL hint — without
    /// waiting on the clock, accounted exactly as a real write would be.
    fn plant_expired(s: &Store, key: &str, value: &[u8]) {
        let past = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
        let entry = Entry::with_expiry(Bytes::copy_from_slice(value), key.len(), false, past, 0);
        s.mem_add(entry.mem_size);
        s.data.insert(key.to_string(), entry);
        s.ttl_keys.insert(key.to_string(), past);
    }

    #[test]
    fn active_expire_cycle_reaps() {
        let s = test_store();
        plant_expired(&s, "expired", b"v");
        s.set("alive".into(), b"v".to_vec(), None);
        let reaped = s.active_expire_cycle(Duration::from_secs(1));
        assert_eq!(reaped, 1);
        assert!(!s.data.contains_key("expired"));
        assert!(s.exists("alive"));
        assert_eq!(s.ttl_keys.len(), 0);
    }

    /// F-1: `expire_pass(100)` reclaimed at most 100 keys a second. One cycle
    /// must keep going while most of what it samples is expired.
    #[test]
    fn active_expire_cycle_is_not_capped_at_a_fixed_key_count() {
        let s = test_store();
        for i in 0..50_000 {
            plant_expired(&s, &format!("t{i}"), b"v");
        }
        let reaped = s.active_expire_cycle(Duration::from_secs(30));
        assert_eq!(reaped, 50_000, "an all-expired index is drained in one cycle");
        assert_eq!(s.len(), 0);
        assert_eq!(s.mem_used(), 0, "reaped bytes are given back");
    }

    /// F-1: `expire_pass` restarted from the front of the TTL index every pass
    /// and gave up after 100 *visited* hints, so once the front held live
    /// long-TTL keys, expired keys behind them were never reached. The cursor
    /// must reach every hint within one sweep: 8000 hints at 20 per round is
    /// 400 rounds, and every cycle runs at least one.
    #[test]
    fn scan_map_sweep_visits_every_entry_exactly_once() {
        // Walking a map in fixed-size steps from a saved cursor must visit each
        // entry once per sweep. A step that ended on a shard's last bucket
        // used to resume at that same shard's first bucket, re-walking it.
        for n in [1usize, 19, 20, 21, 500, 4_096, 10_007] {
            for step in [1usize, 7, 20, 64] {
                let map: DashMap<String, ()> = DashMap::new();
                for i in 0..n {
                    map.insert(format!("k{i}"), ());
                }
                // The cursor is cyclic, so the call that completes a sweep may
                // carry on into the next one; only the first `n` visits are
                // one sweep.
                let mut order: Vec<String> = Vec::new();
                let mut cursor = MapCursor::default();
                let mut calls = 0;
                while order.len() < n {
                    let (_, next) = scan_map(&map, cursor, step, |k, ()| order.push(k.clone()));
                    cursor = next;
                    calls += 1;
                }
                assert_eq!(calls, n.div_ceil(step), "n={n} step={step}: extra calls");
                let sweep: std::collections::HashSet<&String> = order[..n].iter().collect();
                assert_eq!(sweep.len(), n, "n={n} step={step}: a sweep revisited or missed keys");
            }
        }
    }

    #[test]
    fn active_expire_is_not_starved_by_live_long_ttl_keys() {
        let s = test_store();
        for i in 0..4_000 {
            assert!(s.set(format!("long{i}"), b"v".to_vec(), Some(Duration::from_secs(86_400))));
        }
        for i in 0..4_000 {
            plant_expired(&s, &format!("dead{i}"), b"v");
        }
        // Exactly one sweep, with no slack: each cycle runs at least one round of
        // 20 whatever the clock says (the budget is effectively unlimited, so a
        // slow or loaded machine changes nothing), and the cursor never revisits
        // a bucket. This flaked ~5% when a cursor ending on a shard's last
        // bucket wrapped back to that shard's start.
        let budget = Duration::from_secs(3_600);
        let reaped: usize = (0..400).map(|_| s.active_expire_cycle(budget)).sum();
        assert_eq!(reaped, 4_000, "expired keys left unreaped behind live TTL keys");
        assert_eq!(s.ttl_keys.len(), 4_000, "only the live keys' hints remain");
        for i in 0..4_000 {
            assert!(s.data.contains_key(&format!("long{i}")), "a live key was reaped");
        }
    }

    /// A store full of live TTL keys must cost one small round per cycle,
    /// not a walk of its index.
    #[test]
    fn active_expire_cycle_stops_early_on_live_keys() {
        let s = test_store();
        for i in 0..10_000 {
            assert!(s.set(format!("live{i}"), b"v".to_vec(), Some(Duration::from_secs(3_600))));
        }
        assert_eq!(s.expire_round().sampled, EXPIRE_SAMPLE_KEYS);
        assert_eq!(s.active_expire_cycle(Duration::from_secs(5)), 0);
        assert_eq!(s.len(), 10_000);
    }

    /// F-1: under `noeviction`, bytes held by expired-but-unreaped keys
    /// counted against the limit, so a store with **no live keys** refused
    /// writes as out of memory (19,499 of 20,000 in the stress run). A write
    /// must reclaim expired keys before it refuses.
    #[test]
    fn noeviction_reclaims_expired_keys_before_refusing() {
        let s = Store::new(StoreConfig {
            memory_limit: 64 * 1024,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        let mut i = 0;
        while s.mem_used() + 300 < 64 * 1024 {
            plant_expired(&s, &format!("old{i}"), &[b'x'; 100]);
            i += 1;
        }
        // As many fresh writes as the limit holds once the expired keys are
        // gone (~240 accounted bytes each). Before the fix the first one was
        // refused.
        for n in 0..200 {
            assert!(s.set(format!("new{n}"), vec![b'y'; 100], None), "write {n} refused");
        }
        assert!(s.mem_used() <= 64 * 1024);
        // The same holds for the other write paths.
        let s = Store::new(StoreConfig {
            memory_limit: 8 * 1024,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        for i in 0..40 {
            plant_expired(&s, &format!("old{i}"), &[b'x'; 100]);
        }
        assert!(s.mem_used() > 4 * 1024);
        assert_eq!(s.incr_by("ctr", 1), Ok(1));
        assert_eq!(s.set_nx_outcome("nx".into(), vec![b'z'; 2048], None), SetNxOutcome::Inserted);
        assert_eq!(s.hset("h", "f", vec![b'h'; 2048]), Ok(true));
        assert!(s.set("ap".into(), b"a".to_vec(), None));
        assert_eq!(s.append("ap", &[b'b'; 2048]), Some(2049));
    }

    /// F-19: `hset` never consulted the memory budget, so hashes grew past
    /// `memory_limit` under every policy.
    #[test]
    fn hset_respects_the_memory_limit() {
        let limit = 16 * 1024;
        let s = Store::new(StoreConfig {
            memory_limit: limit,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        let mut refused = false;
        for i in 0..100 {
            match s.hset("h", &format!("f{i}"), vec![b'x'; 1024]) {
                Ok(_) => {}
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused, "100 KiB of fields into a 16 KiB store must be refused");
        assert!(s.mem_used() <= limit, "hash grew past the limit: {}", s.mem_used());

        // Under an evicting policy the write makes room instead of refusing.
        let s = Store::new(StoreConfig {
            memory_limit: limit,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        for i in 0..40 {
            assert!(s.set(format!("s{i}"), vec![b'x'; 300], None));
        }
        assert_eq!(s.hset("h", "f", vec![b'y'; 4096]), Ok(true));
        assert!(s.mem_used() <= limit);
    }

    /// F-19 audit: appending to a live key grew it in place with no budget
    /// check, so repeated APPENDs could take the store far past its limit.
    #[test]
    fn append_growth_respects_the_memory_limit() {
        let limit = 8 * 1024;
        let s = Store::new(StoreConfig {
            memory_limit: limit,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        assert!(s.set("log".into(), b"start".to_vec(), None));
        let mut refused = false;
        for _ in 0..100 {
            if s.append("log", &[b'x'; 1024]).is_none() {
                refused = true;
                break;
            }
        }
        assert!(refused, "100 KiB of appends into an 8 KiB store must be refused");
        assert!(s.mem_used() <= limit, "append grew past the limit: {}", s.mem_used());
        let len = s.get("log").map(|v| v.len()).unwrap();
        assert!(
            len > 5 && (len - 5).is_multiple_of(1024),
            "a refused append must leave the value intact"
        );
    }

    /// A refused write under `noeviction` with only live data still refuses —
    /// the reclaim must not turn into eviction.
    #[test]
    fn noeviction_reclaim_never_removes_live_keys() {
        let s = Store::new(StoreConfig {
            memory_limit: 4 * 1024,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        let mut n = 0;
        while s.set(format!("live{n}"), vec![b'x'; 100], Some(Duration::from_secs(3_600))) {
            n += 1;
        }
        assert!(n > 0);
        assert!(!s.set("one-more".into(), vec![b'y'; 100], None));
        assert_eq!(s.len(), n, "no live key may be removed to make room");
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
    fn set_nx_outcome_distinguishes_exists_from_oom() {
        // #477: the boolean `set_nx` conflated "already exists" with an
        // out-of-memory refusal. The outcome variant must tell them apart.
        let s = Store::new(StoreConfig {
            memory_limit: 200,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });
        assert_eq!(s.set_nx_outcome("k".into(), b"v".to_vec(), None), SetNxOutcome::Inserted);
        // Live key present → Exists, NOT OutOfMemory.
        assert_eq!(s.set_nx_outcome("k".into(), b"v2".to_vec(), None), SetNxOutcome::Exists);
        // A fresh key too large for the budget → OutOfMemory, NOT Exists.
        assert_eq!(
            s.set_nx_outcome("big".into(), vec![0u8; 1024], None),
            SetNxOutcome::OutOfMemory
        );

        // The boolean wrapper still means exactly "was it inserted?".
        let s2 = test_store();
        assert!(s2.set_nx("fresh".into(), b"x".to_vec(), None));
        assert!(!s2.set_nx("fresh".into(), b"y".to_vec(), None));
    }

    #[test]
    fn expired_key_is_still_lazily_reaped_on_get() {
        // The compare-and-remove must still reap in the ordinary case: an
        // expired key with no concurrent writer is removed on read.
        let s = test_store();
        let past = Instant::now() - Duration::from_secs(60);
        s.data.insert("k".into(), Entry::with_expiry(Bytes::from_static(b"v"), 1, false, past, 0));
        s.ttl_keys.insert("k".into(), past);

        assert_eq!(s.get("k"), None);
        assert!(!s.data.contains_key("k"), "expired entry should be reaped");
        assert!(!s.ttl_keys.contains_key("k"), "TTL hint should be cleared");
    }

    #[test]
    fn refresh_at_expiry_survives_concurrent_reader_reap() {
        // #478: a reader observes a key expired and, before it reaps, a writer
        // stores a fresh value. The reader's compare-and-remove must not
        // delete that fresh write.
        let s = test_store();

        // Plant an already-expired entry directly so we know the exact
        // `expires_at` a reader would observe — fully deterministic, no sleep.
        let observed = Instant::now() - Duration::from_secs(60);
        s.data.insert(
            "k".into(),
            Entry::with_expiry(Bytes::from_static(b"old"), 1, false, observed, 0),
        );

        // The writer lands in the gap after the reader observed expiry,
        // replacing the entry with a fresh, non-expiring value.
        assert!(s.set("k".into(), b"new".to_vec(), None));

        // The stale reader now runs its reap with the expiry it observed. The
        // live entry carries a different `expires_at` (None), so the reap is a
        // no-op and the fresh value survives.
        assert!(!s.reap_if_expired("k", observed), "reap must not remove the refreshed value");
        assert_eq!(s.get("k").as_deref(), Some(&b"new"[..]));

        // Documents the pre-fix hazard on the same setup: the old
        // unconditional reap (`remove_local`) destroys the fresh write.
        assert!(s.set("k".into(), b"new2".to_vec(), None));
        s.remove_local("k"); // what get()/exists()/pttl() used to call
        assert_eq!(s.get("k"), None, "unconditional reap loses the refresh — the #478 bug");
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
        assert_eq!(s.append("key", b"hello"), Some(5));
        assert_eq!(s.append("key", b" world"), Some(11));
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
        // Insert 3 keys with sleeps larger than LRU_TOUCH_GRANULARITY_NANOS
        // (100ms). Below that granularity `touch` skips the store — LRU
        // semantics are documented to be preserved *at 100ms granularity*.
        s.set("oldest".into(), vec![1u8; 50], None);
        std::thread::sleep(Duration::from_millis(150));
        s.set("middle".into(), vec![2u8; 50], None);
        std::thread::sleep(Duration::from_millis(150));
        s.set("newest".into(), vec![3u8; 50], None);
        std::thread::sleep(Duration::from_millis(150));

        // Touch "oldest" to refresh its LRU timestamp past the coarse
        // granularity window.
        let _ = s.get("oldest");
        std::thread::sleep(Duration::from_millis(150));

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

    // ── Sampled-LRU shard coverage (regression for the front-bias) ──

    /// With the old front-of-map sampling, a key living in a late shard was
    /// never an eviction candidate. Random-shard sampling must be able to
    /// evict the genuinely-oldest key even when the store spans many shards.
    #[test]
    fn sampled_lru_can_evict_oldest_across_many_shards() {
        let s = Store::new(StoreConfig {
            memory_limit: 0, // unlimited: fill without triggering eviction yet
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        // Insert one clearly-oldest key, then a large population across the
        // whole keyspace so `oldest` almost certainly lands in a late shard.
        s.set("ancient".into(), vec![0u8; 50], None);
        std::thread::sleep(Duration::from_millis(150));
        for i in 0..200 {
            s.set(format!("k{i}"), vec![0u8; 50], None);
        }
        // Re-touch every non-ancient key so `ancient` is unambiguously the LRU
        // victim regardless of which shard it hashed into.
        std::thread::sleep(Duration::from_millis(150));
        for i in 0..200 {
            let _ = s.get(&format!("k{i}"));
        }
        std::thread::sleep(Duration::from_millis(150));

        // Sampling is probabilistic: a 16-key sample from a 201-key store can
        // miss `ancient` on any single draw. But `ancient` is the strictly
        // oldest key, so it is the victim of every sample that happens to
        // include it; driving the sampler repeatedly must eventually evict it
        // (and it must NEVER evict it before the newer keys are gone, which is
        // covered separately by `allkeys_lru_evicts_oldest_accessed_key`).
        let mut evicted_ancient = false;
        for _ in 0..500 {
            if s.get("ancient").is_none() {
                evicted_ancient = true;
                break;
            }
            if let Some(victim) = s.lru_victim(false) {
                s.evict_victim(&victim);
            }
        }
        assert!(evicted_ancient, "sampled LRU across many shards never evicted the oldest key");
    }

    /// `lru_victim` with `volatile_only = true` must never return a
    /// persistent (TTL-less) key, even when persistent keys vastly outnumber
    /// the volatile ones and are spread across shards.
    #[test]
    fn sampled_volatile_lru_never_returns_persistent_key() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::VolatileLru,
            compression: CompressionConfig::default(),
        });
        for i in 0..200 {
            s.set(format!("perm{i}"), vec![0u8; 32], None);
        }
        // A small volatile minority.
        for i in 0..5 {
            s.set(format!("vol{i}"), vec![0u8; 32], Some(Duration::from_secs(3600)));
        }
        // Every victim the volatile sampler yields must carry a TTL.
        for _ in 0..500 {
            if let Some(victim) = s.lru_victim(true) {
                assert!(
                    victim.key.starts_with("vol"),
                    "volatile sampler returned persistent key {}",
                    victim.key
                );
            }
        }
    }

    /// `lru_victim` returns `None` when there is nothing eligible: an empty
    /// store, and a volatile-only sample over a store with no TTL'd keys.
    #[test]
    fn lru_victim_none_when_nothing_eligible() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::VolatileLru,
            compression: CompressionConfig::default(),
        });
        assert!(s.lru_victim(false).is_none(), "empty store has no victim");
        s.set("perm".into(), vec![0u8; 16], None);
        assert!(
            s.lru_victim(true).is_none(),
            "volatile sample over persistent-only store must be None"
        );
        assert!(s.lru_victim(false).is_some(), "allkeys sample sees the persistent key");
    }

    /// Fill a store of `limit` bytes under `policy` with 100-byte keys until
    /// the first eviction (or refusal), TTL'd when `ttl` is set.
    fn full_store(limit: usize, policy: EvictionPolicy, ttl: Option<Duration>) -> Arc<Store> {
        let s = Store::new(StoreConfig {
            memory_limit: limit,
            eviction_policy: policy,
            compression: CompressionConfig::default(),
        });
        let mut i = 0;
        while s.mem_used() + 400 < limit {
            assert!(s.set(format!("small{i}"), vec![b's'; 100], ttl));
            i += 1;
        }
        s
    }

    /// F-3: eviction stopped after 100 victims (~25 KB of 100-byte keys), so
    /// under every evicting policy a value larger than that was refused once
    /// the store was full of small keys — 155,921 refused writes under the
    /// default `allkeys-lru` in the stress run.
    #[test]
    fn eviction_makes_room_for_a_large_value_among_small_keys() {
        let limit = 1024 * 1024;
        for (policy, ttl) in [
            (EvictionPolicy::AllKeysLru, None),
            (EvictionPolicy::AllKeysRandom, None),
            (EvictionPolicy::VolatileLru, Some(Duration::from_secs(3_600))),
        ] {
            let s = full_store(limit, policy, ttl);
            let before = s.len();
            assert!(
                s.set("big".into(), vec![b'b'; 256 * 1024], None),
                "{policy:?}: a 256 KiB write into a full 1 MiB store was refused"
            );
            assert!(s.mem_used() <= limit, "{policy:?}: over the limit");
            assert!(before - s.len() > 100, "{policy:?}: needed more than 100 victims");
            assert_eq!(s.get("big").map(|v| v.len()), Some(256 * 1024));
        }
    }

    /// The test above flaked on a loaded CI runner (~50% failures with the
    /// CPU oversubscribed): eviction gave up after 50 ms of wall-clock time,
    /// so whether a write fit depended on how often its thread was
    /// descheduled. Here every step is made slow on purpose — 100 µs, so the
    /// ~1,300 victims take well over 50 ms — and the write must still land.
    #[test]
    fn eviction_is_not_bounded_by_wall_clock_time() {
        let s = full_store(1024 * 1024, EvictionPolicy::AllKeysLru, None);
        let _hook = EvictionHook::install(|_| std::thread::sleep(Duration::from_micros(100)));
        let start = Instant::now();
        assert!(
            s.set("big".into(), vec![b'b'; 256 * 1024], None),
            "a slow-but-progressing eviction refused a write that fits"
        );
        assert!(start.elapsed() > Duration::from_millis(50), "the hook did not slow eviction");
        assert!(s.mem_used() <= 1024 * 1024);
        assert_eq!(s.get("big").map(|v| v.len()), Some(256 * 1024));
    }

    /// What does bound eviction: a writer racing others that take the freed
    /// room as fast as it is freed must be refused, not evict forever. The
    /// hook plays a concurrent writer that, after every victim, stores 1 KiB
    /// without evicting — more than each ~200-byte victim frees.
    #[test]
    fn eviction_gives_up_when_concurrent_writes_take_the_room() {
        static STEPS: AtomicUsize = AtomicUsize::new(0);
        let s = full_store(1024 * 1024, EvictionPolicy::AllKeysLru, None);
        let _hook = EvictionHook::install(|s| {
            let n = STEPS.fetch_add(1, Ordering::Relaxed);
            let key = format!("racer{n}");
            let entry = Entry::new(Bytes::from(vec![b'r'; 1024]), key.len(), false, s.now_nanos());
            s.mem_add(entry.mem_size);
            s.data.insert(key, entry);
        });
        assert!(
            !s.set("big".into(), vec![b'b'; 256 * 1024], None),
            "the write cannot fit while every victim's room is taken"
        );
        // The bound is ~2 × 256 KiB of freed bytes: a few thousand victims,
        // never an unbounded loop.
        let steps = STEPS.load(Ordering::Relaxed);
        assert!(steps > 100 && steps < 20_000, "{steps} eviction steps");
        assert!(s.get("big").is_none());
    }

    /// A value that cannot fit even in an empty store is refused at once,
    /// without evicting anything.
    #[test]
    fn eviction_refuses_a_value_larger_than_the_limit_without_evicting() {
        let s = full_store(64 * 1024, EvictionPolicy::AllKeysLru, None);
        let before = s.len();
        assert!(!s.set("huge".into(), vec![b'h'; 128 * 1024], None));
        assert_eq!(s.len(), before, "nothing may be evicted for an impossible write");
    }

    /// F-4: `volatile-lru` skipped TTL-less keys without counting them, so
    /// with few TTL'd keys every write walked the whole map (~3 ms per write,
    /// 1,256 writes/s). With no TTL'd key at all the refusal must be cheap.
    #[test]
    fn volatile_lru_refusal_does_not_scan_persistent_keys() {
        let s = full_store(32 * 1024 * 1024, EvictionPolicy::VolatileLru, None);
        assert!(s.len() > 100_000);
        let start = Instant::now();
        for i in 0..2_000 {
            assert!(!s.set(format!("x{i}"), vec![b'x'; 1024], None));
        }
        let elapsed = start.elapsed();
        // The old code walked >100k entries per refusal: 2,000 refusals were
        // ~200M entry visits. Now each is a handful of shard probes.
        assert!(elapsed < Duration::from_secs(5), "2000 volatile-lru refusals took {elapsed:?}");
    }

    /// F-5: `allkeys-random` picked its victim with `data.iter().nth(random)`
    /// — O(n) per victim — so filling a store was ~20x slower than LRU.
    #[test]
    fn allkeys_random_eviction_is_not_linear_per_victim() {
        let s = full_store(16 * 1024 * 1024, EvictionPolicy::AllKeysRandom, None);
        let resident = s.len();
        assert!(resident > 50_000);
        let start = Instant::now();
        // Every write now evicts: 100k victims from a ~60k-key store was
        // ~3 billion iterator steps with `nth`.
        for i in 0..100_000 {
            assert!(s.set(format!("more{i}"), vec![b'm'; 100], None));
        }
        let elapsed = start.elapsed();
        assert!(s.mem_used() <= 16 * 1024 * 1024);
        assert!(elapsed < Duration::from_secs(20), "100k random evictions took {elapsed:?}");
    }

    /// The pool keeps good candidates across steps, so the victims of a run
    /// of evictions are the least recently used keys, not just "the oldest of
    /// one 16-key sample".
    #[test]
    fn lru_pool_evicts_cold_keys_before_hot_ones() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        for i in 0..2_000 {
            s.set(format!("k{i}"), vec![0u8; 10], None);
        }
        // Keys 0..1000 are cold; 1000..2000 are touched well after them.
        for i in 0..2_000 {
            let at = if i < 1_000 { 1_000 } else { 1_000_000_000 };
            s.data.get(&format!("k{i}")).unwrap().touch_precise(at);
        }
        let mut hot_evicted = 0;
        for _ in 0..500 {
            let victim = s.lru_victim(false).expect("store is not empty");
            if victim.key["k".len()..].parse::<usize>().unwrap() >= 1_000 {
                hot_evicted += 1;
            }
            s.evict_victim(&victim);
        }
        assert_eq!(hot_evicted, 0, "a hot key was evicted while cold keys remained");
    }

    /// F-19: hashes were invisible to eviction, so a store filled by HSET
    /// refused every write under `allkeys-lru`/`allkeys-random` instead of
    /// evicting.
    #[test]
    fn evicting_policies_can_evict_hashes() {
        for policy in [EvictionPolicy::AllKeysLru, EvictionPolicy::AllKeysRandom] {
            let s = Store::new(StoreConfig {
                memory_limit: 64 * 1024,
                eviction_policy: policy,
                compression: CompressionConfig::default(),
            });
            let mut i = 0;
            while s.mem_used() + 2_000 < 64 * 1024 {
                assert_eq!(s.hset(&format!("h{i}"), "f", vec![b'h'; 1_000]), Ok(true));
                i += 1;
            }
            assert!(
                s.set("str".into(), vec![b's'; 8 * 1024], None),
                "{policy:?}: a store full of hashes refused instead of evicting"
            );
            assert!(s.mem_used() <= 64 * 1024);
        }
    }

    /// Evicting a string key leaves a hash of the same name alone (the two
    /// maps share names; see the module docs on strings and hashes).
    #[test]
    fn evicting_a_string_keeps_a_same_named_hash() {
        let s = test_store();
        s.set("both".into(), b"v".to_vec(), None);
        assert_eq!(s.hset("both", "f", b"hv".to_vec()), Ok(true));
        assert!(
            s.evict_victim(&Victim { key: "both".into(), hash: false, idle_rank: 1 }).is_some()
        );
        assert_eq!(s.get("both"), None);
        assert_eq!(s.hget("both", "f").as_deref(), Some(&b"hv"[..]));
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

    // ── Atomic overwrite (#452) ─────────────────────────────────────
    //
    // `set_local` used to `remove` the old entry to reclaim its bytes and only
    // then check the budget and insert. Two defects fell out of that ordering,
    // and there is one test below for each:
    //
    //   * the key did not exist between the remove and the insert, so a
    //     concurrent reader got a miss on a key nobody had deleted; and
    //   * a write the budget then REFUSED had already destroyed the value it
    //     was failing to replace.
    //
    // The second is deterministic and single-threaded, which is why it leads.

    #[test]
    fn over_budget_overwrite_refuses_the_write_and_keeps_the_existing_value() {
        let s = Store::new(StoreConfig {
            memory_limit: 1024,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });

        // BEFORE: the key is present with the original value. Asserted so the
        // final assertion cannot pass merely because the write never happened.
        assert!(s.set("k".into(), vec![b'a'; 64], None), "the setup write must fit");
        assert_eq!(s.get("k").as_deref(), Some(&[b'a'; 64][..]));
        let mem_before = s.mem_used();

        // A replacement far too large for the limit. Refusing it is correct.
        assert!(!s.set("k".into(), vec![b'b'; 2048], None), "an over-budget write must be refused");

        // AFTER: refusing it must not have deleted what was already there.
        // Against the remove-first ordering this is `None` — the rejected
        // write ate the existing value.
        assert_eq!(
            s.get("k").as_deref(),
            Some(&[b'a'; 64][..]),
            "a refused write must leave the existing value intact"
        );
        assert_eq!(s.mem_used(), mem_before, "a refused write must not move the accounting");
    }

    #[test]
    fn over_budget_overwrite_keeps_the_existing_ttl() {
        let s = Store::new(StoreConfig {
            memory_limit: 1024,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });

        assert!(s.set("sess".into(), vec![b'a'; 64], Some(Duration::from_secs(600))));
        let ttl_before = s.pttl("sess").expect("key must exist before the refused write");
        assert!(ttl_before > 0, "the setup write must have set a TTL");

        assert!(!s.set("sess".into(), vec![b'b'; 2048], Some(Duration::from_secs(600))));

        // Both the value and its expiry survive: the old code removed the
        // entry AND pruned its `ttl_keys` hint on the way out.
        assert_eq!(s.get("sess").as_deref(), Some(&[b'a'; 64][..]));
        assert!(s.pttl("sess").is_some_and(|ms| ms > 0), "the TTL must survive a refused write");
        assert!(s.ttl_keys.contains_key("sess"), "the TTL hint must survive a refused write");
    }

    #[test]
    fn repeated_overwrites_do_not_drift_memory_accounting() {
        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });

        s.set("k".into(), vec![0u8; 128], None);
        let after_first = s.mem_used();

        // The replacement is now charged as a delta and reconciled from the
        // entry `insert` returned. Getting either half wrong shows up here as
        // a monotonic leak (forgot the subtract) or a drain to zero (charged
        // the peek twice).
        for i in 0..500u32 {
            #[allow(clippy::cast_possible_truncation)]
            let fill = i as u8;
            assert!(s.set("k".into(), vec![fill; 128], None));
        }
        assert_eq!(
            s.mem_used(),
            after_first,
            "same-size overwrites must not leak or double-count bytes"
        );

        s.remove("k");
        assert_eq!(s.mem_used(), 0, "accounting must return to zero once the key is gone");
    }

    /// A reader must observe the key **continuously present** while another
    /// thread overwrites it.
    ///
    /// This one is probabilistic — the defect is a race, and closing it means
    /// there is no longer a window to hit, so there is nothing deterministic
    /// left to trigger. It is made tight rather than lucky: three reader
    /// threads spin on `get` for the whole of 300k overwrites, so the writer's
    /// hole is offered millions of chances to be observed. Against the
    /// remove-first ordering it fails within the first few thousand writes on
    /// every machine tried; a regression would have to re-open the window AND
    /// have every one of those millions of reads miss it to sneak through.
    ///
    /// The TTL is not decoration: it puts the `ttl_keys.insert` (a `String`
    /// allocation plus a `DashSet` shard lock) inside the window, which is the
    /// shape a real session write has.
    #[test]
    fn concurrent_reader_never_sees_a_miss_while_a_key_is_overwritten() {
        const WRITES: usize = 300_000;
        const READERS: usize = 3;
        let old = vec![b'a'; 64];
        let new = vec![b'b'; 64];

        let s = Store::new(StoreConfig {
            memory_limit: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            compression: CompressionConfig::default(),
        });

        // BEFORE: the key is present, and stays present for the whole test —
        // nothing below ever deletes it.
        assert!(s.set("hot".into(), old.clone(), Some(Duration::from_secs(600))));
        assert_eq!(s.get("hot").as_deref(), Some(&old[..]));

        let done = std::sync::atomic::AtomicBool::new(false);
        let misses = AtomicUsize::new(0);
        let torn = AtomicUsize::new(0);
        let reads = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..READERS {
                scope.spawn(|| {
                    let mut local_reads = 0usize;
                    while !done.load(Ordering::Relaxed) {
                        match s.get("hot") {
                            None => {
                                misses.fetch_add(1, Ordering::Relaxed);
                            }
                            // Neither value is ever partially written, but
                            // assert it: a "present" that is not one of the
                            // two written values would mean something worse
                            // than a miss.
                            Some(v) if v != old && v != new => {
                                torn.fetch_add(1, Ordering::Relaxed);
                            }
                            Some(_) => {}
                        }
                        local_reads += 1;
                    }
                    reads.fetch_add(local_reads, Ordering::Relaxed);
                });
            }

            for i in 0..WRITES {
                let value = if i % 2 == 0 { new.clone() } else { old.clone() };
                assert!(s.set("hot".into(), value, Some(Duration::from_secs(600))));
            }
            done.store(true, Ordering::Relaxed);
        });

        let reads = reads.load(Ordering::Relaxed);
        assert!(
            reads > WRITES,
            "the readers must have out-run the writer for this to mean anything (reads: {reads})"
        );
        assert_eq!(
            misses.load(Ordering::Relaxed),
            0,
            "a key that is only ever overwritten must never read as absent ({reads} reads)"
        );
        assert_eq!(
            torn.load(Ordering::Relaxed),
            0,
            "a read returned a value that was never written"
        );
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

    // Glob semantics are covered by the Redis table in `store::glob`.

    // ── Hash operations ─────────────────────────────────────────

    #[test]
    fn hset_and_hget() {
        let s = test_store();
        assert_eq!(s.hset("myhash", "field1", b"value1".to_vec()), Ok(true));
        assert_eq!(s.hget("myhash", "field1").as_deref(), Some(&b"value1"[..]));
    }

    #[test]
    fn hset_overwrite_returns_false() {
        let s = test_store();
        assert_eq!(s.hset("myhash", "f", b"v1".to_vec()), Ok(true));
        assert_eq!(s.hset("myhash", "f", b"v2".to_vec()), Ok(false));
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
        let _ = s.hset("myhash", "f1", b"v".to_vec());
        assert_eq!(s.hget("myhash", "f2"), None);
    }

    #[test]
    fn hdel_existing() {
        let s = test_store();
        let _ = s.hset("h", "f", b"v".to_vec());
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
        let _ = s.hset("h", "a", b"1".to_vec());
        let _ = s.hset("h", "b", b"2".to_vec());
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
        let _ = s.hset("h", "x", b"10".to_vec());
        let _ = s.hset("h", "y", b"20".to_vec());
        let mut keys = s.hkeys("h");
        keys.sort();
        assert_eq!(keys, vec!["x", "y"]);
        assert_eq!(s.hvals("h").len(), 2);
    }

    #[test]
    fn hlen() {
        let s = test_store();
        assert_eq!(s.hlen("h"), 0);
        let _ = s.hset("h", "a", b"1".to_vec());
        let _ = s.hset("h", "b", b"2".to_vec());
        assert_eq!(s.hlen("h"), 2);
    }

    #[test]
    fn hexists() {
        let s = test_store();
        let _ = s.hset("h", "a", b"1".to_vec());
        assert!(s.hexists("h", "a"));
        assert!(!s.hexists("h", "b"));
        assert!(!s.hexists("nope", "a"));
    }

    #[test]
    fn hash_type_detection() {
        let s = test_store();
        let _ = s.hset("h", "f", b"v".to_vec());
        assert!(s.is_hash("h"));
        s.set("str".into(), b"v".to_vec(), None);
        assert!(!s.is_hash("str"));
    }

    #[test]
    fn hash_exists_in_global_exists() {
        let s = test_store();
        let _ = s.hset("h", "f", b"v".to_vec());
        assert!(s.exists("h"));
    }

    #[test]
    fn hash_remove_via_global_remove() {
        let s = test_store();
        let _ = s.hset("h", "f", b"v".to_vec());
        assert!(s.remove("h"));
        assert!(!s.exists("h"));
    }

    #[test]
    fn hash_in_global_keys() {
        let s = test_store();
        s.set("str_key".into(), b"v".to_vec(), None);
        let _ = s.hset("hash_key", "f", b"v".to_vec());
        let mut keys = s.keys("*");
        keys.sort();
        assert_eq!(keys, vec!["hash_key", "str_key"]);
    }

    #[test]
    fn hash_in_global_len() {
        let s = test_store();
        s.set("a".into(), b"1".to_vec(), None);
        let _ = s.hset("b", "f", b"2".to_vec());
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn hdel_removes_empty_hash() {
        let s = test_store();
        let _ = s.hset("h", "only", b"v".to_vec());
        s.hdel("h", "only");
        assert!(!s.exists("h"));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn flush_clears_hashes() {
        let s = test_store();
        let _ = s.hset("h", "f", b"v".to_vec());
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
        let _ = s.hset("h", "f1", b"hv1".to_vec());
        let _ = s.hset("h", "f2", b"hv2".to_vec());
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
    ///
    /// `publishes` is recorded separately from `sets` because the two carry
    /// different contracts: a `set` is an intent the replicator must apply,
    /// a publish is an already-applied local state it must only fan out.
    #[derive(Debug, Default)]
    struct RecordingReplicator {
        sets: std::sync::Mutex<Vec<RecordedSet>>,
        removes: std::sync::Mutex<Vec<String>>,
        expires: std::sync::Mutex<Vec<RecordedExpire>>,
        publishes: std::sync::Mutex<Vec<RecordedSet>>,
        removes_published: std::sync::Mutex<Vec<String>>,
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
        fn replicate_published(&self, key: String, value: Vec<u8>, ttl: Option<Duration>) {
            self.publishes.lock().unwrap().push((key, value, ttl));
        }
        fn replicate_hash_field(
            &self,
            _key: &str,
            _field: &str,
            apply: &mut dyn FnMut() -> Option<crate::store::HashFieldState>,
        ) -> bool {
            let _ = apply();
            true
        }
        fn replicate_remove_published(&self, key: String) {
            self.removes_published.lock().unwrap().push(key);
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

    /// A replicator that intercepts reads with a fixed [`ColdRead`] answer
    /// and counts how often it was asked.
    #[derive(Debug)]
    struct ColdReplicator {
        answer: std::sync::Mutex<ColdRead>,
        asked: AtomicUsize,
        fetched_through: AtomicUsize,
    }

    impl ColdReplicator {
        fn answering(answer: ColdRead) -> Arc<Self> {
            Arc::new(Self {
                answer: std::sync::Mutex::new(answer),
                asked: AtomicUsize::new(0),
                fetched_through: AtomicUsize::new(0),
            })
        }
    }

    impl Replicator for ColdReplicator {
        fn replicate_set(&self, _: String, _: Vec<u8>, _: Option<Duration>) -> bool {
            true
        }
        fn replicate_remove(&self, _: &str) -> bool {
            true
        }
        fn replicate_expire(&self, _: &str, _: Duration) -> bool {
            true
        }
        fn replicate_published(&self, _: String, _: Vec<u8>, _: Option<Duration>) {}
        fn replicate_remove_published(&self, _: String) {}
        fn replicate_hash_field(
            &self,
            _key: &str,
            _field: &str,
            apply: &mut dyn FnMut() -> Option<crate::store::HashFieldState>,
        ) -> bool {
            let _ = apply();
            true
        }
        fn fetch_through(&self, _: &str) -> Option<Vec<u8>> {
            self.fetched_through.fetch_add(1, Ordering::SeqCst);
            Some(b"from-cluster".to_vec())
        }
        fn cold_read(&self, _: &str) -> ColdRead {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer.lock().unwrap().clone()
        }
    }

    /// `get_through` asks the replicator before the local map: a `Found`
    /// answer replaces a local hit, `Absent` turns one into a miss, and
    /// `Warm` leaves the local-then-fetch-through path exactly as it was.
    #[test]
    fn get_through_consults_cold_read_before_the_local_map() {
        let s = test_store();
        s.set_local("k".into(), b"local".to_vec(), None);
        let rep = ColdReplicator::answering(ColdRead::Found(b"peer".to_vec()));
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        assert_eq!(s.get_through("k").as_deref(), Some(b"peer".as_slice()));
        assert_eq!(s.get("k").as_deref(), Some(b"local".as_slice()), "the local map is untouched");

        *rep.answer.lock().unwrap() = ColdRead::Absent;
        assert_eq!(s.get_through("k"), None, "a local copy of unknown freshness is a miss");
        assert_eq!(rep.fetched_through.load(Ordering::SeqCst), 0, "no fetch-through while cold");

        *rep.answer.lock().unwrap() = ColdRead::Warm;
        assert_eq!(s.get_through("k").as_deref(), Some(b"local".as_slice()));
        assert_eq!(s.get_through("missing").as_deref(), Some(b"from-cluster".as_slice()));
        assert_eq!(rep.fetched_through.load(Ordering::SeqCst), 1, "a warm miss fetches through");
        assert_eq!(rep.asked.load(Ordering::SeqCst), 4, "every get_through asks once");
    }

    /// Cost of the `cold_read` consultation on the warm read path: a local
    /// hit through `get_through` with a replicator installed (one `RwLock`
    /// read + `Arc` clone + a virtual call answering `Warm`) against the same
    /// read with no replicator. Prints the per-call numbers; run with
    /// `cargo test -p ephpm-kv --release -- --ignored get_through_overhead --nocapture`.
    #[test]
    #[ignore = "timing-only; run by hand in release mode"]
    fn get_through_overhead_of_the_cold_read_check() {
        const N: u32 = 2_000_000;
        let s = test_store();
        s.set_local("k".into(), b"value".to_vec(), None);
        let time = |label: &str| {
            let started = std::time::Instant::now();
            let mut hits = 0u32;
            for _ in 0..N {
                hits += u32::from(s.get_through("k").is_some());
            }
            let per_call = started.elapsed() / N;
            eprintln!("{label}: {per_call:?} per get_through ({hits} hits)");
            per_call
        };
        let bare = time("no replicator      ");
        s.set_replicator(Some(ColdReplicator::answering(ColdRead::Warm) as Arc<dyn Replicator>));
        let warm = time("replicator, Warm   ");
        eprintln!("cold_read consultation adds {:?} per read", warm.saturating_sub(bare));
    }

    /// The default `cold_read` never intercepts, so a replicator that does not
    /// implement it (every one before the catch-up gate) is unaffected.
    #[test]
    fn default_cold_read_is_warm() {
        let s = test_store();
        s.set_local("k".into(), b"local".to_vec(), None);
        let rep = Arc::new(RecordingReplicator::default());
        assert_eq!(rep.cold_read("k"), ColdRead::Warm);
        s.set_replicator(Some(rep as Arc<dyn Replicator>));
        assert_eq!(s.get_through("k").as_deref(), Some(b"local".as_slice()));
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

    // ── Coarse-clock LRU touch ──────────────────────────────────────

    #[test]
    fn coarse_touch_skips_store_within_granularity() {
        // Touches within the granularity window must NOT advance the
        // timestamp — that's the whole point of the coarse-clock filter
        // (skip the cache-line RMW on hot GETs). Uses touch_precise to
        // plant an exact baseline, then verifies that a same-timestamp
        // touch is a no-op.
        use crate::store::entry::LRU_TOUCH_GRANULARITY_NANOS;
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        let entry = s.data.get("k").unwrap();
        // Plant a baseline nanos value.
        entry.touch_precise(1_000_000_000);
        // A touch with the same value must not change anything.
        entry.touch(1_000_000_000);
        assert_eq!(entry.last_accessed_nanos(), 1_000_000_000);
        // A touch just below the granularity window must also be a no-op.
        entry.touch(1_000_000_000 + LRU_TOUCH_GRANULARITY_NANOS - 1);
        assert_eq!(entry.last_accessed_nanos(), 1_000_000_000);
    }

    #[test]
    fn coarse_touch_advances_after_granularity() {
        use crate::store::entry::LRU_TOUCH_GRANULARITY_NANOS;
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        let entry = s.data.get("k").unwrap();
        entry.touch_precise(1_000_000_000);
        // A touch past the granularity window updates the stamp.
        entry.touch(1_000_000_000 + LRU_TOUCH_GRANULARITY_NANOS);
        assert_eq!(entry.last_accessed_nanos(), 1_000_000_000 + LRU_TOUCH_GRANULARITY_NANOS);
    }

    #[test]
    fn coarse_touch_preserves_eviction_ordering() {
        // The LRU eviction test at "allkeys_lru_evicts_oldest_accessed_key"
        // uses 10ms sleeps between operations which is below the 100ms
        // granularity — but with a get() following a sleep, the elapsed
        // now_nanos IS pushed past the granularity. Verify a similar
        // scenario at coarse-clock scale: touch a "cold" key past the
        // granularity and confirm it survives eviction.
        let s = Store::new(StoreConfig {
            memory_limit: 900,
            eviction_policy: EvictionPolicy::AllKeysLru,
            compression: CompressionConfig::default(),
        });
        s.set("oldest".into(), vec![1u8; 50], None);
        std::thread::sleep(Duration::from_millis(150));
        s.set("middle".into(), vec![2u8; 50], None);
        std::thread::sleep(Duration::from_millis(150));
        s.set("newest".into(), vec![3u8; 50], None);
        std::thread::sleep(Duration::from_millis(150));

        // Touch oldest — past coarse granularity, so the store lands.
        let _ = s.get("oldest");
        std::thread::sleep(Duration::from_millis(150));

        assert!(s.set("trigger".into(), vec![4u8; 200], None));
        assert!(s.get("middle").is_none(), "middle should be evicted (untouched, oldest access)");
        assert!(s.get("oldest").is_some(), "oldest should survive (was touched past granularity)");
    }

    // ── TTL side index and active expiry ──────────────────────────────

    #[test]
    fn ttl_keys_populated_on_set_with_ttl() {
        let s = test_store();
        s.set("volatile".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        assert!(s.ttl_keys.contains_key("volatile"), "TTL'd key must appear in the side index");
        s.set("permanent".into(), b"v".to_vec(), None);
        assert!(
            !s.ttl_keys.contains_key("permanent"),
            "persistent key must NOT appear in the side index"
        );
    }

    #[test]
    fn ttl_keys_removed_on_persist() {
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        assert!(s.ttl_keys.contains_key("k"));
        assert!(s.persist("k"), "persist must succeed for a TTL'd key");
        assert!(!s.ttl_keys.contains_key("k"), "persist must drop the key from the TTL index");
    }

    #[test]
    fn ttl_keys_added_on_expire() {
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        assert!(!s.ttl_keys.contains_key("k"));
        assert!(s.expire("k", Duration::from_secs(60)));
        assert!(s.ttl_keys.contains_key("k"), "expire must add the key to the TTL index");
    }

    #[test]
    fn ttl_keys_removed_on_remove() {
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        assert!(s.ttl_keys.contains_key("k"));
        s.remove("k");
        assert!(!s.ttl_keys.contains_key("k"), "remove must drop the TTL index entry");
    }

    #[test]
    fn ttl_keys_flushed_on_flush() {
        let s = test_store();
        s.set("a".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        s.set("b".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        assert_eq!(s.ttl_keys.len(), 2);
        s.flush();
        assert_eq!(s.ttl_keys.len(), 0, "flush must clear the TTL index");
    }

    #[test]
    fn active_expire_prunes_stale_ttl_hints() {
        // A hint can outlive its entry (a remove racing a re-write) or the
        // entry's TTL (persist). Once the hint's deadline has passed, a sample
        // drops it without touching any live entry.
        let s = test_store();
        let past = Instant::now() - Duration::from_secs(60);
        // Case 1: hint points at a key that no longer exists.
        s.ttl_keys.insert("ghost".into(), past);
        // Case 2: hint points at a key whose TTL was cleared.
        s.set("persistent".into(), b"v".to_vec(), None);
        s.ttl_keys.insert("persistent".into(), past); // stale hint
        assert_eq!(s.ttl_keys.len(), 2);

        s.active_expire_cycle(Duration::from_secs(1));
        assert_eq!(s.ttl_keys.len(), 0, "active expiry must self-heal stale hints");
        // The persistent key must NOT be removed.
        assert!(s.exists("persistent"));
    }

    #[test]
    fn active_expire_cycle_respects_its_time_budget() {
        // An enormous backlog must not turn one cycle into an unbounded stall:
        // the cycle stops at its budget and the rest waits for later cycles.
        let s = test_store();
        for i in 0..200_000 {
            plant_expired(&s, &format!("t{i}"), b"v");
        }
        let start = Instant::now();
        let reaped = s.active_expire_cycle(Duration::from_millis(1));
        let elapsed = start.elapsed();
        assert!(reaped < 200_000, "a 1 ms cycle drained a 200k backlog ({elapsed:?})");
        // Loose on purpose: only "bounded", not "fast" — a loaded CI machine
        // can stretch a 1 ms budget, never into a full drain.
        assert!(elapsed < Duration::from_secs(10), "a 1 ms cycle took {elapsed:?}");
    }

    #[test]
    fn active_expire_only_reaps_ttl_keys() {
        // Persistent keys are not in the TTL index, so active expiry cannot
        // touch them however many there are.
        let s = test_store();
        for i in 0..100 {
            s.set(format!("perm{i}"), b"v".to_vec(), None);
        }
        plant_expired(&s, "expired", b"v");

        let reaped = s.active_expire_cycle(Duration::from_secs(1));
        assert_eq!(reaped, 1);
        assert!(!s.data.contains_key("expired"));
        for i in 0..100 {
            assert!(s.exists(&format!("perm{i}")), "perm{i} must survive active expiry");
        }
    }

    #[test]
    fn active_expiry_gives_empty_table_memory_back() {
        // F-12: tables never shrank, so a store whose keys had all expired
        // still held every slot of its peak size.
        let s = test_store();
        for i in 0..100_000 {
            plant_expired(&s, &format!("t{i}"), b"v");
        }
        for i in 0..1_000 {
            assert!(s.set(format!("live{i}"), b"v".to_vec(), Some(Duration::from_secs(600))));
        }
        let peak = s.data.capacity() + s.ttl_keys.capacity();
        // An effectively unlimited budget: the result must not depend on how
        // fast the machine is, only on the cursor sweep.
        for _ in 0..200 {
            s.active_expire_cycle(Duration::from_secs(3_600));
        }
        let after = s.data.capacity() + s.ttl_keys.capacity();
        assert!(after * 10 < peak, "tables not shrunk: {peak} slots -> {after}");
        // Every surviving key is still reachable after the rehash.
        for i in 0..1_000 {
            assert!(s.exists(&format!("live{i}")), "live{i} lost by the shrink");
            assert!(s.ttl_keys.contains_key(&format!("live{i}")));
        }
        assert_eq!(s.len(), 1_000);
    }

    #[test]
    fn shrinking_leaves_dense_tables_alone() {
        let s = test_store();
        for i in 0..50_000 {
            s.set(format!("k{i}"), b"v".to_vec(), None);
        }
        assert_eq!(s.shrink_sparse_tables(), 0);
        for i in 0..50_000 {
            assert!(s.exists(&format!("k{i}")));
        }
    }

    #[test]
    fn a_reap_keeps_the_hint_of_a_newer_write() {
        // The reap of an old entry must not drop the TTL hint a re-write has
        // just recorded for the same key, or the new value would never be
        // actively expired.
        let s = test_store();
        let past = Instant::now() - Duration::from_secs(60);
        assert!(s.set("k".into(), b"new".to_vec(), Some(Duration::from_secs(60))));
        // A sample taken before the re-write still holds the old deadline.
        assert!(!s.reap_if_expired("k", past));
        assert!(s.ttl_keys.contains_key("k"), "the new write's hint must survive");
        assert_eq!(s.get("k").as_deref(), Some(&b"new"[..]));
    }

    #[test]
    fn ttl_keys_stays_consistent_across_overwrite_state_changes() {
        // A key initially TTL'd, then overwritten without TTL, must be
        // dropped from the index. And back the other way.
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        assert!(s.ttl_keys.contains_key("k"));

        // Overwrite without TTL — must clear the hint.
        s.set("k".into(), b"v2".to_vec(), None);
        assert!(!s.ttl_keys.contains_key("k"), "overwrite w/o TTL must clear the hint");

        // Overwrite with TTL — must re-add.
        s.set("k".into(), b"v3".to_vec(), Some(Duration::from_secs(30)));
        assert!(s.ttl_keys.contains_key("k"), "overwrite w/ TTL must re-add the hint");
    }

    #[test]
    fn set_nx_maintains_ttl_index() {
        let s = test_store();
        assert!(s.set_nx("k1".into(), b"v".to_vec(), Some(Duration::from_secs(60))));
        assert!(s.ttl_keys.contains_key("k1"), "set_nx with TTL must add the hint");

        assert!(s.set_nx("k2".into(), b"v".to_vec(), None));
        assert!(!s.ttl_keys.contains_key("k2"), "set_nx without TTL must not add the hint");
    }

    /// **Divergence regression.** Every op that mutates a string key must
    /// reach the replicator. `incr`/`set_nx`/`append`/`persist` are
    /// read-modify-write: they used to apply locally and publish *nothing*, so
    /// on a cluster the writing node advanced and every peer stayed frozen —
    /// silently, forever. That is what pinned `switchboard:gen` at 0 on two of
    /// three nodes.
    ///
    /// These ops necessarily replicate as a publish of the *resulting* value
    /// (see `replicate_published_value`), so the assertion is "the new value was
    /// published", not "the operation was forwarded".
    #[test]
    fn read_modify_write_ops_publish_their_result() {
        for (name, apply, expected) in [
            (
                "incr_by",
                Box::new(|s: &Store| {
                    let _ = s.incr_by("k", 1);
                }) as Box<dyn Fn(&Store)>,
                b"1".to_vec(),
            ),
            (
                "set_nx",
                Box::new(|s: &Store| {
                    s.set_nx("k".into(), b"v".to_vec(), None);
                }),
                b"v".to_vec(),
            ),
            (
                "append",
                Box::new(|s: &Store| {
                    s.set_local("k".into(), b"ab".to_vec(), None);
                    s.append("k", b"cd");
                }),
                b"abcd".to_vec(),
            ),
        ] {
            let s = test_store();
            let rep = Arc::new(RecordingReplicator::default());
            s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

            apply(&s);

            let recorded = rep.publishes.lock().unwrap();
            let entry = recorded
                .iter()
                .find(|(key, _, _)| key == "k")
                .unwrap_or_else(|| panic!("{name} must publish its result to the replicator"));
            assert_eq!(entry.1, expected, "{name} published the wrong value");
            assert!(
                rep.sets.lock().unwrap().is_empty(),
                "{name} must use the publish-only path, not replicate_set (which writes back \
                 locally and would lose the update)"
            );
        }
    }

    /// A counter's replicated value tracks successive increments, and a
    /// created-with-TTL counter publishes its remaining expiry rather than
    /// resurrecting the key as immortal on peers.
    #[test]
    fn incr_publishes_running_total_and_preserves_ttl() {
        let s = test_store();
        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        // Local map is authoritative for the counter (the hook does not write),
        // so seed through set_local and increment.
        assert_eq!(s.incr_by_with_ttl("g", 1, Some(Duration::from_secs(60))).unwrap(), 1);
        assert_eq!(s.incr_by("g", 1).unwrap(), 2);

        let publishes = rep.publishes.lock().unwrap();
        let values: Vec<Vec<u8>> =
            publishes.iter().filter(|(k, _, _)| k == "g").map(|(_, v, _)| v.clone()).collect();
        assert_eq!(values, vec![b"1".to_vec(), b"2".to_vec()], "each INCR publishes the new total");
        // The first publish carried the creation TTL; so does the second, which
        // must not resurrect a windowed counter as immortal.
        let ttls: Vec<Option<Duration>> =
            publishes.iter().filter(|(k, _, _)| k == "g").map(|(_, _, t)| *t).collect();
        assert!(
            ttls.iter().all(Option::is_some),
            "a TTL'd counter must replicate with its expiry on every increment: {ttls:?}"
        );
    }

    /// `persist` removes an expiry locally; peers must be told, or they expire
    /// a key this node made permanent.
    #[test]
    fn persist_republishes_without_a_ttl() {
        let s = test_store();
        s.set_local("k".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        assert!(s.persist("k"));
        let publishes = rep.publishes.lock().unwrap();
        let (_, value, ttl) =
            publishes.iter().find(|(k, _, _)| k == "k").expect("persist must republish the key");
        assert_eq!(value, b"v");
        assert!(ttl.is_none(), "persist must publish the key with no expiry");
    }

    /// **Lost-update regression (PR #416 blocker 4).** Each `incr` must publish
    /// the total *it* produced, captured under the key's shard lock.
    ///
    /// The first fix for the non-replicating `INCR` re-read the key after the
    /// lock had been released, so two concurrent increments could publish the
    /// same total (or each other's) — and the clustered replicator then wrote
    /// that snapshot back into the local store, undoing a newer increment on
    /// the origin node. With the lock-scoped capture the publishes are exactly
    /// the sequence `1..=N`, in some order, with no duplicates and no gaps: a
    /// duplicate here is precisely a lost update on any node that applies them.
    #[test]
    fn concurrent_increments_each_publish_their_own_total() {
        const THREADS: i64 = 8;
        const PER_THREAD: i64 = 25;
        let total = THREADS * PER_THREAD;

        let s = Arc::new(test_store());
        let rep = Arc::new(RecordingReplicator::default());
        s.set_replicator(Some(Arc::clone(&rep) as Arc<dyn Replicator>));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let s = Arc::clone(&s);
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        s.incr_by("c", 1).expect("incr must succeed");
                    }
                });
            }
        });

        assert_eq!(s.get("c").as_deref(), Some(total.to_string().as_bytes()));

        let mut published: Vec<i64> = rep
            .publishes
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _, _)| k == "c")
            .map(|(_, v, _)| String::from_utf8(v.clone()).unwrap().parse::<i64>().unwrap())
            .collect();
        published.sort_unstable();
        assert_eq!(
            published,
            (1..=total).collect::<Vec<_>>(),
            "every increment must publish its own distinct total — a duplicate means one \
             increment published another's value and would overwrite it on apply"
        );
    }

    /// Nothing is published when no replicator is installed — the single-node
    /// path stays a pure local write.
    #[test]
    fn read_modify_write_ops_are_local_without_a_replicator() {
        let s = test_store();
        assert_eq!(s.incr_by("k", 5).unwrap(), 5);
        assert!(s.set_nx("nx".into(), b"v".to_vec(), None));
        assert_eq!(s.get("k").as_deref(), Some(&b"5"[..]));
        assert_eq!(s.get("nx").as_deref(), Some(&b"v"[..]));
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

    // ── Key watches (wait_for_change) ───────────────────────────────

    #[test]
    fn writes_never_create_watch_slots() {
        // The zero-cost invariant: a store that is never waited on must
        // not grow any watch state, no matter how much it is written.
        let s = test_store();
        for i in 0..100 {
            s.set(format!("k{i}"), b"v".to_vec(), None);
            let _ = s.incr_by("counter", 1);
            s.remove(&format!("k{i}"));
        }
        assert_eq!(s.watch_slot_count(), 0, "writes must not allocate watch slots");
        assert!(s.watchers.is_empty());
    }

    #[test]
    fn wait_zero_returns_snapshot_immediately() {
        let s = test_store();
        s.set("k".into(), b"initial".to_vec(), None);
        let start = Instant::now();
        // last_version = 0 → fresh slot at version 1 already exceeds it:
        // register + snapshot without blocking.
        let (ver, val) = s.wait_for_change("k", 0, Duration::from_secs(30)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1), "must not block");
        assert_eq!(ver, 1);
        assert_eq!(val.as_deref(), Some(&b"initial"[..]));
        assert_eq!(s.watch_slot_count(), 1);
    }

    #[test]
    fn wait_snapshot_on_missing_key_has_no_value() {
        let s = test_store();
        let (ver, val) = s.wait_for_change("ghost", 0, Duration::from_secs(1)).unwrap();
        assert_eq!(ver, 1);
        assert_eq!(val, None);
    }

    #[test]
    fn wait_times_out_without_writes() {
        let s = test_store();
        let (ver, _) = s.wait_for_change("k", 0, Duration::from_secs(1)).unwrap();
        let start = Instant::now();
        assert!(s.wait_for_change("k", ver, Duration::from_millis(60)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(60));
    }

    #[test]
    fn set_wakes_waiter_with_new_value() {
        let s = test_store();
        s.set("k".into(), b"old".to_vec(), None);
        let (ver, _) = s.wait_for_change("k", 0, Duration::from_secs(1)).unwrap();

        let waiter = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || s.wait_for_change("k", ver, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(50));
        s.set("k".into(), b"new".to_vec(), None);

        let (new_ver, val) = waiter.join().unwrap().expect("waiter must wake on set");
        assert!(new_ver > ver);
        assert_eq!(val.as_deref(), Some(&b"new"[..]));
    }

    #[test]
    fn del_wakes_waiter_with_absent_value() {
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        let (ver, _) = s.wait_for_change("k", 0, Duration::from_secs(1)).unwrap();

        let waiter = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || s.wait_for_change("k", ver, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(s.remove("k"));

        let (new_ver, val) = waiter.join().unwrap().expect("waiter must wake on del");
        assert!(new_ver > ver);
        assert_eq!(val, None, "deleted key must surface as absent");
    }

    #[test]
    fn incr_wakes_waiter() {
        let s = test_store();
        let (ver, _) = s.wait_for_change("counter", 0, Duration::from_secs(1)).unwrap();

        let waiter = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || s.wait_for_change("counter", ver, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(s.incr_by("counter", 1), Ok(1));

        let (_, val) = waiter.join().unwrap().expect("waiter must wake on incr");
        assert_eq!(val.as_deref(), Some(&b"1"[..]));
    }

    #[test]
    fn expire_does_not_bump_version() {
        // TTL-only changes are not value changes; the eventual expiry
        // reap (a remove_local) is what wakes waiters.
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        let (ver, _) = s.wait_for_change("k", 0, Duration::from_secs(1)).unwrap();
        assert!(s.expire("k", Duration::from_secs(3600)));
        assert!(
            s.wait_for_change("k", ver, Duration::from_millis(60)).is_none(),
            "expire must not bump the watch version"
        );
    }

    #[test]
    fn writes_to_other_keys_do_not_wake_waiter() {
        let s = test_store();
        let (ver, _) = s.wait_for_change("watched", 0, Duration::from_secs(1)).unwrap();
        s.set("unrelated".into(), b"v".to_vec(), None);
        assert!(
            s.wait_for_change("watched", ver, Duration::from_millis(60)).is_none(),
            "unrelated writes must not bump the watched key's version"
        );
    }

    #[test]
    fn flush_wakes_waiters_and_keeps_slots() {
        let s = test_store();
        s.set("k".into(), b"v".to_vec(), None);
        let (ver, _) = s.wait_for_change("k", 0, Duration::from_secs(1)).unwrap();

        let waiter = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || s.wait_for_change("k", ver, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(50));
        s.flush();

        let (new_ver, val) = waiter.join().unwrap().expect("waiter must wake on flush");
        assert!(new_ver > ver);
        assert_eq!(val, None, "flushed key must surface as absent");
        // Slots survive the flush — versions must stay monotonic.
        assert_eq!(s.watch_slot_count(), 1);
    }

    #[test]
    fn versions_are_monotonic_across_writes() {
        let s = test_store();
        let (mut ver, _) = s.wait_for_change("k", 0, Duration::from_secs(1)).unwrap();
        for i in 0..5 {
            s.set("k".into(), format!("v{i}").into_bytes(), None);
            let (new_ver, val) =
                s.wait_for_change("k", ver, Duration::from_secs(1)).expect("version advanced");
            assert!(new_ver > ver, "versions must strictly increase");
            assert_eq!(val.as_deref(), Some(format!("v{i}").as_bytes()));
            ver = new_ver;
        }
    }

    #[test]
    fn multiple_waiters_all_wake_on_one_write() {
        let s = test_store();
        let (ver, _) = s.wait_for_change("k", 0, Duration::from_secs(1)).unwrap();

        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || s.wait_for_change("k", ver, Duration::from_secs(10)))
            })
            .collect();
        std::thread::sleep(Duration::from_millis(50));
        s.set("k".into(), b"fanout".to_vec(), None);

        for w in waiters {
            let (_, val) = w.join().unwrap().expect("every waiter must wake");
            assert_eq!(val.as_deref(), Some(&b"fanout"[..]));
        }
    }
}

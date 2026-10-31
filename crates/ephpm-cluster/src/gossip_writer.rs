//! Bounded writer and garbage collector for this node's gossip KV state.
//!
//! Stress finding F-6: the gossip tier collapsed as the number of distinct
//! keys grew — 58 MB to ~30 GB per node in under three minutes with ~30 MB of
//! real data, then a core pegged forever and replication stopped. Three
//! causes, all fixed here:
//!
//! 1. **Nothing was ever deleted from chitchat state.** Expired values,
//!    `TS:` tombstone markers and copies superseded by another node's newer
//!    write all stayed in every node's state for the life of the process,
//!    and chitchat's delta computation walks every entry of a state that is
//!    newer than the peer's digest, on every exchange. The [`GossipWriter`]
//!    sweep chitchat-`delete()`s expired values, aged tombstone markers and
//!    **retired** entries (ours, once a peer's newer write for the same key
//!    has been applied). A chitchat delete is a chitchat tombstone for the
//!    configured grace period (60 s), after which every node GCs it — so
//!    steady-state gossip memory is the live small keys plus a minute or
//!    two of churn, not every key ever written.
//! 2. **Every write spawned a task that queued on the one chitchat mutex,
//!    each holding a copy of the value.** Writes now go into a bounded,
//!    coalescing pending map and one drain task applies them in batches
//!    under a single lock acquisition. Over [`PENDING_CAP`] distinct keys
//!    the newest writes are shed (counted, logged at a rate-limited WARN):
//!    the local write already succeeded, that key just does not replicate
//!    until it is written again. That is the deliberate trade over blocking
//!    a PHP thread or RESP task on the chitchat mutex.
//! 3. The per-key applied map grew without bound; that is bounded in
//!    `clustered_store` (horizon pruning plus verification on a miss).
//!
//! Reads (`gossip_get` and friends) consult the pending map too, so a write
//! is read-your-write on its origin before the drain has run.
//!
//! ## Why a key is retired, and why that makes tombstones collectable
//!
//! Chitchat state is per origin: a key written by two nodes exists in both
//! their states, and a reader picks the winner by stamp. A `TS:` marker can
//! therefore only be dropped once no state still holds a *live* copy that a
//! later joiner could resurrect. Retiring — a node deleting its own copy of
//! a key once it has applied a peer's strictly newer write or tombstone for
//! it — keeps exactly one state holding each key in steady state, which is
//! what lets a marker go after [`TOMBSTONE_LIFETIME`]. The remaining window
//! is a node partitioned for longer than that while still holding a stale
//! live copy: on heal it can resurrect the deleted key. That is the same
//! trade every tombstone-collecting store makes (Cassandra's
//! `gc_grace_seconds`), and it is documented.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64ct::Encoding;
use chitchat::Chitchat;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::gossip_kv::{
    KV_PREFIX, Stamp, decode_tombstone, encode_tombstone, now_epoch_ms, set_if_newer,
    split_encoded, stamp_of,
};
use crate::node::KV_MARK_KEY;

/// One drained batch: queued ops and queued retirements.
type Batch = (Vec<(String, Pending)>, Vec<(String, Stamp)>);

/// Distinct keys the pending map holds before new keys are shed.
///
/// Sized so the queue itself is bounded to tens of megabytes of base64
/// payload at the gossip tier's 512-byte default threshold, while a burst
/// of ordinary write traffic never touches the cap: the drain empties it
/// every [`DRAIN_IDLE`] or sooner.
pub const PENDING_CAP: usize = 65_536;

/// Distinct keys the retire map holds before new retirements are dropped
/// (a dropped retirement only delays the sweep of that copy).
pub const RETIRE_CAP: usize = 65_536;

/// Pending ops applied per chitchat lock acquisition.
const DRAIN_BATCH: usize = 4_096;

/// Longest the drain task sleeps before looking for pending work when
/// nothing wakes it.
const DRAIN_IDLE: Duration = Duration::from_millis(50);

/// How often this node's own gossip state is swept for garbage, and its
/// catch-up mark refreshed ([`crate::node::KV_MARK_INTERVAL`] is this).
pub const SWEEP_INTERVAL: Duration = crate::node::KV_MARK_INTERVAL;

/// How long a `TS:` tombstone marker stays visible in gossip after the
/// delete it carries, before it is chitchat-deleted (and GC'd 60 s later).
/// A node that misses the marker for longer than this — partitioned, or
/// down and then back with the same generation — can resurrect the key.
pub const TOMBSTONE_LIFETIME: Duration = Duration::from_secs(120);

/// An expired value is deleted from gossip state this long after its
/// expiry, so every peer's own lazy expiry has had a chance to run first.
const EXPIRED_SLACK_MS: u64 = 1_000;

/// Minimum interval between `warn!`-level logs about shed writes.
const SHED_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// One queued mutation of this node's gossip state.
pub(crate) enum PendingOp {
    /// Publish a value (already wire-encoded) with its absolute expiry.
    Set { encoded: String, expiry_ms: u64 },
    /// Publish a tombstone marker.
    Tombstone,
}

/// A queued op with the stamp that orders it against later ops on the key.
pub(crate) struct Pending {
    pub(crate) stamp: Stamp,
    pub(crate) op: PendingOp,
}

/// What a pending op would read as, for the read paths that consult the
/// queue: the stamp and, for a live value, its bytes and expiry.
pub(crate) struct PendingView {
    pub(crate) stamp: Stamp,
    /// `None` for a tombstone.
    pub(crate) value: Option<(Vec<u8>, u64)>,
}

/// Counters a node exposes about its gossip writer, for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GossipWriterStats {
    /// Keys currently waiting for the drain.
    pub pending: usize,
    /// Writes shed because the pending map was at [`PENDING_CAP`].
    pub shed: u64,
    /// Entries removed from this node's gossip state by the sweep: expired
    /// values, aged tombstone markers and retired copies.
    pub swept: u64,
}

/// The bounded, coalescing write queue and sweeper for one node's gossip
/// KV state. See the module docs.
pub struct GossipWriter {
    /// chitchat key → the latest op queued for it.
    pending: DashMap<String, Pending>,
    /// chitchat key → the peer stamp that superseded this node's own copy.
    retire: DashMap<String, Stamp>,
    notify: Notify,
    shed: AtomicU64,
    swept: AtomicU64,
    last_shed_warn: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for GossipWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GossipWriter").field("stats", &self.stats()).finish_non_exhaustive()
    }
}

impl Default for GossipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl GossipWriter {
    /// An empty writer; [`run`](Self::run) must be spawned for it to drain.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
            retire: DashMap::new(),
            notify: Notify::new(),
            shed: AtomicU64::new(0),
            swept: AtomicU64::new(0),
            last_shed_warn: Mutex::new(None),
        }
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> GossipWriterStats {
        GossipWriterStats {
            pending: self.pending.len(),
            shed: self.shed.load(Ordering::Relaxed),
            swept: self.swept.load(Ordering::Relaxed),
        }
    }

    /// Queue a value publish. Returns `false` when it was shed.
    pub(crate) fn enqueue_set(
        &self,
        chitchat_key: String,
        encoded: String,
        expiry_ms: u64,
        stamp: Stamp,
    ) -> bool {
        self.enqueue(chitchat_key, Pending { stamp, op: PendingOp::Set { encoded, expiry_ms } })
    }

    /// Queue a tombstone publish. Returns `false` when it was shed.
    pub(crate) fn enqueue_tombstone(&self, chitchat_key: String, stamp: Stamp) -> bool {
        self.enqueue(chitchat_key, Pending { stamp, op: PendingOp::Tombstone })
    }

    /// Coalescing insert: a newer op for a key replaces the queued one, an
    /// older one is dropped; a new key past the cap is shed.
    fn enqueue(&self, chitchat_key: String, pending: Pending) -> bool {
        // The cap is checked before taking the entry: `DashMap::len` read-locks
        // every shard, and the entry guard holds one of them for writing.
        // Racing inserts can overshoot the cap by a handful of keys; the
        // bound is a backstop, not an exact budget.
        if !self.pending.contains_key(&chitchat_key) && self.pending.len() >= PENDING_CAP {
            self.shed.fetch_add(1, Ordering::Relaxed);
            self.warn_shed(&chitchat_key);
            return false;
        }
        match self.pending.entry(chitchat_key) {
            dashmap::Entry::Occupied(mut slot) => {
                if pending.stamp > slot.get().stamp {
                    slot.insert(pending);
                }
            }
            dashmap::Entry::Vacant(slot) => {
                slot.insert(pending);
            }
        }
        self.notify.notify_one();
        true
    }

    /// Ask the drain to delete this node's own copy of `chitchat_key` if it
    /// is older than `superseded_by` (a peer's write that has been applied).
    pub(crate) fn enqueue_retire(&self, chitchat_key: &str, superseded_by: Stamp) {
        // Same lock-order rule as `enqueue`: never call `len` under an entry.
        if !self.retire.contains_key(chitchat_key) && self.retire.len() >= RETIRE_CAP {
            return;
        }
        match self.retire.entry(chitchat_key.to_string()) {
            dashmap::Entry::Occupied(mut slot) => {
                if superseded_by > *slot.get() {
                    slot.insert(superseded_by);
                }
            }
            dashmap::Entry::Vacant(slot) => {
                slot.insert(superseded_by);
            }
        }
        self.notify.notify_one();
    }

    /// The queued op for `chitchat_key`, as a read would see it. An op
    /// whose expiry has passed reads as a tombstone, matching the state.
    pub(crate) fn pending_view(&self, chitchat_key: &str) -> Option<PendingView> {
        let slot = self.pending.get(chitchat_key)?;
        let value = match &slot.op {
            PendingOp::Tombstone => None,
            PendingOp::Set { encoded, expiry_ms } => {
                if *expiry_ms > 0 && now_epoch_ms() >= *expiry_ms {
                    None
                } else {
                    let (_, _, b64) = split_encoded(encoded)?;
                    let bytes = base64ct::Base64::decode_vec(b64).ok()?;
                    Some((bytes, *expiry_ms))
                }
            }
        };
        Some(PendingView { stamp: slot.stamp, value })
    }

    /// Visit every queued key with whether it currently reads as a live value.
    pub(crate) fn for_each_pending(&self, mut visit: impl FnMut(&str, bool)) {
        for entry in &self.pending {
            let live = match &entry.op {
                PendingOp::Tombstone => false,
                PendingOp::Set { expiry_ms, .. } => *expiry_ms == 0 || now_epoch_ms() < *expiry_ms,
            };
            visit(entry.key(), live);
        }
    }

    fn warn_shed(&self, key: &str) {
        let mut last = self.last_shed_warn.lock();
        let now = Instant::now();
        if last.is_none_or(|t| now.duration_since(t) >= SHED_WARN_INTERVAL) {
            *last = Some(now);
            tracing::warn!(
                key,
                pending = self.pending.len(),
                shed_total = self.shed.load(Ordering::Relaxed),
                "gossip KV write shed: the replication queue is full (the local write \
                 succeeded; this key will not reach peers until it is written again). The \
                 gossip tier moves a few hundred entries per second per peer — this node is \
                 writing distinct small keys faster than that"
            );
        }
    }

    /// The drain and sweep loop. Runs until the chitchat mutex is dropped by
    /// everyone else (the handle shut down) or the task is aborted.
    pub(crate) async fn run(
        self: Arc<Self>,
        chitchat: Arc<tokio::sync::Mutex<Chitchat>>,
        self_id: String,
    ) {
        let mut next_sweep = Instant::now() + SWEEP_INTERVAL;
        loop {
            tokio::select! {
                () = self.notify.notified() => {}
                () = tokio::time::sleep(DRAIN_IDLE) => {}
            }
            // Keep draining while a full batch came out — a burst is
            // applied in a few lock acquisitions, not one per op.
            loop {
                let (ops, retires) = self.take_batch();
                if ops.is_empty() && retires.is_empty() {
                    break;
                }
                let full = ops.len() >= DRAIN_BATCH;
                {
                    let mut guard = chitchat.lock().await;
                    self.apply_batch(&mut guard, &self_id, ops, retires);
                }
                if !full {
                    break;
                }
                tokio::task::yield_now().await;
            }
            if Instant::now() >= next_sweep {
                next_sweep = Instant::now() + SWEEP_INTERVAL;
                let mut guard = chitchat.lock().await;
                let swept = sweep(&mut guard);
                if swept > 0 {
                    self.swept.fetch_add(swept, Ordering::Relaxed);
                    tracing::debug!(swept, "gossip KV state swept");
                }
                // Refresh this node's catch-up mark on the same tick: a
                // joining peer proves it has streamed this node's whole state
                // by receiving a mark set after it joined (see
                // `crate::node::KV_MARK_KEY`).
                guard.self_node_state().set(KV_MARK_KEY, now_epoch_ms().to_string());
            }
        }
    }

    /// Drain **every** queued op and retirement into `chitchat` now, in one
    /// call, under the chitchat lock (reacquired per batch so a huge queue does
    /// not hold the lock in one stretch). Used at graceful drain — in **all**
    /// clustered modes — via [`crate::ClusterHandle::flush_gossip_writer`]
    /// (called by [`crate::ClusterHandle::mark_draining`] on the per-site
    /// clustered leave, directly by the server on a whole-database or KV-only
    /// clustered drain, and by [`crate::ClusterHandle::shutdown`] before the
    /// drain task is aborted): without it the last up to [`DRAIN_IDLE`] of
    /// queued small-tier KV writes never reach gossip state and are lost on
    /// restart (#656). A concurrent `enqueue` after this starts is not waited
    /// for — the caller is shutting down, so no new local writes are expected;
    /// this flushes what is already queued, which is why calling it more than
    /// once over one shutdown is a harmless no-op the second time.
    pub(crate) async fn flush(&self, chitchat: &Arc<tokio::sync::Mutex<Chitchat>>, self_id: &str) {
        loop {
            let (ops, retires) = self.take_batch();
            if ops.is_empty() && retires.is_empty() {
                break;
            }
            let mut guard = chitchat.lock().await;
            self.apply_batch(&mut guard, self_id, ops, retires);
        }
    }

    /// Pull up to one batch of ops, and every queued retirement, out of the
    /// maps. A key re-queued after it was listed but before it was removed
    /// simply stays for the next batch.
    fn take_batch(&self) -> Batch {
        let keys: Vec<String> =
            self.pending.iter().take(DRAIN_BATCH).map(|e| e.key().clone()).collect();
        let ops = keys.into_iter().filter_map(|k| self.pending.remove(&k)).collect();
        let retire_keys: Vec<String> = self.retire.iter().map(|e| e.key().clone()).collect();
        let retires = retire_keys.into_iter().filter_map(|k| self.retire.remove(&k)).collect();
        (ops, retires)
    }

    /// Apply one batch under the lock the caller holds.
    fn apply_batch(
        &self,
        guard: &mut Chitchat,
        self_id: &str,
        ops: Vec<(String, Pending)>,
        retires: Vec<(String, Stamp)>,
    ) {
        let now = now_epoch_ms();
        let state = guard.self_node_state();
        for (key, pending) in ops {
            // `set_if_newer`: an op enqueued after a newer one for the same
            // key was already drained must not regress the state.
            match pending.op {
                PendingOp::Set { encoded, expiry_ms } => {
                    // A value that expired while queued never enters the
                    // state; peers' copies (if any) expire on their own.
                    if expiry_ms > 0 && now >= expiry_ms {
                        continue;
                    }
                    set_if_newer(state, self_id, key, encoded, pending.stamp);
                }
                PendingOp::Tombstone => {
                    let encoded = encode_tombstone(pending.stamp.write);
                    set_if_newer(state, self_id, key, encoded, pending.stamp);
                }
            }
        }
        let mut deleted = 0u64;
        for (key, superseded_by) in retires {
            let Some(encoded) = state.get(&key) else { continue };
            let Some(own) = stamp_of(encoded, self_id) else { continue };
            if own < superseded_by {
                state.delete(&key);
                deleted += 1;
            }
        }
        if deleted > 0 {
            self.swept.fetch_add(deleted, Ordering::Relaxed);
        }
    }
}

/// Delete expired values and aged tombstone markers from this node's own
/// gossip state. Returns how many entries were deleted.
fn sweep(guard: &mut Chitchat) -> u64 {
    let now = now_epoch_ms();
    let marker_cutoff = now
        .saturating_sub(u64::try_from(TOMBSTONE_LIFETIME.as_millis()).expect("lifetime fits u64"));
    let state = guard.self_node_state();
    let garbage: Vec<String> = state
        .iter_prefix(KV_PREFIX)
        .filter(|(_, versioned)| is_garbage(&versioned.value, now, marker_cutoff))
        .map(|(key, _)| key.to_string())
        .collect();
    for key in &garbage {
        state.delete(key);
    }
    garbage.len() as u64
}

/// Whether an encoded entry is expired (plus slack) or an aged marker.
fn is_garbage(encoded: &str, now_ms: u64, marker_cutoff_ms: u64) -> bool {
    if let Some(write) = decode_tombstone(encoded) {
        return Stamp { write, node_tag: 0 }.epoch_ms() < marker_cutoff_ms;
    }
    match split_encoded(encoded) {
        Some((expiry_ms, _, _)) => expiry_ms > 0 && now_ms >= expiry_ms + EXPIRED_SLACK_MS,
        // Undecodable: nothing a reader can use; let it go.
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::net::SocketAddr;

    use chitchat::{ChitchatConfig, ChitchatId, FailureDetectorConfig};
    use tokio::sync::watch;

    use super::*;
    use crate::gossip_kv::{SEQ_BITS, StampClock, encode_value};

    /// Encode a value as the hooks do before queueing it.
    fn encode_for_queue(value: &[u8], expiry_ms: u64, stamp: Stamp) -> String {
        encode_value(value, expiry_ms, stamp.write)
    }

    /// A chitchat instance with no network, for exercising state mutation
    /// and GC exactly as the drain task does.
    fn chitchat(node_id: &str, grace: Duration) -> Chitchat {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let config = ChitchatConfig {
            chitchat_id: ChitchatId::new(node_id.to_string(), 0, addr),
            cluster_id: "gc-test".to_string(),
            gossip_interval: Duration::from_secs(1),
            listen_addr: addr,
            seed_nodes: vec![],
            failure_detector_config: FailureDetectorConfig::default(),
            marked_for_deletion_grace_period: grace,
            catchup_callback: None,
            extra_liveness_predicate: None,
        };
        let (_tx, rx) = watch::channel(HashSet::new());
        Chitchat::with_chitchat_id_and_seeds(config, rx, vec![])
    }

    fn live_kv_keys(c: &mut Chitchat) -> Vec<String> {
        c.self_node_state().iter_prefix(KV_PREFIX).map(|(k, _)| k.to_string()).collect()
    }

    #[test]
    fn enqueue_coalesces_to_the_newest_op_per_key() {
        let w = GossipWriter::new();
        let clock = StampClock::new("n");
        let s1 = clock.next();
        let s2 = clock.next();
        assert!(w.enqueue_set("kv:k".into(), encode_for_queue(b"v1", 0, s1), 0, s1));
        assert!(w.enqueue_set("kv:k".into(), encode_for_queue(b"v2", 0, s2), 0, s2));
        // An older op arriving late never regresses the queued one.
        assert!(w.enqueue_set("kv:k".into(), encode_for_queue(b"v0", 0, s1), 0, s1));
        assert_eq!(w.stats().pending, 1);
        let view = w.pending_view("kv:k").unwrap();
        assert_eq!(view.stamp, s2);
        assert_eq!(view.value.map(|(v, _)| v), Some(b"v2".to_vec()));
        // A newer tombstone replaces the value and reads as absent.
        let s3 = clock.next();
        assert!(w.enqueue_tombstone("kv:k".into(), s3));
        let view = w.pending_view("kv:k").unwrap();
        assert_eq!(view.stamp, s3);
        assert!(view.value.is_none());
    }

    #[test]
    fn enqueue_sheds_new_keys_past_the_cap_but_still_updates_known_ones() {
        let w = GossipWriter::new();
        let clock = StampClock::new("n");
        for i in 0..PENDING_CAP {
            let s = clock.next();
            assert!(w.enqueue_set(format!("kv:{i}"), encode_for_queue(b"v", 0, s), 0, s));
        }
        let s = clock.next();
        assert!(!w.enqueue_set("kv:one-too-many".into(), encode_for_queue(b"v", 0, s), 0, s));
        assert_eq!(w.stats().shed, 1);
        assert_eq!(w.stats().pending, PENDING_CAP);
        // A key already queued is still coalesced, never shed.
        let s = clock.next();
        assert!(w.enqueue_set("kv:0".into(), encode_for_queue(b"v2", 0, s), 0, s));
        assert_eq!(w.stats().shed, 1);
    }

    #[test]
    fn drain_applies_ops_and_drops_values_that_expired_while_queued() {
        let w = GossipWriter::new();
        let clock = StampClock::new("n");
        let mut c = chitchat("n", Duration::from_secs(60));
        let s = clock.next();
        w.enqueue_set("kv:live".into(), encode_for_queue(b"v", 0, s), 0, s);
        let s = clock.next();
        let past = now_epoch_ms() - 10;
        w.enqueue_set("kv:dead".into(), encode_for_queue(b"v", past, s), past, s);
        let s = clock.next();
        w.enqueue_tombstone("kv:gone".into(), s);

        let (ops, retires) = w.take_batch();
        assert_eq!(ops.len(), 3);
        w.apply_batch(&mut c, "n", ops, retires);
        assert_eq!(w.stats().pending, 0);
        let keys = live_kv_keys(&mut c);
        assert!(keys.contains(&"kv:live".to_string()));
        assert!(!keys.contains(&"kv:dead".to_string()), "expired-in-queue never enters state");
        assert!(decode_tombstone(c.self_node_state().get("kv:gone").unwrap()).is_some());
    }

    /// `flush` empties the whole queue into chitchat state in one call —
    /// including more than one `DRAIN_BATCH`-worth — so a graceful drain loses
    /// none of the last queued small-tier writes (#656).
    #[tokio::test]
    async fn flush_drains_every_queued_op_at_once() {
        let w = GossipWriter::new();
        let clock = StampClock::new("n");
        // More than one drain batch, so the loop must iterate.
        let total = DRAIN_BATCH + 10;
        for i in 0..total {
            let s = clock.next();
            w.enqueue_set(format!("kv:{i}"), encode_for_queue(b"v", 0, s), 0, s);
        }
        assert_eq!(w.stats().pending, total);
        let chitchat = Arc::new(tokio::sync::Mutex::new(chitchat("n", Duration::from_secs(60))));
        w.flush(&chitchat, "n").await;
        assert_eq!(w.stats().pending, 0, "flush empties the queue");
        let mut guard = chitchat.lock().await;
        assert_eq!(live_kv_keys(&mut guard).len(), total, "every queued write reached state");
        // Flushing an empty queue is a harmless no-op.
        drop(guard);
        w.flush(&chitchat, "n").await;
        assert_eq!(w.stats().pending, 0);
    }

    #[test]
    fn retire_deletes_only_an_older_own_copy() {
        let w = GossipWriter::new();
        let clock = StampClock::new("me");
        let mut c = chitchat("me", Duration::from_secs(60));
        let mine = clock.next();
        c.self_node_state().set("kv:k", encode_for_queue(b"mine", 0, mine));

        // A peer write OLDER than ours must not retire our copy.
        let older = Stamp::new(mine.write - 1, "peer");
        w.enqueue_retire("kv:k", older);
        let (ops, retires) = w.take_batch();
        w.apply_batch(&mut c, "me", ops, retires);
        assert!(c.self_node_state().get("kv:k").is_some());

        // A strictly newer peer write does.
        let newer = Stamp::new(mine.write + 1, "peer");
        w.enqueue_retire("kv:k", newer);
        let (ops, retires) = w.take_batch();
        w.apply_batch(&mut c, "me", ops, retires);
        assert!(c.self_node_state().get("kv:k").is_none(), "superseded copy is retired");
        assert_eq!(w.stats().swept, 1);
        // Retiring a key we never wrote is a no-op.
        w.enqueue_retire("kv:never", newer);
        let (ops, retires) = w.take_batch();
        w.apply_batch(&mut c, "me", ops, retires);
        assert_eq!(w.stats().swept, 1);
    }

    /// The sweep chitchat-deletes expired values and aged markers; chitchat's
    /// own tombstone GC (its tested behaviour, not ours) then removes them
    /// from the state entirely once the grace period passes.
    #[test]
    fn sweep_removes_expired_values_and_aged_markers() {
        let grace = Duration::from_secs(60);
        let mut c = chitchat("n", grace);
        let clock = StampClock::new("n");
        let now = now_epoch_ms();
        {
            let state = c.self_node_state();
            // Expired well past the slack.
            state.set("kv:expired", encode_for_queue(b"v", now - 5_000, clock.next()));
            // Expired, but inside the slack: left alone this sweep.
            state.set("kv:just-expired", encode_for_queue(b"v", now - 10, clock.next()));
            // Live with a future expiry, and live without one.
            state.set("kv:live-ttl", encode_for_queue(b"v", now + 60_000, clock.next()));
            state.set("kv:live", encode_for_queue(b"v", 0, clock.next()));
            // A marker older than the lifetime, and a fresh one.
            let old_ms = now - u64::try_from(TOMBSTONE_LIFETIME.as_millis()).unwrap() - 1_000;
            state.set("kv:old-marker", encode_tombstone(old_ms << SEQ_BITS));
            state.set("kv:new-marker", encode_tombstone(clock.next().write));
        }

        assert_eq!(sweep(&mut c), 2, "the expired value and the aged marker");
        let keys = live_kv_keys(&mut c);
        assert_eq!(
            keys,
            vec!["kv:just-expired", "kv:live", "kv:live-ttl", "kv:new-marker"],
            "swept entries are invisible to readers immediately"
        );
        // They are still physically present as chitchat tombstones until
        // chitchat's GC runs past the grace period — and are marked so.
        let state = c.self_node_state();
        assert_eq!(state.key_values_including_deleted().count(), 6);
        for key in ["kv:expired", "kv:old-marker"] {
            assert!(state.get_versioned(key).unwrap().is_deleted(), "{key} is tombstoned");
        }
        // A second sweep finds nothing new.
        assert_eq!(sweep(&mut c), 0);
    }

    #[test]
    fn is_garbage_rules() {
        let now = 1_000_000_000_000u64;
        let cutoff = now - 120_000;
        // Live, no TTL.
        assert!(!is_garbage("0:5:AA==", now, cutoff));
        // Live, future expiry.
        assert!(!is_garbage(&format!("{}:5:AA==", now + 1), now, cutoff));
        // Expired but inside the slack.
        assert!(!is_garbage(&format!("{}:5:AA==", now - 500), now, cutoff));
        // Expired past the slack.
        assert!(is_garbage(&format!("{}:5:AA==", now - 2_000), now, cutoff));
        // Markers by age of the stamp they carry.
        assert!(is_garbage(&encode_tombstone((cutoff - 1) << SEQ_BITS), now, cutoff));
        assert!(!is_garbage(&encode_tombstone((cutoff + 1) << SEQ_BITS), now, cutoff));
        // Garbage in, garbage out.
        assert!(is_garbage("not a payload", now, cutoff));
    }
}

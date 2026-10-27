//! Gossip-based KV tier for small values (≤ threshold).
//!
//! Stores key-value pairs directly in chitchat's per-node state.  Each
//! node can write to its own state; chitchat replicates it to every
//! other node automatically via the SWIM gossip protocol.
//!
//! Values are base64-encoded (chitchat stores `String → String`) with
//! an optional TTL and the origin's write stamp encoded as a prefix pair.
//!
//! Wire format: `"{expiry_ms}:{write}:{base64_value}"`.
//!   - `expiry_ms` is the epoch-millisecond expiry (0 = no TTL).
//!   - `write` is the origin's [`Stamp::write`]: a packed
//!     `(epoch_ms << SEQ_BITS) | seq` that is strictly monotone per node.
//!     Readers and the gossip applier order concurrent writes by it, with
//!     the origin's node id breaking ties (see [`Stamp`]).
//!
//! **One stamp per write.** The stamp is taken exactly once, by the
//! synchronous replication hook, before the local copy is written, and the
//! same stamp is what goes into the chitchat state. Before this the hook
//! recorded one wall-clock time for its local bookkeeping and the spawned
//! gossip task stamped the payload with a *second*, later one — so two nodes
//! writing one key within a few milliseconds could each win on the other
//! and never converge (stress finding F-7, 6 of 20 hot keys permanently
//! diverged). Equal stamps used to be treated as "stale" too, so a
//! same-millisecond tie was never resolved at all.
//!
//! **Tombstone form.** A delete is broadcast as a distinct payload
//! `"TS:{write}"`. The value slot is a well-known marker rather than
//! chitchat's own tombstone (chitchat's `subscribe_event` does not fire
//! on `state.delete()`, so peers would never learn about a real
//! tombstone — cross-node deletes would silently drop). The gossip
//! applier decodes this marker and calls `remove_local` on peers when
//! its stamp beats the last-applied write for the key, unifying
//! delete propagation for both the gossip tier and any locally-held
//! data-plane replica of the same key.
//!
//! **Compatibility with earlier formats.** v1 was `"{expiry_ms}:{base64}"`
//! (no stamp); v2 carried a raw epoch-millisecond `write_ms` in the middle
//! field. `decode_value`, `remaining_ttl_ms`, and the subscription decoder
//! accept both so a rolling upgrade does not drop data on the floor. A v1
//! entry decodes as `write = 0`; a v2 entry decodes as its raw millisecond
//! count, which is numerically far below any packed stamp (a packed stamp
//! is the millisecond count shifted left by [`SEQ_BITS`]). So during a
//! mixed-version window an upgraded node's write beats a not-yet-upgraded
//! node's write for the same key on every node — a consistent, if biased,
//! order that converges once the upgrade completes. A pre-upgrade node
//! parses the packed field as a plain number and orders by it, which is
//! also consistent. Only encode is one-way: new writes always emit the
//! packed form.
//!
//! **Cross-version tombstone tolerance.** A pre-tombstone peer decoding
//! `"TS:{write}"` fails to parse the leading field as a `u64`
//! ("TS" is not numeric) and treats the entry as invalid, i.e. silently
//! drops it. That is the correct behaviour during a rolling upgrade —
//! the old peer does not know how to apply a tombstone and would
//! misapply it as a spurious SET of empty bytes if the format were
//! ambiguous. The `TS:` prefix is unambiguously non-numeric.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64ct::{Base64, Encoding};
use chitchat::ChitchatId;

use crate::ClusterHandle;

/// Key prefix for KV entries in chitchat state.
pub(crate) const KV_PREFIX: &str = "kv:";

/// Wire marker for a tombstone (deleted key) in the gossip KV tier.
/// See the module docs for why we don't use chitchat's own `delete()`.
const TOMBSTONE_MARKER: &str = "TS:";

/// Low bits of [`Stamp::write`] that hold the per-millisecond sequence
/// number: up to 2^20 (about a million) distinct stamps per millisecond
/// per node before the clock is pushed ahead of wall time.
pub const SEQ_BITS: u32 = 20;

/// Current time in milliseconds since UNIX epoch.
pub(crate) fn now_epoch_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

/// Absolute epoch-millisecond expiry for `ttl` from now, `0` for no TTL —
/// the form the gossip wire carries.
#[must_use]
pub fn expiry_ms_for(ttl: Option<Duration>) -> u64 {
    ttl.map_or(0, |d| {
        let ttl_ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        now_epoch_ms().saturating_add(ttl_ms)
    })
}

/// Remaining lifetime for an absolute `expiry_ms` (`0` = no TTL), or
/// `None` for no TTL. An expiry already in the past yields `Some(0)`.
#[must_use]
pub fn ttl_from_expiry(expiry_ms: u64) -> Option<Duration> {
    (expiry_ms > 0).then(|| Duration::from_millis(expiry_ms.saturating_sub(now_epoch_ms())))
}

/// A write's position in the cluster-wide last-writer-wins order.
///
/// Ordered lexicographically by `(write, node_tag)`: later wall-clock
/// writes win; two writes in the same millisecond from one node are ordered
/// by the sequence bits inside `write`; two nodes writing in the same
/// millisecond are broken by `node_tag`, the same way on every node. A record
/// is applied only when its stamp is **strictly greater** than the last one
/// applied for its key, which makes every node converge on one winner.
///
/// Clock skew between nodes biases the order toward the faster clock; the
/// cluster is assumed to run NTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp {
    /// `(epoch_ms << SEQ_BITS) | seq`, strictly monotone per origin node.
    pub write: u64,
    /// FNV-1a of the origin node id ([`node_tag`]).
    pub node_tag: u64,
}

impl Stamp {
    /// The stamp that every real stamp beats.
    pub const ZERO: Self = Self { write: 0, node_tag: 0 };

    /// The stamp carried by a wire payload from `origin`.
    #[must_use]
    pub fn new(write: u64, origin: &str) -> Self {
        Self { write, node_tag: node_tag(origin) }
    }

    /// The wall-clock millisecond this stamp was taken in (for packed
    /// stamps; a legacy raw-millisecond stamp reports 0).
    #[must_use]
    pub fn epoch_ms(self) -> u64 {
        self.write >> SEQ_BITS
    }
}

/// Deterministic 64-bit tag for a node id (FNV-1a), identical on every
/// node, platform and build — it is a tie-breaker that all nodes must agree
/// on, so it cannot depend on `std`'s randomised or version-specific hashers.
#[must_use]
pub fn node_tag(node_id: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    node_id.bytes().fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

/// Per-node source of strictly increasing [`Stamp::write`] values.
///
/// Each call returns `max(now_ms << SEQ_BITS, previous + 1)`: the wall-clock
/// millisecond with a per-millisecond sequence, never repeating and never
/// going backwards even if the system clock does. A burst of more than 2^20
/// stamps in one millisecond simply runs ahead into the next millisecond.
#[derive(Debug)]
pub struct StampClock {
    last: AtomicU64,
    node_tag: u64,
}

impl StampClock {
    /// A clock issuing stamps on behalf of `node_id`.
    #[must_use]
    pub fn new(node_id: &str) -> Self {
        Self { last: AtomicU64::new(0), node_tag: node_tag(node_id) }
    }

    /// The tag this clock stamps with.
    #[must_use]
    pub fn node_tag(&self) -> u64 {
        self.node_tag
    }

    /// Issue the next stamp.
    pub fn next(&self) -> Stamp {
        self.next_after(None)
    }

    /// Issue the next stamp, strictly above `seen` when given (a
    /// hybrid-logical-clock receive and send in one step): the result beats
    /// `seen` whatever the two nodes' tags are, and every later stamp from this
    /// clock does too.
    ///
    /// Without it, a node that installs a peer's write and then writes the
    /// same key **within the same millisecond** can issue a smaller stamp than
    /// the one it just read (the peer's per-millisecond sequence ran ahead of
    /// this node's), and last-writer-wins silently discards the newer write.
    ///
    /// `seen` only moves the clock up to [`MAX_OBSERVED_DRIFT_MS`] ahead of
    /// this node's wall time. A stamp further ahead (a peer with a fast clock,
    /// or a corrupt one) is counted in [`METRIC_STAMP_DRIFT_CLAMPED`] and
    /// clamped, so one bad peer cannot drag every node's clock into the future
    /// (where applied-map pruning and tombstone aging, which go by stamp age,
    /// stop working) or saturate it at `u64::MAX` (where it would repeat
    /// stamps). The write is then stamped below `seen` and loses to it, as any
    /// write loses to a newer one.
    pub fn next_after(&self, seen: Option<Stamp>) -> Stamp {
        let now = now_epoch_ms();
        let base = now << SEQ_BITS;
        let floor = seen.map_or(0, |s| clamp_observed(s.write, now).saturating_add(1));
        let mut prev = self.last.load(Ordering::Relaxed);
        loop {
            let next = base.max(prev.saturating_add(1)).max(floor);
            match self.last.compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return Stamp { write: next, node_tag: self.node_tag },
                Err(seen) => prev = seen,
            }
        }
    }
}

/// How far ahead of this node's wall clock an observed stamp may pull its
/// [`StampClock`] ([`StampClock::next_after`]). Deliberately not
/// configurable: the cluster is assumed to run NTP, so honest skew is far
/// below this.
pub const MAX_OBSERVED_DRIFT_MS: u64 = 5_000;

/// Counter: observed stamps further than [`MAX_OBSERVED_DRIFT_MS`] ahead of
/// this node's clock, clamped instead of followed.
pub const METRIC_STAMP_DRIFT_CLAMPED: &str = "ephpm_cluster_stamp_drift_clamped_total";

/// `write` capped at `now + MAX_OBSERVED_DRIFT_MS` (packed), counting and
/// (at most once a minute) logging a cap.
fn clamp_observed(write: u64, now_ms: u64) -> u64 {
    static LAST_WARN_MS: AtomicU64 = AtomicU64::new(0);
    let ceiling = now_ms.saturating_add(MAX_OBSERVED_DRIFT_MS) << SEQ_BITS;
    if write <= ceiling {
        return write;
    }
    metrics::counter!(METRIC_STAMP_DRIFT_CLAMPED).increment(1);
    let last = LAST_WARN_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) >= 60_000
        && LAST_WARN_MS.compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    {
        tracing::warn!(
            ahead_ms = (write >> SEQ_BITS).saturating_sub(now_ms),
            max_drift_ms = MAX_OBSERVED_DRIFT_MS,
            "a peer's write stamp is too far ahead of this node's clock; not following it \
             (check NTP on every node)"
        );
    }
    ceiling
}

/// Encode a binary value with an absolute expiry and its write stamp into
/// the chitchat string format `"{expiry_ms}:{write}:{b64}"`.
pub(crate) fn encode_value(value: &[u8], expiry_ms: u64, write: u64) -> String {
    let b64 = Base64::encode_string(value);
    format!("{expiry_ms}:{write}:{b64}")
}

/// Encode a tombstone marker stamped with the origin's write stamp.
///
/// Peers ordering the tombstone against a concurrent SET on the same key
/// need the stamp to decide who wins; this reuses the same per-key
/// last-writer-wins ordering the SET applier already runs.
pub(crate) fn encode_tombstone(write: u64) -> String {
    format!("{TOMBSTONE_MARKER}{write}")
}

/// Parse a tombstone-encoded payload. Returns the origin write stamp when
/// `encoded` is a well-formed tombstone marker, otherwise `None`.
pub(crate) fn decode_tombstone(encoded: &str) -> Option<u64> {
    let rest = encoded.strip_prefix(TOMBSTONE_MARKER)?;
    rest.parse::<u64>().ok()
}

/// Split an encoded value into (expiry_ms, write, base64) while
/// accepting both the v1 two-field format and the three-field format.
///
/// v1 entries are reported with `write = 0` — the smallest possible
/// value, which lets the applier treat them as strictly older than any
/// current-format entry and always apply them once.
pub(crate) fn split_encoded(encoded: &str) -> Option<(u64, u64, &str)> {
    let (first, rest) = encoded.split_once(':')?;
    let expiry_ms: u64 = first.parse().ok()?;
    if let Some((second, tail)) = rest.split_once(':') {
        // Three-field form. `second` MUST parse as u64 for this to be a
        // stamped payload; if it doesn't, fall back to treating `rest` as
        // legacy base64 (unlikely — base64 alphabet has no ':' — but
        // symmetric with the accept-both contract).
        if let Ok(write) = second.parse::<u64>() {
            return Some((expiry_ms, write, tail));
        }
    }
    // v1 two-field form: no stamp, `rest` is the base64 payload.
    Some((expiry_ms, 0, rest))
}

/// Decode a chitchat value string back to binary, checking TTL.
///
/// Accepts BOTH the three-field format and the v1 two-field format —
/// necessary during a rolling upgrade where some peers still run the
/// pre-stamp code path. Returns `None` if the entry has expired.
fn decode_value(encoded: &str) -> Option<Vec<u8>> {
    let (expiry_ms, _write, b64) = split_encoded(encoded)?;

    if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
        return None; // expired
    }

    Base64::decode_vec(b64).ok()
}

/// Decode and also return the origin write stamp. Used by the gossip
/// applier for stale-write detection.
///
/// v1 legacy entries return `write = 0`, which sorts before any
/// stamped entry — so a legacy entry always applies on first arrival.
#[cfg(test)]
fn decode_value_with_write(encoded: &str) -> Option<(Vec<u8>, u64)> {
    let (expiry_ms, write, b64) = split_encoded(encoded)?;
    if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
        return None;
    }
    let bytes = Base64::decode_vec(b64).ok()?;
    Some((bytes, write))
}

/// Remaining TTL from an encoded value, in milliseconds. Accepts both
/// v1 and stamped wire formats (rolling-upgrade compatible).
///
/// Returns `None` if no TTL is set or the value format is invalid.
/// Returns `Some(0)` if already expired.
///
/// Retained for tests + rolling-upgrade documentation. The production
/// scanners in `gossip_get` / `gossip_pttl` compute the remaining TTL
/// inline so they can also handle tombstones and last-writer-wins
/// ordering in the same pass.
#[cfg(test)]
fn remaining_ttl_ms(encoded: &str) -> Option<u64> {
    let (expiry_ms, _write, _b64) = split_encoded(encoded)?;
    if expiry_ms == 0 {
        return None; // no TTL
    }
    Some(expiry_ms.saturating_sub(now_epoch_ms()))
}

/// The winner for one key across every node state and the pending queue:
/// the highest stamp seen, and what it carried.
/// The stamp an encoded gossip entry (value or tombstone marker) carries,
/// attributed to `origin`. `None` for an undecodable entry.
pub(crate) fn stamp_of(encoded: &str, origin: &str) -> Option<Stamp> {
    if let Some(write) = decode_tombstone(encoded) {
        return Some(Stamp::new(write, origin));
    }
    split_encoded(encoded).map(|(_, write, _)| Stamp::new(write, origin))
}

/// Write `encoded` under `key` in this node's own state unless the entry
/// already there carries a stamp at or above `stamp`. Returns whether it
/// was written.
///
/// Chitchat's `set` keeps whatever lands last. Two writes to one key take
/// their stamps in order in the synchronous hook but can reach the drain in
/// the other order (one enqueued after the newer one was already drained),
/// so without this the older write could land last: the origin's local copy
/// and applied map would say one thing while its gossip state — the only
/// thing a peer that missed the newer write will ever see — says the other,
/// and the two would never reconcile.
pub(crate) fn set_if_newer(
    state: &mut chitchat::NodeState,
    self_id: &str,
    key: String,
    encoded: String,
    stamp: Stamp,
) -> bool {
    if let Some(existing) = state.get(&key)
        && let Some(current) = stamp_of(existing, self_id)
        && current >= stamp
    {
        return false;
    }
    state.set(key, encoded);
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The winning entry's stamp.
    pub stamp: Stamp,
    /// The winning value and its absolute expiry (`0` = none); `None` when
    /// the winner is a tombstone **or an already-expired value** — an
    /// expired newest entry means the key is gone, and an older live copy
    /// from another node must not resurrect it.
    pub value: Option<(Vec<u8>, u64)>,
}

impl Resolved {
    fn offer(best: &mut Option<Self>, stamp: Stamp, value: Option<(Vec<u8>, u64)>) {
        if best.as_ref().is_none_or(|w| stamp > w.stamp) {
            *best = Some(Self { stamp, value });
        }
    }

    /// Offer one encoded gossip entry attributed to `origin`.
    fn offer_encoded(best: &mut Option<Self>, encoded: &str, origin: &str, now_ms: u64) {
        if let Some(write) = decode_tombstone(encoded) {
            Self::offer(best, Stamp::new(write, origin), None);
            return;
        }
        let Some((expiry_ms, write, b64)) = split_encoded(encoded) else { return };
        let stamp = Stamp::new(write, origin);
        if expiry_ms > 0 && now_ms >= expiry_ms {
            Self::offer(best, stamp, None);
            return;
        }
        if let Ok(bytes) = Base64::decode_vec(b64) {
            Self::offer(best, stamp, Some((bytes, expiry_ms)));
        }
    }

    /// Remaining TTL of the winning value in milliseconds: `None` for no TTL
    /// or no live value.
    #[must_use]
    pub fn remaining_ttl_ms(&self) -> Option<u64> {
        let (_, expiry_ms) = self.value.as_ref()?;
        (*expiry_ms > 0).then(|| expiry_ms.saturating_sub(now_epoch_ms()))
    }
}

/// Resolve `chitchat_key` under a held chitchat lock: the self state, every
/// known node state (dead ones included — a node that wrote last and then
/// died still holds the newest entry) and this node's pending queue.
pub(crate) fn resolve_locked(
    guard: &mut chitchat::Chitchat,
    writer: &crate::gossip_writer::GossipWriter,
    chitchat_key: &str,
) -> Option<Resolved> {
    let now_ms = now_epoch_ms();
    let mut best: Option<Resolved> = None;
    let self_id = guard.self_chitchat_id().node_id.clone();
    if let Some(encoded) = guard.self_node_state().get(chitchat_key) {
        Resolved::offer_encoded(&mut best, encoded, &self_id, now_ms);
    }
    for (node_id, state) in guard.node_states() {
        if node_id.node_id == self_id {
            continue;
        }
        if let Some(encoded) = state.get(chitchat_key) {
            Resolved::offer_encoded(&mut best, encoded, &node_id.node_id, now_ms);
        }
    }
    if let Some(pending) = writer.pending_view(chitchat_key) {
        Resolved::offer(&mut best, pending.stamp, pending.value);
    }
    best
}

impl ClusterHandle {
    /// Issue the next write stamp on behalf of this node.
    ///
    /// Every replicated write takes exactly one stamp here, before its local
    /// copy is written, and carries that stamp on every transport.
    #[must_use]
    pub fn stamp(&self) -> Stamp {
        self.clock.next()
    }

    /// Issue the next write stamp, strictly above `seen` when given — the
    /// stamp for a write that must order after a write this node has already
    /// applied (see [`StampClock::next_after`]).
    #[must_use]
    pub fn stamp_after(&self, seen: Option<Stamp>) -> Stamp {
        self.clock.next_after(seen)
    }

    /// This node's tie-break tag ([`node_tag`] of its node id).
    #[must_use]
    pub fn node_tag(&self) -> u64 {
        self.clock.node_tag()
    }

    /// Set a small key in the gossip KV tier, stamping the write now.
    ///
    /// The value is stored in this node's chitchat state and replicated
    /// to all other nodes via gossip (typically within 1-3 seconds).
    /// Callers that also keep a local copy should take the stamp
    /// themselves and use [`gossip_set_queued`](Self::gossip_set_queued)
    /// so both carry the same one.
    pub fn gossip_set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Stamp {
        let stamp = self.stamp();
        self.gossip_set_queued(key, value, expiry_ms_for(ttl), stamp);
        stamp
    }

    /// Set a small key in the gossip KV tier with a stamp and absolute
    /// expiry the caller already fixed.
    ///
    /// Queues the write on this node's [`GossipWriter`](crate::gossip_writer::GossipWriter);
    /// the drain task puts it into chitchat state within milliseconds. Reads
    /// on this node see it immediately. Synchronous — the replication hooks
    /// call it from PHP threads and RESP tasks with no runtime context —
    /// and never blocks. Returns `false` when the queue was full and the
    /// write was shed (the key does not replicate until it is written again).
    pub fn gossip_set_queued(&self, key: &str, value: &[u8], expiry_ms: u64, stamp: Stamp) -> bool {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let encoded = encode_value(value, expiry_ms, stamp.write);
        self.writer.enqueue_set(chitchat_key, encoded, expiry_ms, stamp)
    }

    /// Resolve the newest entry for `key` across every node state and this
    /// node's pending writes. `None` when no node has ever written it (or
    /// everything about it has been garbage-collected).
    pub async fn gossip_resolve(&self, key: &str) -> Option<Resolved> {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        resolve_locked(&mut guard, &self.writer, &chitchat_key)
    }

    /// Get a small key from the gossip KV tier.
    ///
    /// Searches this node's state, every known node's, and this node's
    /// pending writes, and returns the value with the highest stamp
    /// (tombstones included) — the same last-writer-wins rule the applier
    /// runs, applied at read time so nodes still catching up show
    /// consistent results. If the newest visible entry is a tombstone or
    /// has expired, the key reads as deleted — a stale live copy on another
    /// node cannot resurrect it.
    pub async fn gossip_get(&self, key: &str) -> Option<Vec<u8>> {
        self.gossip_resolve(key).await?.value.map(|(bytes, _)| bytes)
    }

    /// Broadcast a delete for `key` across the gossip KV tier, stamping it
    /// now. See [`gossip_del_queued`](Self::gossip_del_queued).
    pub fn gossip_del(&self, key: &str) -> bool {
        let stamp = self.stamp();
        self.gossip_del_queued(key, stamp)
    }

    /// Broadcast a delete for `key` across the gossip KV tier with a stamp
    /// the caller already fixed.
    ///
    /// Writes a tombstone marker (`"TS:{write}"`) into this node's
    /// chitchat state so peers observe it via `subscribe_event`
    /// (chitchat's real `state.delete()` does **not** fire subscribers,
    /// so a plain delete would be invisible to other nodes' appliers).
    /// The applier on peer nodes calls `Store::remove_local` when the
    /// tombstone's stamp beats their last-applied write for the key,
    /// which drops both the gossip-materialized copy and any locally-held
    /// data-plane replica of the same key. Returns `true` regardless of
    /// whether this node had previously set the key locally — the
    /// intent-to-delete has been broadcast either way.
    ///
    /// Synchronous and non-blocking, like [`gossip_set_queued`](Self::gossip_set_queued).
    /// Returns `false` when the queue was full and the tombstone was shed.
    pub fn gossip_del_queued(&self, key: &str, stamp: Stamp) -> bool {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        self.writer.enqueue_tombstone(chitchat_key, stamp)
    }

    /// Ask the writer to delete this node's own gossip copy of `key` if it
    /// is older than `superseded_by` — a peer's write for the key that has
    /// just been applied locally. See [`crate::gossip_writer`] for why.
    pub fn gossip_retire(&self, key: &str, superseded_by: Stamp) {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        self.writer.enqueue_retire(&chitchat_key, superseded_by);
    }

    /// Counters of this node's gossip writer (pending, shed, swept).
    #[must_use]
    pub fn gossip_writer_stats(&self) -> crate::gossip_writer::GossipWriterStats {
        self.writer.stats()
    }

    /// Size of this node's **own** gossip KV state: entries readers can see
    /// (values and tombstone markers) and entries chitchat-deleted by the
    /// sweep that are awaiting chitchat's grace-period GC.
    pub async fn gossip_state_size(&self) -> GossipStateSize {
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        let mut size = GossipStateSize::default();
        for (key, versioned) in guard.self_node_state().key_values_including_deleted() {
            if !key.starts_with(KV_PREFIX) {
                continue;
            }
            if versioned.is_deleted() {
                size.awaiting_gc += 1;
            } else {
                size.live += 1;
            }
        }
        size
    }

    /// Whether this node's own gossip state holds a visible entry (value or
    /// tombstone marker) for `key` — i.e. whether this node is still
    /// publishing it.
    pub async fn gossip_self_publishes(&self, key: &str) -> bool {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        guard.self_node_state().get(&chitchat_key).is_some()
    }

    /// Check if a small key exists in the gossip KV tier (not expired).
    pub async fn gossip_exists(&self, key: &str) -> bool {
        self.gossip_get(key).await.is_some()
    }

    /// Get the remaining TTL of a gossip key in milliseconds.
    ///
    /// Returns `None` if the key has no TTL, does not exist, or the
    /// newest visible entry is a tombstone (the key reads as deleted).
    pub async fn gossip_pttl(&self, key: &str) -> Option<u64> {
        self.gossip_resolve(key).await?.remaining_ttl_ms()
    }

    /// List all gossip KV keys visible to this node (across all live
    /// nodes and this node's pending writes), excluding expired entries.
    pub async fn gossip_keys(&self) -> Vec<String> {
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        let mut keys = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Pending writes first: a queued tombstone hides the key even if a
        // state still holds a live copy, and a queued set shows it.
        let mut hidden = std::collections::HashSet::new();
        self.writer.for_each_pending(|chitchat_key, live| {
            if let Some(stripped) = chitchat_key.strip_prefix(KV_PREFIX) {
                if live {
                    if seen.insert(stripped.to_string()) {
                        keys.push(stripped.to_string());
                    }
                } else {
                    hidden.insert(stripped.to_string());
                }
            }
        });
        seen.extend(hidden);

        // Collect from self.
        collect_kv_keys(guard.self_node_state(), &mut keys, &mut seen);

        // Collect from all live nodes.
        for node_id in guard.live_nodes().cloned().collect::<Vec<_>>() {
            if let Some(state) = guard.node_state(&node_id) {
                collect_kv_keys(state, &mut keys, &mut seen);
            }
        }

        keys.sort();
        keys
    }

    /// Subscribe to gossip KV changes across all nodes.
    ///
    /// Delivers a [`KvChangeEvent`] per change: either a `Set` (with the
    /// decoded value, remaining TTL, and origin stamp) or a
    /// `Tombstone` (with just the origin stamp). Peers use the
    /// tombstone variant to drop their local copies of a remotely-deleted
    /// key — see [`crate::clustered_store::start_gossip_applier`]. The
    /// stamp's `write` is 0 for legacy pre-stamp peers.
    pub async fn subscribe_kv_changes<F>(&self, callback: F)
    where
        F: Fn(KvChangeEvent<'_>) + Send + Sync + 'static,
    {
        let chitchat = self.handle.chitchat();
        let guard = chitchat.lock().await;
        guard
            .subscribe_event(KV_PREFIX, move |event| {
                let origin = event.node.node_id.as_str();
                // Tombstones first (unambiguous prefix).
                if let Some(write) = decode_tombstone(event.value) {
                    callback(KvChangeEvent::Tombstone {
                        key: event.key,
                        stamp: Stamp::new(write, origin),
                        node: event.node,
                    });
                    return;
                }
                // Parse the raw fields ourselves so an event whose
                // encoded expiry has already passed still surfaces to
                // the applier as a delete. If we filtered it out, a
                // shortened TTL that expired before the gossip cycle
                // delivered the event would silently leave the peer's
                // longer-TTL copy in place — the very race that made
                // clustered `EXPIRE` a no-op in KV v1. Delivering it as
                // a Tombstone (with the origin's stamp for
                // last-writer-wins) lets the applier drop the peer
                // copy on receipt.
                if let Some((expiry_ms, write, b64)) = split_encoded(event.value) {
                    let stamp = Stamp::new(write, origin);
                    if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
                        callback(KvChangeEvent::Tombstone {
                            key: event.key,
                            stamp,
                            node: event.node,
                        });
                        return;
                    }
                    if let Ok(value) = Base64::decode_vec(b64) {
                        callback(KvChangeEvent::Set {
                            key: event.key,
                            value: &value,
                            ttl: ttl_from_expiry(expiry_ms),
                            stamp,
                            node: event.node,
                        });
                    }
                }
            })
            .forever();
    }
}

/// Entry counts of one node's own gossip KV state
/// ([`ClusterHandle::gossip_state_size`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GossipStateSize {
    /// Entries visible to readers: live values, expired values the sweep
    /// has not reached yet, and tombstone markers within their lifetime.
    pub live: usize,
    /// Entries the sweep has chitchat-deleted; chitchat removes them for
    /// good once its grace period passes.
    pub awaiting_gc: usize,
}

/// A change observed on the gossip KV tier by
/// [`ClusterHandle::subscribe_kv_changes`].
///
/// `Set` variants carry the new value + TTL; `Tombstone` variants signal
/// a remote delete that peers should apply to their local copies.
#[derive(Debug, Copy, Clone)]
pub enum KvChangeEvent<'a> {
    /// A key was set (created or overwritten). See
    /// [`ClusterHandle::subscribe_kv_changes`] for field semantics.
    Set {
        /// The affected key (without the `kv:` prefix).
        key: &'a str,
        /// The new decoded value bytes.
        value: &'a [u8],
        /// Remaining TTL, if the entry carries one.
        ttl: Option<Duration>,
        /// Origin stamp for last-writer-wins ordering. `write == 0`
        /// marks a legacy pre-stamp peer's write.
        stamp: Stamp,
        /// The node that produced the change.
        node: &'a ChitchatId,
    },
    /// A key was deleted on the origin node. Peers apply the delete
    /// locally when `stamp` beats their last-applied write for
    /// `key` (see [`crate::clustered_store::start_gossip_applier`]).
    Tombstone {
        /// The affected key (without the `kv:` prefix).
        key: &'a str,
        /// Origin stamp for last-writer-wins ordering.
        stamp: Stamp,
        /// The node that produced the delete.
        node: &'a ChitchatId,
    },
}

impl<'a> KvChangeEvent<'a> {
    /// The affected key, common to both variants.
    #[must_use]
    pub fn key(&self) -> &'a str {
        match self {
            Self::Set { key, .. } | Self::Tombstone { key, .. } => key,
        }
    }

    /// The originating node, common to both variants.
    #[must_use]
    pub fn node(&self) -> &'a ChitchatId {
        match self {
            Self::Set { node, .. } | Self::Tombstone { node, .. } => node,
        }
    }

    /// The origin stamp, common to both variants.
    #[must_use]
    pub fn stamp(&self) -> Stamp {
        match self {
            Self::Set { stamp, .. } | Self::Tombstone { stamp, .. } => *stamp,
        }
    }
}

/// Helper: collect non-expired, non-tombstoned KV keys from a node state.
fn collect_kv_keys(
    state: &chitchat::NodeState,
    keys: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    for (key, value) in state.key_values() {
        if let Some(stripped) = key.strip_prefix(KV_PREFIX) {
            // Skip tombstones outright — a deleted key must not appear
            // in KEYS output. Also skip expired entries.
            if !seen.contains(stripped)
                && decode_tombstone(value).is_none()
                && decode_value(value).is_some()
            {
                seen.insert(stripped.to_string());
                keys.push(stripped.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamped(value: &[u8], ttl: Option<Duration>) -> String {
        encode_value(value, expiry_ms_for(ttl), StampClock::new("test").next().write)
    }

    #[test]
    fn encode_decode_no_ttl() {
        let value = b"hello world";
        let encoded = stamped(value, None);
        // Wire form: "{expiry}:{write}:{b64}", so the leading "0:"
        // (no TTL) is followed by another numeric colon-terminated field.
        assert!(encoded.starts_with("0:"));
        assert!(encoded.matches(':').count() >= 2);
        let decoded = decode_value(&encoded).expect("should decode");
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_with_ttl() {
        let value = b"test data";
        let encoded = stamped(value, Some(Duration::from_secs(3600)));
        // Expiry should be non-zero, and the stamp should also be non-zero.
        let (expiry, write, _b64) = split_encoded(&encoded).unwrap();
        assert!(expiry > 0);
        assert!(write > 0);
        // Should decode fine (not expired).
        let decoded = decode_value(&encoded).expect("should decode");
        assert_eq!(decoded, value);
    }

    #[test]
    fn decode_expired_returns_none() {
        // Manually craft an expired entry (expiry in the past).
        let b64 = Base64::encode_string(b"old data");
        let encoded = format!("1:1:{b64}"); // epoch ms = 1 → long expired
        assert!(decode_value(&encoded).is_none());
    }

    #[test]
    fn decode_accepts_legacy_two_field_format() {
        // A pre-stamp peer emits "{expiry}:{b64}" with no stamp slot. The
        // applier still needs to accept those during a rolling upgrade or
        // data goes on the floor.
        let value = b"legacy peer wrote this";
        let b64 = Base64::encode_string(value);
        let legacy = format!("0:{b64}"); // no TTL, no stamp
        let decoded = decode_value(&legacy).expect("legacy form should decode");
        assert_eq!(decoded, value);
        let (bytes, write) = decode_value_with_write(&legacy).expect("legacy form should split");
        assert_eq!(bytes, value);
        assert_eq!(write, 0, "legacy entries report write = 0");
        assert!(remaining_ttl_ms(&legacy).is_none());
    }

    #[test]
    fn decode_accepts_current_three_field_format() {
        // Round-trip through encode_value → decode_value_with_write.
        let value = b"stamped write";
        let encoded = stamped(value, None);
        let (bytes, write) = decode_value_with_write(&encoded).expect("should decode");
        assert_eq!(bytes, value);
        assert!(write > 0, "stamped entries carry a real stamp");
    }

    /// A v2 peer's raw-millisecond `write_ms` must sort below every packed
    /// stamp, so a mixed-version cluster orders consistently (upgraded
    /// nodes' writes win) instead of flapping.
    #[test]
    fn raw_millisecond_stamps_sort_below_packed_ones() {
        let raw_now = now_epoch_ms();
        let packed = StampClock::new("n").next().write;
        assert!(raw_now < packed);
        // Even a raw stamp far in the future (clock skew of a year) loses.
        assert!(raw_now + 365 * 24 * 3600 * 1000 < packed);
        assert_eq!(Stamp { write: packed, node_tag: 0 }.epoch_ms(), packed >> SEQ_BITS);
    }

    /// Stamps from one clock never repeat and never go backwards.
    #[test]
    fn stamp_clock_is_strictly_monotone() {
        let clock = StampClock::new("n");
        let mut prev = clock.next();
        for _ in 0..10_000 {
            let next = clock.next();
            assert!(next > prev, "{next:?} must beat {prev:?}");
            assert_eq!(next.node_tag, prev.node_tag);
            prev = next;
        }
        // Many stamps in one millisecond ride the sequence bits, not the
        // clock: the millisecond part moves at most by the elapsed time.
        assert!(prev.epoch_ms() <= now_epoch_ms() + 1);
    }

    /// A write stamped after observing a peer's stamp beats it, even when the
    /// peer's sequence for the current millisecond is ahead of this clock's
    /// and the peer's tag sorts higher — the same-millisecond read-then-write
    /// that made a GETEX/GETDEL on a non-holder lose to the value it read.
    #[test]
    fn observed_stamp_is_always_beaten() {
        let clock = StampClock::new("n");
        let peer = Stamp { write: (now_epoch_ms() << SEQ_BITS) + 5, node_tag: u64::MAX };
        // Unobserved, this clock's next stamp in the same millisecond loses.
        let blind = StampClock::new("n").next();
        if blind.epoch_ms() == peer.epoch_ms() {
            assert!(blind < peer);
        }
        let mine = clock.next_after(Some(peer));
        assert!(mine > peer, "{mine:?} must beat the observed {peer:?}");
        // Observing an older stamp never moves the clock backwards.
        assert!(clock.next_after(Some(Stamp { write: 1, node_tag: 0 })) > mine);
    }

    /// A far-future or near-`u64::MAX` observed stamp moves the clock at most
    /// `MAX_OBSERVED_DRIFT_MS` ahead, and the clock keeps issuing strictly
    /// increasing stamps afterwards (no saturation, no repeats).
    #[test]
    fn observed_stamps_beyond_the_drift_bound_are_clamped() {
        let ceiling = |now: u64| (now + MAX_OBSERVED_DRIFT_MS) << SEQ_BITS;
        for far in [(now_epoch_ms() + 3_600_000) << SEQ_BITS, u64::MAX - 1, u64::MAX] {
            let clock = StampClock::new("n");
            let first = clock.next_after(Some(Stamp { write: far, node_tag: 0 }));
            assert!(first.write <= ceiling(now_epoch_ms()) + 1, "{first:?} followed {far}");
            let mut prev = first;
            for _ in 0..1_000 {
                let next = clock.next();
                assert!(next > prev && next.write <= ceiling(now_epoch_ms()) + 1_001);
                prev = next;
            }
        }
        // Within the bound the observed stamp is still followed exactly.
        let near = (now_epoch_ms() + 2_000) << SEQ_BITS;
        let clock = StampClock::new("n");
        assert_eq!(clock.next_after(Some(Stamp { write: near, node_tag: 0 })).write, near + 1);
    }

    /// Same `write`, different nodes: the tag breaks the tie, the same way
    /// everywhere, and neither write is "equal" to the other.
    #[test]
    fn same_millisecond_writes_are_ordered_by_node_tag() {
        let a = Stamp::new(42 << SEQ_BITS, "node-a");
        let b = Stamp::new(42 << SEQ_BITS, "node-b");
        assert_ne!(a, b);
        assert_eq!(a > b, node_tag("node-a") > node_tag("node-b"));
        assert_eq!(a.max(b), b.max(a), "the winner is the same whoever asks");
        // A later write beats any tag.
        let later = Stamp::new((42 << SEQ_BITS) + 1, "node-a");
        assert!(later > a && later > b);
        // The tag is stable and deterministic.
        assert_eq!(node_tag("node-a"), node_tag("node-a"));
        assert_ne!(node_tag("node-a"), node_tag("node-b"));
    }

    #[test]
    fn remaining_ttl_no_expiry() {
        let encoded = stamped(b"x", None);
        assert!(remaining_ttl_ms(&encoded).is_none());
    }

    #[test]
    fn remaining_ttl_future() {
        let encoded = stamped(b"x", Some(Duration::from_secs(60)));
        let ttl = remaining_ttl_ms(&encoded).expect("should have TTL");
        // Should be roughly 60 seconds (in ms), within a second tolerance.
        assert!(ttl > 58_000);
        assert!(ttl <= 60_000);
    }

    #[test]
    fn remaining_ttl_expired() {
        let b64 = Base64::encode_string(b"x");
        let encoded = format!("1:1:{b64}");
        let ttl = remaining_ttl_ms(&encoded).expect("should have TTL");
        assert_eq!(ttl, 0);
    }

    #[test]
    fn remaining_ttl_expired_legacy_two_field() {
        // Legacy peer emits "{expiry}:{b64}" — remaining_ttl_ms MUST still
        // report a value or a rolling upgrade would silently break TTL
        // observability on freshly-written keys from the older peer.
        let b64 = Base64::encode_string(b"x");
        let encoded = format!("1:{b64}");
        let ttl = remaining_ttl_ms(&encoded).expect("legacy TTL should decode");
        assert_eq!(ttl, 0);
    }

    #[test]
    fn tombstone_roundtrip_carries_stamp() {
        let write = StampClock::new("n").next().write;
        let encoded = encode_tombstone(write);
        assert_eq!(decode_tombstone(&encoded), Some(write));
        // decode_value must NOT treat the tombstone as a value — it has
        // no colons after "TS:" so split_encoded returns None.
        assert!(decode_value(&encoded).is_none(), "tombstone must not decode as a value");
        assert!(decode_value_with_write(&encoded).is_none());
        assert!(remaining_ttl_ms(&encoded).is_none());
    }

    #[test]
    fn tombstone_marker_is_unambiguous() {
        // A legitimate v1 or stamped value's leading field is always a
        // numeric expiry_ms — decode_tombstone must reject those.
        let v2 = stamped(b"real value", None);
        assert!(decode_tombstone(&v2).is_none(), "stamped payloads are not tombstones");
        let v1 = format!("0:{}", Base64::encode_string(b"legacy"));
        assert!(decode_tombstone(&v1).is_none(), "v1 payloads are not tombstones");
        // And a garbage-tombstone (bad stamp) is rejected.
        assert!(decode_tombstone("TS:not-a-number").is_none());
        assert!(decode_tombstone("TS:").is_none());
    }

    #[test]
    fn binary_data_roundtrip() {
        // Test with arbitrary binary data including null bytes.
        let value: Vec<u8> = (0..=255).collect();
        let encoded = stamped(&value, None);
        let decoded = decode_value(&encoded).expect("should decode");
        assert_eq!(decoded, value);
    }

    #[test]
    fn expiry_helpers_round_trip() {
        assert_eq!(expiry_ms_for(None), 0);
        assert!(ttl_from_expiry(0).is_none());
        let expiry = expiry_ms_for(Some(Duration::from_secs(10)));
        let ttl = ttl_from_expiry(expiry).unwrap();
        assert!(ttl > Duration::from_secs(9) && ttl <= Duration::from_secs(10));
        assert_eq!(ttl_from_expiry(1), Some(Duration::ZERO), "past expiry reads as zero left");
    }
}

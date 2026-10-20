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
const KV_PREFIX: &str = "kv:";

/// Wire marker for a tombstone (deleted key) in the gossip KV tier.
/// See the module docs for why we don't use chitchat's own `delete()`.
const TOMBSTONE_MARKER: &str = "TS:";

/// Low bits of [`Stamp::write`] that hold the per-millisecond sequence
/// number: up to 2^20 (about a million) distinct stamps per millisecond
/// per node before the clock is pushed ahead of wall time.
pub const SEQ_BITS: u32 = 20;

/// Current time in milliseconds since UNIX epoch.
fn now_epoch_ms() -> u64 {
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
        let base = now_epoch_ms() << SEQ_BITS;
        let mut prev = self.last.load(Ordering::Relaxed);
        loop {
            let next = base.max(prev.saturating_add(1));
            match self.last.compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return Stamp { write: next, node_tag: self.node_tag },
                Err(seen) => prev = seen,
            }
        }
    }
}

/// Encode a binary value with an absolute expiry and its write stamp into
/// the chitchat string format `"{expiry_ms}:{write}:{b64}"`.
fn encode_value(value: &[u8], expiry_ms: u64, write: u64) -> String {
    let b64 = Base64::encode_string(value);
    format!("{expiry_ms}:{write}:{b64}")
}

/// Encode a tombstone marker stamped with the origin's write stamp.
///
/// Peers ordering the tombstone against a concurrent SET on the same key
/// need the stamp to decide who wins; this reuses the same per-key
/// last-writer-wins ordering the SET applier already runs.
fn encode_tombstone(write: u64) -> String {
    format!("{TOMBSTONE_MARKER}{write}")
}

/// Parse a tombstone-encoded payload. Returns the origin write stamp when
/// `encoded` is a well-formed tombstone marker, otherwise `None`.
fn decode_tombstone(encoded: &str) -> Option<u64> {
    let rest = encoded.strip_prefix(TOMBSTONE_MARKER)?;
    rest.parse::<u64>().ok()
}

/// Split an encoded value into (expiry_ms, write, base64) while
/// accepting both the v1 two-field format and the three-field format.
///
/// v1 entries are reported with `write = 0` — the smallest possible
/// value, which lets the applier treat them as strictly older than any
/// current-format entry and always apply them once.
fn split_encoded(encoded: &str) -> Option<(u64, u64, &str)> {
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
/// their stamps in order in the synchronous hook but reach chitchat from
/// separate tasks, so without this the older write can land last: the
/// origin's local copy and applied map then say one thing while its gossip
/// state — the only thing a peer that missed the newer write will ever see —
/// says the other, and the two never reconcile.
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

/// The winner so far while scanning one key across node states: the
/// highest stamp seen, and what it carried (`None` = tombstone).
struct Winner<T> {
    stamp: Stamp,
    payload: Option<T>,
}

impl<T> Winner<T> {
    /// Replace the winner when `stamp` is strictly newer.
    fn offer(best: &mut Option<Self>, stamp: Stamp, payload: Option<T>) {
        if best.as_ref().is_none_or(|w| stamp > w.stamp) {
            *best = Some(Self { stamp, payload });
        }
    }
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
    /// themselves and use [`gossip_set_stamped`](Self::gossip_set_stamped)
    /// so both carry the same one.
    pub async fn gossip_set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Stamp {
        let stamp = self.stamp();
        self.gossip_set_stamped(key, value, expiry_ms_for(ttl), stamp).await;
        stamp
    }

    /// Set a small key in the gossip KV tier with a stamp and absolute
    /// expiry the caller already fixed.
    ///
    /// The state write is ordered by stamp (see [`set_if_newer`]): two
    /// writes to one key from this node reach chitchat from separate tasks,
    /// and the older one must not land last.
    pub async fn gossip_set_stamped(&self, key: &str, value: &[u8], expiry_ms: u64, stamp: Stamp) {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let encoded = encode_value(value, expiry_ms, stamp.write);
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        let self_id = guard.self_chitchat_id().node_id.clone();
        set_if_newer(guard.self_node_state(), &self_id, chitchat_key, encoded, stamp);
    }

    /// Get a small key from the gossip KV tier.
    ///
    /// Searches this node's state and every live node's, and returns the
    /// value with the highest stamp (tombstones included) — the same
    /// last-writer-wins rule the applier runs, applied at read time so
    /// nodes still catching up show consistent results. If the newest
    /// visible entry is a tombstone, the key reads as deleted — a stale
    /// live copy on another node cannot resurrect a concurrent delete.
    pub async fn gossip_get(&self, key: &str) -> Option<Vec<u8>> {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;

        let mut best: Option<Winner<Vec<u8>>> = None;
        let consider = |encoded: &str, origin: &str, best: &mut Option<Winner<Vec<u8>>>| {
            if let Some(write) = decode_tombstone(encoded) {
                Winner::offer(best, Stamp::new(write, origin), None);
                return;
            }
            if let Some((value, write)) = decode_value_with_write(encoded) {
                Winner::offer(best, Stamp::new(write, origin), Some(value));
            }
        };

        let self_id = guard.self_chitchat_id().node_id.clone();
        if let Some(encoded) = guard.self_node_state().get(&chitchat_key) {
            consider(encoded, &self_id, &mut best);
        }
        for node_id in guard.live_nodes().cloned().collect::<Vec<_>>() {
            if let Some(state) = guard.node_state(&node_id)
                && let Some(encoded) = state.get(&chitchat_key)
            {
                consider(encoded, &node_id.node_id, &mut best);
            }
        }
        best.and_then(|w| w.payload)
    }

    /// Broadcast a delete for `key` across the gossip KV tier, stamping it
    /// now. See [`gossip_del_stamped`](Self::gossip_del_stamped).
    pub async fn gossip_del(&self, key: &str) -> bool {
        let stamp = self.stamp();
        self.gossip_del_stamped(key, stamp).await
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
    pub async fn gossip_del_stamped(&self, key: &str, stamp: Stamp) -> bool {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let encoded = encode_tombstone(stamp.write);
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        let self_id = guard.self_chitchat_id().node_id.clone();
        set_if_newer(guard.self_node_state(), &self_id, chitchat_key, encoded, stamp);
        true
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
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;

        // Same last-writer-wins scan as gossip_get, but return the TTL
        // of the winning entry (None for tombstones and TTL-less values).
        let mut best: Option<Winner<Option<u64>>> = None;
        let consider = |encoded: &str, origin: &str, best: &mut Option<Winner<Option<u64>>>| {
            if let Some(write) = decode_tombstone(encoded) {
                Winner::offer(best, Stamp::new(write, origin), None);
                return;
            }
            if let Some((expiry_ms, write, _b64)) = split_encoded(encoded) {
                if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
                    return; // already expired
                }
                let ttl = if expiry_ms == 0 {
                    None
                } else {
                    Some(expiry_ms.saturating_sub(now_epoch_ms()))
                };
                Winner::offer(best, Stamp::new(write, origin), Some(ttl));
            }
        };
        let self_id = guard.self_chitchat_id().node_id.clone();
        if let Some(encoded) = guard.self_node_state().get(&chitchat_key) {
            consider(encoded, &self_id, &mut best);
        }
        for node_id in guard.live_nodes().cloned().collect::<Vec<_>>() {
            if let Some(state) = guard.node_state(&node_id)
                && let Some(encoded) = state.get(&chitchat_key)
            {
                consider(encoded, &node_id.node_id, &mut best);
            }
        }
        best.and_then(|w| w.payload).flatten()
    }

    /// List all gossip KV keys visible to this node (across all live
    /// nodes), excluding expired entries.
    pub async fn gossip_keys(&self) -> Vec<String> {
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        let mut keys = Vec::new();
        let mut seen = std::collections::HashSet::new();

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

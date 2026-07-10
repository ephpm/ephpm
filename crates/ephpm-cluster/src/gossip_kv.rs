//! Gossip-based KV tier for small values (≤ threshold).
//!
//! Stores key-value pairs directly in chitchat's per-node state.  Each
//! node can write to its own state; chitchat replicates it to every
//! other node automatically via the SWIM gossip protocol.
//!
//! Values are base64-encoded (chitchat stores `String → String`) with
//! an optional TTL and the origin's wall-clock write time encoded as a
//! millisecond prefix pair.
//!
//! Wire format: `"{expiry_ms}:{write_ms}:{base64_value}"`.
//!   - `expiry_ms` is the epoch-millisecond expiry (0 = no TTL).
//!   - `write_ms` is the epoch-millisecond the origin node produced the
//!     value. Used by the gossip applier for last-arrival-wins ordering
//!     so a slow echo of an older write cannot clobber a newer one.
//!
//! **Tombstone form.** A delete is broadcast as a distinct payload
//! `"TS:{write_ms}"`. The value slot is a well-known marker rather than
//! chitchat's own tombstone (chitchat's `subscribe_event` does not fire
//! on `state.delete()`, so peers would never learn about a real
//! tombstone — cross-node deletes would silently drop). The gossip
//! applier decodes this marker and calls `remove_local` on peers when
//! its `write_ms` beats the last-applied write for the key, unifying
//! delete propagation for both the gossip tier and any locally-held
//! data-plane replica of the same key.
//!
//! **Legacy format compatibility.** The v1 format was
//! `"{expiry_ms}:{base64_value}"` (no `write_ms`). `decode_value`,
//! `remaining_ttl_ms`, and the subscription decoder still accept it so a
//! rolling upgrade from a pre-`write_ms` peer does not drop data on the
//! floor. Legacy entries are treated as `write_ms = 0`, which means the
//! applier will always apply them (they are strictly older than any
//! current-format entry). Only encode is one-way: new writes always
//! emit the three-field form.
//!
//! **Cross-version tombstone tolerance.** A pre-tombstone peer decoding
//! `"TS:{write_ms}"` fails to parse the leading field as a `u64`
//! ("TS" is not numeric) and treats the entry as invalid, i.e. silently
//! drops it. That is the correct behaviour during a rolling upgrade —
//! the old peer does not know how to apply a tombstone and would
//! misapply it as a spurious SET of empty bytes if the format were
//! ambiguous. The `TS:` prefix is unambiguously non-numeric.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64ct::{Base64, Encoding};
use chitchat::ChitchatId;

use crate::ClusterHandle;

/// Key prefix for KV entries in chitchat state.
const KV_PREFIX: &str = "kv:";

/// Wire marker for a tombstone (deleted key) in the gossip KV tier.
/// See the module docs for why we don't use chitchat's own `delete()`.
const TOMBSTONE_MARKER: &str = "TS:";

/// Current time in milliseconds since UNIX epoch.
fn now_epoch_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

/// Encode a binary value + optional TTL into the chitchat string format.
///
/// Emits the current three-field form `"{expiry_ms}:{write_ms}:{b64}"`.
/// `write_ms` is stamped at encode time so the applier on remote nodes
/// can order overlapping writes deterministically (last arrival wins).
fn encode_value(value: &[u8], ttl: Option<Duration>) -> String {
    let now_ms = now_epoch_ms();
    let expiry_ms = ttl.map_or(0u64, |d| {
        let ttl_ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        now_ms.saturating_add(ttl_ms)
    });
    let b64 = Base64::encode_string(value);
    format!("{expiry_ms}:{now_ms}:{b64}")
}

/// Encode a tombstone marker stamped with the origin `write_ms`.
///
/// Peers ordering the tombstone against a concurrent SET on the same key
/// need `write_ms` to decide who wins; this reuses the same per-key
/// last-arrival-wins ordering the SET applier already runs.
fn encode_tombstone() -> String {
    let now_ms = now_epoch_ms();
    format!("{TOMBSTONE_MARKER}{now_ms}")
}

/// Parse a tombstone-encoded payload. Returns the origin `write_ms` when
/// `encoded` is a well-formed tombstone marker, otherwise `None`.
fn decode_tombstone(encoded: &str) -> Option<u64> {
    let rest = encoded.strip_prefix(TOMBSTONE_MARKER)?;
    rest.parse::<u64>().ok()
}

/// Split an encoded value into (expiry_ms, write_ms, base64) while
/// accepting both the v1 two-field format and the v2 three-field format.
///
/// v1 entries are reported with `write_ms = 0` — the smallest possible
/// value, which lets the applier treat them as strictly older than any
/// current-format entry and always apply them once.
fn split_encoded(encoded: &str) -> Option<(u64, u64, &str)> {
    let (first, rest) = encoded.split_once(':')?;
    let expiry_ms: u64 = first.parse().ok()?;
    if let Some((second, tail)) = rest.split_once(':') {
        // Three-field form. `second` MUST parse as u64 for this to be a
        // v2 payload; if it doesn't, fall back to treating `rest` as
        // legacy base64 (unlikely — base64 alphabet has no ':' — but
        // symmetric with the accept-both contract).
        if let Ok(write_ms) = second.parse::<u64>() {
            return Some((expiry_ms, write_ms, tail));
        }
    }
    // v1 two-field form: no write_ms, `rest` is the base64 payload.
    Some((expiry_ms, 0, rest))
}

/// Decode a chitchat value string back to binary, checking TTL.
///
/// Accepts BOTH the v2 three-field format and the v1 two-field format —
/// necessary during a rolling upgrade where some peers still run the
/// pre-`write_ms` code path. Returns `None` if the entry has expired.
fn decode_value(encoded: &str) -> Option<Vec<u8>> {
    let (expiry_ms, _write_ms, b64) = split_encoded(encoded)?;

    if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
        return None; // expired
    }

    Base64::decode_vec(b64).ok()
}

/// Decode and also return the origin write time. Used by the gossip
/// applier for stale-write detection.
///
/// v1 legacy entries return `write_ms = 0`, which sorts before any
/// v2 timestamp — so a legacy entry always applies on first arrival.
fn decode_value_with_write_ms(encoded: &str) -> Option<(Vec<u8>, u64)> {
    let (expiry_ms, write_ms, b64) = split_encoded(encoded)?;
    if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
        return None;
    }
    let bytes = Base64::decode_vec(b64).ok()?;
    Some((bytes, write_ms))
}

/// Remaining TTL from an encoded value, in milliseconds. Accepts both
/// v1 and v2 wire formats (rolling-upgrade compatible).
///
/// Returns `None` if no TTL is set or the value format is invalid.
/// Returns `Some(0)` if already expired.
///
/// Retained for tests + rolling-upgrade documentation. The production
/// scanners in `gossip_get` / `gossip_pttl` compute the remaining TTL
/// inline so they can also handle tombstones and last-arrival-wins
/// ordering in the same pass.
#[cfg(test)]
fn remaining_ttl_ms(encoded: &str) -> Option<u64> {
    let (expiry_ms, _write_ms, _b64) = split_encoded(encoded)?;
    if expiry_ms == 0 {
        return None; // no TTL
    }
    Some(expiry_ms.saturating_sub(now_epoch_ms()))
}

impl ClusterHandle {
    /// Set a small key in the gossip KV tier.
    ///
    /// The value is stored in this node's chitchat state and replicated
    /// to all other nodes via gossip (typically within 1-3 seconds).
    pub async fn gossip_set(&self, key: &str, value: &[u8], ttl: Option<Duration>) {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let encoded = encode_value(value, ttl);
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        guard.self_node_state().set(chitchat_key, encoded);
    }

    /// Get a small key from the gossip KV tier.
    ///
    /// Searches this node's state first, then all other live nodes.
    /// Returns the first non-expired match found. If the newest visible
    /// entry (highest `write_ms`) is a tombstone, the key reads as
    /// deleted — a stale live copy on another node cannot resurrect a
    /// concurrent delete.
    pub async fn gossip_get(&self, key: &str) -> Option<Vec<u8>> {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;

        // Scan self + every live peer, pick the entry with the highest
        // origin write_ms (tombstones included). Same last-arrival-wins
        // rule the applier runs, applied at read time so nodes still
        // catching up show consistent results.
        let mut best: Option<(u64, Option<Vec<u8>>)> = None;
        let consider = |encoded: &str, best: &mut Option<(u64, Option<Vec<u8>>)>| {
            if let Some(ts_write_ms) = decode_tombstone(encoded) {
                if best.as_ref().is_none_or(|(seen, _)| ts_write_ms > *seen) {
                    *best = Some((ts_write_ms, None));
                }
                return;
            }
            if let Some((value, write_ms)) = decode_value_with_write_ms(encoded) {
                if best.as_ref().is_none_or(|(seen, _)| write_ms > *seen) {
                    *best = Some((write_ms, Some(value)));
                }
            }
        };

        if let Some(encoded) = guard.self_node_state().get(&chitchat_key) {
            consider(encoded, &mut best);
        }
        for node_id in guard.live_nodes().cloned().collect::<Vec<_>>() {
            if let Some(state) = guard.node_state(&node_id) {
                if let Some(encoded) = state.get(&chitchat_key) {
                    consider(encoded, &mut best);
                }
            }
        }
        best.and_then(|(_, value)| value)
    }

    /// Broadcast a delete for `key` across the gossip KV tier.
    ///
    /// Writes a tombstone marker (`"TS:{write_ms}"`) into this node's
    /// chitchat state so peers observe it via `subscribe_event`
    /// (chitchat's real `state.delete()` does **not** fire subscribers,
    /// so a plain delete would be invisible to other nodes' appliers).
    /// The applier on peer nodes calls `Store::remove_local` when the
    /// tombstone's `write_ms` beats their last-applied write for the key,
    /// which drops both the gossip-materialized copy and any locally-held
    /// data-plane replica of the same key. Returns `true` regardless of
    /// whether this node had previously set the key locally — the
    /// intent-to-delete has been broadcast either way.
    pub async fn gossip_del(&self, key: &str) -> bool {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let encoded = encode_tombstone();
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        guard.self_node_state().set(chitchat_key, encoded);
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

        // Same last-arrival-wins scan as gossip_get, but return the TTL
        // of the winning entry (None for tombstones and TTL-less values).
        let mut best: Option<(u64, Option<u64>)> = None;
        let consider = |encoded: &str, best: &mut Option<(u64, Option<u64>)>| {
            if let Some(ts_write_ms) = decode_tombstone(encoded) {
                if best.as_ref().is_none_or(|(seen, _)| ts_write_ms > *seen) {
                    *best = Some((ts_write_ms, None));
                }
                return;
            }
            if let Some((expiry_ms, write_ms, _b64)) = split_encoded(encoded) {
                if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
                    return; // already expired
                }
                let ttl = if expiry_ms == 0 {
                    None
                } else {
                    Some(expiry_ms.saturating_sub(now_epoch_ms()))
                };
                if best.as_ref().is_none_or(|(seen, _)| write_ms > *seen) {
                    *best = Some((write_ms, ttl));
                }
            }
        };
        if let Some(encoded) = guard.self_node_state().get(&chitchat_key) {
            consider(encoded, &mut best);
        }
        for node_id in guard.live_nodes().cloned().collect::<Vec<_>>() {
            if let Some(state) = guard.node_state(&node_id) {
                if let Some(encoded) = state.get(&chitchat_key) {
                    consider(encoded, &mut best);
                }
            }
        }
        best.and_then(|(_, ttl)| ttl)
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
    /// decoded value, remaining TTL, and origin `write_ms`) or a
    /// `Tombstone` (with just the origin `write_ms`). Peers use the
    /// tombstone variant to drop their local copies of a remotely-deleted
    /// key — see [`crate::clustered_store::start_gossip_applier`]. The
    /// `write_ms` is 0 for legacy pre-`write_ms` peers.
    pub async fn subscribe_kv_changes<F>(&self, callback: F)
    where
        F: Fn(KvChangeEvent<'_>) + Send + Sync + 'static,
    {
        let chitchat = self.handle.chitchat();
        let guard = chitchat.lock().await;
        guard
            .subscribe_event(KV_PREFIX, move |event| {
                // Tombstones first (unambiguous prefix).
                if let Some(write_ms) = decode_tombstone(event.value) {
                    callback(KvChangeEvent::Tombstone {
                        key: event.key,
                        write_ms,
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
                // a Tombstone (with the origin's `write_ms` for
                // last-arrival-wins) lets the applier drop the peer
                // copy on receipt.
                if let Some((expiry_ms, write_ms, b64)) = split_encoded(event.value) {
                    if expiry_ms > 0 && now_epoch_ms() >= expiry_ms {
                        callback(KvChangeEvent::Tombstone {
                            key: event.key,
                            write_ms,
                            node: event.node,
                        });
                        return;
                    }
                    if let Ok(value) = Base64::decode_vec(b64) {
                        let ttl = if expiry_ms == 0 {
                            None
                        } else {
                            Some(Duration::from_millis(expiry_ms.saturating_sub(now_epoch_ms())))
                        };
                        callback(KvChangeEvent::Set {
                            key: event.key,
                            value: &value,
                            ttl,
                            write_ms,
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
        /// Origin `write_ms` for last-arrival-wins ordering. `0` marks
        /// a legacy pre-`write_ms` peer's write.
        write_ms: u64,
        /// The node that produced the change.
        node: &'a ChitchatId,
    },
    /// A key was deleted on the origin node. Peers apply the delete
    /// locally when `write_ms` beats their last-applied write for
    /// `key` (see [`crate::clustered_store::start_gossip_applier`]).
    Tombstone {
        /// The affected key (without the `kv:` prefix).
        key: &'a str,
        /// Origin `write_ms` for last-arrival-wins ordering.
        write_ms: u64,
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

    /// The origin `write_ms`, common to both variants.
    #[must_use]
    pub fn write_ms(&self) -> u64 {
        match self {
            Self::Set { write_ms, .. } | Self::Tombstone { write_ms, .. } => *write_ms,
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

    #[test]
    fn encode_decode_no_ttl() {
        let value = b"hello world";
        let encoded = encode_value(value, None);
        // v2 wire form: "{expiry}:{write_ms}:{b64}", so the leading "0:"
        // (no TTL) is followed by another numeric colon-terminated field.
        assert!(encoded.starts_with("0:"));
        assert!(encoded.matches(':').count() >= 2);
        let decoded = decode_value(&encoded).expect("should decode");
        assert_eq!(decoded, value);
    }

    #[test]
    fn encode_decode_with_ttl() {
        let value = b"test data";
        let encoded = encode_value(value, Some(Duration::from_secs(3600)));
        // Expiry should be non-zero, and write_ms should also be non-zero.
        let (expiry, write_ms, _b64) = split_encoded(&encoded).unwrap();
        assert!(expiry > 0);
        assert!(write_ms > 0);
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
        // A pre-write_ms peer emits "{expiry}:{b64}" with no write_ms
        // slot. The applier still needs to accept those during a rolling
        // upgrade or data goes on the floor.
        let value = b"legacy peer wrote this";
        let b64 = Base64::encode_string(value);
        let legacy = format!("0:{b64}"); // no TTL, no write_ms
        let decoded = decode_value(&legacy).expect("legacy form should decode");
        assert_eq!(decoded, value);
        let (bytes, write_ms) =
            decode_value_with_write_ms(&legacy).expect("legacy form should split");
        assert_eq!(bytes, value);
        assert_eq!(write_ms, 0, "legacy entries report write_ms = 0");
        assert!(remaining_ttl_ms(&legacy).is_none());
    }

    #[test]
    fn decode_accepts_current_three_field_format() {
        // Round-trip through encode_value → decode_value_with_write_ms.
        let value = b"v2 write";
        let encoded = encode_value(value, None);
        let (bytes, write_ms) = decode_value_with_write_ms(&encoded).expect("v2 should decode");
        assert_eq!(bytes, value);
        assert!(write_ms > 0, "v2 entries carry a real write_ms");
    }

    #[test]
    fn remaining_ttl_no_expiry() {
        let encoded = encode_value(b"x", None);
        assert!(remaining_ttl_ms(&encoded).is_none());
    }

    #[test]
    fn remaining_ttl_future() {
        let encoded = encode_value(b"x", Some(Duration::from_secs(60)));
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
    fn tombstone_roundtrip_carries_write_ms() {
        let encoded = encode_tombstone();
        let write_ms = decode_tombstone(&encoded).expect("tombstone must decode");
        assert!(write_ms > 0, "tombstone must be stamped with a real write_ms");
        // decode_value must NOT treat the tombstone as a value — it has
        // no colons after "TS:" so split_encoded returns None.
        assert!(decode_value(&encoded).is_none(), "tombstone must not decode as a value");
        assert!(decode_value_with_write_ms(&encoded).is_none());
        assert!(remaining_ttl_ms(&encoded).is_none());
    }

    #[test]
    fn tombstone_marker_is_unambiguous() {
        // A legitimate v1 or v2 value's leading field is always a numeric
        // expiry_ms — decode_tombstone must reject those.
        let v2 = encode_value(b"real value", None);
        assert!(decode_tombstone(&v2).is_none(), "v2 payloads are not tombstones");
        let v1 = format!("0:{}", Base64::encode_string(b"legacy"));
        assert!(decode_tombstone(&v1).is_none(), "v1 payloads are not tombstones");
        // And a garbage-tombstone (bad write_ms) is rejected.
        assert!(decode_tombstone("TS:not-a-number").is_none());
        assert!(decode_tombstone("TS:").is_none());
    }

    #[test]
    fn binary_data_roundtrip() {
        // Test with arbitrary binary data including null bytes.
        let value: Vec<u8> = (0..=255).collect();
        let encoded = encode_value(&value, None);
        let decoded = decode_value(&encoded).expect("should decode");
        assert_eq!(decoded, value);
    }
}

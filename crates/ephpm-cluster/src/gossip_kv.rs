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
//! **Legacy format compatibility.** The v1 format was
//! `"{expiry_ms}:{base64_value}"` (no `write_ms`). `decode_value`,
//! `remaining_ttl_ms`, and the subscription decoder still accept it so a
//! rolling upgrade from a pre-`write_ms` peer does not drop data on the
//! floor. Legacy entries are treated as `write_ms = 0`, which means the
//! applier will always apply them (they are strictly older than any
//! current-format entry). Only encode is one-way: new writes always
//! emit the three-field form.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64ct::{Base64, Encoding};
use chitchat::ChitchatId;

use crate::ClusterHandle;

/// Key prefix for KV entries in chitchat state.
const KV_PREFIX: &str = "kv:";

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
    /// Returns the first non-expired match found.
    pub async fn gossip_get(&self, key: &str) -> Option<Vec<u8>> {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;

        // Check self first (fastest path).
        if let Some(encoded) = guard.self_node_state().get(&chitchat_key) {
            if let Some(value) = decode_value(encoded) {
                return Some(value);
            }
        }
        // Check all other live nodes.
        for node_id in guard.live_nodes().cloned().collect::<Vec<_>>() {
            if let Some(state) = guard.node_state(&node_id) {
                if let Some(encoded) = state.get(&chitchat_key) {
                    if let Some(value) = decode_value(encoded) {
                        return Some(value);
                    }
                }
            }
        }
        None
    }

    /// Delete a small key from this node's gossip state.
    ///
    /// Only deletes the key if this node owns it (i.e., this node
    /// originally set it). Deletion propagates via gossip.
    pub async fn gossip_del(&self, key: &str) -> bool {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;
        let state = guard.self_node_state();
        if state.get(&chitchat_key).is_some() {
            state.delete(&chitchat_key);
            true
        } else {
            false
        }
    }

    /// Check if a small key exists in the gossip KV tier (not expired).
    pub async fn gossip_exists(&self, key: &str) -> bool {
        self.gossip_get(key).await.is_some()
    }

    /// Get the remaining TTL of a gossip key in milliseconds.
    ///
    /// Returns `None` if the key has no TTL or does not exist.
    pub async fn gossip_pttl(&self, key: &str) -> Option<u64> {
        let chitchat_key = format!("{KV_PREFIX}{key}");
        let chitchat = self.handle.chitchat();
        let mut guard = chitchat.lock().await;

        // Check self first.
        if let Some(encoded) = guard.self_node_state().get(&chitchat_key) {
            return remaining_ttl_ms(encoded);
        }
        // Check live nodes.
        for node_id in guard.live_nodes().cloned().collect::<Vec<_>>() {
            if let Some(state) = guard.node_state(&node_id) {
                if let Some(encoded) = state.get(&chitchat_key) {
                    return remaining_ttl_ms(encoded);
                }
            }
        }
        None
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
    /// The callback receives the key (without the `kv:` prefix), the
    /// decoded value, the remaining TTL (when the entry carries one),
    /// the origin `write_ms` stamped by the writer (0 for pre-`write_ms`
    /// legacy entries), and the node that changed it. Used by the
    /// gossip→local-store applier and hot-key invalidation notifications.
    pub async fn subscribe_kv_changes<F>(&self, callback: F)
    where
        F: Fn(&str, &[u8], Option<Duration>, u64, &ChitchatId) + Send + Sync + 'static,
    {
        let chitchat = self.handle.chitchat();
        let guard = chitchat.lock().await;
        guard
            .subscribe_event(KV_PREFIX, move |event| {
                if let Some((value, write_ms)) = decode_value_with_write_ms(event.value) {
                    let ttl = remaining_ttl_ms(event.value)
                        .filter(|ms| *ms > 0)
                        .map(Duration::from_millis);
                    callback(event.key, &value, ttl, write_ms, event.node);
                }
            })
            .forever();
    }
}

/// Helper: collect non-expired KV keys from a node state.
fn collect_kv_keys(
    state: &chitchat::NodeState,
    keys: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    for (key, value) in state.key_values() {
        if let Some(stripped) = key.strip_prefix(KV_PREFIX) {
            if !seen.contains(stripped) && decode_value(value).is_some() {
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
    fn binary_data_roundtrip() {
        // Test with arbitrary binary data including null bytes.
        let value: Vec<u8> = (0..=255).collect();
        let encoded = encode_value(&value, None);
        let decoded = decode_value(&encoded).expect("should decode");
        assert_eq!(decoded, value);
    }
}

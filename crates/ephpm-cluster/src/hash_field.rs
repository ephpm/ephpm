//! Transport encoding for replicated **hash fields**.
//!
//! # Why per field
//!
//! The KV replication tiers (gossip and the data plane) carry flat
//! `key -> value` items, each ordered by its own [`Stamp`](crate::Stamp).
//! A hash is replicated as one such item **per field**, so two nodes writing
//! different fields of one hash concurrently both win — last-writer-wins is
//! decided per field. Replicating the whole hash as one value would make the
//! later of two concurrent `HSET`s to *different* fields silently erase the
//! other's field cluster-wide.
//!
//! Every field item rides the **gossip** tier, which already reaches every
//! node and has anti-entropy, so hash reads (`HGET`, `HGETALL`, ...) are
//! answered locally on every node once it has caught up with its peers. A
//! node still catching up after a (re)start resolves the named fields through
//! its peers first (the data plane's stamped read answers a field's transport
//! key from the hash), and refuses whole-hash commands until it has caught up
//! (see `Replicator::resolve_hash_fields` / `resolve_hash_whole`). A field value too large for a gossip
//! payload is refused at write time (see
//! [`ephpm_kv::store::HashWriteError::TooLargeForCluster`]) rather than
//! stored on one node only.
//!
//! # Key encoding
//!
//! ```text
//!   \x1e h <decimal byte length of key> : <key> <field>
//! ```
//!
//! [`MARK`] is ASCII **Record Separator** (`0x1E`), followed by the tag `h`.
//! The key's length is explicit, so any key and field — including ones that
//! contain `\x1e`, `:` or digits — round-trip unambiguously. Composed with the
//! per-site envelope, the site envelope is outermost:
//! `\x1f<site>\x1f\x1eh<len>:<key><field>`.
//!
//! As with the site envelope, global string keys are not re-encoded, so a
//! string key that literally has this exact shape would be read on peers as a
//! hash field. Keys beginning with `\x1e` are **reserved** in clustered mode.
//!
//! # Value encoding
//!
//! ```text
//!   [VERSION u8 = 1][key expiry: Unix ms, u64 BE][field expiry: Unix ms, u64 BE][value]
//! ```
//!
//! `0` = no expiry. The key (whole-hash) expiry travels on every field, so a
//! peer learns it from whichever field reaches it first — including a node
//! that joins later and receives the fields in any order. The gossip item's
//! own expiry is the earlier of the two, so the writer's sweep reclaims a
//! field's gossip entry once either has passed, and a peer that receives an
//! already-expired item gets it as a delete.
//!
//! # Mixed-version clusters
//!
//! A node predating hash replication receives these items as ordinary string
//! keys under the encoded name. It never reads them (they are unreachable
//! through any command), but they occupy memory there until the next restart
//! or `FLUSHALL`; finish a rolling upgrade before relying on clustered
//! hashes.

use std::time::Duration;

use ephpm_kv::store::HashFieldState;

/// First byte of an encoded hash-field transport key: ASCII Record Separator.
pub const MARK: char = '\u{1e}';

/// Tag byte after [`MARK`] identifying a hash field.
const TAG: char = 'h';

/// Value header version.
const VERSION: u8 = 1;

/// Bytes the value header adds in front of the field value.
pub const HEADER_LEN: usize = 1 + 8 + 8;

/// Encode the transport key for `field` of hash `key`.
#[must_use]
pub fn encode_key(key: &str, field: &str) -> String {
    let len = key.len().to_string();
    let mut out = String::with_capacity(2 + len.len() + 1 + key.len() + field.len());
    out.push(MARK);
    out.push(TAG);
    out.push_str(&len);
    out.push(':');
    out.push_str(key);
    out.push_str(field);
    out
}

/// Decode a transport key into `(hash key, field)`, or `None` when it is not
/// a well-formed hash-field key.
#[must_use]
pub fn decode_key(transport: &str) -> Option<(&str, &str)> {
    let rest = transport.strip_prefix(MARK)?.strip_prefix(TAG)?;
    let colon = rest.find(':')?;
    let digits = &rest[..colon];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // No leading zeros: one canonical encoding per key.
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    let len: usize = digits.parse().ok()?;
    let body = &rest[colon + 1..];
    if len > body.len() || !body.is_char_boundary(len) {
        return None;
    }
    Some(body.split_at(len))
}

/// Encode a field's replicated value: the header and the bytes. Returns the
/// payload and the gossip item's absolute expiry (`0` = none) — the earlier
/// of the field's and the key's.
#[must_use]
pub fn encode_value(
    value: &[u8],
    field_ttl: Option<Duration>,
    hash_ttl: Option<Duration>,
) -> (Vec<u8>, u64) {
    let field_ms = crate::gossip_kv::expiry_ms_for(field_ttl);
    let hash_ms = crate::gossip_kv::expiry_ms_for(hash_ttl);
    let mut out = Vec::with_capacity(HEADER_LEN + value.len());
    out.push(VERSION);
    out.extend_from_slice(&hash_ms.to_be_bytes());
    out.extend_from_slice(&field_ms.to_be_bytes());
    out.extend_from_slice(value);
    let item_ms = match (field_ms, hash_ms) {
        (0, h) => h,
        (f, 0) => f,
        (f, h) => f.min(h),
    };
    (out, item_ms)
}

/// Decode a replicated field value into the state to apply, or `None` for a
/// payload this node does not understand (a newer version, or truncated).
/// Expiries already in the past decode as a zero remaining TTL, which the
/// store applies as a delete.
#[must_use]
pub fn decode_value(payload: &[u8]) -> Option<HashFieldState> {
    let (&version, rest) = payload.split_first()?;
    if version != VERSION || rest.len() < 16 {
        return None;
    }
    let hash_ms = u64::from_be_bytes(rest[..8].try_into().ok()?);
    let field_ms = u64::from_be_bytes(rest[8..16].try_into().ok()?);
    let remaining = |ms: u64| {
        (ms > 0).then(|| Duration::from_millis(ms.saturating_sub(crate::gossip_kv::now_epoch_ms())))
    };
    Some(HashFieldState {
        value: Some(rest[16..].to_vec()),
        field_ttl: remaining(field_ms),
        hash_ttl: remaining(hash_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_whatever_they_contain() {
        for (key, field) in [
            ("h", "f"),
            ("", ""),
            ("user:1", "name"),
            ("a\u{1e}h3:b", "12:x"),
            ("123", "456"),
            ("ключ", "поле"),
            ("k", ""),
        ] {
            let encoded = encode_key(key, field);
            assert_eq!(decode_key(&encoded), Some((key, field)), "{key:?}/{field:?}");
        }
    }

    #[test]
    fn foreign_keys_do_not_decode() {
        for k in [
            "plain",
            "\u{1e}",
            "\u{1e}h",
            "\u{1e}h:",
            "\u{1e}hx:a",
            "\u{1e}h05:abcde",
            "\u{1e}h9:short",
            "\u{1e}x1:ab",
        ] {
            assert_eq!(decode_key(k), None, "{k:?}");
        }
        // A length that would split a multi-byte character is refused.
        assert_eq!(decode_key("\u{1e}h1:é"), None);
    }

    #[test]
    fn values_round_trip_with_both_expiries() {
        let (payload, item_ms) =
            encode_value(b"v", Some(Duration::from_secs(50)), Some(Duration::from_secs(100)));
        assert_eq!(payload.len(), HEADER_LEN + 1);
        let now = crate::gossip_kv::now_epoch_ms();
        assert!(item_ms > now + 49_000 && item_ms <= now + 50_000, "the earlier expiry");
        let state = decode_value(&payload).unwrap();
        assert_eq!(state.value.as_deref(), Some(&b"v"[..]));
        assert!(state.field_ttl.unwrap() > Duration::from_secs(49));
        assert!(state.hash_ttl.unwrap() > Duration::from_secs(99));

        let (payload, item_ms) = encode_value(b"", None, None);
        assert_eq!(item_ms, 0);
        let state = decode_value(&payload).unwrap();
        assert_eq!((state.field_ttl, state.hash_ttl), (None, None));
        assert_eq!(decode_value(&[2, 0, 0]), None);
        assert_eq!(decode_value(&[1, 0]), None);
    }
}

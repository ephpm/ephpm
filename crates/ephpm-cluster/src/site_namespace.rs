//! Transport-key namespacing for per-vhost (multi-tenant) KV replication.
//!
//! # Why
//!
//! In multi-tenant mode (`[server] sites_dir`) every vhost gets its **own**
//! [`Store`](ephpm_kv::store::Store) — a separate `DashMap`, not a prefix in a
//! shared map, so tenant A's keys are physically unreachable from tenant B.
//! That isolation is what makes the per-site keyspace safe, but the cluster
//! transports (chitchat gossip and the KV data plane) are flat: they carry a
//! single `key -> value` namespace with no notion of which tenant a key
//! belongs to.
//!
//! This module supplies the missing dimension. A per-site write is put on the
//! wire under an **envelope key** that names its site, and the receiving node
//! decodes that envelope to route the write into *that site's* store. The
//! global (non-tenant) keyspace keeps riding the wire exactly as before.
//!
//! # Encoding
//!
//! ```text
//!   global key   :  <key>                       (unchanged, byte-for-byte)
//!   per-site key :  \x1f <site> \x1f <key>
//! ```
//!
//! [`SEP`] is ASCII **Unit Separator** (`0x1F`) — a C0 control character whose
//! entire purpose in the ASCII standard is delimiting fields inside a record.
//! It is chosen because:
//!
//! * **It cannot appear in a site key.** Site keys are the validated vhost
//!   directory names (`[a-z0-9._-]`, see [`is_valid_site_key`]), so the
//!   separator can never be ambiguous *within* the envelope — the first `\x1f`
//!   after the leading one always ends the site field. Two sites therefore can
//!   never decode to each other.
//! * **It does not occur in practice in real keys.** Application keys come from
//!   PHP (`ephpm_kv_*`), Predis/RESP, and the session/cache drop-ins; all of
//!   them use printable text (`user:42`, `PHPREDIS_SESSION:…`, `wp:options`).
//!   A non-printable control byte is not something a key generator emits.
//!
//! # The one reserved shape (documented, not silent)
//!
//! Because global keys are deliberately **not** re-encoded (the global wire
//! format has to stay byte-for-byte identical), the leading `\x1f` is a
//! *reservation* rather than a proof: a global key that literally begins with
//! `\x1f`, followed by a valid site key, followed by another `\x1f`, would
//! decode as a per-site key and be routed into that site's store. Every other
//! global key — including one containing `\x1f` anywhere but position 0, or one
//! starting with `\x1f` whose next field is not a valid site key — is
//! unambiguous and routes globally. Keys beginning with `\x1f` are therefore
//! **reserved** for this envelope and must not be used by applications.
//!
//! # Tier rules are unchanged
//!
//! The small/large tier split compares **`value.len()`** against
//! `[cluster.kv] small_key_threshold` — it does not consider the key — so
//! wrapping a key in this envelope does **not** shrink the effective payload
//! budget. The envelope does add `2 + site.len()` bytes to the key on the wire
//! (a few dozen bytes of gossip/data-plane overhead per key).

/// Field separator for the per-site envelope: ASCII Unit Separator (`0x1F`).
///
/// Also the marker byte at position 0 that distinguishes an enveloped key from
/// a global one. See the module docs for why this byte.
pub const SEP: char = '\u{1f}';

/// Maximum accepted site-key length, matching `ephpm-server`'s router rule
/// (DNS names cap at 253; 255 leaves a little slack). Without this a peer
/// could name a store with a 64 KiB key — the transport's own key cap — and
/// the name is attacker-chosen.
const MAX_SITE_KEY_LEN: usize = 255;

/// Is `site` a valid site key — the same rule the router validates a vhost
/// directory name against (`[a-z0-9._-]`, non-empty, `<= 255` bytes, and no
/// empty dot-label)?
///
/// Duplicated here (rather than depending on `ephpm-server`, which depends on
/// *this* crate) so the decoder can reject a malformed or hostile site field
/// before it is ever used to name a store. A site key can never contain
/// [`SEP`], which is what makes the envelope unambiguous.
///
/// The empty-label rule (which rejects `..`, a leading dot and a trailing dot)
/// is not needed for safety *here* — a KV site key names an in-memory store and
/// is never joined to a path — but keeping the two predicates identical is what
/// lets the rest of the system reason about one site-key notion instead of two.
#[must_use]
pub fn is_valid_site_key(site: &str) -> bool {
    if site.is_empty() || site.len() > MAX_SITE_KEY_LEN {
        return false;
    }
    if !site
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    {
        return false;
    }
    // An empty label means a leading dot, a trailing dot, or a `..` segment.
    !site.split('.').any(str::is_empty)
}

/// Does `transport_key` *claim* to be a per-site envelope (i.e. does it open
/// with [`SEP`])?
///
/// [`decode`] deliberately collapses "plain global key" and "malformed
/// envelope" into `None`, because both are non-routable as a site. Callers that
/// must fail **closed** — the replication apply paths, which would otherwise
/// flatten an undecodable tenant key into the shared global keyspace — use this
/// to tell the two apart and drop the malformed case.
#[must_use]
pub fn is_enveloped(transport_key: &str) -> bool {
    transport_key.starts_with(SEP)
}

/// Wrap `key` in the per-site envelope for `site`: `\x1f<site>\x1f<key>`.
///
/// The caller is responsible for `site` being a valid site key. An invalid one
/// produces an envelope that [`decode`] refuses; the replication apply paths
/// pair that with [`is_enveloped`] to drop it, so the write routes nowhere
/// rather than into a wrong store or into the shared global keyspace.
#[must_use]
pub fn encode(site: &str, key: &str) -> String {
    let mut out = String::with_capacity(2 + site.len() + key.len());
    out.push(SEP);
    out.push_str(site);
    out.push(SEP);
    out.push_str(key);
    out
}

/// Split a transport key back into `(site, key)`, or `None` when it is a
/// global (non-enveloped) key.
///
/// Returns `None` unless the key begins with [`SEP`], has a second [`SEP`],
/// and the field between them is a valid site key.
///
/// `None` is deliberately ambiguous between "plain global key" and "malformed
/// envelope", because neither is routable *as a site*. Callers that must fail
/// closed distinguish the two with [`is_enveloped`]: a key that never claimed
/// a site routes to the global store, while a key that claimed one and failed
/// to decode is dropped rather than flattened into the shared keyspace.
#[must_use]
pub fn decode(transport_key: &str) -> Option<(&str, &str)> {
    let rest = transport_key.strip_prefix(SEP)?;
    let (site, key) = rest.split_once(SEP)?;
    if is_valid_site_key(site) { Some((site, key)) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_site_and_key() {
        let t = encode("blog.example.com", "user:42");
        assert_eq!(decode(&t), Some(("blog.example.com", "user:42")));
    }

    #[test]
    fn global_keys_are_not_enveloped_and_decode_to_none() {
        // The global wire format is unchanged: an ordinary key decodes as
        // "not a site key" and therefore routes to the global store.
        for key in ["user:42", "PHPREDIS_SESSION:abc", "", "a:b:c", "hot:12345"] {
            assert_eq!(decode(key), None, "global key {key:?} must not decode as per-site");
        }
    }

    #[test]
    fn a_key_may_contain_the_separator_after_position_zero() {
        // Only a LEADING separator opens an envelope, so an interior 0x1f is
        // just a byte in a global key.
        let key = format!("weird{SEP}key");
        assert_eq!(decode(&key), None);
    }

    #[test]
    fn malformed_envelopes_fall_back_to_global() {
        // Leading SEP but no second SEP → not an envelope.
        assert_eq!(decode(&format!("{SEP}no-second-sep")), None);
        // Empty site field → invalid site key.
        assert_eq!(decode(&format!("{SEP}{SEP}key")), None);
        // Uppercase / slash / traversal are not valid site keys, so an
        // attacker-shaped envelope cannot name an arbitrary store.
        assert_eq!(decode(&format!("{SEP}NotASite{SEP}k")), None);
        assert_eq!(decode(&format!("{SEP}../etc{SEP}k")), None);
        assert_eq!(decode(&format!("{SEP}a/b{SEP}k")), None);
    }

    #[test]
    fn two_sites_never_decode_to_each_other() {
        let a = encode("alice.test", "shared-key-name");
        let b = encode("bob.test", "shared-key-name");
        assert_ne!(a, b, "the same key in two sites must differ on the wire");
        assert_eq!(decode(&a).unwrap().0, "alice.test");
        assert_eq!(decode(&b).unwrap().0, "bob.test");
        // ...and both carry the same inner key.
        assert_eq!(decode(&a).unwrap().1, decode(&b).unwrap().1);
    }

    #[test]
    fn site_key_cannot_contain_the_separator_so_the_split_is_unambiguous() {
        assert!(!is_valid_site_key(&format!("a{SEP}b")));
        // An inner key containing a separator still round-trips, because only
        // the FIRST separator after the leading one splits the fields.
        let inner = format!("k{SEP}with{SEP}seps");
        let t = encode("site.test", &inner);
        assert_eq!(decode(&t), Some(("site.test", inner.as_str())));
    }

    #[test]
    fn site_key_charset_matches_the_router_rule() {
        assert!(is_valid_site_key("blog.example.com"));
        assert!(is_valid_site_key("a-b_c.1"));
        assert!(!is_valid_site_key(""));
        assert!(!is_valid_site_key("UPPER"));
        assert!(!is_valid_site_key("has space"));
        assert!(!is_valid_site_key("a/b"));
    }

    #[test]
    fn site_key_rejects_empty_labels_and_overlong_names() {
        // Empty dot-labels, matching the router: these are what `..`, a
        // leading dot and a trailing dot all reduce to.
        assert!(!is_valid_site_key(".."));
        assert!(!is_valid_site_key("."));
        assert!(!is_valid_site_key(".leading"));
        assert!(!is_valid_site_key("trailing."));
        assert!(!is_valid_site_key("a..b"));
        // Length cap: the site name is attacker-chosen and becomes a map key.
        assert!(is_valid_site_key(&"a".repeat(MAX_SITE_KEY_LEN)));
        assert!(!is_valid_site_key(&"a".repeat(MAX_SITE_KEY_LEN + 1)));
    }

    #[test]
    fn is_enveloped_separates_global_keys_from_malformed_envelopes() {
        // A plain global key never claimed a site: safe to route globally.
        assert!(!is_enveloped("user:42"));
        assert!(!is_enveloped(""));
        assert!(!is_enveloped(&format!("weird{SEP}key")));
        // These all CLAIM a site and fail to decode. `decode` returns None for
        // each, so only `is_enveloped` can tell the apply path to drop them
        // instead of flattening them into the global keyspace.
        for hostile in [
            format!("{SEP}no-second-sep"),
            format!("{SEP}{SEP}key"),
            format!("{SEP}NotASite{SEP}k"),
            format!("{SEP}../etc{SEP}k"),
            format!("{SEP}..{SEP}k"),
            format!("{SEP}{}{SEP}k", "a".repeat(MAX_SITE_KEY_LEN + 1)),
        ] {
            assert!(is_enveloped(&hostile), "{hostile:?} claims a site");
            assert_eq!(decode(&hostile), None, "{hostile:?} must not decode");
        }
        // A well-formed envelope is both enveloped and decodable.
        let good = encode("blog.example.com", "k");
        assert!(is_enveloped(&good));
        assert!(decode(&good).is_some());
    }
}

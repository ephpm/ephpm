//! HS256 token minting, plus the two constant-time comparisons this module
//! needs.
//!
//! # Format, and who owns it
//!
//! The session this module issues is a **compact HS256 JWT** — the same shape
//! the in-tree `jwt` builtin (`crates/ephpm-middleware-builtins/src/jwt.rs`)
//! already verifies, down to `alg` pinning and a mandatory `exp`. That is not
//! a coincidence and not a new invention: the hot-path verifier is a separate
//! module, this one only *issues*, and picking the format the workspace
//! already verifies is what keeps the two from diverging.
//!
//! Nothing here verifies a session token. Reading the session cookie is the
//! verifier's job; see the crate docs for how the two modules compose.
//!
//! # Why it is self-contained
//!
//! No server-side session record exists, anywhere. The token carries its own
//! subject, its own expiry and its own audience, and a restart or a second
//! node changes nothing — there is no store to lose, replicate or evict. It
//! also keeps the module clear of the process-global (not per-site)
//! middleware KV surface tracked in ephpm#376.
//!
//! The direct consequence, which belongs in the operator's head and not just
//! in a footnote: **an issued token cannot be revoked before it expires.**
//! Session lifetime is therefore the blast radius of a stolen cookie, which
//! is why `session_ttl_secs` defaults to eight hours rather than a week.
//!
//! # Key separation
//!
//! The OAuth `state` cookie is signed with a key *derived* from the session
//! secret rather than the secret itself
//! ([`derive_key`]). A `state` token and a session token can then never be
//! swapped for one another even though both are HS256 and both are minted
//! here — forging one from the other would require inverting HMAC.

use base64ct::{Base64UrlUnpadded, Encoding as _};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;

/// Fixed JOSE header. `alg` is pinned so a verifier can reject anything else
/// (the `alg: none` family of attacks).
const HEADER_JSON: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// Domain-separation label for the OAuth `state` signing key.
pub const STATE_KEY_LABEL: &[u8] = b"ephpm-github-auth/oauth-state/v1";

/// Derive a subkey from the session secret for a specific purpose.
///
/// `HMAC-SHA256(secret, label)`. Used so the `state` cookie and the session
/// cookie are signed under keys that cannot be substituted for each other.
#[must_use]
pub fn derive_key(secret: &[u8], label: &[u8]) -> Vec<u8> {
    let mut mac = new_mac(secret);
    mac.update(label);
    mac.finalize().into_bytes().to_vec()
}

/// `Hmac<Sha256>` over an arbitrary-length key.
///
/// `new_from_slice` is documented as infallible for HMAC (any key length is
/// accepted, being hashed down when over-long), so the `expect` is a type
/// artefact rather than a reachable path.
fn new_mac(key: &[u8]) -> Hmac<Sha256> {
    Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length")
}

/// Mint a compact HS256 JWT carrying `claims`.
///
/// `claims` must be a JSON object; the caller is responsible for including
/// `exp`. Returns `None` only if `claims` cannot be serialised, which for a
/// `serde_json::Value` built in-process does not happen.
#[must_use]
pub fn mint(secret: &[u8], claims: &serde_json::Value) -> Option<String> {
    let payload = serde_json::to_vec(claims).ok()?;
    let header_b64 = Base64UrlUnpadded::encode_string(HEADER_JSON.as_bytes());
    let payload_b64 = Base64UrlUnpadded::encode_string(&payload);

    let mut mac = new_mac(secret);
    mac.update(header_b64.as_bytes());
    mac.update(b".");
    mac.update(payload_b64.as_bytes());
    let sig = Base64UrlUnpadded::encode_string(&mac.finalize().into_bytes());

    Some(format!("{header_b64}.{payload_b64}.{sig}"))
}

/// Verify a token minted by [`mint`] and return its claims.
///
/// **Only used for this module's own OAuth `state` cookie.** The session
/// cookie is verified by the separate hot-path module, never here.
///
/// The signature is checked *before* the payload is parsed, so no
/// unauthenticated JSON is ever deserialised, and `exp` is mandatory.
#[must_use]
pub fn verify(secret: &[u8], token: &str, now: u64) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (header_b64, payload_b64, sig_b64) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }

    let sig = Base64UrlUnpadded::decode_vec(sig_b64).ok()?;
    let mut mac = new_mac(secret);
    mac.update(header_b64.as_bytes());
    mac.update(b".");
    mac.update(payload_b64.as_bytes());
    // Constant-time: `verify_slice` compares through `subtle::ConstantTimeEq`.
    mac.verify_slice(&sig).ok()?;

    // The MAC is ours, but still pin the algorithm.
    let header: serde_json::Value =
        serde_json::from_slice(&Base64UrlUnpadded::decode_vec(header_b64).ok()?).ok()?;
    if header.get("alg").and_then(serde_json::Value::as_str) != Some("HS256") {
        return None;
    }

    let claims: serde_json::Value =
        serde_json::from_slice(&Base64UrlUnpadded::decode_vec(payload_b64).ok()?).ok()?;
    let exp = claims.get("exp").and_then(serde_json::Value::as_u64)?;
    if exp <= now {
        return None;
    }
    Some(claims)
}

/// Constant-time equality for two secrets of arbitrary, possibly differing,
/// length.
///
/// Comparing the HMACs rather than the bytes is what makes the timing
/// independent of both the contents *and* the length of `presented` — a plain
/// `==` on `&str` leaks the length of the shared secret and the position of
/// the first differing byte, which is enough to walk a token out of a server
/// one character at a time.
///
/// `key` only needs to be unpredictable to the attacker; the module passes
/// the derived state key.
#[must_use]
pub fn constant_time_eq(key: &[u8], presented: &[u8], expected: &[u8]) -> bool {
    let mut a = new_mac(key);
    a.update(presented);
    let mut b = new_mac(key);
    b.update(expected);
    a.verify_slice(&b.finalize().into_bytes()).is_ok()
}

/// Fill `out` with cryptographically secure random bytes.
///
/// # Panics
///
/// Panics if the OS entropy source fails. That is not a recoverable
/// condition for a module whose whole job is issuing credentials, and
/// `declare!` converts the panic into a fail-closed `500` rather than a
/// login with a predictable `state`.
pub fn random_bytes(out: &mut [u8]) {
    getrandom::fill(out).expect("OS entropy source unavailable");
}

/// 32 bytes of entropy, base64url-encoded — the OAuth `state` nonce.
#[must_use]
pub fn random_nonce() -> String {
    let mut buf = [0u8; 32];
    random_bytes(&mut buf);
    Base64UrlUnpadded::encode_string(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a-test-secret-at-least-32-bytes-long!!";

    fn claims(exp: u64) -> serde_json::Value {
        serde_json::json!({ "sub": "octocat", "exp": exp })
    }

    #[test]
    fn round_trip() {
        let t = mint(SECRET, &claims(2_000)).expect("mint");
        let got = verify(SECRET, &t, 1_000).expect("verify");
        assert_eq!(got["sub"], "octocat");
    }

    #[test]
    fn minted_token_has_the_jwt_shape_the_builtin_verifier_expects() {
        let t = mint(SECRET, &claims(2_000)).expect("mint");
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3, "compact JWS serialisation");
        let header = Base64UrlUnpadded::decode_vec(parts[0]).expect("header b64url");
        let header: serde_json::Value = serde_json::from_slice(&header).expect("header json");
        assert_eq!(header["alg"], "HS256");
        assert_eq!(header["typ"], "JWT");
        // Unpadded base64url: no '=' anywhere, and no '+' or '/'.
        assert!(!t.contains('='), "padding would break cookie and URL use");
        assert!(!t.contains('+') && !t.contains('/'));
    }

    #[test]
    fn wrong_secret_fails() {
        let t = mint(SECRET, &claims(2_000)).expect("mint");
        assert!(verify(b"different-secret", &t, 1_000).is_none());
    }

    #[test]
    fn tampered_payload_fails() {
        let t = mint(SECRET, &claims(2_000)).expect("mint");
        let mut parts: Vec<String> = t.split('.').map(str::to_owned).collect();
        parts[1] = Base64UrlUnpadded::encode_string(br#"{"sub":"admin","exp":2000}"#);
        assert!(verify(SECRET, &parts.join("."), 1_000).is_none());
    }

    #[test]
    fn expired_and_missing_exp_fail() {
        let t = mint(SECRET, &claims(1_000)).expect("mint");
        assert!(verify(SECRET, &t, 1_000).is_none(), "exp == now is expired");
        assert!(verify(SECRET, &t, 5_000).is_none());
        let no_exp = mint(SECRET, &serde_json::json!({ "sub": "x" })).expect("mint");
        assert!(verify(SECRET, &no_exp, 1_000).is_none(), "exp is mandatory");
    }

    #[test]
    fn alg_none_is_rejected_even_with_a_valid_mac() {
        // Forge a token through the same MAC path but with `alg: none`.
        let header_b64 = Base64UrlUnpadded::encode_string(br#"{"alg":"none"}"#);
        let payload_b64 = Base64UrlUnpadded::encode_string(br#"{"sub":"x","exp":2000}"#);
        let mut mac = new_mac(SECRET);
        mac.update(header_b64.as_bytes());
        mac.update(b".");
        mac.update(payload_b64.as_bytes());
        let sig = Base64UrlUnpadded::encode_string(&mac.finalize().into_bytes());
        let forged = format!("{header_b64}.{payload_b64}.{sig}");
        assert!(verify(SECRET, &forged, 1_000).is_none());
    }

    #[test]
    fn malformed_tokens_fail() {
        for bad in ["", "a", "a.b", "a.b.c.d", "....", "not-base64!.x.y"] {
            assert!(verify(SECRET, bad, 1_000).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn derived_keys_are_domain_separated() {
        let state_key = derive_key(SECRET, STATE_KEY_LABEL);
        assert_ne!(state_key.as_slice(), SECRET, "the state key is not the session key");
        // A token signed for one domain must not verify under the other.
        let t = mint(&state_key, &claims(2_000)).expect("mint");
        assert!(verify(SECRET, &t, 1_000).is_none());
        let s = mint(SECRET, &claims(2_000)).expect("mint");
        assert!(verify(&state_key, &s, 1_000).is_none());
    }

    #[test]
    fn constant_time_eq_matches_semantics_of_equality() {
        let k = b"key";
        assert!(constant_time_eq(k, b"abc", b"abc"));
        assert!(!constant_time_eq(k, b"abc", b"abd"));
        assert!(!constant_time_eq(k, b"abc", b"abcd"), "length differences are unequal");
        assert!(!constant_time_eq(k, b"", b"abc"));
        assert!(constant_time_eq(k, b"", b""));
    }

    #[test]
    fn nonces_are_unique_and_url_safe() {
        let a = random_nonce();
        let b = random_nonce();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43, "32 bytes, unpadded base64url");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}

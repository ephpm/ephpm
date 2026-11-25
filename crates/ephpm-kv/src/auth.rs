//! HMAC-derived per-site RESP authentication.
//!
//! In multi-tenant deployments, each virtual host receives its own RESP
//! password derived from a master secret and the hostname. This allows
//! PHP applications to use standard Redis clients (`predis`, `phpredis`)
//! with per-site credentials injected as `$_SERVER` variables
//! (`EPHPM_REDIS_HOST` / `_PORT` / `_USERNAME` / `_PASSWORD`). They are
//! registered through the SAPI's `register_server_variables` hook, so they
//! live in `$_SERVER` only — not in `$_ENV`, and not in `getenv()`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// HMAC-SHA256 type alias.
type HmacSha256 = Hmac<Sha256>;

/// Derive a per-site RESP password from a master secret and hostname.
///
/// Returns a lowercase hex string (64 characters).
///
/// # Panics
///
/// Panics if HMAC initialization fails (should never happen — HMAC-SHA256
/// accepts any key length).
///
/// # Examples
///
/// ```
/// use ephpm_kv::auth::derive_site_password;
///
/// let pw = derive_site_password("my-secret", "example.com");
/// assert_eq!(pw.len(), 64);
/// // Deterministic — same inputs always produce same output.
/// assert_eq!(pw, derive_site_password("my-secret", "example.com"));
/// ```
#[must_use]
pub fn derive_site_password(secret: &str, hostname: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(hostname.as_bytes());
    let result = mac.finalize();
    let bytes = result.into_bytes();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Domain-separation label for the single shared SQL-wire-listener credential.
///
/// Deliberately **outside** the site-key alphabet (`[a-z0-9._-]`, see
/// [`is_valid_site_key`]) — the uppercase letters and the `:` cannot appear in a
/// site key — so no site, however named, derives a per-site password that
/// collides with the shared wire credential. (The original `"ephpm-db-wire"`
/// *was* a valid site key, so a site literally named that aliased the shared
/// credential; finding L1 of the F4 review.)
///
/// [`is_valid_site_key`]: ephpm_config::is_valid_site_key
const SERVER_WIRE_LABEL: &str = "EPHPM:db-wire";

/// Derive the shared SQL-wire-listener password from a master secret.
///
/// `HMAC-SHA256(secret, "EPHPM:db-wire")`, hex-encoded — the same construction
/// [`derive_site_password`] uses, with a fixed label instead of a site key, so
/// the shared credential is stable across restarts and derivable by any
/// co-located process that can read the same `[kv] secret`. The label is outside
/// the site-key alphabet, so it can never collide with a per-site password.
///
/// Returns a lowercase hex string (64 characters).
///
/// # Examples
///
/// ```
/// use ephpm_kv::auth::derive_wire_password;
///
/// let pw = derive_wire_password("my-secret");
/// assert_eq!(pw.len(), 64);
/// assert_eq!(pw, derive_wire_password("my-secret"));
/// ```
#[must_use]
pub fn derive_wire_password(secret: &str) -> String {
    derive_site_password(secret, SERVER_WIRE_LABEL)
}

/// Constant-time validation of a provided password against the derived value.
///
/// Computes `HMAC-SHA256(secret, hostname)` and compares the hex-encoded
/// result against `provided` using constant-time equality to prevent
/// timing side-channels.
#[must_use]
pub fn validate_site_password(secret: &str, hostname: &str, provided: &str) -> bool {
    let expected = derive_site_password(secret, hostname);
    constant_time_eq(expected.as_bytes(), provided.as_bytes())
}

/// Constant-time byte comparison to prevent timing attacks.
///
/// Length is not secret (it always leaks via the early return), but for
/// equal-length inputs the loop always visits every byte so no timing oracle
/// reveals where the first mismatch is. Shared with the legacy single-password
/// `AUTH` path in [`crate::server`] so both authentication modes compare
/// credentials in constant time.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_is_deterministic() {
        let pw1 = derive_site_password("secret", "example.com");
        let pw2 = derive_site_password("secret", "example.com");
        assert_eq!(pw1, pw2);
    }

    #[test]
    fn derive_returns_64_char_hex() {
        let pw = derive_site_password("secret", "example.com");
        assert_eq!(pw.len(), 64);
        assert!(pw.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_hosts_produce_different_passwords() {
        let pw1 = derive_site_password("secret", "alice.com");
        let pw2 = derive_site_password("secret", "bob.com");
        assert_ne!(pw1, pw2);
    }

    #[test]
    fn different_secrets_produce_different_passwords() {
        let pw1 = derive_site_password("secret-a", "example.com");
        let pw2 = derive_site_password("secret-b", "example.com");
        assert_ne!(pw1, pw2);
    }

    #[test]
    fn validate_correct_password() {
        let pw = derive_site_password("secret", "example.com");
        assert!(validate_site_password("secret", "example.com", &pw));
    }

    #[test]
    fn validate_wrong_password() {
        assert!(!validate_site_password("secret", "example.com", "wrong-password"));
    }

    #[test]
    fn validate_empty_password() {
        assert!(!validate_site_password("secret", "example.com", ""));
    }

    #[test]
    fn validate_wrong_host() {
        let pw = derive_site_password("secret", "alice.com");
        assert!(!validate_site_password("secret", "bob.com", &pw));
    }

    /// L1: the wire-credential label must live outside the site-key alphabet, or
    /// a tenant could name a site that derives the shared wire password.
    #[test]
    fn wire_label_is_outside_the_site_key_alphabet() {
        assert!(
            !ephpm_config::is_valid_site_key(SERVER_WIRE_LABEL),
            "the wire label {SERVER_WIRE_LABEL:?} must NOT be a valid site key, or a site named it \
             would alias the shared wire credential"
        );
    }

    /// L1: no site key (which is always lowercase `[a-z0-9._-]`) can derive the
    /// shared wire password — including a site literally named after the old
    /// label, which used to collide.
    #[test]
    fn wire_password_does_not_collide_with_any_site_key() {
        let wire = derive_wire_password("secret");
        // The pre-fix label was a valid site key and aliased the credential.
        assert_ne!(
            wire,
            derive_site_password("secret", "ephpm-db-wire"),
            "a site named after the old label must no longer derive the wire credential"
        );
        // A plausible tenant-chosen lowercased spelling of the new label, too.
        assert_ne!(wire, derive_site_password("secret", "ephpm:db-wire"));
    }
}

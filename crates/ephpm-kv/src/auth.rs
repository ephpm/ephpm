//! HMAC-derived per-site RESP authentication.
//!
//! In multi-tenant deployments, each virtual host receives its own RESP
//! password derived from a master secret and the hostname. This allows
//! PHP applications to use standard Redis clients (`predis`, `phpredis`)
//! with per-site credentials injected via `$_ENV`.

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
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
}

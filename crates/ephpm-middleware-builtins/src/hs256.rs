//! Shared HS256 token verification, used by both token-validating builtins.
//!
//! [`jwt`](crate::jwt) (API bearer tokens, `401` on failure) and
//! [`session_cookie`](crate::session_cookie) (browser session cookies, `302`
//! to a login service on failure) are deliberately separate modules — two
//! threat models, two failure behaviours, two config surfaces. What they must
//! **not** have separately is the verification itself: one implementation,
//! one set of tests, no chance of one growing a weakness the other fixed.
//!
//! The policy is strict by construction:
//!
//! * the signature is checked **first**, so no unauthenticated JSON is ever
//!   parsed;
//! * comparison is constant-time (`hmac::Mac::verify_slice`);
//! * `alg` is pinned to `HS256` after verification, so `alg: none` is
//!   rejected even when the HMAC happens to match;
//! * `exp` is **required** — a token that cannot expire is a config bug;
//! * `nbf` is honoured when present, `iss`/`aud` when configured;
//! * the **per-tenant `site` binding** is enforced when the caller supplies
//!   the request's canonical site key — see [`Hs256Policy::verify`] and issue
//!   #396.

use base64ct::{Base64UrlUnpadded, Encoding};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// An HS256 validation policy: the shared secret plus the optional
/// registered-claim constraints. Built once at `init`.
pub struct Hs256Policy {
    secret: Vec<u8>,
    issuer: Option<String>,
    audience: Option<String>,
}

impl Hs256Policy {
    /// Build a policy from an already-validated secret and claim constraints.
    #[must_use]
    pub fn new(secret: Vec<u8>, issuer: Option<String>, audience: Option<String>) -> Self {
        Self { secret, issuer, audience }
    }

    /// Read `secret`/`issuer`/`audience` out of a mount's `config` table.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when `secret` is missing, empty or
    /// not a string, or when `issuer`/`audience` are present but not strings.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        let secret = config
            .get("secret")
            .ok_or("`secret` is required (HS256 shared secret)")?
            .as_str()
            .ok_or("`secret` must be a string")?;
        if secret.is_empty() {
            return Err("`secret` must not be empty".into());
        }
        Ok(Self::new(
            secret.as_bytes().to_vec(),
            opt_str(config, "issuer")?,
            opt_str(config, "audience")?,
        ))
    }

    /// Verify `token` against this policy at time `now` (unix seconds).
    /// Returns the raw claims JSON on success, `None` on any failure.
    ///
    /// `expected_site` is the request's **canonical site key** (what
    /// `Router::resolve_site` produced, surfaced to a module as
    /// [`ephpm_middleware::Request::vhost_id`]). When it is `Some`, the token's
    /// `site` claim must be a string equal to it, or verification fails — a
    /// token with **no** `site` claim, or one bound to a **different** site,
    /// fails closed. This is the fix for issue #396: the `github-auth` issuer
    /// binds each session to one preview via the `site` claim, and this is the
    /// check that honours that binding, so a session minted for preview A does
    /// not verify on preview B. Pass `None` to skip the check — the `jwt` API
    /// gate has no tenancy semantics and always does.
    ///
    /// Every rejection collapses to `None` on purpose: the caller turns that
    /// into one indistinguishable outcome, so a client cannot learn *why* a
    /// token was refused (bad signature vs expired vs wrong site).
    #[must_use]
    pub fn verify(&self, token: &str, now: u64, expected_site: Option<&str>) -> Option<String> {
        let mut parts = token.split('.');
        let (header_b64, payload_b64, sig_b64) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }

        // Signature first — never parse unauthenticated JSON.
        let sig = Base64UrlUnpadded::decode_vec(sig_b64).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).ok()?;
        mac.update(header_b64.as_bytes());
        mac.update(b".");
        mac.update(payload_b64.as_bytes());
        // Constant-time comparison via the hmac crate.
        mac.verify_slice(&sig).ok()?;

        // The signature is ours, but still pin the algorithm: HS256 only.
        let header: serde_json::Value =
            serde_json::from_slice(&Base64UrlUnpadded::decode_vec(header_b64).ok()?).ok()?;
        if header.get("alg").and_then(serde_json::Value::as_str) != Some("HS256") {
            return None;
        }

        let payload = Base64UrlUnpadded::decode_vec(payload_b64).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;

        // `exp` is required — a token that cannot expire is a config bug.
        // RFC 7519 NumericDate allows non-integer values, so accept a JSON
        // float (floored) as well as an integer rather than rejecting a valid
        // token.
        let exp = claims.get("exp").and_then(numeric_date)?;
        if exp <= now {
            return None;
        }
        if let Some(nbf) = claims.get("nbf")
            && numeric_date(nbf)? > now
        {
            return None;
        }
        if let Some(expected) = &self.issuer
            && claims.get("iss").and_then(serde_json::Value::as_str) != Some(expected.as_str())
        {
            return None;
        }
        if let Some(expected) = &self.audience {
            let ok = match claims.get("aud") {
                Some(serde_json::Value::String(aud)) => aud == expected,
                Some(serde_json::Value::Array(auds)) => {
                    auds.iter().any(|a| a.as_str() == Some(expected.as_str()))
                }
                _ => false,
            };
            if !ok {
                return None;
            }
        }

        // Per-tenant binding (issue #396). When the caller keys this request to
        // a site, the token must be bound to the *same* site. Absent, non-string
        // or mismatched `site` all collapse to "not this tenant" and fail closed.
        if let Some(site) = expected_site
            && claims.get("site").and_then(serde_json::Value::as_str) != Some(site)
        {
            return None;
        }

        String::from_utf8(payload).ok()
    }
}

/// Read an optional non-empty string key from a mount's `config` table.
///
/// An absent key, JSON `null`, and the empty string all mean "unset" — the
/// same shape every builtin's optional string knobs use.
///
/// # Errors
///
/// Returns a message when the key is present but not a string.
pub fn opt_str(config: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    match config.get(key) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        None | Some(serde_json::Value::Null | serde_json::Value::String(_)) => Ok(None),
        Some(other) => Err(format!("`{key}` must be a string, got {other}")),
    }
}

/// Read an optional boolean key, falling back to `default` when absent.
///
/// # Errors
///
/// Returns a message when the key is present but not a boolean. A security
/// knob is never allowed to be silently mistyped into its permissive state.
pub fn opt_bool(config: &serde_json::Value, key: &str, default: bool) -> Result<bool, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("`{key}` must be a boolean, got {other}")),
    }
}

/// Current unix time in whole seconds (0 if the clock predates the epoch).
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Parse an RFC 7519 NumericDate claim (`exp`/`nbf`) as seconds since the
/// epoch. Accepts a JSON integer or a JSON float (floored to whole seconds,
/// negatives rejected); returns `None` for any other JSON type. This keeps
/// enforcement identical while tolerating the non-integer NumericDates the
/// spec permits.
fn numeric_date(v: &serde_json::Value) -> Option<u64> {
    if let Some(u) = v.as_u64() {
        return Some(u);
    }
    let f = v.as_f64()?;
    // floor() keeps the "not valid until this whole second" semantics; only
    // finite, non-negative values within u64 range map to a NumericDate.
    if f.is_finite() && (0.0..18_446_744_073_709_551_616.0).contains(&f) {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "bounds checked: finite, in [0, u64::MAX), floored"
        )]
        Some(f.floor() as u64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret-please-rotate";

    /// Forge a token through the same HMAC code path the module verifies
    /// with (independent of `Hs256Policy::verify`'s parsing).
    pub(crate) fn sign(secret: &str, header_json: &str, claims_json: &str) -> String {
        let h = Base64UrlUnpadded::encode_string(header_json.as_bytes());
        let p = Base64UrlUnpadded::encode_string(claims_json.as_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
        mac.update(format!("{h}.{p}").as_bytes());
        let sig = Base64UrlUnpadded::encode_string(&mac.finalize().into_bytes());
        format!("{h}.{p}.{sig}")
    }

    fn policy() -> Hs256Policy {
        Hs256Policy::from_config(&serde_json::json!({ "secret": SECRET })).expect("init")
    }

    fn token(claims: &str) -> String {
        sign(SECRET, r#"{"alg":"HS256","typ":"JWT"}"#, claims)
    }

    #[test]
    fn from_config_rejects_a_missing_or_unusable_secret() {
        assert!(Hs256Policy::from_config(&serde_json::Value::Null).is_err());
        assert!(Hs256Policy::from_config(&serde_json::json!({ "secret": "" })).is_err());
        assert!(Hs256Policy::from_config(&serde_json::json!({ "secret": 42 })).is_err());
    }

    #[test]
    fn valid_token_returns_its_claims_json() {
        let claims = r#"{"sub":"u1","exp":2000}"#;
        assert_eq!(policy().verify(&token(claims), 1000, None).as_deref(), Some(claims));
    }

    #[test]
    fn tampering_with_any_segment_is_rejected() {
        let good = token(r#"{"sub":"u1","exp":2000}"#);
        let mut parts = good.split('.');
        let (h, p, s) =
            (parts.next().expect("h"), parts.next().expect("p"), parts.next().expect("s"));
        // Swapped payload with a signature that no longer covers it.
        let forged_payload = Base64UrlUnpadded::encode_string(br#"{"sub":"root","exp":2000}"#);
        assert!(policy().verify(&format!("{h}.{forged_payload}.{s}"), 1000, None).is_none());
        // Flipped last signature character.
        let flipped = if s.ends_with('A') { "B" } else { "A" };
        let tampered_sig = format!("{}{flipped}", &s[..s.len() - 1]);
        assert!(policy().verify(&format!("{h}.{p}.{tampered_sig}"), 1000, None).is_none());
        // Truncated signature.
        assert!(policy().verify(&format!("{h}.{p}."), 1000, None).is_none());
    }

    #[test]
    fn exp_is_mandatory_and_enforced() {
        assert!(policy().verify(&token(r#"{"sub":"u1"}"#), 1000, None).is_none());
        assert!(
            policy().verify(&token(r#"{"exp":1000}"#), 1000, None).is_none(),
            "exp == now expires"
        );
        assert!(policy().verify(&token(r#"{"exp":1001}"#), 1000, None).is_some());
    }

    #[test]
    fn alg_none_is_rejected_even_with_a_valid_hmac() {
        let t = sign(SECRET, r#"{"alg":"none"}"#, r#"{"exp":2000}"#);
        assert!(policy().verify(&t, 1000, None).is_none());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let t = sign("other-secret", r#"{"alg":"HS256"}"#, r#"{"exp":2000}"#);
        assert!(policy().verify(&t, 1000, None).is_none());
    }

    /// Issue #396, at the crypto core: a session bound to one site must not
    /// verify on another. The `None` calls reproduce the **pre-fix** verifier
    /// (which took no site argument and ignored the claim) — and they still
    /// accept the token, which is exactly the cross-tenant bypass. The site-
    /// aware calls are the fix: accepted only on its own site, and a token
    /// with no `site` claim fails closed under enforcement.
    #[test]
    fn a_site_bound_token_verifies_only_on_its_own_site() {
        let claims = r#"{"sub":"u1","site":"alpha","exp":2000}"#;
        let t = token(claims);

        // Pre-fix behaviour (no site enforcement): accepted regardless. This
        // is what shipped before #396 and why the bug was invisible.
        assert!(policy().verify(&t, 1000, None).is_some());

        // The fix: accepted on its own site, rejected on any other.
        assert_eq!(policy().verify(&t, 1000, Some("alpha")).as_deref(), Some(claims));
        assert!(
            policy().verify(&t, 1000, Some("beta")).is_none(),
            "a session for `alpha` must not verify on `beta`"
        );

        // A token with NO site claim fails closed once a site is required...
        let no_site = token(r#"{"sub":"u1","exp":2000}"#);
        assert!(policy().verify(&no_site, 1000, Some("alpha")).is_none());
        // ...but is fine when the caller does not require one (the `jwt` lane).
        assert!(policy().verify(&no_site, 1000, None).is_some());

        // A non-string `site` claim is not a tenant match either.
        let bad_site = token(r#"{"sub":"u1","site":42,"exp":2000}"#);
        assert!(policy().verify(&bad_site, 1000, Some("alpha")).is_none());
    }

    #[test]
    fn opt_bool_rejects_a_mistyped_security_knob() {
        assert_eq!(opt_bool(&serde_json::json!({}), "k", true), Ok(true));
        assert_eq!(opt_bool(&serde_json::json!({ "k": false }), "k", true), Ok(false));
        // "false" as a STRING must not be read as `true` (nor silently as
        // false): a mistyped security knob has to fail startup.
        assert!(opt_bool(&serde_json::json!({ "k": "false" }), "k", true).is_err());
    }
}

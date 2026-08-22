//! Cookie reading and `Set-Cookie` construction.
//!
//! # Why these attributes
//!
//! Every cookie this module sets — the session and the short-lived OAuth
//! `state` — carries the same set, and each one is load-bearing:
//!
//! * **`HttpOnly`** — the token *is* the session. A preview site is
//!   pre-production code, which is precisely where XSS lives; script must not
//!   be able to read the value that gets a stranger past the gate.
//! * **`Secure`** (default on) — the token is a bearer credential, so it must
//!   never ride a plaintext hop. Configurable only because a local
//!   `http://…localhost` preview cannot set it at all, and silently dropping
//!   the flag there would be worse than making the operator ask for it.
//! * **`SameSite=Lax`** — `Strict` breaks the product: a preview link is
//!   almost always clicked from somewhere else (a PR comment, Slack), and
//!   `Strict` withholds the cookie on that first cross-site navigation, so an
//!   already-logged-in user gets bounced back through GitHub every time.
//!   `Lax` sends it on top-level GET navigations, which is exactly that case
//!   and also exactly what the OAuth callback needs. `None` would be strictly
//!   worse — it permits the cookie on cross-site subrequests, which nothing
//!   here needs.
//! * **`Path`** (default `/`) — the gate protects the whole site, so the
//!   cookie has to be sent for the whole site.
//! * **`Max-Age`** — a real expiry, matching the `exp` inside the signed
//!   token. The signature is what actually enforces it; `Max-Age` just stops
//!   the browser from keeping a corpse around.
//! * **no `Domain`** — deliberately absent, which makes the cookie
//!   **host-only**. Adding `Domain=.preview.example.com` would send one
//!   tenant's session to every other tenant on the wildcard, and in this
//!   product the tenants are different customers' code. Host-only is the
//!   isolation boundary, and it is not configurable for that reason.

use std::fmt::Write as _;

/// `SameSite` values this module will emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    /// Sent on same-site requests and top-level cross-site GET navigations.
    Lax,
    /// Never sent on any cross-site request.
    Strict,
    /// Sent on all cross-site requests; requires `Secure`.
    None,
}

impl SameSite {
    /// Parse the config spelling (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns a message naming the accepted values for anything else.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "lax" => Ok(Self::Lax),
            "strict" => Ok(Self::Strict),
            "none" => Ok(Self::None),
            other => Err(format!("`cookie_samesite` must be Lax, Strict or None, got {other:?}")),
        }
    }

    /// The attribute value as it appears in a `Set-Cookie` header.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lax => "Lax",
            Self::Strict => "Strict",
            Self::None => "None",
        }
    }
}

/// Attributes shared by every cookie this module sets.
#[derive(Clone, Debug)]
pub struct CookieAttrs {
    /// `Path` attribute.
    pub path: String,
    /// Emit `Secure`.
    pub secure: bool,
    /// `SameSite` attribute.
    pub same_site: SameSite,
}

/// Build a `Set-Cookie` value.
///
/// `max_age` of `0` emits `Max-Age=0` plus an expired `Expires`, i.e. a
/// deletion — used when a login attempt consumes its `state` cookie.
#[must_use]
pub fn set_cookie(name: &str, value: &str, max_age: i64, attrs: &CookieAttrs) -> String {
    let mut out = String::with_capacity(name.len() + value.len() + 96);
    // `name` and `value` are validated at init / produced by this crate, so
    // neither can contain a `;` or a control character.
    let _ = write!(out, "{name}={value}; Path={}; Max-Age={max_age}", attrs.path);
    if max_age == 0 {
        out.push_str("; Expires=Thu, 01 Jan 1970 00:00:00 GMT");
    }
    out.push_str("; HttpOnly");
    if attrs.secure {
        out.push_str("; Secure");
    }
    let _ = write!(out, "; SameSite={}", attrs.same_site.as_str());
    out
}

/// Read one cookie out of a raw `Cookie:` request header.
///
/// Returns the **first** occurrence, which is what RFC 6265 §5.4 orders most
/// specific first, and — more to the point — is what stops an attacker who
/// can set a cookie on a broader path or a parent domain from shadowing the
/// real session by appending a second one.
#[must_use]
pub fn read_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// Reject cookie names that would need quoting or could not round-trip.
///
/// RFC 6265 says a cookie name is an RFC 7230 `token`. Validating at `init`
/// means [`set_cookie`] can concatenate without escaping.
///
/// # Errors
///
/// Returns a message when the name is empty or contains a separator,
/// a control character, or a non-ASCII byte.
pub fn validate_cookie_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("cookie name must not be empty".into());
    }
    const SEPARATORS: &[char] = &[
        '(', ')', '<', '>', '@', ',', ';', ':', '\\', '"', '/', '[', ']', '?', '=', '{', '}', ' ',
    ];
    if name.chars().any(|c| !c.is_ascii() || c.is_ascii_control() || SEPARATORS.contains(&c)) {
        return Err(format!("cookie name {name:?} is not a valid RFC 6265 token"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs() -> CookieAttrs {
        CookieAttrs { path: "/".into(), secure: true, same_site: SameSite::Lax }
    }

    #[test]
    fn set_cookie_carries_every_required_attribute() {
        let c = set_cookie("ephpm_session", "abc.def.ghi", 28_800, &attrs());
        assert_eq!(
            c,
            "ephpm_session=abc.def.ghi; Path=/; Max-Age=28800; HttpOnly; Secure; SameSite=Lax"
        );
        // The isolation-critical negative: never a Domain attribute.
        assert!(!c.contains("Domain"), "cookies must stay host-only");
    }

    #[test]
    fn secure_can_be_dropped_for_plaintext_local_previews() {
        let a = CookieAttrs { secure: false, ..attrs() };
        let c = set_cookie("s", "v", 60, &a);
        assert!(!c.contains("Secure"));
        assert!(c.contains("HttpOnly"), "HttpOnly is never optional");
    }

    #[test]
    fn zero_max_age_deletes() {
        let c = set_cookie("s", "", 0, &attrs());
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Expires=Thu, 01 Jan 1970"));
    }

    #[test]
    fn read_cookie_finds_values_and_ignores_neighbours() {
        let h = "a=1; ephpm_session=tok.en; b=2";
        assert_eq!(read_cookie(h, "ephpm_session"), Some("tok.en"));
        assert_eq!(read_cookie(h, "a"), Some("1"));
        assert_eq!(read_cookie(h, "missing"), None);
        // A name that is a prefix/suffix of another must not match.
        assert_eq!(read_cookie("ephpm_session_x=no", "ephpm_session"), None);
        assert_eq!(read_cookie("x_ephpm_session=no", "ephpm_session"), None);
    }

    #[test]
    fn duplicate_cookie_resolves_to_the_first() {
        // A shadowing cookie set on a parent domain or broader path arrives
        // after the host-only one; taking the first is what ignores it.
        assert_eq!(read_cookie("s=real; s=forged", "s"), Some("real"));
    }

    #[test]
    fn cookie_name_validation() {
        assert!(validate_cookie_name("ephpm_session").is_ok());
        assert!(validate_cookie_name("__Host-sess").is_ok());
        for bad in ["", "has space", "has;semi", "has=eq", "has,comma", "tab\there", "caf\u{e9}"] {
            assert!(validate_cookie_name(bad).is_err(), "{bad:?} must be rejected");
        }
    }
}

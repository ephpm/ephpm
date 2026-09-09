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
//! * **`Domain`** — **absent by default** (host-only), and configurable to a
//!   fleet apex (`cookie_domain = ".preview.example.com"`) only for the
//!   single-OAuth-App wildcard flow. A GitHub OAuth App permits exactly one
//!   callback host, so a `*.preview` fleet must funnel every callback through
//!   one apex vhost; the session it mints there has to reach the target
//!   subdomain, which needs a domain-scoped cookie.
//!
//!   This *was* declared "not configurable — host-only is the isolation
//!   boundary", and for a naive design it would be: a domain-wide cookie is
//!   sent to every tenant on the wildcard. What makes it safe here is the
//!   per-tenant **`site` claim binding** (issue #396): the session verifier
//!   (`session-cookie` / `preview-gate`) accepts a token only on the vhost its
//!   `site` claim names, so a domain-scoped session minted for `pr-1` is inert
//!   on `pr-2` even though the browser sends it there. The cookie travels
//!   fleet-wide; the *authority* does not. Leave `cookie_domain` unset for a
//!   single-host deployment — host-only stays the default.

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
    /// `Domain` attribute, when set — makes the cookie fleet-wide instead of
    /// host-only, for the single-OAuth-App apex flow. `None` = host-only (the
    /// default and the isolation-safe choice for a single-host deployment). See
    /// the module docs for why a domain-scoped cookie is safe here (the `site`
    /// claim binding, issue #396).
    pub domain: Option<String>,
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
    if let Some(domain) = &attrs.domain {
        // Validated at init (`validate_cookie_domain`), so it cannot carry a
        // `;` or a control character that would break out of the attribute.
        let _ = write!(out, "; Domain={domain}");
    }
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

/// Validate a `cookie_domain` value for use in a `Set-Cookie` `Domain`
/// attribute.
///
/// A leading dot is permitted (the conventional `.preview.example.com`
/// spelling) and normalised away by the caller for comparisons. The value is
/// interpolated into a header, so it must be a plain DNS name — ASCII letters,
/// digits, `-` and `.`, at least one dot, no `..`, no leading/trailing dot on
/// the *label* portion.
///
/// # Errors
///
/// Returns a message when the value is empty or contains anything outside the
/// host character set.
pub fn validate_cookie_domain(domain: &str) -> Result<(), String> {
    let bare = domain.strip_prefix('.').unwrap_or(domain);
    let ok = !bare.is_empty()
        && bare.len() <= 253
        && bare.contains('.')
        && !bare.starts_with('.')
        && !bare.ends_with('.')
        && !bare.contains("..")
        && bare.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`cookie_domain` must be a DNS name like `.preview.example.com`, got {domain:?}"
        ))
    }
}

/// Whether `host` is within cookie `domain` — the open-redirect / same-fleet
/// guard for the apex flow. `domain` may carry a leading dot; comparison is on
/// the bare form. A host equal to the domain, or ending in `.<domain>`, is in.
#[must_use]
pub fn host_in_domain(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let bare = domain.strip_prefix('.').unwrap_or(domain).to_ascii_lowercase();
    host == bare || host.ends_with(&format!(".{bare}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs() -> CookieAttrs {
        CookieAttrs { path: "/".into(), secure: true, same_site: SameSite::Lax, domain: None }
    }

    #[test]
    fn set_cookie_carries_every_required_attribute() {
        let c = set_cookie("ephpm_session", "abc.def.ghi", 28_800, &attrs());
        assert_eq!(
            c,
            "ephpm_session=abc.def.ghi; Path=/; Max-Age=28800; HttpOnly; Secure; SameSite=Lax"
        );
        // Host-only by default: no Domain unless one is configured.
        assert!(!c.contains("Domain"), "cookies stay host-only unless cookie_domain is set");
    }

    #[test]
    fn a_configured_domain_makes_the_cookie_fleet_wide() {
        let a = CookieAttrs { domain: Some(".preview.example.com".into()), ..attrs() };
        let c = set_cookie("ephpm_session", "abc.def.ghi", 28_800, &a);
        assert!(
            c.contains("; Domain=.preview.example.com"),
            "a configured cookie_domain must appear in Set-Cookie: {c}"
        );
        // Still carries the isolation-critical attributes.
        assert!(c.contains("HttpOnly") && c.contains("Secure") && c.contains("SameSite=Lax"));
    }

    #[test]
    fn cookie_domain_validation() {
        for ok in [".preview.example.com", "preview.example.com", "a.b.c", ".ephpm.dev"] {
            assert!(validate_cookie_domain(ok).is_ok(), "{ok:?} must be accepted");
        }
        for bad in
            ["", ".", "no-dot", "has space.com", "a..b.com", "trailing.com.", "caf\u{e9}.com"]
        {
            assert!(validate_cookie_domain(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn host_in_domain_is_the_same_fleet_guard() {
        assert!(host_in_domain("pr-1.preview.example.com", ".preview.example.com"));
        assert!(host_in_domain("pr-1.preview.example.com", "preview.example.com"));
        assert!(host_in_domain("preview.example.com", ".preview.example.com"), "apex itself is in");
        // Case- and trailing-dot-insensitive.
        assert!(host_in_domain("PR-1.Preview.Example.com.", ".preview.example.com"));
        // Not in: a different domain, or a suffix-only string match.
        assert!(!host_in_domain("pr-1.preview.evil.com", ".preview.example.com"));
        assert!(!host_in_domain("evilpreview.example.com", ".preview.example.com"));
        assert!(!host_in_domain("preview.example.com.attacker.com", ".preview.example.com"));
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

//! Return-to validation — the open-redirect and header-injection defence.
//!
//! A login gate has to remember where the user was going, and that value is
//! attacker-controlled: anyone can hand a victim
//! `https://preview.example.com/anything` and the gate will faithfully carry
//! the path through GitHub and back into a `Location:` header. If the check
//! is sloppy the gate becomes an open redirector wearing the preview site's
//! hostname — which is exactly the property phishing wants, because the
//! *first* hop is genuinely the site the victim expects.
//!
//! [`sanitize_return_to`] therefore does not try to parse a URL and compare
//! origins. It applies an allowlist: a return-to is either an unambiguous
//! **same-origin absolute path** or it is replaced by `/`. There is no
//! rejection path a caller can forget to handle.
//!
//! # What is rejected, and why
//!
//! | Input shape | Example | Why |
//! |---|---|---|
//! | absolute URL | `https://evil.test/x` | different origin |
//! | scheme-relative | `//evil.test/x` | browsers resolve this against the current scheme — a different origin |
//! | backslash forms | `/\evil.test`, `\\evil.test` | WHATWG URL parsing treats `\` as `/`, so these are scheme-relative too |
//! | any `\` at all | `/a\b` | belt and braces: no legitimate path needs one, and normalisation differs between browsers |
//! | non-`/` start | `evil.test`, `?a=1`, `javascript:…` | relative or a scheme; only absolute paths are same-origin by construction |
//! | control characters | a raw CR/LF (a decoded `%0d%0a`) | `Location:` header injection / response splitting |
//! | `..` segments | `/..//evil.test` | same-origin per RFC 3986 and WHATWG, rejected anyway — no return-to needs one |
//! | non-ASCII | `/café` | a `Location` value must be ASCII; clients percent-encode, so a raw non-ASCII byte is a sign of something else |
//! | over-long | 2 KiB+ | bounded work, bounded header |
//! | the gate's own paths | `/…/auth/github/login` | a redirect loop, not a destination |
//!
//! Percent-encoded separators (`/%2f%2fevil.test`, `/%5c%5cevil.test`) are
//! **kept**. They are not a bypass: browsers do not decode `%2f`/`%5c` before
//! resolving a URL, so the request stays on this origin as a path whose first
//! segment happens to be `%2f%2fevil.test`. Rejecting them would break
//! legitimate paths that contain encoded slashes.

/// Longest return-to accepted. Anything beyond this is replaced by `/`.
const MAX_RETURN_TO: usize = 2048;

/// Reduce an attacker-supplied return-to to a safe same-origin path.
///
/// Returns the input unchanged when it is an unambiguous absolute path on
/// this origin, and `"/"` in every other case — including the empty string.
/// `reserved` lists paths the gate owns (login, callback); a return-to that
/// targets one of them would loop, so it is replaced too.
///
/// This is deliberately total: there is no `Err` for a caller to swallow.
#[must_use]
pub fn sanitize_return_to(raw: &str, reserved: &[&str]) -> String {
    if is_safe_return_to(raw, reserved) { raw.to_owned() } else { "/".to_owned() }
}

/// The predicate behind [`sanitize_return_to`], split out so tests can state
/// the intent directly.
#[must_use]
pub fn is_safe_return_to(raw: &str, reserved: &[&str]) -> bool {
    if raw.len() > MAX_RETURN_TO {
        return false;
    }
    // Must be an absolute path. This alone rules out `https://evil.test`,
    // `javascript:…`, `evil.test` and `?a=1`.
    let Some(rest) = raw.strip_prefix('/') else {
        return false;
    };
    // `//evil.test` is scheme-relative; `/\evil.test` is the same thing after
    // WHATWG backslash normalisation. Checking the *whole* string for `\`
    // covers both and costs nothing.
    if rest.starts_with('/') || raw.contains('\\') {
        return false;
    }
    // ASCII, no controls, no DEL. Blocks CR/LF response splitting into the
    // `Location:` header, and raw tabs/NULs that different parsers disagree
    // about.
    if !raw.is_ascii() || raw.chars().any(|c| c.is_ascii_control()) {
        return false;
    }
    // A space is legal in a path only percent-encoded; a raw one would be
    // re-parsed by some clients as the end of the header value.
    if raw.contains(' ') {
        return false;
    }
    // Reject `..` segments. `/..//evil.test` is *technically* still
    // same-origin — RFC 3986 dot-segment removal leaves the authority alone,
    // and so does the WHATWG parser — but the value of that guarantee is
    // "every client agrees with the spec", which is not a bet worth taking on
    // a redirect target when no legitimate return-to needs a `..`.
    let path_part = raw.split(['?', '#']).next().unwrap_or(raw);
    if path_part.split('/').any(|seg| seg == "..") {
        return false;
    }
    // The gate's own endpoints are not destinations.
    let path_only = raw.split(['?', '#']).next().unwrap_or(raw);
    !reserved.contains(&path_only)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths the gate owns, as `lib.rs` passes them.
    const RESERVED: &[&str] = &["/_ephpm/auth/github/login", "/_ephpm/auth/github/callback"];

    #[test]
    fn plain_paths_survive() {
        for ok in [
            "/",
            "/index.php",
            "/wp-admin/",
            "/a/b/c?d=e&f=g",
            "/search?q=%20quoted%20",
            "/page#anchor",
            // Encoded separators stay: browsers do not decode them before
            // resolving, so these remain same-origin paths.
            "/%2f%2fevil.test",
            "/%5c%5cevil.test",
            // A query value that merely *mentions* another origin is fine —
            // the browser still navigates to this origin.
            "/redirect?next=https://evil.test",
        ] {
            assert!(is_safe_return_to(ok, RESERVED), "{ok} should be accepted");
            assert_eq!(sanitize_return_to(ok, RESERVED), ok);
        }
    }

    #[test]
    fn hostile_inputs_collapse_to_root() {
        // Every one of these was tried against the gate; each must come back
        // as `/` rather than as a redirect off this origin.
        for bad in [
            "",
            // Absolute URLs, every scheme shape.
            "https://evil.test",
            "http://evil.test/x",
            "//evil.test",
            "///evil.test",
            "////evil.test",
            "https:/\\evil.test",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            // Dot segments: same-origin per spec, rejected anyway (see the
            // predicate) because it costs nothing and removes a whole class
            // of "does this client normalise like the spec says" doubt.
            "/..//evil.test",
            "/a/../../evil",
            "/..",
            "/foo/..",
            // Backslash normalisation.
            "/\\evil.test",
            "\\\\evil.test",
            "\\/evil.test",
            "/path\\..\\..\\evil",
            // Not absolute.
            "evil.test",
            "?next=/",
            "#fragment",
            "../../etc/passwd",
            // Userinfo trick — still not a path.
            "//user@evil.test/",
            "https://preview.example.com@evil.test/",
            // Header injection / response splitting.
            "/ok\r\nSet-Cookie: pwned=1",
            "/ok\nLocation: https://evil.test",
            "/ok\rX: y",
            "/ok\0",
            "/ok\ttab",
            "/ok\x0bvtab",
            // Raw space ends a header value for some parsers.
            "/ok evil",
            // Non-ASCII.
            "/café",
            "/\u{202e}gnp.exe",
            // Loops back into the gate.
            "/_ephpm/auth/github/login",
            "/_ephpm/auth/github/callback",
            "/_ephpm/auth/github/callback?code=x&state=y",
        ] {
            assert!(!is_safe_return_to(bad, RESERVED), "{bad:?} must be rejected");
            assert_eq!(sanitize_return_to(bad, RESERVED), "/", "{bad:?} must collapse to /");
        }
    }

    #[test]
    fn over_long_is_rejected() {
        let long = format!("/{}", "a".repeat(MAX_RETURN_TO));
        assert!(!is_safe_return_to(&long, RESERVED));
        // Exactly at the cap is still fine.
        let at_cap = format!("/{}", "a".repeat(MAX_RETURN_TO - 1));
        assert_eq!(at_cap.len(), MAX_RETURN_TO);
        assert!(is_safe_return_to(&at_cap, RESERVED));
    }

    #[test]
    fn reserved_match_is_exact_not_prefix() {
        // A real application page that merely shares a prefix with the gate's
        // endpoints is a legitimate destination.
        assert!(is_safe_return_to("/_ephpm/auth/github/login-help", RESERVED));
        assert!(is_safe_return_to("/_ephpm/auth/github/callbacks", RESERVED));
    }
}

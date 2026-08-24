//! `redirect` — ePHPm native middleware that enforces canonical URLs with a
//! single `301`/`308` redirect **before** PHP runs.
//!
//! Analogous to Traefik's `redirectscheme`/`redirectregex`, Caddy's `redir`,
//! Cloudflare redirect rules, or an nginx `return 301`. It composes several
//! canonicalization rules — scheme, host, trailing slash — computes the final
//! canonical URL **once**, and redirects a single time only when the request
//! is not already canonical (so it can never loop).
//!
//! Configuration (`[[middleware]] config = { ... }`), all optional:
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `force_https` (bool) | `false` | redirect `http` → `https` |
//! | `canonical_host` (string) | unset | `"www"` forces the apex → `www.`; `"apex"` (alias `"non-www"`) strips a leading `www.` |
//! | `host_map` (object) | unset | explicit `source-host` → `canonical-host` map (exact, case-insensitive key); wins over `canonical_host` on a match |
//! | `trailing_slash` (string) | unset | `"add"` appends a `/` (except the root and paths whose last segment looks like a file, i.e. contains a `.`); `"strip"` removes trailing `/` (except the root) |
//! | `status` (integer) | `308` | redirect status — `301` or `308`; `308` preserves the request method |
//! | `forwarded_proto_header` (string) | `"X-Forwarded-Proto"` | header the current scheme is derived from |
//!
//! **Scheme derivation.** The v1 middleware ABI exposes no request scheme or
//! "is secure" flag, so the current scheme is read from
//! `forwarded_proto_header` (default `X-Forwarded-Proto`); a request with no
//! such header is treated as `http`. Behind a TLS-terminating proxy the proxy
//! **must** set that header, or `force_https` would redirect an
//! already-secure request and loop — the same requirement nginx/Traefik place
//! on the operator.
//!
//! **Scope.** Config is per-mount (there is no per-vhost config idiom in the
//! ABI). Use `host_map` to canonicalize several hosts from one mount; the
//! request's own `Host` header is what every rule is computed against.

use ephpm_middleware::{Middleware, Request, Response};

/// The canonical-host policy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostPolicy {
    /// Force the apex form to `www.` (`example.com` → `www.example.com`).
    Www,
    /// Strip a leading `www.` (`www.example.com` → `example.com`).
    Apex,
}

/// The trailing-slash policy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlashPolicy {
    /// Append a trailing `/` (except the root and file-like paths).
    Add,
    /// Remove trailing `/` (except the root).
    Strip,
}

/// Redirect policy, built once at `init`.
pub struct Redirect {
    force_https: bool,
    canonical_host: Option<HostPolicy>,
    /// Exact source→canonical host map; keys are stored lower-cased.
    host_map: Vec<(String, String)>,
    trailing_slash: Option<SlashPolicy>,
    status: u16,
    forwarded_proto_header: String,
}

/// Read an optional boolean config key with a default.
fn opt_bool(config: &serde_json::Value, key: &str, default: bool) -> Result<bool, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("`{key}` must be a boolean, got {other}")),
    }
}

/// Read an optional string config key with a default.
fn opt_string(config: &serde_json::Value, key: &str, default: &str) -> Result<String, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default.to_owned()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("`{key}` must be a string, got {other}")),
    }
}

/// True when `host` begins with a `www.` label (case-insensitive).
fn has_www_prefix(host: &str) -> bool {
    host.len() > 4 && host[..4].eq_ignore_ascii_case("www.")
}

/// The host with a leading `www.` removed, or `None` when there is no such
/// prefix (or removing it would leave the host empty).
fn strip_www_prefix(host: &str) -> Option<&str> {
    if has_www_prefix(host) {
        let rest = &host[4..];
        (!rest.is_empty()).then_some(rest)
    } else {
        None
    }
}

/// Split a `Host` header value into `(host, Option<port>)`. Handles bracketed
/// IPv6 literals (`[::1]:8080`) and only treats an all-digit tail after the
/// last `:` as a port.
fn split_host(value: &str) -> (&str, Option<&str>) {
    if value.starts_with('[') {
        // Bracketed IPv6 literal: the host is everything through `]`.
        if let Some(idx) = value.find(']') {
            let host = &value[..=idx];
            let port = value[idx + 1..].strip_prefix(':').filter(|p| !p.is_empty());
            return (host, port);
        }
        return (value, None);
    }
    match value.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (value, None),
    }
}

/// True when the last path segment looks like a file (contains a `.`).
fn last_segment_has_dot(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|seg| seg.contains('.'))
}

impl Redirect {
    /// The canonical host for `host` (case preserved when nothing applies).
    fn canonical_host(&self, host: &str) -> String {
        for (src, dst) in &self.host_map {
            if host.eq_ignore_ascii_case(src) {
                return dst.clone();
            }
        }
        match self.canonical_host {
            Some(HostPolicy::Www) => {
                if has_www_prefix(host) {
                    host.to_owned()
                } else {
                    format!("www.{host}")
                }
            }
            Some(HostPolicy::Apex) => strip_www_prefix(host).unwrap_or(host).to_owned(),
            None => host.to_owned(),
        }
    }

    /// The canonical path for `path` under the trailing-slash policy.
    fn canonical_path(&self, path: &str) -> String {
        match self.trailing_slash {
            Some(SlashPolicy::Strip) => {
                if path.len() > 1 && path.ends_with('/') {
                    let trimmed = path.trim_end_matches('/');
                    if trimmed.is_empty() { "/".to_owned() } else { trimmed.to_owned() }
                } else {
                    path.to_owned()
                }
            }
            Some(SlashPolicy::Add) => {
                if path.ends_with('/') || last_segment_has_dot(path) {
                    path.to_owned()
                } else {
                    format!("{path}/")
                }
            }
            None => path.to_owned(),
        }
    }

    /// The current request scheme, derived from `forwarded_proto_header`
    /// (first value of a comma list); `http` when the header is absent.
    fn current_scheme<'a>(&self, req: &'a Request<'_>) -> &'a str {
        match req.header(&self.forwarded_proto_header) {
            Some(v) => {
                let first = v.split(',').next().unwrap_or(v).trim();
                if first.eq_ignore_ascii_case("https") { "https" } else { "http" }
            }
            None => "http",
        }
    }
}

impl Middleware for Redirect {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let canonical_host = match config.get("canonical_host") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => match s.to_ascii_lowercase().as_str() {
                "www" => Some(HostPolicy::Www),
                "apex" | "non-www" => Some(HostPolicy::Apex),
                other => {
                    return Err(format!(
                        "`canonical_host` must be \"www\" or \"apex\", got \"{other}\""
                    ));
                }
            },
            Some(other) => return Err(format!("`canonical_host` must be a string, got {other}")),
        };

        let host_map = match config.get("host_map") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Object(map)) => {
                let mut out = Vec::with_capacity(map.len());
                for (k, v) in map {
                    let dst = v.as_str().ok_or_else(|| {
                        format!("`host_map` values must be strings, got {v} for key `{k}`")
                    })?;
                    if dst.is_empty() {
                        return Err(format!("`host_map` value for key `{k}` must not be empty"));
                    }
                    out.push((k.to_ascii_lowercase(), dst.to_owned()));
                }
                out
            }
            Some(other) => return Err(format!("`host_map` must be an object, got {other}")),
        };

        let trailing_slash = match config.get("trailing_slash") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => match s.to_ascii_lowercase().as_str() {
                "add" => Some(SlashPolicy::Add),
                "strip" => Some(SlashPolicy::Strip),
                other => {
                    return Err(format!(
                        "`trailing_slash` must be \"add\" or \"strip\", got \"{other}\""
                    ));
                }
            },
            Some(other) => return Err(format!("`trailing_slash` must be a string, got {other}")),
        };

        let status = match config.get("status") {
            None | Some(serde_json::Value::Null) => 308,
            Some(v) => {
                let n =
                    v.as_u64().ok_or_else(|| format!("`status` must be 301 or 308, got {v}"))?;
                if n != 301 && n != 308 {
                    return Err(format!("`status` must be 301 or 308, got {n}"));
                }
                u16::try_from(n).unwrap_or(308)
            }
        };

        let forwarded_proto_header =
            opt_string(config, "forwarded_proto_header", "X-Forwarded-Proto")?;
        if forwarded_proto_header.is_empty() {
            return Err("`forwarded_proto_header` must not be empty".into());
        }

        Ok(Self {
            force_https: opt_bool(config, "force_https", false)?,
            canonical_host,
            host_map,
            trailing_slash,
            status,
            forwarded_proto_header,
        })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        // Without an authority we cannot build an absolute Location; pass through.
        let Some(host_hdr) = req.header("Host").filter(|h| !h.is_empty()) else {
            return Response::cont();
        };
        let (host, port) = split_host(host_hdr);

        let scheme_cur = self.current_scheme(req);
        let scheme_can = if self.force_https { "https" } else { scheme_cur };

        let host_can = self.canonical_host(host);

        let path_cur = {
            let p = req.path();
            if p.is_empty() { "/" } else { p }
        };
        let path_can = self.canonical_path(path_cur);

        let changed = scheme_cur != scheme_can
            || !host.eq_ignore_ascii_case(&host_can)
            || path_cur != path_can;
        if !changed {
            return Response::cont();
        }

        let query = req.query();
        let mut location = String::with_capacity(
            scheme_can.len() + 3 + host_can.len() + path_can.len() + query.len() + 8,
        );
        location.push_str(scheme_can);
        location.push_str("://");
        location.push_str(&host_can);
        if let Some(p) = port {
            location.push(':');
            location.push_str(p);
        }
        location.push_str(&path_can);
        if !query.is_empty() {
            location.push('?');
            location.push_str(query);
        }

        Response::respond(self.status, "").header("Location", location)
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;

    fn redirect(config: serde_json::Value) -> Redirect {
        Redirect::init(&config).expect("init")
    }

    fn invoke(
        mw: &Redirect,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
    ) -> Response {
        let ctx = RequestCtx::new(method, path, query, "203.0.113.9", "example.test", headers);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn hdr(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    fn location(resp: &Response) -> Option<&str> {
        resp.__headers()
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("Location"))
            .map(|(_, v)| v.as_str())
    }

    // ── force_https ────────────────────────────────────────────────────────

    #[test]
    fn force_https_redirects_http_to_https() {
        let mw = redirect(serde_json::json!({ "force_https": true }));
        let resp = invoke(&mw, "GET", "/page", "", &[hdr("Host", "example.com")]);
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 308);
        assert_eq!(location(&resp), Some("https://example.com/page"));
    }

    #[test]
    fn force_https_is_a_noop_when_already_https() {
        let mw = redirect(serde_json::json!({ "force_https": true }));
        let resp = invoke(
            &mw,
            "GET",
            "/page",
            "",
            &[hdr("Host", "example.com"), hdr("X-Forwarded-Proto", "https")],
        );
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn scheme_read_from_first_forwarded_proto_value() {
        let mw = redirect(serde_json::json!({ "force_https": true }));
        // A list "https, http" means the edge saw https — no redirect.
        let resp = invoke(
            &mw,
            "GET",
            "/",
            "",
            &[hdr("Host", "example.com"), hdr("X-Forwarded-Proto", "https, http")],
        );
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn custom_forwarded_proto_header_is_honored() {
        let mw = redirect(serde_json::json!({
            "force_https": true,
            "forwarded_proto_header": "X-Scheme",
        }));
        let resp =
            invoke(&mw, "GET", "/", "", &[hdr("Host", "example.com"), hdr("X-Scheme", "https")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    // ── canonical host ─────────────────────────────────────────────────────

    #[test]
    fn www_to_apex() {
        let mw = redirect(serde_json::json!({ "canonical_host": "apex" }));
        let resp = invoke(&mw, "GET", "/p", "", &[hdr("Host", "www.example.com")]);
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(location(&resp), Some("http://example.com/p"));
    }

    #[test]
    fn apex_already_canonical_continues() {
        let mw = redirect(serde_json::json!({ "canonical_host": "apex" }));
        let resp = invoke(&mw, "GET", "/p", "", &[hdr("Host", "example.com")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn apex_to_www_other_direction() {
        let mw = redirect(serde_json::json!({ "canonical_host": "www" }));
        let resp = invoke(&mw, "GET", "/p", "", &[hdr("Host", "example.com")]);
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(location(&resp), Some("http://www.example.com/p"));
    }

    #[test]
    fn www_already_canonical_continues() {
        let mw = redirect(serde_json::json!({ "canonical_host": "www" }));
        let resp = invoke(&mw, "GET", "/p", "", &[hdr("Host", "www.example.com")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn host_map_exact_match_wins() {
        let mw = redirect(serde_json::json!({
            "host_map": { "old.example.com": "new.example.com" },
        }));
        let resp = invoke(&mw, "GET", "/p", "", &[hdr("Host", "Old.Example.com")]);
        assert_eq!(location(&resp), Some("http://new.example.com/p"));
    }

    #[test]
    fn host_case_only_difference_does_not_redirect() {
        // No policy → an uppercase Host is not forced to lowercase (no loop-y
        // cosmetic redirect).
        let mw = redirect(serde_json::json!({ "force_https": false }));
        let resp = invoke(&mw, "GET", "/p", "", &[hdr("Host", "Example.COM")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    // ── trailing slash ─────────────────────────────────────────────────────

    #[test]
    fn trailing_slash_add() {
        let mw = redirect(serde_json::json!({ "trailing_slash": "add" }));
        let resp = invoke(&mw, "GET", "/blog", "", &[hdr("Host", "example.com")]);
        assert_eq!(location(&resp), Some("http://example.com/blog/"));
    }

    #[test]
    fn trailing_slash_add_skips_file_like_and_root() {
        let mw = redirect(serde_json::json!({ "trailing_slash": "add" }));
        assert_eq!(
            invoke(&mw, "GET", "/style.css", "", &[hdr("Host", "example.com")]).__action(),
            ACTION_CONTINUE
        );
        assert_eq!(
            invoke(&mw, "GET", "/", "", &[hdr("Host", "example.com")]).__action(),
            ACTION_CONTINUE
        );
    }

    #[test]
    fn trailing_slash_strip() {
        let mw = redirect(serde_json::json!({ "trailing_slash": "strip" }));
        let resp = invoke(&mw, "GET", "/blog/", "", &[hdr("Host", "example.com")]);
        assert_eq!(location(&resp), Some("http://example.com/blog"));
    }

    #[test]
    fn trailing_slash_strip_keeps_root() {
        let mw = redirect(serde_json::json!({ "trailing_slash": "strip" }));
        let resp = invoke(&mw, "GET", "/", "", &[hdr("Host", "example.com")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    // ── query preservation, status, combined rules ─────────────────────────

    #[test]
    fn query_string_is_preserved() {
        let mw = redirect(serde_json::json!({ "force_https": true }));
        let resp = invoke(&mw, "GET", "/s", "q=1&x=2", &[hdr("Host", "example.com")]);
        assert_eq!(location(&resp), Some("https://example.com/s?q=1&x=2"));
    }

    #[test]
    fn status_301_selection() {
        let mw = redirect(serde_json::json!({ "force_https": true, "status": 301 }));
        let resp = invoke(&mw, "GET", "/", "", &[hdr("Host", "example.com")]);
        assert_eq!(resp.__status(), 301);
    }

    #[test]
    fn default_status_is_308() {
        let mw = redirect(serde_json::json!({ "force_https": true }));
        let resp = invoke(&mw, "GET", "/", "", &[hdr("Host", "example.com")]);
        assert_eq!(resp.__status(), 308);
    }

    #[test]
    fn all_rules_collapse_into_one_redirect() {
        let mw = redirect(serde_json::json!({
            "force_https": true,
            "canonical_host": "apex",
            "trailing_slash": "add",
        }));
        let resp = invoke(&mw, "GET", "/blog", "page=2", &[hdr("Host", "www.example.com")]);
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(location(&resp), Some("https://example.com/blog/?page=2"));
    }

    #[test]
    fn port_is_preserved() {
        let mw = redirect(serde_json::json!({ "canonical_host": "apex" }));
        let resp = invoke(&mw, "GET", "/p", "", &[hdr("Host", "www.example.com:8080")]);
        assert_eq!(location(&resp), Some("http://example.com:8080/p"));
    }

    #[test]
    fn already_canonical_no_config_continues() {
        let mw = redirect(serde_json::Value::Null);
        let resp = invoke(&mw, "GET", "/p", "a=1", &[hdr("Host", "example.com")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn missing_host_header_passes_through() {
        let mw = redirect(serde_json::json!({ "force_https": true }));
        let resp = invoke(&mw, "GET", "/p", "", &[]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    // ── config validation ──────────────────────────────────────────────────

    #[test]
    fn bad_config_fails_init() {
        assert!(Redirect::init(&serde_json::json!({ "status": 302 })).is_err());
        assert!(Redirect::init(&serde_json::json!({ "canonical_host": "root" })).is_err());
        assert!(Redirect::init(&serde_json::json!({ "trailing_slash": "keep" })).is_err());
        assert!(Redirect::init(&serde_json::json!({ "force_https": "yes" })).is_err());
        assert!(Redirect::init(&serde_json::json!({ "host_map": { "a": 1 } })).is_err());
        assert!(Redirect::init(&serde_json::json!({ "forwarded_proto_header": "" })).is_err());
    }
}

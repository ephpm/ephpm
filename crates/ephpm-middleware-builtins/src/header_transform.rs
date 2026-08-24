//! `header-transform` — ePHPm native middleware that rewrites request headers
//! seen by PHP and response headers sent to the client.
//!
//! Analogous to Traefik's `headers` (`customRequestHeaders` /
//! `customResponseHeaders`), Kong's request/response transformer, or nginx's
//! `proxy_set_header` / `add_header` / `more_clear_headers`.
//!
//! # Two phases
//!
//! - **Request phase** ([`Middleware::invoke`]) — set request headers before
//!   PHP runs (PHP reads them as `$_SERVER['HTTP_<NAME>']`).
//! - **Response phase** ([`ResponseMiddleware::invoke_response`]) — set or
//!   remove response headers on the way out, on **every** response (PHP,
//!   static file, error page).
//!
//! Configuration (`[[middleware]] config = { ... }`), all optional:
//!
//! ```toml
//! [middleware.config.request]
//! set = { "X-Env" = "prod", "X-Tenant" = "acme" }
//!
//! [middleware.config.response]
//! set    = { "X-Served-By" = "ephpm" }
//! remove = ["Server", "X-Powered-By"]
//! ```
//!
//! | section | key | effect |
//! |---------|-----|--------|
//! | `request` | `set` (object) | replace-or-add each request header PHP sees |
//! | `response` | `set` (object) | replace-or-add each response header |
//! | `response` | `remove` (array) | delete each response header (case-insensitive) |
//!
//! # v1 ABI scope (why there is no `add` or request `remove`)
//!
//! The host applies both request-header overrides and response `set` as
//! **replace-or-add** (one occurrence, case-insensitive) — there is no
//! duplicate-append primitive in the v1 middleware ABI, so `add` and `set`
//! would be identical; only `set` is offered. And the request phase can only
//! *override* a request header, not delete one, so request-side `remove` is
//! not offered (it would be a silent no-op). Header **removal is response-side
//! only**, where the ABI supports it. Both are honest reflections of the ABI,
//! not omissions.

use ephpm_middleware::{Middleware, Request, Response, ResponseMiddleware, ResponseView};

/// A parsed set of `(name, value)` header assignments, order preserved.
type Assignments = Vec<(String, String)>;

/// Header rewrite policy, built once at `init`.
pub struct HeaderTransform {
    request_set: Assignments,
    response_set: Assignments,
    response_remove: Vec<String>,
}

/// Parse an optional `{ name: value, ... }` object into ordered string pairs.
/// Rejects non-string values and empty names.
fn parse_set(section: &serde_json::Value, path: &str) -> Result<Assignments, String> {
    match section.get("set") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Object(map)) => {
            let mut out = Vec::with_capacity(map.len());
            for (name, value) in map {
                if name.is_empty() {
                    return Err(format!("`{path}.set` has an empty header name"));
                }
                let v = value.as_str().ok_or_else(|| {
                    format!("`{path}.set` values must be strings, got {value} for `{name}`")
                })?;
                out.push((name.clone(), v.to_owned()));
            }
            Ok(out)
        }
        Some(other) => Err(format!("`{path}.set` must be an object, got {other}")),
    }
}

/// Parse an optional `["Name", ...]` array of header names to remove.
fn parse_remove(section: &serde_json::Value, path: &str) -> Result<Vec<String>, String> {
    match section.get("remove") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let name = item.as_str().ok_or_else(|| {
                    format!("`{path}.remove` entries must be strings, got {item}")
                })?;
                if name.is_empty() {
                    return Err(format!("`{path}.remove` has an empty header name"));
                }
                out.push(name.to_owned());
            }
            Ok(out)
        }
        Some(other) => Err(format!("`{path}.remove` must be an array, got {other}")),
    }
}

/// Fetch a top-level section object (`request` / `response`), defaulting to a
/// JSON null (an empty section) when absent. Rejects a non-object section.
fn section<'a>(config: &'a serde_json::Value, key: &str) -> Result<&'a serde_json::Value, String> {
    const NULL: serde_json::Value = serde_json::Value::Null;
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(&NULL),
        Some(v @ serde_json::Value::Object(_)) => Ok(v),
        Some(other) => Err(format!("`{key}` must be an object, got {other}")),
    }
}

impl Middleware for HeaderTransform {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let request = section(config, "request")?;
        let response = section(config, "response")?;

        // `request.remove` cannot be honored (see module docs); reject it loudly
        // rather than silently ignore it.
        if request.get("remove").is_some_and(|v| !v.is_null()) {
            return Err("`request.remove` is not supported: the v1 ABI request phase can \
                 only set/override request headers, not delete them"
                .into());
        }

        Ok(Self {
            request_set: parse_set(request, "request")?,
            response_set: parse_set(response, "response")?,
            response_remove: parse_remove(response, "response")?,
        })
    }

    fn invoke(&self, _req: &Request<'_>) -> Response {
        if self.request_set.is_empty() {
            return Response::cont();
        }
        let mut r = Response::rewrite();
        for (name, value) in &self.request_set {
            r = r.header(name.clone(), value.clone());
        }
        r
    }
}

impl ResponseMiddleware for HeaderTransform {
    fn invoke_response(&self, _req: &Request<'_>, resp: &mut ResponseView<'_>) {
        for name in &self.response_remove {
            resp.remove_header(name.clone());
        }
        for (name, value) in &self.response_set {
            resp.set_header(name.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request / Response views by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_REWRITE};
    use ephpm_middleware::host::{RequestCtx, ResponseCtx, host_table};

    use super::*;

    fn init(config: serde_json::Value) -> HeaderTransform {
        HeaderTransform::init(&config).expect("init")
    }

    fn hdr(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    fn invoke(mw: &HeaderTransform) -> Response {
        let ctx = RequestCtx::new("GET", "/index.php", "", "203.0.113.9", "example.test", &[]);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn invoke_response(
        mw: &HeaderTransform,
        resp_headers: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        let ctx = RequestCtx::new("GET", "/", "", "203.0.113.9", "example.test", &[]);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        let mut rctx = ResponseCtx::new(200, resp_headers, b"body".to_vec());
        {
            // SAFETY: `rctx` outlives the view; host_table() is 'static.
            let mut view = unsafe { ResponseView::from_raw(rctx.as_ptr(), host_table()) };
            mw.invoke_response(&req, &mut view);
            let (status, body, set, remove) = view.__into_parts();
            for name in remove {
                rctx.remove_header(&name);
            }
            for (n, v) in set {
                rctx.set_header(&n, &v);
            }
            if let Some(s) = status {
                rctx.set_status(s);
            }
            if let Some(b) = body {
                rctx.replace_body(b);
            }
        }
        let (_status, headers, _body) = rctx.into_parts();
        headers
    }

    fn get<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    // ── request phase ─────────────────────────────────────────────────────

    #[test]
    fn request_set_injects_headers() {
        let mw = init(serde_json::json!({
            "request": { "set": { "X-Env": "prod", "X-Tenant": "acme" } }
        }));
        let resp = invoke(&mw);
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(get(resp.__headers(), "X-Env"), Some("prod"));
        assert_eq!(get(resp.__headers(), "X-Tenant"), Some("acme"));
    }

    #[test]
    fn no_request_config_continues() {
        let mw = init(serde_json::json!({
            "response": { "set": { "X-A": "b" } }
        }));
        assert_eq!(invoke(&mw).__action(), ACTION_CONTINUE);
    }

    // ── response phase ────────────────────────────────────────────────────

    #[test]
    fn response_set_replaces_or_adds() {
        let mw = init(serde_json::json!({
            "response": { "set": { "X-Served-By": "ephpm", "Content-Type": "text/plain" } }
        }));
        let out = invoke_response(&mw, vec![hdr("Content-Type", "text/html")]);
        assert_eq!(get(&out, "X-Served-By"), Some("ephpm"));
        // Replace, not duplicate.
        assert_eq!(get(&out, "Content-Type"), Some("text/plain"));
        assert_eq!(out.iter().filter(|(n, _)| n.eq_ignore_ascii_case("Content-Type")).count(), 1);
    }

    #[test]
    fn response_remove_deletes() {
        let mw = init(serde_json::json!({
            "response": { "remove": ["Server", "X-Powered-By"] }
        }));
        let out = invoke_response(
            &mw,
            vec![hdr("Server", "nginx"), hdr("X-Powered-By", "PHP/8.5"), hdr("X-Keep", "1")],
        );
        assert_eq!(get(&out, "Server"), None);
        assert_eq!(get(&out, "X-Powered-By"), None);
        assert_eq!(get(&out, "X-Keep"), Some("1"));
    }

    #[test]
    fn remove_then_set_same_header_nets_set() {
        let mw = init(serde_json::json!({
            "response": { "set": { "Server": "ephpm" }, "remove": ["Server"] }
        }));
        let out = invoke_response(&mw, vec![hdr("Server", "nginx")]);
        assert_eq!(get(&out, "Server"), Some("ephpm"));
        assert_eq!(out.iter().filter(|(n, _)| n.eq_ignore_ascii_case("Server")).count(), 1);
    }

    // ── config validation ─────────────────────────────────────────────────

    #[test]
    fn request_remove_is_rejected() {
        assert!(
            HeaderTransform::init(&serde_json::json!({
                "request": { "remove": ["X-Foo"] }
            }))
            .is_err()
        );
    }

    #[test]
    fn bad_config_fails_init() {
        assert!(
            HeaderTransform::init(&serde_json::json!({ "request": { "set": { "X": 1 } } }))
                .is_err()
        );
        assert!(
            HeaderTransform::init(&serde_json::json!({ "response": { "remove": [42] } })).is_err()
        );
        assert!(HeaderTransform::init(&serde_json::json!({ "request": "nope" })).is_err());
        assert!(HeaderTransform::init(&serde_json::json!({ "response": { "set": [] } })).is_err());
    }

    #[test]
    fn empty_config_is_a_noop() {
        let mw = init(serde_json::Value::Null);
        assert_eq!(invoke(&mw).__action(), ACTION_CONTINUE);
        let out = invoke_response(&mw, vec![hdr("X-A", "b")]);
        assert_eq!(get(&out, "X-A"), Some("b"));
    }
}

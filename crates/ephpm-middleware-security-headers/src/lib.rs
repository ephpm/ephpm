//! `security-headers` — ePHPm native middleware that appends standard
//! security headers to every client response.
//!
//! The chain verdict is always `CONTINUE`: PHP runs normally and the headers
//! ride along on whatever response it produces.
//!
//! Configuration (`[[middleware]] config = { ... }`), all optional:
//!
//! | key | default | header |
//! |-----|---------|--------|
//! | `hsts` (bool) | `true` | `Strict-Transport-Security: max-age=63072000; includeSubDomains` |
//! | `csp` (string) | unset | `Content-Security-Policy` |
//! | `frame_options` (string) | `"DENY"` | `X-Frame-Options` (empty string disables) |
//! | `content_type_options` (bool) | `true` | `X-Content-Type-Options: nosniff` |
//! | `referrer_policy` (string) | `"strict-origin-when-cross-origin"` | `Referrer-Policy` (empty string disables) |

use ephpm_middleware::{Middleware, Request, Response, declare};

/// Configured security-header set, built once at `init`.
pub struct SecurityHeaders {
    hsts: bool,
    csp: Option<String>,
    frame_options: Option<String>,
    content_type_options: bool,
    referrer_policy: Option<String>,
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
fn opt_string(
    config: &serde_json::Value,
    key: &str,
    default: Option<&str>,
) -> Result<Option<String>, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default.map(str::to_owned)),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("`{key}` must be a string, got {other}")),
    }
}

impl Middleware for SecurityHeaders {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        Ok(Self {
            hsts: opt_bool(config, "hsts", true)?,
            csp: opt_string(config, "csp", None)?.filter(|s| !s.is_empty()),
            frame_options: opt_string(config, "frame_options", Some("DENY"))?
                .filter(|s| !s.is_empty()),
            content_type_options: opt_bool(config, "content_type_options", true)?,
            referrer_policy: opt_string(
                config,
                "referrer_policy",
                Some("strict-origin-when-cross-origin"),
            )?
            .filter(|s| !s.is_empty()),
        })
    }

    fn invoke(&self, _req: &Request<'_>) -> Response {
        let mut r = Response::cont();
        if self.hsts {
            r = r.response_header(
                "Strict-Transport-Security",
                "max-age=63072000; includeSubDomains",
            );
        }
        if let Some(csp) = &self.csp {
            r = r.response_header("Content-Security-Policy", csp.as_str());
        }
        if let Some(fo) = &self.frame_options {
            r = r.response_header("X-Frame-Options", fo.as_str());
        }
        if self.content_type_options {
            r = r.response_header("X-Content-Type-Options", "nosniff");
        }
        if let Some(rp) = &self.referrer_policy {
            r = r.response_header("Referrer-Policy", rp.as_str());
        }
        r
    }
}

declare!(SecurityHeaders);

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::ACTION_CONTINUE;
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;

    fn ctx() -> RequestCtx {
        RequestCtx::new("GET", "/index.php", "", "203.0.113.9", "example.test", &[])
    }

    fn invoke_with(config: serde_json::Value) -> Response {
        let mw = SecurityHeaders::init(&config).expect("init");
        let ctx = ctx();
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn header_value<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
        resp.__response_headers()
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn defaults_emit_four_headers_and_continue() {
        let resp = invoke_with(serde_json::Value::Null);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
        assert_eq!(
            header_value(&resp, "Strict-Transport-Security"),
            Some("max-age=63072000; includeSubDomains")
        );
        assert_eq!(header_value(&resp, "X-Frame-Options"), Some("DENY"));
        assert_eq!(header_value(&resp, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(header_value(&resp, "Referrer-Policy"), Some("strict-origin-when-cross-origin"));
        assert_eq!(header_value(&resp, "Content-Security-Policy"), None);
        assert_eq!(resp.__response_headers().len(), 4);
    }

    #[test]
    fn csp_is_emitted_when_configured() {
        let resp = invoke_with(serde_json::json!({ "csp": "default-src 'self'" }));
        assert_eq!(header_value(&resp, "Content-Security-Policy"), Some("default-src 'self'"));
    }

    #[test]
    fn knobs_disable_individual_headers() {
        let resp = invoke_with(serde_json::json!({
            "hsts": false,
            "frame_options": "",
            "content_type_options": false,
            "referrer_policy": "",
        }));
        assert_eq!(resp.__action(), ACTION_CONTINUE);
        assert!(resp.__response_headers().is_empty(), "{:?}", resp.__response_headers());
    }

    #[test]
    fn frame_options_value_is_respected() {
        let resp = invoke_with(serde_json::json!({ "frame_options": "SAMEORIGIN" }));
        assert_eq!(header_value(&resp, "X-Frame-Options"), Some("SAMEORIGIN"));
    }

    #[test]
    fn wrong_typed_config_fails_init() {
        assert!(SecurityHeaders::init(&serde_json::json!({ "hsts": "yes" })).is_err());
        assert!(SecurityHeaders::init(&serde_json::json!({ "csp": 42 })).is_err());
    }
}

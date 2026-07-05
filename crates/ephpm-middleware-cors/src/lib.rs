//! `cors` — ePHPm native middleware implementing CORS.
//!
//! Preflight requests (`OPTIONS` with `Access-Control-Request-Method`) from
//! an allowed origin are answered directly with `204` — PHP never runs.
//! Other requests from an allowed origin `CONTINUE` to PHP with
//! `Access-Control-Allow-Origin` (and friends) appended to the eventual
//! response. Requests without an `Origin` header, or from an origin not in
//! the allow list, pass through untouched (per spec: no CORS headers).
//!
//! Configuration (`[[middleware]] config = { ... }`):
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `allow_origins` (array of strings) | **required** | allowed origins; `"*"` allows all |
//! | `allow_methods` (string) | `"GET, POST, PUT, PATCH, DELETE, OPTIONS"` | preflight `Access-Control-Allow-Methods` |
//! | `allow_headers` (string) | `"Content-Type, Authorization"` | preflight `Access-Control-Allow-Headers` |
//! | `allow_credentials` (bool) | `false` | emit `Access-Control-Allow-Credentials: true` (and echo the origin instead of `*`) |
//! | `max_age` (integer seconds) | `86400` | preflight `Access-Control-Max-Age` |

use ephpm_middleware::{Middleware, Request, Response, declare};

/// Default `Access-Control-Allow-Methods` value.
const DEFAULT_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";
/// Default `Access-Control-Allow-Headers` value.
const DEFAULT_HEADERS: &str = "Content-Type, Authorization";

/// CORS policy, built once at `init`.
pub struct Cors {
    allow_origins: Vec<String>,
    /// True when `allow_origins` contains `"*"`.
    wildcard: bool,
    allow_methods: String,
    allow_headers: String,
    allow_credentials: bool,
    max_age: u64,
}

impl Cors {
    /// The `Access-Control-Allow-Origin` value for an allowed `origin`.
    /// Credentialed responses must echo the origin — `*` is forbidden there.
    fn allow_origin_value<'a>(&'a self, origin: &'a str) -> &'a str {
        if self.wildcard && !self.allow_credentials { "*" } else { origin }
    }
}

impl Middleware for Cors {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let origins = config
            .get("allow_origins")
            .ok_or("`allow_origins` is required (array of origins; \"*\" allows all)")?;
        let origins = origins
            .as_array()
            .ok_or_else(|| format!("`allow_origins` must be an array, got {origins}"))?;
        let allow_origins: Vec<String> = origins
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`allow_origins` entries must be strings, got {v}"))
            })
            .collect::<Result<_, _>>()?;
        if allow_origins.is_empty() {
            return Err("`allow_origins` must not be empty".into());
        }

        let string_or = |key: &str, default: &str| -> Result<String, String> {
            match config.get(key) {
                None | Some(serde_json::Value::Null) => Ok(default.to_owned()),
                Some(serde_json::Value::String(s)) => Ok(s.clone()),
                Some(other) => Err(format!("`{key}` must be a string, got {other}")),
            }
        };
        let allow_credentials = match config.get("allow_credentials") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(format!("`allow_credentials` must be a boolean, got {other}"));
            }
        };
        let max_age = match config.get("max_age") {
            None | Some(serde_json::Value::Null) => 86_400,
            Some(v) => v
                .as_u64()
                .ok_or_else(|| format!("`max_age` must be a non-negative integer, got {v}"))?,
        };

        Ok(Self {
            wildcard: allow_origins.iter().any(|o| o == "*"),
            allow_origins,
            allow_methods: string_or("allow_methods", DEFAULT_METHODS)?,
            allow_headers: string_or("allow_headers", DEFAULT_HEADERS)?,
            allow_credentials,
            max_age,
        })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        // Not a cross-origin request: nothing to do.
        let Some(origin) = req.header("Origin") else {
            return Response::cont();
        };
        // Origin not allowed: per spec, simply omit the CORS headers — the
        // browser enforces the failure; the server stays silent.
        if !self.wildcard && !self.allow_origins.iter().any(|o| o == origin) {
            return Response::cont();
        }
        let allow_origin = self.allow_origin_value(origin);

        // Preflight: answer directly, PHP never runs.
        if req.method().eq_ignore_ascii_case("OPTIONS")
            && req.header("Access-Control-Request-Method").is_some()
        {
            let mut r = Response::respond(204, "")
                .header("Access-Control-Allow-Origin", allow_origin)
                .header("Access-Control-Allow-Methods", self.allow_methods.as_str())
                .header("Access-Control-Allow-Headers", self.allow_headers.as_str())
                .header("Access-Control-Max-Age", self.max_age.to_string())
                .header("Vary", "Origin");
            if self.allow_credentials {
                r = r.header("Access-Control-Allow-Credentials", "true");
            }
            return r;
        }

        // Actual request: continue to PHP, appending the CORS headers to the
        // eventual response.
        let mut r = Response::cont()
            .response_header("Access-Control-Allow-Origin", allow_origin)
            .response_header("Vary", "Origin");
        if self.allow_credentials {
            r = r.response_header("Access-Control-Allow-Credentials", "true");
        }
        r
    }
}

declare!(Cors);

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;

    fn cors(config: serde_json::Value) -> Cors {
        Cors::init(&config).expect("init")
    }

    fn invoke(mw: &Cors, method: &str, headers: &[(String, String)]) -> Response {
        let ctx = RequestCtx::new(method, "/api/x", "", "203.0.113.9", "example.test", headers);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn hdr(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    fn find<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    #[test]
    fn init_requires_allow_origins() {
        assert!(Cors::init(&serde_json::Value::Null).is_err());
        assert!(Cors::init(&serde_json::json!({ "allow_origins": [] })).is_err());
        assert!(Cors::init(&serde_json::json!({ "allow_origins": "https://a" })).is_err());
    }

    #[test]
    fn no_origin_header_passes_through() {
        let mw = cors(serde_json::json!({ "allow_origins": ["*"] }));
        let resp = invoke(&mw, "GET", &[]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
        assert!(resp.__response_headers().is_empty());
    }

    #[test]
    fn disallowed_origin_gets_no_cors_headers() {
        let mw = cors(serde_json::json!({ "allow_origins": ["https://good.test"] }));
        let resp = invoke(&mw, "GET", &[hdr("Origin", "https://evil.test")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
        assert!(resp.__response_headers().is_empty());
    }

    #[test]
    fn allowed_origin_is_echoed_on_actual_request() {
        let mw = cors(serde_json::json!({ "allow_origins": ["https://good.test"] }));
        let resp = invoke(&mw, "GET", &[hdr("Origin", "https://good.test")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
        let rh = resp.__response_headers();
        assert_eq!(find(rh, "Access-Control-Allow-Origin"), Some("https://good.test"));
        assert_eq!(find(rh, "Vary"), Some("Origin"));
        assert_eq!(find(rh, "Access-Control-Allow-Credentials"), None);
    }

    #[test]
    fn wildcard_origin_without_credentials_is_star() {
        let mw = cors(serde_json::json!({ "allow_origins": ["*"] }));
        let resp = invoke(&mw, "GET", &[hdr("Origin", "https://any.test")]);
        assert_eq!(find(resp.__response_headers(), "Access-Control-Allow-Origin"), Some("*"));
    }

    #[test]
    fn wildcard_with_credentials_echoes_the_origin() {
        let mw = cors(serde_json::json!({ "allow_origins": ["*"], "allow_credentials": true }));
        let resp = invoke(&mw, "GET", &[hdr("Origin", "https://any.test")]);
        let rh = resp.__response_headers();
        assert_eq!(find(rh, "Access-Control-Allow-Origin"), Some("https://any.test"));
        assert_eq!(find(rh, "Access-Control-Allow-Credentials"), Some("true"));
    }

    #[test]
    fn preflight_responds_204_with_policy_headers() {
        let mw = cors(serde_json::json!({
            "allow_origins": ["https://good.test"],
            "max_age": 600,
        }));
        let resp = invoke(
            &mw,
            "OPTIONS",
            &[hdr("Origin", "https://good.test"), hdr("Access-Control-Request-Method", "PUT")],
        );
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 204);
        let h = resp.__headers();
        assert_eq!(find(h, "Access-Control-Allow-Origin"), Some("https://good.test"));
        assert_eq!(find(h, "Access-Control-Allow-Methods"), Some(DEFAULT_METHODS));
        assert_eq!(find(h, "Access-Control-Allow-Headers"), Some(DEFAULT_HEADERS));
        assert_eq!(find(h, "Access-Control-Max-Age"), Some("600"));
        assert_eq!(find(h, "Vary"), Some("Origin"));
    }

    #[test]
    fn options_without_request_method_is_not_a_preflight() {
        let mw = cors(serde_json::json!({ "allow_origins": ["*"] }));
        let resp = invoke(&mw, "OPTIONS", &[hdr("Origin", "https://any.test")]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }
}

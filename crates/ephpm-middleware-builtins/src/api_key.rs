//! `api-key` — ePHPm native middleware validating an API key on the request
//! before PHP runs, then forwarding the resolved **consumer identity** to PHP.
//!
//! Analogous to Kong's `key-auth`, AWS API Gateway API keys / usage plans, and
//! Tyk: a request carrying a recognised key is admitted and tagged with the
//! consumer it belongs to; a request with a missing or unrecognised key is
//! short-circuited with `401` and PHP never runs.
//!
//! The key is read from a configurable request header (default `X-Api-Key`)
//! and, only when explicitly enabled, from a query parameter (default off —
//! see the security note). It is validated against either a static
//! `key → consumer-id` map baked into the config, a KV lookup (`kv_get` on a
//! `kv_key_template` like `apikey:<key>` whose value is the consumer id), or
//! both (the static map is consulted first). On success the module `REWRITE`s
//! the request, injecting the consumer id in a header (default
//! `X-Consumer-Id`) that PHP reads — the exact mechanism `jwt` uses to forward
//! claims. The injected header **overwrites** any same-named header the client
//! sent (the host's `override_header` replaces, not appends), so a client
//! cannot spoof its consumer identity.
//!
//! ## Security
//!
//! * **Constant-time comparison.** Static keys are compared with a
//!   constant-time equality check (`subtle::ConstantTimeEq`) so the match does
//!   not leak how many leading bytes were correct — closing the timing oracle
//!   that a naive `==` would open. All configured keys are compared on every
//!   request (no early return on the first match). Only the *lengths* of keys
//!   can differ in timing, which is not a practical attack surface. The KV
//!   path is an exact-key store lookup and does not compare secrets in Rust.
//! * **The key value is never logged.** This module emits no logs containing
//!   the presented key.
//! * **Query parameter is off by default.** Query strings routinely end up in
//!   access logs, proxy logs, browser history and `Referer` headers, so a key
//!   in the URL leaks far more readily than one in a header. Enable
//!   `query_param` only when a client genuinely cannot set a header.
//! * **Composes with `ratelimit`.** Point the `ratelimit` module's
//!   `key_headers` at the same header (e.g. `["X-Api-Key"]`) to get per-key
//!   rate limiting in front of, or alongside, this auth gate.
//!
//! Configuration (`[[middleware]] config = { ... }`):
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `header` (string) | `"X-Api-Key"` | request header carrying the key |
//! | `query_param` (string) | unset (disabled) | also accept the key from this query parameter — see the security note |
//! | `keys` (object) | unset | static `key → consumer-id` map |
//! | `kv_key_template` (string) | unset | KV lookup key with a `<key>` placeholder, e.g. `apikey:<key>`; the value is the consumer id |
//! | `consumer_header` (string) | `"X-Consumer-Id"` | header injected for PHP with the resolved consumer id |
//!
//! At least one of `keys` / `kv_key_template` must be configured.

use ephpm_middleware::{Middleware, Request, Response};
use subtle::ConstantTimeEq;

/// The literal replaced with the presented key in `kv_key_template`.
const KEY_PLACEHOLDER: &str = "<key>";

/// API-key validation policy, built once at `init`.
pub struct ApiKey {
    header: String,
    query_param: Option<String>,
    consumer_header: String,
    /// Static `key → consumer-id` entries. Keys are stored as bytes for the
    /// constant-time comparison.
    keys: Vec<(Vec<u8>, String)>,
    /// KV lookup template containing [`KEY_PLACEHOLDER`], e.g. `apikey:<key>`.
    kv_key_template: Option<String>,
}

/// Constant-time byte-slice equality. Wraps [`subtle::ConstantTimeEq`] so the
/// comparison does not short-circuit on the first differing byte (unequal
/// lengths still return `false` fast, leaking only length). This is the helper
/// the static-key match uses; it is unit-tested directly.
#[must_use]
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// URL-decode a query-string component (`+` → space, `%XX` → byte). Invalid
/// escapes are passed through literally rather than failing the lookup.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Return the (decoded) value of query parameter `name` in `query`, if present.
fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == name).then(|| percent_decode(v))
    })
}

impl ApiKey {
    /// Extract the presented key: the configured header first, then the query
    /// parameter when enabled. Empty values count as absent.
    fn extract_key(&self, req: &Request<'_>) -> Option<String> {
        if let Some(v) = req.header(&self.header) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
        if let Some(param) = &self.query_param
            && let Some(v) = query_value(req.query(), param)
            && !v.is_empty()
        {
            return Some(v);
        }
        None
    }

    /// Constant-time match of `presented` against the static key map. Every
    /// entry is compared (no early return) so the number of matching leading
    /// bytes is not observable via timing.
    fn match_static(&self, presented: &[u8]) -> Option<&str> {
        let mut matched: Option<&str> = None;
        for (key, consumer) in &self.keys {
            if ct_eq(presented, key) {
                matched = Some(consumer.as_str());
            }
        }
        matched
    }

    /// Look the presented key up in the KV store via `kv_key_template`. The
    /// stored value (UTF-8, non-empty) is the consumer id.
    fn match_kv(&self, req: &Request<'_>, presented: &str) -> Option<String> {
        let template = self.kv_key_template.as_ref()?;
        let lookup = template.replace(KEY_PLACEHOLDER, presented);
        let value = req.host().kv_get(&lookup)?;
        let consumer = String::from_utf8(value).ok()?;
        (!consumer.is_empty()).then_some(consumer)
    }

    /// Admit the request, injecting the consumer id for PHP (mirrors how `jwt`
    /// forwards its claims via a request header).
    fn grant(&self, consumer: &str) -> Response {
        Response::rewrite().header(self.consumer_header.as_str(), consumer)
    }

    /// `401` with a `WWW-Authenticate`-style hint naming the expected header.
    /// The key value is deliberately absent from the body.
    fn unauthorized(&self, body: &'static str) -> Response {
        Response::respond(401, body)
            .header("WWW-Authenticate", format!("ApiKey header=\"{}\"", self.header))
    }
}

impl Middleware for ApiKey {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let opt_str = |key: &str| -> Result<Option<String>, String> {
            match config.get(key) {
                Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                None | Some(serde_json::Value::Null | serde_json::Value::String(_)) => Ok(None),
                Some(other) => Err(format!("`{key}` must be a string, got {other}")),
            }
        };

        let header = opt_str("header")?.unwrap_or_else(|| "X-Api-Key".to_owned());
        let query_param = opt_str("query_param")?;
        let consumer_header =
            opt_str("consumer_header")?.unwrap_or_else(|| "X-Consumer-Id".to_owned());

        let keys = match config.get("keys") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(v) => {
                let map = v.as_object().ok_or("`keys` must be an object of key -> consumer-id")?;
                map.iter()
                    .map(|(key, consumer)| {
                        if key.is_empty() {
                            return Err("`keys` entries must have a non-empty key".to_owned());
                        }
                        let consumer = consumer.as_str().ok_or_else(|| {
                            format!("`keys[\"{key}\"]` must be a string consumer-id")
                        })?;
                        Ok((key.as_bytes().to_vec(), consumer.to_owned()))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
        };

        let kv_key_template = opt_str("kv_key_template")?;
        if let Some(template) = &kv_key_template
            && !template.contains(KEY_PLACEHOLDER)
        {
            return Err(format!(
                "`kv_key_template` must contain the `{KEY_PLACEHOLDER}` placeholder"
            ));
        }

        if keys.is_empty() && kv_key_template.is_none() {
            return Err("at least one of `keys` or `kv_key_template` must be configured".into());
        }

        Ok(Self { header, query_param, consumer_header, keys, kv_key_template })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        let Some(key) = self.extract_key(req) else {
            return self.unauthorized("missing api key");
        };
        if let Some(consumer) = self.match_static(key.as_bytes()) {
            return self.grant(consumer);
        }
        if let Some(consumer) = self.match_kv(req, &key) {
            return self.grant(&consumer);
        }
        self.unauthorized("invalid api key")
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_RESPOND, ACTION_REWRITE};
    use ephpm_middleware::host::{RequestCtx, host_table, set_kv_store};

    use super::*;

    fn api_key(config: serde_json::Value) -> ApiKey {
        ApiKey::init(&config).expect("init")
    }

    /// Invoke with headers and an optional query string against a fresh ctx.
    fn invoke_q(mw: &ApiKey, query: &str, headers: &[(String, String)]) -> Response {
        let ctx = RequestCtx::new("GET", "/api/x", query, "203.0.113.9", "example.test", headers);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn invoke(mw: &ApiKey, headers: &[(String, String)]) -> Response {
        invoke_q(mw, "", headers)
    }

    fn hdr(name: &str, value: &str) -> Vec<(String, String)> {
        vec![(name.to_owned(), value.to_owned())]
    }

    /// Wire a real in-memory Store into the host table (first call wins; all
    /// tests in this binary share it) and seed one `apikey:*` entry via the
    /// host's own `kv_set`.
    fn setup_kv_with(entries: &[(&str, &str)]) {
        set_kv_store(&ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default()));
        let ctx = RequestCtx::new("GET", "/", "", "127.0.0.1", "seed", &[]);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        for (k, v) in entries {
            assert!(req.host().kv_set(k, v.as_bytes(), 0), "seed kv_set failed for {k}");
        }
    }

    fn consumer_header(resp: &Response) -> Option<String> {
        resp.__headers()
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("X-Consumer-Id"))
            .map(|(_, v)| v.clone())
    }

    fn assert_401(resp: &Response, body: &str) {
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 401);
        assert_eq!(resp.__body(), body.as_bytes());
        // Never leak the key; always hint via WWW-Authenticate.
        assert!(
            resp.__headers().iter().any(|(n, _)| n.eq_ignore_ascii_case("WWW-Authenticate")),
            "401 must carry a WWW-Authenticate hint",
        );
    }

    #[test]
    fn ct_eq_is_correct() {
        assert!(ct_eq(b"correct-key", b"correct-key"));
        assert!(!ct_eq(b"correct-key", b"correct-keZ"));
        assert!(!ct_eq(b"correct-key", b"correct-ke")); // length mismatch
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"a", b""));
    }

    #[test]
    fn init_requires_a_store() {
        // No keys and no KV template → misconfiguration.
        assert!(ApiKey::init(&serde_json::json!({})).is_err());
        assert!(ApiKey::init(&serde_json::json!({ "header": "X-Api-Key" })).is_err());
        // `keys` must be an object; entries must be string consumer-ids.
        assert!(ApiKey::init(&serde_json::json!({ "keys": "nope" })).is_err());
        assert!(ApiKey::init(&serde_json::json!({ "keys": { "k": 42 } })).is_err());
        // `kv_key_template` must contain the placeholder.
        assert!(ApiKey::init(&serde_json::json!({ "kv_key_template": "apikey:" })).is_err());
        // Valid minimal configs.
        assert!(ApiKey::init(&serde_json::json!({ "keys": { "k": "c" } })).is_ok());
        assert!(ApiKey::init(&serde_json::json!({ "kv_key_template": "apikey:<key>" })).is_ok());
    }

    #[test]
    fn valid_static_key_rewrites_with_consumer() {
        let mw = api_key(serde_json::json!({ "keys": { "secret-abc": "consumer-7" } }));
        let resp = invoke(&mw, &hdr("X-Api-Key", "secret-abc"));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(consumer_header(&resp).as_deref(), Some("consumer-7"));
    }

    #[test]
    fn invalid_static_key_is_401() {
        let mw = api_key(serde_json::json!({ "keys": { "secret-abc": "consumer-7" } }));
        assert_401(&invoke(&mw, &hdr("X-Api-Key", "wrong")), "invalid api key");
    }

    #[test]
    fn missing_key_is_401() {
        let mw = api_key(serde_json::json!({ "keys": { "secret-abc": "consumer-7" } }));
        assert_401(&invoke(&mw, &[]), "missing api key");
        // Present-but-empty header also counts as missing.
        assert_401(&invoke(&mw, &hdr("X-Api-Key", "   ")), "missing api key");
    }

    #[test]
    fn custom_header_and_consumer_header() {
        let mw = api_key(serde_json::json!({
            "header": "X-Key",
            "consumer_header": "X-Who",
            "keys": { "k1": "alice" },
        }));
        let resp = invoke(&mw, &hdr("X-Key", "k1"));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        let who = resp
            .__headers()
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("X-Who"))
            .map(|(_, v)| v.as_str());
        assert_eq!(who, Some("alice"));
    }

    #[test]
    fn query_param_disabled_by_default() {
        let mw = api_key(serde_json::json!({ "keys": { "qk": "qc" } }));
        // Key only in the query string, but query_param is off → 401 missing.
        assert_401(&invoke_q(&mw, "api_key=qk", &[]), "missing api key");
    }

    #[test]
    fn query_param_when_enabled() {
        let mw = api_key(serde_json::json!({
            "query_param": "api_key",
            "keys": { "qk": "qc" },
        }));
        let resp = invoke_q(&mw, "foo=1&api_key=qk&bar=2", &[]);
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(consumer_header(&resp).as_deref(), Some("qc"));
        // Header still takes precedence over the query param.
        let resp = invoke_q(&mw, "api_key=wrong", &hdr("X-Api-Key", "qk"));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(consumer_header(&resp).as_deref(), Some("qc"));
        // URL-encoded value round-trips.
        let mw2 = api_key(serde_json::json!({
            "query_param": "api_key",
            "keys": { "a b": "spaced" },
        }));
        let resp = invoke_q(&mw2, "api_key=a%20b", &[]);
        assert_eq!(consumer_header(&resp).as_deref(), Some("spaced"));
    }

    #[test]
    fn kv_backed_valid_and_invalid() {
        setup_kv_with(&[("apikey:live-key", "kv-consumer-1")]);
        let mw = api_key(serde_json::json!({ "kv_key_template": "apikey:<key>" }));
        // Valid: value in the store is the consumer id.
        let resp = invoke(&mw, &hdr("X-Api-Key", "live-key"));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(consumer_header(&resp).as_deref(), Some("kv-consumer-1"));
        // Absent key → 401 invalid.
        assert_401(&invoke(&mw, &hdr("X-Api-Key", "no-such-key")), "invalid api key");
    }

    #[test]
    fn static_map_takes_precedence_then_kv() {
        setup_kv_with(&[("apikey:kv-only", "from-kv")]);
        let mw = api_key(serde_json::json!({
            "keys": { "static-only": "from-static" },
            "kv_key_template": "apikey:<key>",
        }));
        // Static hit.
        assert_eq!(
            consumer_header(&invoke(&mw, &hdr("X-Api-Key", "static-only"))).as_deref(),
            Some("from-static"),
        );
        // Falls through to KV.
        assert_eq!(
            consumer_header(&invoke(&mw, &hdr("X-Api-Key", "kv-only"))).as_deref(),
            Some("from-kv"),
        );
    }
}

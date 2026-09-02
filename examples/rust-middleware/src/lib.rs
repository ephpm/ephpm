//! `api-gate` — a complete, loadable ePHPm middleware written in Rust.
//!
//! This is the worked example for the **dynamic (dlopen) lane** of the native
//! middleware ABI. It is a real module, not a toy: it is built as a `cdylib`,
//! mounted by path in `[[middleware]]`, `dlopen`ed by a stock ePHPm binary at
//! startup, and it decides every PHP-bound request before the request body is
//! read.
//!
//! It exists because the ABI is easier to use than the C-shim-based
//! [elephc example][elephc] makes it look. A Rust module needs **no shim at
//! all**: [`ephpm_middleware::declare!`] emits the four exported C symbols,
//! the ABI-version handshake, the config parse, panic containment and the
//! response marshaling. Everything below the `declare!` line at the bottom of
//! this file is ordinary safe Rust — there is not one `unsafe` block in the
//! module itself.
//!
//! [elephc]: https://github.com/ephpm/elephc-middleware-example
//!
//! # What it does
//!
//! An API gateway in front of a PHP front controller. Requests under
//! `prefix` must carry a known `X-Api-Key`; everything else is untouched.
//!
//! | Situation | Verdict | Result |
//! |---|---|---|
//! | Path outside `prefix` | `CONTINUE` | PHP runs unchanged; `X-Api-Gate: bypass` appended to the response |
//! | Missing / unknown `X-Api-Key` | `RESPOND` | `401` JSON, PHP never runs |
//! | Key revoked at runtime (KV) | `RESPOND` | `403` JSON, PHP never runs |
//! | Over the request budget | `RESPOND` | `429` JSON + `Retry-After`, PHP never runs |
//! | Accepted | `REWRITE` | version prefix stripped from `REQUEST_URI`, `X-Api-Tenant` injected, PHP runs |
//!
//! All three verdicts are on the hot path, including `RESPOND` — the one that
//! has to marshal a status, a body and headers back out of module memory.
//!
//! # Host callbacks used
//!
//! The host callback table is what makes a native module worth writing, so
//! this example leans on it rather than mentioning it:
//!
//! * [`kv_incr_ttl`] — one atomic call that both increments the window counter
//!   and stamps its TTL *only when the call creates the key*. That is the
//!   whole fixed-window rate limiter, and it becomes **cluster-wide** with no
//!   change to this code when KV replication is on: every node counts into
//!   the same key.
//! * [`kv_get`] — the revocation check. The key lives in the same store PHP
//!   sees through `ephpm_kv_set()`, so an application page can revoke a tenant
//!   and the *next* request through the gate is rejected in native code,
//!   without a restart or a config reload.
//! * `log` — module diagnostics go through the host's `tracing` subscriber and
//!   land in ePHPm's own log stream, correctly levelled.
//!
//! [`kv_incr_ttl`]: ephpm_middleware::Host::kv_incr_ttl
//! [`kv_get`]: ephpm_middleware::Host::kv_get
//!
//! # Two rules the ABI asks of every module
//!
//! Both are handled for you by `declare!`, but a module author should know
//! they exist, because a hand-written C module has to honour them itself:
//!
//! 1. **ABI-major gating.** `ephpm_middleware_init` must refuse a host whose
//!    ABI major is newer than the module was built against — a struct layout
//!    disagreement that loads anyway is memory corruption, not a warning.
//!    `declare!` compares `abi_version >> 24` against
//!    [`ABI_V1`](ephpm_middleware::abi::ABI_V1) and returns `-1` on mismatch.
//! 2. **Pointer lifetime.** Every pointer written into `EphpmResponse` must
//!    stay valid until `invoke` *returns*; the host copies before unwinding.
//!    So a verdict may not hand out pointers into a temporary. `declare!`
//!    parks the marshaled body, path and header strings in a thread-local that
//!    lives until the next `invoke` on that thread. Returning an owned
//!    [`Response`] — as every function below does — is always safe.
//!
//! # Configuration
//!
//! Passed as the `[[middleware]] config` table, serialised to JSON:
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `keys` (object) | **required**, non-empty | `"api key" -> "tenant"` map |
//! | `prefix` (string) | `"/api/"` | only paths with this prefix are gated |
//! | `strip_prefix` (string) | unset | removed from the front of the path on `REWRITE` |
//! | `requests_per_window` (integer) | `100` | budget per tenant per 10-second window |
//!
//! # Failure posture
//!
//! Authentication is **fail-closed**: an unknown key is rejected, and a panic
//! anywhere in `invoke` is converted by `declare!` into a `500` rather than a
//! silent pass. Rate limiting is **fail-open**: if the KV store is
//! unavailable, the request is allowed with a warning. Dropping all traffic
//! because the counter tier hiccuped turns a soft protection into a hard
//! outage — the same trade-off the builtin `ratelimit` module makes.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ephpm_middleware::abi::{LOG_INFO, LOG_WARN};
use ephpm_middleware::{Middleware, Request, Response};

/// Fixed-window length, in seconds. Coarser windows mean less KV churn.
const WINDOW_SECS: u64 = 10;

/// Counter-key TTL: one window plus slack for clock skew between nodes, so a
/// counter can outlive its window slightly but never leak.
const KEY_TTL_SECS: i64 = 30;

/// Default path prefix the gate applies to.
const DEFAULT_PREFIX: &str = "/api/";

/// Default per-tenant request budget per window.
const DEFAULT_BUDGET: i64 = 100;

/// The gate's policy, built once at `init` and shared by every request
/// thread. `Middleware` requires `Send + Sync`, which this is because it is
/// immutable after construction — all mutable state lives in the KV store.
pub struct ApiGate {
    /// Only paths starting with this are gated.
    prefix: String,
    /// Stripped from the front of the path on the accepted (`REWRITE`) path.
    strip_prefix: Option<String>,
    /// API key -> tenant identity.
    keys: HashMap<String, String>,
    /// Requests allowed per tenant per [`WINDOW_SECS`] window.
    budget: i64,
}

impl ApiGate {
    /// KV key holding the revocation marker for `tenant`. Any value present
    /// means revoked; PHP can set it with
    /// `ephpm_kv_set("apigate:revoked:alpha", "1", 3600)`.
    fn revocation_key(tenant: &str) -> String {
        format!("apigate:revoked:{tenant}")
    }

    /// KV key holding this tenant's counter for the current window. The vhost
    /// is part of the key so two sites in one process cannot share a budget —
    /// and it is the router's canonical site key, never the `Host` header,
    /// which a caller could vary to mint a fresh budget (issue #390). On a
    /// multi-tenant node the counter also lands in that vhost's own KV store
    /// (issue #376), so the key component is belt-and-braces.
    fn window_key(vhost: &str, tenant: &str, window: u64) -> String {
        format!("apigate:rl:{vhost}:{tenant}:{window}")
    }

    /// The path PHP should see, with `strip_prefix` removed. Returns `None`
    /// when there is nothing to strip, so the module can skip emitting a
    /// rewrite it does not need.
    fn stripped_path(&self, path: &str) -> Option<String> {
        let strip = self.strip_prefix.as_deref()?;
        let rest = path.strip_prefix(strip)?;
        // Stripping must not produce an empty or relative path.
        Some(if rest.starts_with('/') { rest.to_owned() } else { format!("/{rest}") })
    }

    /// A JSON error body plus the headers every rejection carries.
    fn reject(status: u16, message: &str) -> Response {
        Response::respond(status, format!(r#"{{"error":"{message}"}}"#))
            .header("Content-Type", "application/json")
            .header("X-Api-Gate", "reject")
    }
}

impl Middleware for ApiGate {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let keys_value = config
            .get("keys")
            .ok_or("`keys` is required: an object mapping API key -> tenant name")?;
        let keys_object =
            keys_value.as_object().ok_or("`keys` must be an object of \"key\": \"tenant\"")?;
        if keys_object.is_empty() {
            return Err("`keys` must contain at least one API key".into());
        }
        let keys = keys_object
            .iter()
            .map(|(key, tenant)| {
                tenant
                    .as_str()
                    .map(|t| (key.clone(), t.to_owned()))
                    .ok_or_else(|| format!("tenant for key `{key}` must be a string, got {tenant}"))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        let prefix = match config.get("prefix") {
            None | Some(serde_json::Value::Null) => DEFAULT_PREFIX.to_owned(),
            Some(v) => v.as_str().ok_or("`prefix` must be a string")?.to_owned(),
        };
        let strip_prefix = match config.get("strip_prefix") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(v.as_str().ok_or("`strip_prefix` must be a string")?.to_owned()),
        };
        let budget = match config.get("requests_per_window") {
            None | Some(serde_json::Value::Null) => DEFAULT_BUDGET,
            Some(v) => v.as_i64().ok_or("`requests_per_window` must be an integer")?,
        };
        if budget <= 0 {
            return Err("`requests_per_window` must be > 0".into());
        }

        Ok(Self { prefix, strip_prefix, keys, budget })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        // ── CONTINUE ──────────────────────────────────────────────────────
        // Not our surface. Hand the request on untouched; the response header
        // is appended to whatever PHP ultimately produces.
        if !req.path().starts_with(&self.prefix) {
            return Response::cont().response_header("X-Api-Gate", "bypass");
        }

        let host = req.host();

        // ── RESPOND: authentication (fail-closed) ─────────────────────────
        let Some(tenant) = req.header("X-Api-Key").and_then(|key| self.keys.get(key)) else {
            return Self::reject(401, "missing or unknown X-Api-Key")
                .header("WWW-Authenticate", "ApiKey realm=\"api-gate\"");
        };

        // ── RESPOND: runtime revocation, read from the shared KV store ────
        // Any value means revoked. This is the same store PHP writes through
        // `ephpm_kv_set()`, so revocation takes effect on the next request.
        if host.kv_get(&Self::revocation_key(tenant)).is_some() {
            host.log(LOG_INFO, &format!("api-gate: rejecting revoked tenant `{tenant}`"));
            return Self::reject(403, "api key revoked");
        }

        // ── RESPOND: fixed-window rate limit (fail-open) ──────────────────
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let window = now / WINDOW_SECS;
        // `vhost_id()` is the canonical site key, `None` when the request
        // matched no vhost. Bucket those together rather than trusting the
        // header — otherwise varying `Host` mints a fresh budget.
        let vhost = req.vhost_id().unwrap_or(ephpm_middleware::UNMATCHED_VHOST);
        let key = Self::window_key(vhost, tenant, window);

        // One atomic call: increment, and stamp the TTL only if this call
        // created the key. A counter can therefore never exist without an
        // expiry, and every node in the cluster counts into the same key.
        let remaining = match host.kv_incr_ttl(&key, 1, KEY_TTL_SECS) {
            Some(count) if count > self.budget => {
                let retry_after = WINDOW_SECS - (now % WINDOW_SECS);
                return Self::reject(429, "rate limit exceeded")
                    .header("Retry-After", retry_after.to_string())
                    .header("X-RateLimit-Limit", self.budget.to_string())
                    .header("X-RateLimit-Remaining", "0");
            }
            Some(count) => (self.budget - count).max(0),
            None => {
                host.log(
                    LOG_WARN,
                    &format!("api-gate: KV unavailable — failing open for tenant `{tenant}`"),
                );
                self.budget
            }
        };

        // ── REWRITE: accepted ─────────────────────────────────────────────
        // `header` sets a REQUEST header the PHP side sees as
        // `$_SERVER['HTTP_X_API_TENANT']`; `response_header` adds a header to
        // the client's response. `path` rewrites `REQUEST_URI`.
        //
        // v1 semantics worth knowing: in fpm mode the script has already been
        // resolved by the time the chain runs, so a path rewrite changes
        // `REQUEST_URI` (what a front controller routes on) and not which file
        // executes. In worker mode every request goes through PHP and the
        // booted framework routes on the rewritten `REQUEST_URI`.
        let mut verdict = Response::rewrite()
            .header("X-Api-Tenant", tenant)
            .response_header("X-Api-Gate", "allow")
            .response_header("X-RateLimit-Limit", self.budget.to_string())
            .response_header("X-RateLimit-Remaining", remaining.to_string());
        if let Some(stripped) = self.stripped_path(req.path()) {
            verdict = verdict.path(stripped);
        }
        verdict
    }

    fn shutdown(&self) {
        // Nothing to release — the policy is plain owned data and the KV store
        // belongs to the host. Shown because the ABI calls it at shutdown and
        // a module holding a thread or a file handle must clean up here.
    }

    fn describe() -> &'static str {
        concat!("api-gate ", env!("CARGO_PKG_VERSION"), " (rust middleware example)")
    }
}

// The whole C ABI: four exported symbols, the version handshake, config
// parsing, panic containment and response marshaling. This one line is the
// entire difference between a Rust module and the ~180-line hand-written C
// shim the elephc example needs.
ephpm_middleware::declare!(ApiGate);

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // The tests build the FFI request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND, ACTION_REWRITE};
    use ephpm_middleware::host::{RequestCtx, host_table, set_kv_store};

    use super::*;

    /// Wire a real in-memory KV store into the host table. The table is
    /// process-wide and first-call-wins, so every test uses a unique vhost to
    /// keep its counters to itself.
    fn setup_kv() {
        set_kv_store(&ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default()));
    }

    /// A gate that knows exactly one key, `k-<tenant>`, for `tenant`.
    ///
    /// Every test uses its own tenant name: the host KV store is
    /// process-wide and first-call-wins, so tests running in parallel share
    /// one store, and a shared tenant would let one test's revocation marker
    /// or window counter decide another test's verdict.
    fn gate_for(tenant: &str) -> ApiGate {
        let mut keys = serde_json::Map::new();
        keys.insert(format!("k-{tenant}"), serde_json::Value::String(tenant.to_owned()));
        ApiGate::init(&serde_json::json!({
            "keys": keys,
            "prefix": "/api/",
            "strip_prefix": "/api/v1",
            "requests_per_window": 3,
        }))
        .expect("init")
    }

    fn invoke(mw: &ApiGate, vhost: &str, path: &str, headers: &[(String, String)]) -> Response {
        let ctx = RequestCtx::new("GET", path, "", "198.51.100.7", vhost, headers);
        // SAFETY: `ctx` outlives the borrowed view, and `host_table()` is
        // 'static — exactly the contract `from_raw` documents.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    fn key_header(tenant: &str) -> Vec<(String, String)> {
        vec![("X-Api-Key".to_owned(), format!("k-{tenant}"))]
    }

    fn find<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    #[test]
    fn init_rejects_unusable_config() {
        assert!(ApiGate::init(&serde_json::Value::Null).is_err(), "no `keys`");
        assert!(ApiGate::init(&serde_json::json!({ "keys": {} })).is_err(), "empty `keys`");
        assert!(ApiGate::init(&serde_json::json!({ "keys": [] })).is_err(), "`keys` not an object");
        assert!(
            ApiGate::init(&serde_json::json!({ "keys": { "k": 1 } })).is_err(),
            "tenant not a string"
        );
        assert!(
            ApiGate::init(&serde_json::json!({ "keys": { "k": "t" }, "requests_per_window": 0 }))
                .is_err(),
            "zero budget"
        );
        let mw = ApiGate::init(&serde_json::json!({ "keys": { "k": "t" } })).expect("defaults");
        assert_eq!(mw.prefix, DEFAULT_PREFIX);
        assert_eq!(mw.budget, DEFAULT_BUDGET);
        assert!(mw.strip_prefix.is_none());
    }

    #[test]
    fn off_prefix_paths_continue_untouched() {
        setup_kv();
        let resp = invoke(&gate_for("bypass"), "vh-bypass", "/health.php", &[]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
        assert_eq!(find(resp.__response_headers(), "X-Api-Gate"), Some("bypass"));
        // No key was required, and nothing was rewritten.
        assert!(resp.__rewrite_path().is_none());
    }

    #[test]
    fn missing_or_unknown_key_is_rejected_with_401() {
        setup_kv();
        let mw = gate_for("auth");
        for headers in [vec![], vec![("X-Api-Key".to_owned(), "nope".to_owned())]] {
            let resp = invoke(&mw, "vh-401", "/api/v1/users", &headers);
            assert_eq!(resp.__action(), ACTION_RESPOND);
            assert_eq!(resp.__status(), 401);
            assert_eq!(find(resp.__headers(), "Content-Type"), Some("application/json"));
            assert!(find(resp.__headers(), "WWW-Authenticate").is_some());
        }
    }

    #[test]
    fn accepted_request_rewrites_the_path_and_injects_the_tenant() {
        setup_kv();
        let resp = invoke(&gate_for("rw"), "vh-ok", "/api/v1/users", &key_header("rw"));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert_eq!(resp.__rewrite_path(), Some("/users"));
        // Request-header override -> $_SERVER['HTTP_X_API_TENANT'].
        assert_eq!(find(resp.__headers(), "X-Api-Tenant"), Some("rw"));
        // Client-visible headers.
        assert_eq!(find(resp.__response_headers(), "X-Api-Gate"), Some("allow"));
        assert_eq!(find(resp.__response_headers(), "X-RateLimit-Limit"), Some("3"));
    }

    #[test]
    fn a_path_with_nothing_to_strip_is_still_accepted() {
        setup_kv();
        let resp =
            invoke(&gate_for("nostrip"), "vh-nostrip", "/api/v2/users", &key_header("nostrip"));
        assert_eq!(resp.__action(), ACTION_REWRITE);
        assert!(resp.__rewrite_path().is_none(), "no rewrite is emitted when nothing is stripped");
        assert_eq!(find(resp.__headers(), "X-Api-Tenant"), Some("nostrip"));
    }

    #[test]
    fn revoking_a_tenant_in_the_kv_store_rejects_the_next_request() {
        setup_kv();
        let mw = gate_for("revoke");
        let allowed = invoke(&mw, "vh-revoke", "/api/v1/x", &key_header("revoke"));
        assert_eq!(allowed.__action(), ACTION_REWRITE);

        // Exactly what PHP's `ephpm_kv_set()` writes — same store, same key.
        let ctx = RequestCtx::new("GET", "/api/v1/x", "", "198.51.100.7", "vh-revoke", &[]);
        // SAFETY: as in `invoke` above.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert!(req.host().kv_set(&ApiGate::revocation_key("revoke"), b"1", 60));

        let resp = invoke(&mw, "vh-revoke", "/api/v1/x", &key_header("revoke"));
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 403);
    }

    #[test]
    fn exceeding_the_window_budget_is_rejected_with_429() {
        setup_kv();
        let mw = gate_for("burst");
        // Budget is 3 per 10s window. A window boundary can reset the counter
        // once mid-loop, so drive well past the budget before asserting.
        let mut limited = None;
        for _ in 0..12 {
            let resp = invoke(&mw, "vh-429", "/api/v1/x", &key_header("burst"));
            if resp.__status() == 429 {
                limited = Some(resp);
                break;
            }
        }
        let resp = limited.expect("budget of 3 must be exhausted within 12 requests");
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(find(resp.__headers(), "X-RateLimit-Remaining"), Some("0"));
        let retry: u64 =
            find(resp.__headers(), "Retry-After").expect("Retry-After").parse().expect("integer");
        assert!(retry > 0 && retry <= WINDOW_SECS, "Retry-After must fall inside the window");
    }

    #[test]
    fn a_separate_tenant_has_a_separate_budget() {
        setup_kv();
        let mw = ApiGate::init(&serde_json::json!({
            "keys": { "k-a": "ten-a", "k-b": "ten-b" },
            "requests_per_window": 2,
        }))
        .expect("init");
        let a = vec![("X-Api-Key".to_owned(), "k-a".to_owned())];
        let b = vec![("X-Api-Key".to_owned(), "k-b".to_owned())];
        // Burn tenant a's budget.
        for _ in 0..6 {
            let _ = invoke(&mw, "vh-split", "/api/x", &a);
        }
        // Tenant b is unaffected on its first request.
        assert_ne!(invoke(&mw, "vh-split", "/api/x", &b).__status(), 429);
    }
}

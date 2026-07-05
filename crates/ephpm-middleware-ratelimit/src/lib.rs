//! `ratelimit` — ePHPm native middleware: fixed-window per-client rate
//! limiting backed by the embedded (cluster-replicated) KV store.
//!
//! Requests are counted in 10-second windows (less KV churn than 1-second
//! windows): each window allows `per_ip_rps * 10 + burst` requests per
//! client. The counter key is `mw:rl:{vhost}:{client}:{window_index}`,
//! created with a TTL via `kv_set_nx` (since `kv_incr` cannot set TTLs) and
//! bumped with a single atomic `kv_incr` — which is also what makes the
//! limit cluster-wide when KV replication is on. Over the limit the client
//! gets `429` with a `Retry-After` for the seconds left in the window.
//!
//! **Fail-open by design:** when the KV store is unavailable (`kv_incr`
//! errors), the request is allowed through with a warning log. For a rate
//! limiter, availability beats strictness — dropping every request because
//! the KV tier hiccuped would turn a soft protection into a hard outage. If
//! you need fail-closed admission control, use an auth middleware instead.
//!
//! Configuration (`[[middleware]] config = { ... }`):
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `per_ip_rps` (integer) | **required**, > 0 | sustained requests/second per client |
//! | `burst` (integer) | `per_ip_rps` | extra headroom on top of the per-window allowance |
//! | `key_headers` (array of strings) | unset | identify clients by the first present header (e.g. `X-Api-Key`) instead of the client IP |

use std::time::{SystemTime, UNIX_EPOCH};

use ephpm_middleware::abi::LOG_WARN;
use ephpm_middleware::{Middleware, Request, Response, declare};

/// Fixed window length in seconds.
const WINDOW_SECS: u64 = 10;
/// Counter-key TTL: one window plus slack for clock skew across nodes.
const KEY_TTL_SECS: i64 = 30;

/// Rate-limit policy, built once at `init`.
pub struct RateLimit {
    per_ip_rps: u64,
    burst: u64,
    key_headers: Vec<String>,
}

impl RateLimit {
    /// Requests allowed per client per window.
    fn allowance(&self) -> i64 {
        i64::try_from(self.per_ip_rps.saturating_mul(WINDOW_SECS).saturating_add(self.burst))
            .unwrap_or(i64::MAX)
    }

    /// Client identity: the first present `key_headers` header, else the IP.
    fn client_key<'a>(&self, req: &'a Request<'_>) -> &'a str {
        self.key_headers.iter().find_map(|h| req.header(h)).unwrap_or_else(|| req.remote_ip())
    }
}

impl Middleware for RateLimit {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let per_ip_rps = config
            .get("per_ip_rps")
            .ok_or("`per_ip_rps` is required (requests/second per client, > 0)")?
            .as_u64()
            .ok_or("`per_ip_rps` must be a positive integer")?;
        if per_ip_rps == 0 {
            return Err("`per_ip_rps` must be > 0".into());
        }
        let burst = match config.get("burst") {
            None | Some(serde_json::Value::Null) => per_ip_rps,
            Some(v) => v.as_u64().ok_or("`burst` must be a non-negative integer")?,
        };
        let key_headers = match config.get("key_headers") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(v) => v
                .as_array()
                .ok_or("`key_headers` must be an array of header names")?
                .iter()
                .map(|h| {
                    h.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("`key_headers` entries must be strings, got {h}"))
                })
                .collect::<Result<_, _>>()?,
        };
        Ok(Self { per_ip_rps, burst, key_headers })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let window = now / WINDOW_SECS;
        let client = self.client_key(req);
        let key = format!("mw:rl:{}:{}:{}", req.vhost_id(), client, window);

        let host = req.host();
        // Create the counter with a TTL when absent (kv_incr cannot set
        // one); when the key already exists this is a no-op.
        let _ = host.kv_set_nx(&key, b"0", KEY_TTL_SECS);
        let Some(count) = host.kv_incr(&key, 1) else {
            // Fail-open: see the crate docs for the rationale.
            host.log(
                LOG_WARN,
                &format!("ratelimit: KV store unavailable — failing open for client {client}"),
            );
            return Response::cont();
        };

        if count > self.allowance() {
            let retry_after = WINDOW_SECS - (now % WINDOW_SECS);
            return Response::respond(429, "rate limit exceeded")
                .header("Retry-After", retry_after.to_string());
        }
        Response::cont()
    }
}

declare!(RateLimit);

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
    use ephpm_middleware::host::{RequestCtx, host_table, set_kv_store};

    use super::*;

    /// Wire a real in-memory Store into the host table (first call wins;
    /// all tests in this binary share it, so each uses a unique vhost).
    fn setup_kv() {
        set_kv_store(&ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default()));
    }

    fn invoke(mw: &RateLimit, vhost: &str, ip: &str, headers: &[(String, String)]) -> Response {
        let ctx = RequestCtx::new("GET", "/api/x", "", ip, vhost, headers);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    #[test]
    fn init_validates_config() {
        assert!(RateLimit::init(&serde_json::Value::Null).is_err());
        assert!(RateLimit::init(&serde_json::json!({ "per_ip_rps": 0 })).is_err());
        assert!(RateLimit::init(&serde_json::json!({ "per_ip_rps": "fast" })).is_err());
        assert!(
            RateLimit::init(&serde_json::json!({ "per_ip_rps": 5, "key_headers": "X-Api-Key" }))
                .is_err()
        );
        let mw = RateLimit::init(&serde_json::json!({ "per_ip_rps": 5 })).expect("init");
        // burst defaults to per_ip_rps: 5*10 + 5.
        assert_eq!(mw.allowance(), 55);
    }

    #[test]
    fn first_request_is_always_allowed() {
        setup_kv();
        let mw = RateLimit::init(&serde_json::json!({ "per_ip_rps": 1 })).expect("init");
        let resp = invoke(&mw, "vhost-first", "198.51.100.1", &[]);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn over_limit_gets_429_with_retry_after() {
        setup_kv();
        let mw =
            RateLimit::init(&serde_json::json!({ "per_ip_rps": 1, "burst": 0 })).expect("init");
        // Allowance is 10/window. Even if a window boundary lands mid-loop
        // (resetting the counter once), 3x the allowance must trip the limit.
        let mut limited = None;
        for _ in 0..30 {
            let resp = invoke(&mw, "vhost-429", "198.51.100.2", &[]);
            if resp.__action() == ACTION_RESPOND {
                limited = Some(resp);
                break;
            }
        }
        let resp = limited.expect("rate limit never tripped within 3x the allowance");
        assert_eq!(resp.__status(), 429);
        assert_eq!(resp.__body(), b"rate limit exceeded");
        let retry: u64 = resp
            .__headers()
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("Retry-After"))
            .map(|(_, v)| v.parse().expect("numeric Retry-After"))
            .expect("Retry-After present");
        assert!((1..=WINDOW_SECS).contains(&retry), "retry_after = {retry}");
    }

    #[test]
    fn key_header_separates_clients() {
        setup_kv();
        let mw = RateLimit::init(&serde_json::json!({
            "per_ip_rps": 1,
            "burst": 0,
            "key_headers": ["X-Api-Key"],
        }))
        .expect("init");
        let key_a = [("X-Api-Key".to_owned(), "alpha".to_owned())];
        let key_b = [("X-Api-Key".to_owned(), "beta".to_owned())];
        // Exhaust client alpha (same IP for everyone — the header is the key).
        let mut tripped = false;
        for _ in 0..30 {
            if invoke(&mw, "vhost-keys", "198.51.100.3", &key_a).__action() == ACTION_RESPOND {
                tripped = true;
                break;
            }
        }
        assert!(tripped, "alpha never rate-limited");
        // Client beta's first request in any window is always allowed.
        let resp = invoke(&mw, "vhost-keys", "198.51.100.3", &key_b);
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }
}

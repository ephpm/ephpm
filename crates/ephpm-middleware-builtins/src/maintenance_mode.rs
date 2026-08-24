//! `maintenance-mode` — ePHPm native middleware that flips a tenant into a
//! 503 holding page the instant a per-site flag appears in the embedded
//! (cluster-replicated) KV store — no redeploy, no restart. The flagship demo
//! of the KV store as a control plane: analogous to Cloudflare's maintenance
//! mode or an HAProxy `monitor-uri`, but driven by a single KV key you can set
//! from PHP (`ephpm_kv_set`) or the RESP interface.
//!
//! Per request the module builds a per-site key from a configurable template
//! (default `mw:maintenance:<vhost>`, with `<vhost>` replaced by the request's
//! vhost id) and `kv_get`s it. If the key is present and *truthy* the request
//! is short-circuited with a `503` holding page carrying a `Retry-After`
//! header. If the key is absent the request `CONTINUE`s to PHP untouched.
//!
//! **Bypass.** Operators need to verify a site while it is "down". Two escape
//! hatches let a request `CONTINUE` even during maintenance:
//! - `bypass_ips` — exact IPs or CIDR ranges (client IP is taken *after*
//!   trusted-proxy resolution, so it is the real client, not the proxy).
//! - `bypass_paths` — path prefixes kept live (e.g. `/healthz` so the load
//!   balancer's health check still passes and the tenant is not evicted).
//!
//! Bypass is checked *before* the KV lookup, so a health check never even
//! touches the KV store.
//!
//! **Fail-OPEN by design.** The embedded `kv_get` accessor returns `None` for
//! both "key absent" and "KV store unavailable" — and both paths `CONTINUE`.
//! That is deliberate: a KV blip must **not** take *every* tenant down. A
//! maintenance flag is a soft, operator-driven signal; failing closed here
//! would turn a transient KV hiccup into a fleet-wide outage. This is the
//! **opposite** of an IP-allowlist / auth gate (which must fail *closed* — see
//! the `jwt` module): there, availability must never beat correctness; here it
//! must. Choose this module only for maintenance signalling, never for access
//! control.
//!
//! Configuration (`[[middleware]] config = { ... }`), all optional:
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `key_template` (string) | `"mw:maintenance:<vhost>"` | KV key checked per request; `<vhost>` is replaced with the request's vhost id |
//! | `retry_after` (integer seconds) | `300` | value of the `Retry-After` header on the 503 |
//! | `body` (string) | built-in minimal HTML | holding-page body served with the 503 |
//! | `content_type` (string) | `"text/html; charset=utf-8"` | `Content-Type` of the holding page |
//! | `bypass_ips` (array of strings) | unset | exact IPs or CIDR ranges whose requests continue during maintenance |
//! | `bypass_paths` (array of strings) | unset | path prefixes that stay live during maintenance |

use std::net::IpAddr;

use ephpm_middleware::abi::LOG_DEBUG;
use ephpm_middleware::{Middleware, Request, Response};

/// Default KV key template. `<vhost>` is substituted per request.
const DEFAULT_KEY_TEMPLATE: &str = "mw:maintenance:<vhost>";
/// Placeholder replaced with the request's vhost id in the key template.
const VHOST_PLACEHOLDER: &str = "<vhost>";
/// Default `Retry-After` (seconds).
const DEFAULT_RETRY_AFTER: u64 = 300;
/// Default holding-page `Content-Type`.
const DEFAULT_CONTENT_TYPE: &str = "text/html; charset=utf-8";
/// Minimal built-in holding page.
const DEFAULT_BODY: &str = "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>Down for maintenance</title>\n</head>\n<body>\n<h1>We&rsquo;ll be right back</h1>\n<p>This site is temporarily down for maintenance. Please try again shortly.</p>\n</body>\n</html>\n";

/// One entry from `bypass_ips`: a single address or a CIDR range.
enum IpMatcher {
    /// Exact IP match.
    Exact(IpAddr),
    /// CIDR: network address plus prefix length (bits).
    Cidr { network: IpAddr, prefix: u8 },
}

impl IpMatcher {
    /// Parse an exact IP (`203.0.113.4`, `2001:db8::1`) or CIDR
    /// (`203.0.113.0/24`, `2001:db8::/32`).
    fn parse(spec: &str) -> Result<Self, String> {
        if let Some((net, bits)) = spec.split_once('/') {
            let network: IpAddr = net
                .parse()
                .map_err(|_| format!("`bypass_ips`: invalid CIDR network in {spec:?}"))?;
            let prefix: u8 =
                bits.parse().map_err(|_| format!("`bypass_ips`: invalid prefix in {spec:?}"))?;
            let max = if network.is_ipv4() { 32 } else { 128 };
            if prefix > max {
                return Err(format!("`bypass_ips`: prefix /{prefix} too large in {spec:?}"));
            }
            Ok(IpMatcher::Cidr { network, prefix })
        } else {
            let ip: IpAddr =
                spec.parse().map_err(|_| format!("`bypass_ips`: invalid IP {spec:?}"))?;
            Ok(IpMatcher::Exact(ip))
        }
    }

    /// Does `ip` fall within this matcher?
    fn matches(&self, ip: IpAddr) -> bool {
        match self {
            IpMatcher::Exact(want) => *want == ip,
            IpMatcher::Cidr { network, prefix } => cidr_contains(*network, *prefix, ip),
        }
    }
}

/// Compare the high `prefix` bits of two same-family addresses.
fn cidr_contains(network: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    match (network, ip) {
        (IpAddr::V4(net), IpAddr::V4(addr)) => prefix_match(&net.octets(), &addr.octets(), prefix),
        (IpAddr::V6(net), IpAddr::V6(addr)) => prefix_match(&net.octets(), &addr.octets(), prefix),
        // Mixed families never match.
        _ => false,
    }
}

/// True when `a` and `b` agree on the first `prefix` bits.
fn prefix_match(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let mut bits = usize::from(prefix);
    for (x, y) in a.iter().zip(b.iter()) {
        if bits == 0 {
            break;
        }
        if bits >= 8 {
            if x != y {
                return false;
            }
            bits -= 8;
        } else {
            // Compare the top `bits` bits of this byte.
            let mask = 0xFFu8 << (8 - bits);
            return (x & mask) == (y & mask);
        }
    }
    true
}

/// A KV maintenance value is "on" unless it is empty or an explicit falsey
/// marker. This lets an operator disable maintenance by *setting* the flag to
/// `0`/`false`/`off` without having to delete the key.
fn is_truthy(value: &[u8]) -> bool {
    let s = std::str::from_utf8(value).unwrap_or("").trim();
    if s.is_empty() {
        return false;
    }
    !matches!(s.to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
}

/// Maintenance-mode policy, built once at `init`.
pub struct MaintenanceMode {
    key_template: String,
    retry_after: u64,
    body: String,
    content_type: String,
    bypass_ips: Vec<IpMatcher>,
    bypass_paths: Vec<String>,
}

impl MaintenanceMode {
    /// Build the per-request KV key by substituting the vhost id.
    fn key_for(&self, vhost: &str) -> String {
        self.key_template.replace(VHOST_PLACEHOLDER, vhost)
    }

    /// Does this request qualify for a bypass (path prefix or client IP)?
    fn is_bypassed(&self, req: &Request<'_>) -> bool {
        let path = req.path();
        if self.bypass_paths.iter().any(|p| path.starts_with(p.as_str())) {
            return true;
        }
        if !self.bypass_ips.is_empty()
            && let Ok(ip) = req.remote_ip().parse::<IpAddr>()
            && self.bypass_ips.iter().any(|m| m.matches(ip))
        {
            return true;
        }
        false
    }
}

/// Read an optional array-of-strings config key.
fn opt_string_array(config: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    match config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .ok_or_else(|| format!("`{key}` must be an array of strings"))?
            .iter()
            .map(|e| {
                e.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`{key}` entries must be strings, got {e}"))
            })
            .collect(),
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

impl Middleware for MaintenanceMode {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let key_template = opt_string(config, "key_template", DEFAULT_KEY_TEMPLATE)?;
        if key_template.is_empty() {
            return Err("`key_template` must not be empty".into());
        }
        let retry_after = match config.get("retry_after") {
            None | Some(serde_json::Value::Null) => DEFAULT_RETRY_AFTER,
            Some(v) => v
                .as_u64()
                .ok_or_else(|| format!("`retry_after` must be a non-negative integer, got {v}"))?,
        };
        let body = opt_string(config, "body", DEFAULT_BODY)?;
        let content_type = opt_string(config, "content_type", DEFAULT_CONTENT_TYPE)?;
        let bypass_ips = opt_string_array(config, "bypass_ips")?
            .iter()
            .map(|s| IpMatcher::parse(s))
            .collect::<Result<_, _>>()?;
        let bypass_paths = opt_string_array(config, "bypass_paths")?;

        Ok(Self { key_template, retry_after, body, content_type, bypass_ips, bypass_paths })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        // Bypass first — a health check must never depend on the KV store.
        if self.is_bypassed(req) {
            return Response::cont();
        }

        let key = self.key_for(req.vhost_id());
        let host = req.host();
        // `kv_get` returns None for BOTH "absent" and "KV unavailable" — both
        // fall through to CONTINUE. That is the fail-OPEN choice (see the
        // module docs): a KV blip must not black-hole every tenant.
        match host.kv_get(&key) {
            Some(value) if is_truthy(&value) => Response::respond(503, self.body.clone())
                .header("Retry-After", self.retry_after.to_string())
                .header("Content-Type", self.content_type.clone()),
            _ => {
                host.log(LOG_DEBUG, &format!("maintenance-mode: {key} not set — continuing"));
                Response::cont()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
    use ephpm_middleware::host::{RequestCtx, host_table, set_kv_store};

    use super::*;

    /// Wire a real in-memory Store into the host table (first call wins; all
    /// tests in this binary share it, so each uses a unique vhost).
    fn setup_kv() {
        set_kv_store(&ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default()));
    }

    fn invoke(mw: &MaintenanceMode, path: &str, ip: &str, vhost: &str) -> Response {
        let ctx = RequestCtx::new("GET", path, "", ip, vhost, &[]);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    /// Turn maintenance on for a vhost via the same KV key the module reads.
    fn set_flag(vhost: &str, value: &[u8]) {
        let key = format!("mw:maintenance:{vhost}");
        let ctx = RequestCtx::new("GET", "/", "", "127.0.0.1", vhost, &[]);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert!(req.host().kv_set(&key, value, 0), "kv_set failed");
    }

    #[test]
    fn init_defaults_and_validation() {
        let mw = MaintenanceMode::init(&serde_json::Value::Null).expect("init");
        assert_eq!(mw.key_template, DEFAULT_KEY_TEMPLATE);
        assert_eq!(mw.retry_after, DEFAULT_RETRY_AFTER);
        assert_eq!(mw.key_for("acme.test"), "mw:maintenance:acme.test");
        // Bad config is rejected.
        assert!(MaintenanceMode::init(&serde_json::json!({ "key_template": "" })).is_err());
        assert!(MaintenanceMode::init(&serde_json::json!({ "retry_after": "soon" })).is_err());
        assert!(
            MaintenanceMode::init(&serde_json::json!({ "bypass_ips": ["not-an-ip"] })).is_err()
        );
        assert!(
            MaintenanceMode::init(&serde_json::json!({ "bypass_ips": ["10.0.0.0/40"] })).is_err()
        );
        assert!(MaintenanceMode::init(&serde_json::json!({ "bypass_paths": "/healthz" })).is_err());
    }

    #[test]
    fn flag_unset_continues() {
        setup_kv();
        let mw = MaintenanceMode::init(&serde_json::Value::Null).expect("init");
        let resp = invoke(&mw, "/", "198.51.100.1", "vhost-unset");
        assert_eq!(resp.__action(), ACTION_CONTINUE);
    }

    #[test]
    fn flag_set_returns_503_with_retry_after_and_body() {
        setup_kv();
        let mw = MaintenanceMode::init(&serde_json::json!({ "retry_after": 120 })).expect("init");
        set_flag("vhost-503", b"1");
        let resp = invoke(&mw, "/anything", "198.51.100.2", "vhost-503");
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 503);
        assert_eq!(resp.__body(), DEFAULT_BODY.as_bytes());
        let find = |name: &str| {
            resp.__headers()
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(find("Retry-After"), Some("120"));
        assert_eq!(find("Content-Type"), Some(DEFAULT_CONTENT_TYPE));
    }

    #[test]
    fn falsey_flag_value_continues() {
        setup_kv();
        let mw = MaintenanceMode::init(&serde_json::Value::Null).expect("init");
        set_flag("vhost-falsey", b"0");
        assert_eq!(invoke(&mw, "/", "198.51.100.3", "vhost-falsey").__action(), ACTION_CONTINUE);
        set_flag("vhost-falsey", b"off");
        assert_eq!(invoke(&mw, "/", "198.51.100.3", "vhost-falsey").__action(), ACTION_CONTINUE);
    }

    #[test]
    fn custom_body_is_served() {
        setup_kv();
        let mw =
            MaintenanceMode::init(&serde_json::json!({ "body": "gone fishing" })).expect("init");
        set_flag("vhost-body", b"true");
        let resp = invoke(&mw, "/", "198.51.100.4", "vhost-body");
        assert_eq!(resp.__body(), b"gone fishing");
    }

    #[test]
    fn bypass_exact_ip_continues_during_maintenance() {
        setup_kv();
        let mw = MaintenanceMode::init(&serde_json::json!({
            "bypass_ips": ["203.0.113.7"],
        }))
        .expect("init");
        set_flag("vhost-ip", b"1");
        // Operator's IP sails through.
        assert_eq!(invoke(&mw, "/", "203.0.113.7", "vhost-ip").__action(), ACTION_CONTINUE);
        // Everyone else gets the 503.
        assert_eq!(invoke(&mw, "/", "203.0.113.8", "vhost-ip").__action(), ACTION_RESPOND);
    }

    #[test]
    fn bypass_cidr_continues_during_maintenance() {
        setup_kv();
        let mw = MaintenanceMode::init(&serde_json::json!({
            "bypass_ips": ["10.0.0.0/8", "2001:db8::/32"],
        }))
        .expect("init");
        set_flag("vhost-cidr", b"1");
        assert_eq!(invoke(&mw, "/", "10.4.5.6", "vhost-cidr").__action(), ACTION_CONTINUE);
        assert_eq!(invoke(&mw, "/", "2001:db8::dead", "vhost-cidr").__action(), ACTION_CONTINUE);
        // Outside the range → still down.
        assert_eq!(invoke(&mw, "/", "11.0.0.1", "vhost-cidr").__action(), ACTION_RESPOND);
    }

    #[test]
    fn bypass_path_continues_during_maintenance() {
        setup_kv();
        let mw = MaintenanceMode::init(&serde_json::json!({
            "bypass_paths": ["/healthz", "/status"],
        }))
        .expect("init");
        set_flag("vhost-path", b"1");
        assert_eq!(
            invoke(&mw, "/healthz", "198.51.100.9", "vhost-path").__action(),
            ACTION_CONTINUE
        );
        assert_eq!(
            invoke(&mw, "/status/live", "198.51.100.9", "vhost-path").__action(),
            ACTION_CONTINUE
        );
        // A normal path is still down.
        assert_eq!(invoke(&mw, "/", "198.51.100.9", "vhost-path").__action(), ACTION_RESPOND);
    }

    #[test]
    fn custom_key_template_is_used() {
        setup_kv();
        let mw = MaintenanceMode::init(&serde_json::json!({
            "key_template": "flags:down:<vhost>",
        }))
        .expect("init");
        // Set the flag under the CUSTOM key.
        let key = "flags:down:vhost-tmpl";
        let ctx = RequestCtx::new("GET", "/", "", "127.0.0.1", "vhost-tmpl", &[]);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert!(req.host().kv_set(key, b"1", 0));
        assert_eq!(invoke(&mw, "/", "198.51.100.10", "vhost-tmpl").__action(), ACTION_RESPOND);
    }
}

//! `ip-allowlist` — ePHPm native middleware that allows or denies requests by
//! client IP against CIDR lists. The analog of Traefik's `ipallowlist`, Kong's
//! `ip-restriction`, and nginx `allow`/`deny`.
//!
//! The client IP is taken from [`Request::remote_ip`], which the host has
//! already resolved through the trusted-proxy configuration — this module
//! never parses `X-Forwarded-For` itself.
//!
//! Decision order (a request is evaluated exactly once):
//!
//! 1. If the client IP matches any `deny` CIDR → **`403`** (deny wins over
//!    allow, always).
//! 2. Else if it matches any `allow` CIDR → **`CONTINUE`**.
//! 3. Else apply the `default` policy (`"allow"` → CONTINUE, `"deny"` → 403).
//!
//! **Fail-closed security gate.** This is an access-control filter, so it fails
//! closed, in two senses:
//!
//! * **Config errors fail startup.** A malformed CIDR, a non-array `allow`/
//!   `deny`, or an unknown `default` value makes `init` return `Err`, which the
//!   host treats as a hard startup failure — the module never loads half-parsed.
//! * **An unparseable client IP is denied.** If `remote_ip()` is empty or not a
//!   valid address it cannot match `allow`, so it is rejected with `403` unless
//!   `default = "allow"` explicitly opens the gate.
//!
//! Request-phase only: the verdict is decided before PHP runs and this module
//! touches no response-phase API.
//!
//! ## Configuration (`[[middleware]] config = { ... }`)
//!
//! | key | default | meaning |
//! |-----|---------|---------|
//! | `allow` (array of CIDR strings) | `[]` | client IPs allowed through; v4 and v6, e.g. `"10.0.0.0/8"`, `"2001:db8::/32"`. A bare address is a `/32` (v4) or `/128` (v6). |
//! | `deny` (array of CIDR strings) | `[]` | client IPs rejected with `403`; takes precedence over `allow`. |
//! | `default` (string) | `"deny"` | verdict when no rule matches: `"allow"` or `"deny"`. |
//!
//! ### Scope
//!
//! Configuration is per `[[middleware]]` block. Like the sibling modules
//! (`cors`, `security-headers`), the policy is read once at `init` and applied
//! to every request the block sees; the module does not itself branch on
//! [`Request::vhost_id`]. Per-site policies are expressed the same way per-site
//! behaviour is expressed for the other modules — by scoping the `[[middleware]]`
//! block to a site in the host configuration — not by a per-vhost config map
//! inside this module (v1).

use std::net::IpAddr;

use ephpm_middleware::{Middleware, Request, Response};
use ipnetwork::IpNetwork;

/// Plain-text body returned on a `403` denial.
const DENIED_BODY: &str = "Forbidden: your IP address is not permitted.";

/// The verdict applied when a client IP matches neither list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DefaultPolicy {
    /// Continue to PHP.
    Allow,
    /// Reject with `403`.
    Deny,
}

/// IP allow/deny policy, built once at `init`.
pub struct IpAllowlist {
    allow: Vec<IpNetwork>,
    deny: Vec<IpNetwork>,
    default: DefaultPolicy,
}

/// Parse an optional array-of-CIDR config key into networks. A missing or null
/// key is an empty list; any non-array, non-string, or unparseable entry is a
/// hard error (fail-closed at startup).
fn parse_cidrs(config: &serde_json::Value, key: &str) -> Result<Vec<IpNetwork>, String> {
    let arr = match config.get(key) {
        None | Some(serde_json::Value::Null) => return Ok(Vec::new()),
        Some(serde_json::Value::Array(a)) => a,
        Some(other) => {
            return Err(format!("`{key}` must be an array of CIDR strings, got {other}"));
        }
    };
    arr.iter()
        .map(|v| {
            let s = v
                .as_str()
                .ok_or_else(|| format!("`{key}` entries must be CIDR strings, got {v}"))?;
            s.parse::<IpNetwork>()
                .map_err(|e| format!("`{key}` entry {s:?} is not a valid CIDR: {e}"))
        })
        .collect()
}

impl IpAllowlist {
    /// True when `ip` falls inside any network in `nets`.
    fn matches(nets: &[IpNetwork], ip: IpAddr) -> bool {
        nets.iter().any(|n| n.contains(ip))
    }
}

impl Middleware for IpAllowlist {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let allow = parse_cidrs(config, "allow")?;
        let deny = parse_cidrs(config, "deny")?;
        let default = match config.get("default") {
            None | Some(serde_json::Value::Null) => DefaultPolicy::Deny,
            Some(serde_json::Value::String(s)) => match s.as_str() {
                "allow" => DefaultPolicy::Allow,
                "deny" => DefaultPolicy::Deny,
                other => {
                    return Err(format!("`default` must be \"allow\" or \"deny\", got {other:?}"));
                }
            },
            Some(other) => {
                return Err(format!("`default` must be \"allow\" or \"deny\", got {other}"));
            }
        };
        Ok(Self { allow, deny, default })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        // The host has already applied trusted-proxy resolution; parse the
        // client address. An unparseable/empty IP cannot match `allow`, so it
        // is treated as unmatched and subject to the fail-closed default below.
        let parsed = req.remote_ip().parse::<IpAddr>().ok();

        if let Some(ip) = parsed {
            // Deny always wins.
            if Self::matches(&self.deny, ip) {
                return denied();
            }
            if Self::matches(&self.allow, ip) {
                return Response::cont();
            }
        }

        match self.default {
            DefaultPolicy::Allow => Response::cont(),
            DefaultPolicy::Deny => denied(),
        }
    }
}

/// The `403` response with a small plain-text body.
fn denied() -> Response {
    Response::respond(403, DENIED_BODY).header("Content-Type", "text/plain; charset=utf-8")
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // tests build the FFI Request view by hand.

    use ephpm_middleware::abi::{ACTION_CONTINUE, ACTION_RESPOND};
    use ephpm_middleware::host::{RequestCtx, host_table};

    use super::*;

    fn build(config: serde_json::Value) -> IpAllowlist {
        IpAllowlist::init(&config).expect("init")
    }

    fn invoke(mw: &IpAllowlist, ip: &str) -> Response {
        let ctx = RequestCtx::new("GET", "/index.php", "", ip, "example.test", &[]);
        // SAFETY: `ctx` outlives the view; host_table() is 'static.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        mw.invoke(&req)
    }

    #[test]
    fn ipv4_allow_and_default_deny() {
        let mw = build(serde_json::json!({ "allow": ["10.0.0.0/8"] }));
        // In-range → continue.
        assert_eq!(invoke(&mw, "10.1.2.3").__action(), ACTION_CONTINUE);
        // Out-of-range → default deny → 403.
        let resp = invoke(&mw, "192.168.1.1");
        assert_eq!(resp.__action(), ACTION_RESPOND);
        assert_eq!(resp.__status(), 403);
        assert_eq!(resp.__body(), DENIED_BODY.as_bytes());
    }

    #[test]
    fn ipv6_allow_matches() {
        let mw = build(serde_json::json!({ "allow": ["2001:db8::/32"] }));
        assert_eq!(invoke(&mw, "2001:db8::dead:beef").__action(), ACTION_CONTINUE);
        assert_eq!(invoke(&mw, "2001:dead::1").__action(), ACTION_RESPOND);
    }

    #[test]
    fn cidr_boundary_is_respected() {
        // /24 covers .0–.255 only.
        let mw = build(serde_json::json!({ "allow": ["203.0.113.0/24"], "default": "deny" }));
        assert_eq!(invoke(&mw, "203.0.113.255").__action(), ACTION_CONTINUE);
        assert_eq!(invoke(&mw, "203.0.114.0").__action(), ACTION_RESPOND);
        // A /32 host route: exact match only.
        let host = mw2_host();
        assert_eq!(invoke(&host, "198.51.100.7").__action(), ACTION_CONTINUE);
        assert_eq!(invoke(&host, "198.51.100.8").__action(), ACTION_RESPOND);
    }

    fn mw2_host() -> IpAllowlist {
        build(serde_json::json!({ "allow": ["198.51.100.7/32"] }))
    }

    #[test]
    fn deny_takes_precedence_over_allow() {
        // Same address is in both lists — deny must win.
        let mw = build(serde_json::json!({
            "allow": ["10.0.0.0/8"],
            "deny": ["10.6.6.6/32"],
            "default": "allow",
        }));
        assert_eq!(invoke(&mw, "10.6.6.6").__action(), ACTION_RESPOND);
        // A different in-allow address still continues.
        assert_eq!(invoke(&mw, "10.6.6.7").__action(), ACTION_CONTINUE);
        // A broader deny range wins over a narrower allow.
        let broad = build(serde_json::json!({
            "allow": ["192.168.1.0/24"],
            "deny": ["192.168.0.0/16"],
        }));
        assert_eq!(invoke(&broad, "192.168.1.50").__action(), ACTION_RESPOND);
    }

    #[test]
    fn default_allow_lets_unmatched_through() {
        let mw = build(serde_json::json!({ "deny": ["10.0.0.0/8"], "default": "allow" }));
        // Not denied, not in any allow list → default allow.
        assert_eq!(invoke(&mw, "8.8.8.8").__action(), ACTION_CONTINUE);
        // Still denied when in the deny list.
        assert_eq!(invoke(&mw, "10.0.0.1").__action(), ACTION_RESPOND);
    }

    #[test]
    fn default_deny_is_the_default_when_unset() {
        // No allow, no deny, no default: everything is denied (fail-closed).
        let mw = build(serde_json::json!({}));
        assert_eq!(invoke(&mw, "8.8.8.8").__action(), ACTION_RESPOND);
        assert_eq!(invoke(&mw, "10.0.0.1").__action(), ACTION_RESPOND);
    }

    #[test]
    fn unparseable_client_ip_is_denied_by_default() {
        let mw = build(serde_json::json!({ "allow": ["0.0.0.0/0"] }));
        // A valid any-v4 address is allowed…
        assert_eq!(invoke(&mw, "1.2.3.4").__action(), ACTION_CONTINUE);
        // …but a garbage/empty IP cannot match allow → fail closed.
        assert_eq!(invoke(&mw, "not-an-ip").__action(), ACTION_RESPOND);
        assert_eq!(invoke(&mw, "").__action(), ACTION_RESPOND);
    }

    #[test]
    fn unparseable_client_ip_follows_default_allow() {
        // With default=allow, an unparseable IP that hits no deny rule passes.
        let mw = build(serde_json::json!({ "deny": ["10.0.0.0/8"], "default": "allow" }));
        assert_eq!(invoke(&mw, "not-an-ip").__action(), ACTION_CONTINUE);
    }

    #[test]
    fn malformed_config_fails_init_closed() {
        // Bad CIDR string.
        assert!(IpAllowlist::init(&serde_json::json!({ "allow": ["10.0.0.0/999"] })).is_err());
        assert!(IpAllowlist::init(&serde_json::json!({ "deny": ["nonsense"] })).is_err());
        // Wrong types.
        assert!(IpAllowlist::init(&serde_json::json!({ "allow": "10.0.0.0/8" })).is_err());
        assert!(IpAllowlist::init(&serde_json::json!({ "allow": [42] })).is_err());
        // Unknown default policy.
        assert!(IpAllowlist::init(&serde_json::json!({ "default": "maybe" })).is_err());
        assert!(IpAllowlist::init(&serde_json::json!({ "default": true })).is_err());
    }
}

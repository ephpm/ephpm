//! Access policy for the **data-bearing** internal endpoints: the Prometheus
//! endpoint (`[server.metrics] path`, default `/metrics`) and the request
//! timeline (`/_ephpm/requests`).
//!
//! # Why these two, and not the probes
//!
//! Both endpoints are answered before `trusted_hosts`, site resolution and
//! the middleware chain (a Kubernetes probe or a Prometheus scrape addresses a
//! pod by raw IP, so a `Host`-gated endpoint would never be reachable). What
//! they return is not harmless:
//!
//! * `/metrics` carries every tenant's normalized SQL digests, vhost names
//!   (OPcache invalidation labels), DB-proxy upstream `host:port` labels, and
//!   per-handler traffic shape.
//! * `/_ephpm/requests` is one process-wide ring buffer of the last requests'
//!   paths — every vhost's, with no host field.
//!
//! The probes stay open on purpose. `/_ephpm/health`, `/_ephpm/ready` and
//! `/_ephpm/primary` answer a load balancer or kubelet that sends no
//! credentials, and what they reveal is a status code. The one detail-bearing
//! probe body — `/_ephpm/ready`'s "which DB upstream is unreachable" reason —
//! is shown only to clients the `[server.diagnostics]` IP ACL admits.
//!
//! # Two different threats, two different controls
//!
//! **Network exposure** (someone on the internet reaching the port) is what
//! the IP ACL (`allow` / `deny`) and the separate `[server.metrics] listen`
//! address are for. They are a makeshift firewall and **default to open**, so
//! an upgrade with no config change serves exactly what it served before.
//!
//! **Tenant isolation** on a multi-site node is a different problem. Every
//! tenant's PHP runs on the node, so it can reach loopback, every internal
//! port, and every unix socket the server listens on — any address the
//! scraper can use, a tenant can use too. No IP ACL or listener address can
//! tell them apart; only a **credential** can: the bearer `token`, or a client
//! certificate on the mTLS metrics listener (`client_ca`). So on a multi-site
//! node (outside `ephpm dev`) these endpoints are refused with `403` until one
//! of those credentials is configured — see [`InternalAccess::new`].
//!
//! # Evaluation order
//!
//! 1. Credential requirement (multi-site): no token and no mTLS → `403`
//!    naming the setting.
//! 2. IP ACL: `deny` wins over `allow`; an empty `allow` admits everything.
//!    The client address is the TCP peer resolved through
//!    `[server.security] trusted_proxies`, exactly as `REMOTE_ADDR` is. When an
//!    ACL is configured, a request carrying forwarding headers (`Forwarded`,
//!    `X-Forwarded-For`, `X-Real-IP`) from a peer that is **not** a trusted
//!    proxy has an unknown origin — typically a same-host reverse proxy whose
//!    loopback address would otherwise launder every internet client behind
//!    it — and is refused. Spoofing those headers can only remove access.
//! 3. Bearer token, when set (`401` otherwise; constant-time compare).
//!
//! Invalid ACL entries are a startup error (`Config::validate`); if one
//! reaches this module anyway it is dropped. A dropped `allow` entry narrows
//! the list, but a dropped `deny` entry would widen it, so an unparseable
//! `deny` entry denies everything instead.

use std::net::IpAddr;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Response, StatusCode};
use ipnet::IpNet;

use crate::body::{self, ServerBody};

/// Ranges that are not internet-routable: loopback, RFC 1918, RFC 6598 shared
/// space, IPv6 ULA and link-local. Used only to decide whether an ACL "admits
/// public addresses" for the startup warning — never as an access default.
pub const NON_PUBLIC: &[&str] = &[
    "127.0.0.0/8",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "::1/128",
    "fc00::/7",
    "fe80::/10",
];

/// Request headers that mean "this connection is relaying someone else's
/// request". From an untrusted peer they make the origin unknowable.
const FORWARDING_HEADERS: &[&str] = &["forwarded", "x-forwarded-for", "x-real-ip"];

/// The verdict for one request against an [`InternalAccess`] policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Serve the endpoint.
    Allow,
    /// Multi-site node with no credential configured: never served.
    CredentialRequired,
    /// The client address is refused by the ACL (or its origin is unknown).
    Forbidden,
    /// The address is allowed but the bearer token is missing or wrong.
    Unauthorized,
}

/// The facts about a request the policy decides on, gathered by the router.
#[derive(Debug, Clone, Copy)]
pub struct Client<'a> {
    /// Client address after `trusted_proxies` resolution.
    pub ip: IpAddr,
    /// Whether the TCP peer is a configured trusted proxy.
    pub peer_is_trusted_proxy: bool,
    /// Whether the connection presented a client certificate the mTLS
    /// metrics listener verified against `client_ca`.
    pub mtls_verified: bool,
    /// The request headers (for forwarding markers and `Authorization`).
    pub headers: &'a HeaderMap<HeaderValue>,
}

/// A resolved access policy for one endpoint family.
#[derive(Debug, Clone)]
pub struct InternalAccess {
    /// Config section named in denial messages, e.g. `"[server.metrics]"`.
    section: &'static str,
    allow: Vec<IpNet>,
    deny: Vec<IpNet>,
    /// A `deny` entry failed to parse: deny everyone (dropping it would widen).
    deny_all: bool,
    token: Option<Box<[u8]>>,
    /// Multi-site: a credential (token, or a verified client certificate) is
    /// mandatory.
    require_credential: bool,
}

impl InternalAccess {
    /// Build a policy from the raw config values.
    ///
    /// `require_credential` is `true` on a multi-site node outside dev mode:
    /// the endpoint is then served only to a request that carries the token or
    /// arrived over a verified mTLS connection.
    #[must_use]
    pub fn new(
        section: &'static str,
        allow: &[String],
        deny: &[String],
        token: Option<&str>,
        require_credential: bool,
    ) -> Self {
        let allow: Vec<IpNet> = allow.iter().filter_map(|s| parse_net(s)).collect();
        let parsed_deny: Vec<IpNet> = deny.iter().filter_map(|s| parse_net(s)).collect();
        let deny_all = parsed_deny.len() != deny.len();
        Self {
            section,
            allow,
            deny: parsed_deny,
            deny_all,
            token: token.map(|t| t.trim().as_bytes().to_vec().into_boxed_slice()),
            require_credential,
        }
    }

    /// Override the multi-site credential requirement (`ephpm dev` lifts it).
    pub fn set_require_credential(&mut self, require: bool) {
        self.require_credential = require;
    }

    /// Whether this policy refuses every request that lacks a credential.
    #[must_use]
    pub fn requires_credential(&self) -> bool {
        self.require_credential
    }

    /// Whether a bearer token is configured.
    #[must_use]
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Whether any IP ACL is configured (non-empty `allow` or `deny`).
    #[must_use]
    pub fn has_acl(&self) -> bool {
        !self.allow.is_empty() || !self.deny.is_empty() || self.deny_all
    }

    /// Whether the ACL admits some internet-routable address. An empty
    /// `allow` admits everything; otherwise some `allow` entry must reach
    /// outside the [`NON_PUBLIC`] ranges. (`deny` is ignored here: a deny list
    /// carving holes in an open allowlist still leaves it open.)
    #[must_use]
    pub fn admits_public(&self) -> bool {
        if self.deny_all {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        let private: Vec<IpNet> = NON_PUBLIC.iter().filter_map(|s| parse_net(s)).collect();
        self.allow.iter().any(|net| !private.iter().any(|p| p.contains(net)))
    }

    /// Whether the client address alone passes the IP ACL (no credential or
    /// token check).
    #[must_use]
    pub fn ip_allowed(&self, client: &Client<'_>) -> bool {
        if !self.has_acl() {
            return true;
        }
        if self.deny_all {
            return false;
        }
        if !client.peer_is_trusted_proxy
            && FORWARDING_HEADERS.iter().any(|h| client.headers.contains_key(*h))
        {
            return false;
        }
        let ip = client.ip.to_canonical();
        if self.deny.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|net| net.contains(&ip))
    }

    /// Decide whether to serve the endpoint to `client`.
    #[must_use]
    pub fn check(&self, client: &Client<'_>) -> Verdict {
        if self.require_credential && self.token.is_none() && !client.mtls_verified {
            return Verdict::CredentialRequired;
        }
        if !self.ip_allowed(client) {
            return Verdict::Forbidden;
        }
        match &self.token {
            // An mTLS-verified client already proved a credential; the token is
            // still required when one is set (both layers are explicit config).
            None => Verdict::Allow,
            Some(expected) => {
                let presented = client
                    .headers
                    .get(hyper::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(bearer_token);
                match presented {
                    Some(p) if constant_time_eq(p.as_bytes(), expected) => Verdict::Allow,
                    _ => Verdict::Unauthorized,
                }
            }
        }
    }

    /// The response for a refused request. Names the config section so an
    /// operator who just upgraded learns which knob to set, but never echoes
    /// the configured ACL or token.
    ///
    /// # Panics
    ///
    /// Never in practice: every header is a fixed valid ASCII value, so the
    /// response builder cannot fail.
    #[must_use]
    pub fn deny_response(&self, verdict: Verdict) -> Response<ServerBody> {
        let section = self.section;
        let (status, msg) = match verdict {
            Verdict::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                format!(
                    "401 Unauthorized: this endpoint requires the bearer token configured as \
                     {section} token."
                ),
            ),
            Verdict::CredentialRequired => (
                StatusCode::FORBIDDEN,
                format!(
                    "403 Forbidden: on a multi-site node this endpoint is served only with a \
                     credential, because every tenant's PHP can reach this listener. Set \
                     {section} token (or serve it on an mTLS [server.metrics] listen with \
                     client_ca)."
                ),
            ),
            Verdict::Forbidden | Verdict::Allow => (
                StatusCode::FORBIDDEN,
                format!("403 Forbidden: client address not permitted by {section} allow/deny."),
            ),
        };
        let mut builder = Response::builder()
            .status(status)
            .header("content-type", "text/plain")
            .header("cache-control", "no-store");
        if status == StatusCode::UNAUTHORIZED {
            builder = builder.header("www-authenticate", "Bearer");
        }
        builder.body(body::buffered(Full::new(Bytes::from(msg)))).expect("static deny response")
    }
}

/// Parse an ACL entry: a CIDR (`10.0.0.0/8`) or a bare address (`192.0.2.7`,
/// meaning a single host).
#[must_use]
pub fn parse_net(s: &str) -> Option<IpNet> {
    let s = s.trim();
    s.parse::<IpNet>().ok().or_else(|| s.parse::<IpAddr>().ok().map(IpNet::from))
}

/// Extract the token from an `Authorization: Bearer <token>` value. The scheme
/// is case-insensitive (RFC 7235); surrounding whitespace is ignored.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim()).filter(|t| !t.is_empty())
}

/// Length-leaking but content-constant-time comparison. The length of a
/// bearer token is not the secret; its bytes are.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client<'a>(ip: &str, headers: &'a HeaderMap<HeaderValue>) -> Client<'a> {
        Client {
            ip: ip.parse().unwrap(),
            peer_is_trusted_proxy: false,
            mtls_verified: false,
            headers,
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap<HeaderValue> {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn policy(allow: &[&str], deny: &[&str], token: Option<&str>) -> InternalAccess {
        InternalAccess::new("[server.metrics]", &strings(allow), &strings(deny), token, false)
    }

    /// The upgrade guarantee: no config means everyone is served, exactly as
    /// before the ACL existed — including forwarded requests.
    #[test]
    fn default_is_open() {
        let p = policy(&[], &[], None);
        let h = HeaderMap::new();
        for ip in ["127.0.0.1", "10.1.2.3", "203.0.113.9", "2001:db8::1"] {
            assert_eq!(p.check(&client(ip, &h)), Verdict::Allow, "{ip}");
        }
        let fwd = headers(&[("x-forwarded-for", "198.51.100.1")]);
        assert_eq!(p.check(&client("127.0.0.1", &fwd)), Verdict::Allow);
        assert!(p.admits_public());
        assert!(!p.has_acl());
    }

    #[test]
    fn allow_restricts_and_deny_wins() {
        let p = policy(&["10.0.0.0/8", "127.0.0.1"], &["10.9.0.0/16"], None);
        let h = HeaderMap::new();
        assert_eq!(p.check(&client("10.1.2.3", &h)), Verdict::Allow);
        assert_eq!(p.check(&client("127.0.0.1", &h)), Verdict::Allow);
        assert_eq!(p.check(&client("10.9.1.1", &h)), Verdict::Forbidden, "deny wins");
        assert_eq!(p.check(&client("203.0.113.9", &h)), Verdict::Forbidden);
        assert!(!p.admits_public());

        // Deny-only: empty allow means "everyone except".
        let d = policy(&[], &["203.0.113.0/24"], None);
        assert_eq!(d.check(&client("203.0.113.9", &h)), Verdict::Forbidden);
        assert_eq!(d.check(&client("198.51.100.1", &h)), Verdict::Allow);
    }

    #[test]
    fn ipv4_mapped_peers_match_as_ipv4() {
        let p = policy(&["10.0.0.0/8"], &[], None);
        let h = HeaderMap::new();
        assert_eq!(p.check(&client("::ffff:10.0.0.5", &h)), Verdict::Allow);
        assert_eq!(p.check(&client("::ffff:203.0.113.9", &h)), Verdict::Forbidden);
    }

    /// With an ACL configured, a same-host reverse proxy (loopback peer, not
    /// a trusted proxy) relaying an internet client must not launder it.
    #[test]
    fn forwarded_requests_from_an_untrusted_peer_are_refused_under_an_acl() {
        let p = policy(&["127.0.0.0/8"], &[], None);
        for name in ["x-forwarded-for", "forwarded", "x-real-ip"] {
            let h = headers(&[(name, "203.0.113.9")]);
            assert_eq!(p.check(&client("127.0.0.1", &h)), Verdict::Forbidden, "{name}");
        }
        // From a trusted proxy the resolved client address decides instead.
        let h = headers(&[("x-forwarded-for", "127.0.0.7")]);
        let trusted = Client {
            ip: "127.0.0.7".parse().unwrap(),
            peer_is_trusted_proxy: true,
            mtls_verified: false,
            headers: &h,
        };
        assert_eq!(p.check(&trusted), Verdict::Allow);
    }

    #[test]
    fn an_unparseable_deny_entry_denies_everyone() {
        let p = policy(&[], &["not-a-cidr"], None);
        let h = HeaderMap::new();
        assert_eq!(p.check(&client("127.0.0.1", &h)), Verdict::Forbidden);
        // An unparseable allow entry only narrows.
        let a = policy(&["not-a-cidr", "10.0.0.0/8"], &[], None);
        assert_eq!(a.check(&client("10.0.0.1", &h)), Verdict::Allow);
        assert_eq!(a.check(&client("127.0.0.1", &h)), Verdict::Forbidden);
    }

    #[test]
    fn token_is_required_on_top_of_the_acl() {
        let p = policy(&["127.0.0.0/8"], &[], Some("s3cret-token"));
        let none = HeaderMap::new();
        assert_eq!(p.check(&client("127.0.0.1", &none)), Verdict::Unauthorized);
        let wrong = headers(&[("authorization", "Bearer nope")]);
        assert_eq!(p.check(&client("127.0.0.1", &wrong)), Verdict::Unauthorized);
        let basic = headers(&[("authorization", "Basic s3cret-token")]);
        assert_eq!(p.check(&client("127.0.0.1", &basic)), Verdict::Unauthorized);
        let right = headers(&[("authorization", "bearer s3cret-token")]);
        assert_eq!(p.check(&client("127.0.0.1", &right)), Verdict::Allow);
        // The token never overrides the ACL.
        assert_eq!(p.check(&client("203.0.113.9", &right)), Verdict::Forbidden);
    }

    /// Multi-site: no credential configured → refused for everyone, even
    /// loopback (tenant PHP is on loopback too). A token or a verified client
    /// certificate lifts it.
    #[test]
    fn multi_site_requires_a_credential() {
        let h = HeaderMap::new();
        let none = InternalAccess::new("[server.metrics]", &[], &[], None, true);
        assert_eq!(none.check(&client("127.0.0.1", &h)), Verdict::CredentialRequired);

        let mut mtls = client("127.0.0.1", &h);
        mtls.mtls_verified = true;
        assert_eq!(none.check(&mtls), Verdict::Allow);

        let tok = InternalAccess::new("[server.metrics]", &[], &[], Some("t"), true);
        assert_eq!(tok.check(&client("127.0.0.1", &h)), Verdict::Unauthorized);
        let right = headers(&[("authorization", "Bearer t")]);
        assert_eq!(tok.check(&client("127.0.0.1", &right)), Verdict::Allow);
    }

    #[test]
    fn deny_responses_carry_the_right_status_and_name_the_knob() {
        let p = policy(&[], &[], Some("s3cret-token"));
        let r = p.deny_response(Verdict::Unauthorized);
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(r.headers().get("www-authenticate").unwrap(), "Bearer");
        assert_eq!(p.deny_response(Verdict::Forbidden).status(), StatusCode::FORBIDDEN);
        assert_eq!(p.deny_response(Verdict::CredentialRequired).status(), StatusCode::FORBIDDEN);
    }
}

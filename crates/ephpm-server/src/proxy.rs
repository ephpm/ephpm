//! Built-in single-hop HTTP reverse proxy (`[[server.proxy]]`).
//!
//! # What this is
//!
//! An **ordered rule list**. Each rule matches on **host** and **path** and
//! forwards a matched request to exactly **one** upstream. Rules are tried
//! top-to-bottom and the **first match wins**. A matched rule short-circuits all
//! local serving — [`Router::handle_inner`](crate::router::Router) consults the
//! proxy *before* it resolves a vhost, serves a static file, runs PHP, or
//! terminates a native WebSocket. So a proxied host/path is not served locally
//! at all; it belongs to the backend.
//!
//! It is deliberately a **single-hop forwarder, not an edge load balancer**.
//! There is one upstream per rule and no pool, health checks, retries, circuit
//! breaking, or response rewriting. Those are v2+ (documented as out of scope in
//! `site/content/`).
//!
//! # Why it exists
//!
//! The load-bearing case is **multi-PHP-version hosting**: ePHPm runs one PHP
//! version per process, so an edge instance can route by host to
//! version-pinned backends (`pr-a.preview → :9084` on PHP 8.4, `pr-b → :9085` on
//! 8.5) with clean URLs, no ports in the URL, and TLS terminated once here. The
//! secondary case is **strangler migration** — route `/api` to a legacy backend
//! and move an app onto ePHPm route by route.
//!
//! # Streaming
//!
//! Both directions stream. The incoming request body is boxed and handed
//! straight to hyper's client (no buffering — uploads stream up), and the
//! upstream response body is boxed into a [`ServerBody`] (no buffering — SSE and
//! large downloads stream down). `read_timeout_secs` bounds only the time to
//! obtain the response **head**, never the streamed body.
//!
//! # WebSockets
//!
//! A matched rule whose request is an RFC 6455 upgrade is tunnelled: ePHPm opens
//! a fresh HTTP/1.1 connection to the upstream (not the pooled client — an
//! upgraded socket is single-use), relays the `101`, then copies bytes in both
//! directions until either side closes. This is a raw tunnel — distinct from the
//! native WebSocket *termination* in [`crate::websocket`], which parses frames
//! and runs PHP per event.
//!
//! # Forwarded headers
//!
//! The original `Host` is preserved. `X-Forwarded-For` is set to the resolved
//! client IP (the inbound proxy chain has already been collapsed to that IP by
//! `[server.security] trusted_proxies`), `X-Forwarded-Proto` to the client-facing
//! scheme, and `X-Forwarded-Host` to the original `Host`. Hop-by-hop headers are
//! stripped in both directions (except the upgrade headers on the WebSocket
//! path).

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use ephpm_config::ProxyRuleConfig;
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrics::counter;

use crate::body::{self, ServerBody};
use crate::router::RequestBody;
use crate::websocket::is_upgrade_request;

/// The request-body type handed to the proxy client. The incoming body (hyper
/// `Incoming` on TCP, `H3RequestBody` on HTTP/3) is boxed into this single type
/// so one concrete [`Client`] can forward every transport's requests while still
/// streaming the body through unbuffered.
///
/// Unsync (rather than `BoxBody`) because a [`RequestBody`] is `Send + Unpin`
/// but not `Sync`; hyper's client requires only `Send`, so this suffices.
type ProxyReqBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// Hop-by-hop headers (RFC 7230 §6.1) that must not be forwarded end-to-end.
/// Stripped from both the forwarded request and the relayed response — except on
/// the WebSocket path, which deliberately keeps `Connection`/`Upgrade`.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// How a request host is matched against a rule's `host`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostMatcher {
    /// `"*"` or omitted — matches any host.
    Any,
    /// Exact host (already lowercased).
    Exact(String),
    /// `"*.example.com"` — exactly one leftmost label over the stored base
    /// (`example.com`).
    Wildcard(String),
    /// `".example.com"` — the apex (`example.com`) or any subdomain. Stores the
    /// base without the leading dot.
    Suffix(String),
}

impl HostMatcher {
    /// Compile the config `host` string into a matcher. Assumes the syntax has
    /// already passed [`ProxyRuleConfig::validate_host`].
    fn compile(host: Option<&str>) -> Self {
        match host.map(str::trim) {
            None | Some("" | "*") => Self::Any,
            Some(h) if h.starts_with("*.") => Self::Wildcard(h[2..].to_ascii_lowercase()),
            Some(h) if h.starts_with('.') => Self::Suffix(h[1..].to_ascii_lowercase()),
            Some(h) => Self::Exact(h.to_ascii_lowercase()),
        }
    }

    /// Does `host` (already lowercased, port and trailing dot stripped) match?
    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(want) => host == want,
            Self::Wildcard(base) => host
                .strip_suffix(base)
                .and_then(|label| label.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty() && !label.contains('.')),
            Self::Suffix(base) => {
                host == base || host.strip_suffix(base).is_some_and(|p| p.ends_with('.'))
            }
        }
    }
}

/// How a request path is matched against a rule's `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathMatcher {
    /// Segment-aware prefix: `/api` matches `/api`, `/api/`, `/api/x` but not
    /// `/apiary`. `/` matches everything.
    Prefix(String),
    /// The request path must equal this exactly.
    Exact(String),
}

impl PathMatcher {
    fn compile(path: &str, exact: bool) -> Self {
        if exact { Self::Exact(path.to_string()) } else { Self::Prefix(path.to_string()) }
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(want) => path == want,
            Self::Prefix(prefix) => {
                let Some(rest) = path.strip_prefix(prefix.as_str()) else { return false };
                // Match at a segment boundary only, so `/api` does not match
                // `/apiary`. An empty remainder is an exact hit; a prefix that
                // already ends in `/` needs no boundary check.
                rest.is_empty() || prefix.ends_with('/') || rest.starts_with('/')
            }
        }
    }
}

/// One compiled `[[server.proxy]]` rule: its matchers, its upstream, and its own
/// pooled client (per-rule so `connect_timeout_secs` can differ between rules;
/// each client pools connections to exactly this rule's one authority).
struct CompiledRule {
    host: HostMatcher,
    path: PathMatcher,
    /// The upstream authority (`host[:port]`), used to build the outgoing URI.
    authority: Authority,
    /// `host:port` for a manual `TcpStream::connect` on the WebSocket path, with
    /// the default port 80 filled in when the config omitted it.
    connect_target: String,
    /// Time to obtain the upstream response head (`None` when disabled).
    read_timeout: Option<Duration>,
    /// TCP connect deadline for the WebSocket path (`None` when disabled). The
    /// pooled `client` enforces the same bound via its connector.
    connect_timeout: Option<Duration>,
    /// Pooled HTTP client for the ordinary (non-upgrade) path.
    client: Client<HttpConnector, ProxyReqBody>,
}

/// The reverse-proxy engine: the ordered, compiled rule list.
///
/// `None` on the [`Router`](crate::router::Router) when `[[server.proxy]]` is
/// empty — the request path pays one `Option::is_none()`.
pub struct ProxyEngine {
    rules: Vec<CompiledRule>,
}

impl ProxyEngine {
    /// Compile the config rules, or `None` when there are none.
    ///
    /// Every rule has already passed [`ephpm_config::Config::validate`] at
    /// startup, so upstream/host parsing here cannot fail for the cases validate
    /// checks; a defensive parse failure logs and skips the rule rather than
    /// panicking.
    #[must_use]
    pub fn new(rules: &[ProxyRuleConfig]) -> Option<Self> {
        if rules.is_empty() {
            return None;
        }
        let compiled: Vec<CompiledRule> =
            rules.iter().enumerate().filter_map(|(i, rule)| Self::compile_rule(i, rule)).collect();
        if compiled.is_empty() {
            return None;
        }
        tracing::info!(rules = compiled.len(), "built-in reverse proxy enabled");
        Some(Self { rules: compiled })
    }

    /// Compile one rule, returning `None` (with an error log) if its upstream
    /// somehow fails to parse despite startup validation.
    fn compile_rule(index: usize, rule: &ProxyRuleConfig) -> Option<CompiledRule> {
        let authority_str = match rule.upstream_authority() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(rule = index, "skipping invalid [[server.proxy]] upstream: {e}");
                return None;
            }
        };
        let authority = match authority_str.parse::<Authority>() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(
                    rule = index,
                    upstream = %authority_str,
                    "skipping [[server.proxy]] rule — upstream is not a valid HTTP authority: {e}"
                );
                return None;
            }
        };
        let connect_target = if authority.port_u16().is_some() {
            authority_str.clone()
        } else {
            format!("{authority_str}:80")
        };

        let connect_timeout = non_zero_secs(rule.connect_timeout_secs);
        let read_timeout = non_zero_secs(rule.read_timeout_secs);

        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(connect_timeout);
        connector.enforce_http(true);
        let client: Client<HttpConnector, ProxyReqBody> =
            Client::builder(TokioExecutor::new()).build(connector);

        Some(CompiledRule {
            host: HostMatcher::compile(rule.host.as_deref()),
            path: PathMatcher::compile(&rule.path, rule.path_exact),
            authority,
            connect_target,
            read_timeout,
            connect_timeout,
            client,
        })
    }

    /// Index of the first rule matching `host` (already normalized: lowercased,
    /// port and trailing dot stripped) and `path` (percent-decoded), or `None`
    /// when no rule matches. Cheap and non-consuming: the caller checks this
    /// first and only hands the request to [`forward`](Self::forward) on a hit,
    /// so a non-match falls through to local serving without moving the request.
    #[must_use]
    pub fn match_index(&self, host: &str, path: &str) -> Option<usize> {
        self.rules.iter().position(|r| r.host.matches(host) && r.path.matches(path))
    }

    /// Forward `req` to the rule at `idx` (from [`match_index`](Self::match_index))
    /// and return the response, streaming both directions.
    ///
    /// `effective_addr`/`is_https` are the resolved client identity from
    /// `[server.security] trusted_proxies`; `original_host_header` is the raw
    /// client `Host` used for `X-Forwarded-Host`.
    pub async fn forward<B: RequestBody>(
        &self,
        idx: usize,
        req: Request<B>,
        original_host_header: &str,
        effective_addr: SocketAddr,
        is_https: bool,
    ) -> Response<ServerBody> {
        let rule = &self.rules[idx];
        counter!("ephpm_proxy_requests_total").increment(1);
        if is_upgrade_request(&req) {
            proxy_upgrade(rule, req, original_host_header, effective_addr, is_https).await
        } else {
            proxy_http(rule, req, original_host_header, effective_addr, is_https).await
        }
    }
}

/// `0` disables a timeout knob; anything else is seconds.
fn non_zero_secs(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Build the outgoing upstream URI: `http://<upstream-authority><original path
/// and query>`. v1 forwards the original path unchanged (no rewriting).
fn upstream_uri<B>(req: &Request<B>, authority: &Authority) -> Uri {
    let path_and_query =
        req.uri().path_and_query().cloned().unwrap_or_else(|| PathAndQuery::from_static("/"));
    Uri::builder()
        .scheme(Scheme::HTTP)
        .authority(authority.clone())
        .path_and_query(path_and_query)
        .build()
        .expect("valid upstream URI from validated authority")
}

/// Copy the client headers to the upstream, dropping hop-by-hop headers, and add
/// the `X-Forwarded-*` set. `keep_upgrade` keeps `Connection`/`Upgrade` for the
/// WebSocket tunnel.
fn build_forward_headers(
    src: &HeaderMap,
    original_host_header: &str,
    effective_addr: SocketAddr,
    is_https: bool,
    keep_upgrade: bool,
) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len() + 3);
    for (name, value) in src {
        if is_hop_by_hop(name.as_str(), keep_upgrade) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }

    // X-Forwarded-For: the resolved client IP. The inbound proxy chain has
    // already been collapsed to this IP by `trusted_proxies`, so ePHPm sets a
    // single honest value rather than passing an untrusted client-supplied
    // chain through.
    insert_str(&mut out, "x-forwarded-for", &effective_addr.ip().to_string());
    insert_str(&mut out, "x-forwarded-proto", if is_https { "https" } else { "http" });
    if !original_host_header.is_empty() {
        insert_str(&mut out, "x-forwarded-host", original_host_header);
    }
    out
}

/// Whether `name` is a hop-by-hop header. On the WebSocket path the two upgrade
/// headers are kept so the tunnel can be negotiated end to end.
fn is_hop_by_hop(name: &str, keep_upgrade: bool) -> bool {
    if keep_upgrade
        && (name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("upgrade"))
    {
        return false;
    }
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// Insert a header, replacing any existing value, ignoring an unparseable value.
fn insert_str(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), v);
    }
}

/// Forward an ordinary (non-upgrade) request and stream the response back.
async fn proxy_http<B: RequestBody>(
    rule: &CompiledRule,
    req: Request<B>,
    original_host_header: &str,
    effective_addr: SocketAddr,
    is_https: bool,
) -> Response<ServerBody> {
    let uri = upstream_uri(&req, &rule.authority);
    let method = req.method().clone();
    let headers =
        build_forward_headers(req.headers(), original_host_header, effective_addr, is_https, false);

    // Box the incoming body into `ProxyReqBody` — this streams the body through
    // to the upstream without buffering it.
    let body: ProxyReqBody = req.into_body().map_err(std::io::Error::other).boxed_unsync();

    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(dst) = builder.headers_mut() {
        *dst = headers;
    }
    let upstream_req = builder.body(body).expect("valid upstream request");

    let send = rule.client.request(upstream_req);
    let result = match rule.read_timeout {
        Some(deadline) => match tokio::time::timeout(deadline, send).await {
            Ok(r) => r,
            Err(_) => {
                counter!("ephpm_proxy_errors_total", "kind" => "read_timeout").increment(1);
                return bad_gateway("upstream did not send a response head in time");
            }
        },
        None => send.await,
    };

    let upstream = match result {
        Ok(resp) => resp,
        Err(e) => {
            counter!("ephpm_proxy_errors_total", "kind" => "connect").increment(1);
            tracing::debug!(%e, "reverse proxy upstream request failed");
            return bad_gateway("upstream is unreachable");
        }
    };

    // Relay status + headers (hop-by-hop stripped) and stream the body.
    let (parts, incoming) = upstream.into_parts();
    let mut response = Response::builder().status(parts.status);
    if let Some(dst) = response.headers_mut() {
        for (name, value) in &parts.headers {
            if !is_hop_by_hop(name.as_str(), false) {
                dst.append(name.clone(), value.clone());
            }
        }
    }
    let out_body: ServerBody = incoming.map_err(std::io::Error::other).boxed();
    response.body(out_body).expect("valid proxied response")
}

/// Tunnel a WebSocket upgrade to the upstream over a fresh HTTP/1.1 connection.
async fn proxy_upgrade<B: RequestBody>(
    rule: &CompiledRule,
    mut req: Request<B>,
    original_host_header: &str,
    effective_addr: SocketAddr,
    is_https: bool,
) -> Response<ServerBody> {
    // Capture the downstream (client-facing) upgrade before the request is
    // consumed.
    let downstream = hyper::upgrade::on(&mut req);

    let uri = upstream_uri(&req, &rule.authority);
    let method = req.method().clone();
    // Keep Connection/Upgrade so the upstream negotiates the same upgrade.
    let headers =
        build_forward_headers(req.headers(), original_host_header, effective_addr, is_https, true);

    // Connect a dedicated TCP connection (not the pooled client — an upgraded
    // socket is single-use).
    let connect = tokio::net::TcpStream::connect(rule.connect_target.clone());
    let stream = match rule.connect_timeout {
        Some(d) => match tokio::time::timeout(d, connect).await {
            Ok(Ok(s)) => s,
            _ => {
                counter!("ephpm_proxy_errors_total", "kind" => "ws_connect").increment(1);
                return bad_gateway("upstream WebSocket backend is unreachable");
            }
        },
        None => match connect.await {
            Ok(s) => s,
            Err(_) => {
                counter!("ephpm_proxy_errors_total", "kind" => "ws_connect").increment(1);
                return bad_gateway("upstream WebSocket backend is unreachable");
            }
        },
    };

    let io = TokioIo::new(stream);
    let (mut sender, conn) =
        match hyper::client::conn::http1::handshake::<_, Empty<Bytes>>(io).await {
            Ok(pair) => pair,
            Err(e) => {
                counter!("ephpm_proxy_errors_total", "kind" => "ws_handshake").increment(1);
                tracing::debug!(%e, "reverse proxy upstream WebSocket handshake failed");
                return bad_gateway("upstream WebSocket handshake failed");
            }
        };
    // Drive the client connection (with upgrade support) in the background.
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!(%e, "reverse proxy upstream WebSocket connection closed");
        }
    });

    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(dst) = builder.headers_mut() {
        *dst = headers;
    }
    let upstream_req = builder.body(Empty::<Bytes>::new()).expect("valid upstream upgrade request");

    let upstream_resp = match sender.send_request(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            counter!("ephpm_proxy_errors_total", "kind" => "ws_request").increment(1);
            tracing::debug!(%e, "reverse proxy upstream WebSocket request failed");
            return bad_gateway("upstream WebSocket request failed");
        }
    };

    if upstream_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        // The upstream refused the upgrade — relay its response verbatim.
        let (parts, incoming) = upstream_resp.into_parts();
        let mut response = Response::builder().status(parts.status);
        if let Some(dst) = response.headers_mut() {
            for (name, value) in &parts.headers {
                if !is_hop_by_hop(name.as_str(), false) {
                    dst.append(name.clone(), value.clone());
                }
            }
        }
        let out_body: ServerBody = incoming.map_err(std::io::Error::other).boxed();
        return response.body(out_body).expect("valid proxied response");
    }

    // Build the 101 to hand back to the client, carrying the upstream's upgrade
    // headers so the client completes the same handshake.
    let mut response = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    if let Some(dst) = response.headers_mut() {
        for (name, value) in upstream_resp.headers() {
            // Keep the upgrade headers; drop the rest of the hop-by-hop set.
            if name.as_str().eq_ignore_ascii_case("connection")
                || name.as_str().eq_ignore_ascii_case("upgrade")
                || name.as_str().to_ascii_lowercase().starts_with("sec-websocket")
            {
                dst.append(name.clone(), value.clone());
            }
        }
    }
    let response = response
        .body(body::buffered(Full::new(Bytes::new())))
        .expect("valid 101 switching protocols response");

    // Capture the upstream upgrade and tunnel bytes once both sides are upgraded.
    let upstream_upgrade = hyper::upgrade::on(upstream_resp);
    tokio::spawn(async move {
        let (client_io, server_io) = match tokio::try_join!(downstream, upstream_upgrade) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!(%e, "reverse proxy WebSocket upgrade did not complete");
                return;
            }
        };
        let mut client_io = TokioIo::new(client_io);
        let mut server_io = TokioIo::new(server_io);
        match tokio::io::copy_bidirectional(&mut client_io, &mut server_io).await {
            Ok((c2s, s2c)) => {
                tracing::debug!(c2s, s2c, "reverse proxy WebSocket tunnel closed");
            }
            Err(e) => tracing::debug!(%e, "reverse proxy WebSocket tunnel error"),
        }
    });

    counter!("ephpm_proxy_ws_tunnels_total").increment(1);
    response
}

/// A `502 Bad Gateway` with a short plain-text body — never a hang.
fn bad_gateway(reason: &'static str) -> Response<ServerBody> {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body::buffered(Full::new(Bytes::from(format!("502 Bad Gateway: {reason}")))))
        .expect("valid 502 response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_any_matches_everything() {
        let m = HostMatcher::compile(None);
        assert!(m.matches("anything.example.com"));
        assert_eq!(HostMatcher::compile(Some("*")), HostMatcher::Any);
    }

    #[test]
    fn host_exact() {
        let m = HostMatcher::compile(Some("App.Example.com"));
        assert!(m.matches("app.example.com"));
        assert!(!m.matches("api.example.com"));
        assert!(!m.matches("app.example.com.evil.com"));
    }

    #[test]
    fn host_wildcard_is_one_leftmost_label() {
        let m = HostMatcher::compile(Some("*.example.com"));
        assert!(m.matches("pr-a.example.com"));
        assert!(!m.matches("example.com"), "wildcard does not match the apex");
        assert!(!m.matches("a.b.example.com"), "wildcard is one label only");
        assert!(!m.matches("app.other.com"));
    }

    #[test]
    fn host_suffix_matches_apex_and_subdomains() {
        let m = HostMatcher::compile(Some(".example.com"));
        assert!(m.matches("example.com"), "suffix matches the apex");
        assert!(m.matches("a.example.com"));
        assert!(m.matches("a.b.example.com"));
        assert!(!m.matches("notexample.com"));
        assert!(!m.matches("example.com.evil.com"));
    }

    #[test]
    fn path_prefix_is_segment_aware() {
        let m = PathMatcher::compile("/api", false);
        assert!(m.matches("/api"));
        assert!(m.matches("/api/"));
        assert!(m.matches("/api/v1/users"));
        assert!(!m.matches("/apiary"), "prefix must stop at a segment boundary");
        assert!(!m.matches("/"));
    }

    #[test]
    fn path_root_prefix_matches_all() {
        let m = PathMatcher::compile("/", false);
        assert!(m.matches("/"));
        assert!(m.matches("/anything/here"));
    }

    #[test]
    fn path_exact() {
        let m = PathMatcher::compile("/healthz", true);
        assert!(m.matches("/healthz"));
        assert!(!m.matches("/healthz/"));
        assert!(!m.matches("/healthz/sub"));
    }

    fn rule(host: Option<&str>, path: &str, exact: bool) -> ProxyRuleConfig {
        ProxyRuleConfig {
            host: host.map(str::to_string),
            path: path.to_string(),
            path_exact: exact,
            upstream: "http://127.0.0.1:9000".to_string(),
            connect_timeout_secs: 5,
            read_timeout_secs: 60,
        }
    }

    #[test]
    fn first_match_wins_in_order() {
        let engine = ProxyEngine::new(&[
            rule(Some("app.example.com"), "/api", false),
            rule(Some("app.example.com"), "/", false),
            rule(None, "/", false),
        ])
        .expect("engine");

        assert_eq!(engine.match_index("app.example.com", "/api/v1"), Some(0));
        assert_eq!(engine.match_index("app.example.com", "/home"), Some(1));
        assert_eq!(engine.match_index("other.example.com", "/home"), Some(2));
    }

    #[test]
    fn empty_rules_disable_the_engine() {
        assert!(ProxyEngine::new(&[]).is_none());
    }

    #[test]
    fn no_match_returns_none() {
        let engine = ProxyEngine::new(&[rule(Some("app.example.com"), "/api", false)]).unwrap();
        assert_eq!(engine.match_index("other.example.com", "/api"), None);
        assert_eq!(engine.match_index("app.example.com", "/other"), None);
    }

    #[test]
    fn hop_by_hop_detection_respects_the_upgrade_exception() {
        assert!(is_hop_by_hop("Transfer-Encoding", false));
        assert!(is_hop_by_hop("Connection", false));
        assert!(is_hop_by_hop("Upgrade", false));
        // On the WebSocket path the two upgrade headers are kept.
        assert!(!is_hop_by_hop("Connection", true));
        assert!(!is_hop_by_hop("Upgrade", true));
        assert!(is_hop_by_hop("Transfer-Encoding", true));
        assert!(!is_hop_by_hop("Content-Type", false));
    }

    #[test]
    fn forward_headers_set_x_forwarded_and_preserve_host() {
        let mut src = HeaderMap::new();
        src.insert("host", HeaderValue::from_static("app.example.com"));
        src.insert("connection", HeaderValue::from_static("keep-alive"));
        src.insert("x-forwarded-for", HeaderValue::from_static("9.9.9.9"));
        let addr: SocketAddr = "203.0.113.7:54321".parse().unwrap();

        let out = build_forward_headers(&src, "app.example.com", addr, true, false);
        assert_eq!(out.get("host").unwrap(), "app.example.com", "Host preserved");
        assert!(out.get("connection").is_none(), "hop-by-hop stripped");
        assert_eq!(
            out.get("x-forwarded-for").unwrap(),
            "203.0.113.7",
            "XFF is the resolved client IP, not the spoofable inbound value"
        );
        assert_eq!(out.get("x-forwarded-proto").unwrap(), "https");
        assert_eq!(out.get("x-forwarded-host").unwrap(), "app.example.com");
    }

    #[test]
    fn ws_upgrade_kept_in_forward_headers() {
        let mut src = HeaderMap::new();
        src.insert("connection", HeaderValue::from_static("Upgrade"));
        src.insert("upgrade", HeaderValue::from_static("websocket"));
        let addr: SocketAddr = "203.0.113.7:1".parse().unwrap();
        let out = build_forward_headers(&src, "h", addr, false, true);
        assert_eq!(out.get("connection").unwrap(), "Upgrade");
        assert_eq!(out.get("upgrade").unwrap(), "websocket");
    }
}

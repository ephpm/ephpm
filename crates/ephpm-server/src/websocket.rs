//! Native WebSocket termination (`[server.websocket]`) — **experimental**.
//!
//! # The model
//!
//! Rust owns every socket; PHP is invoked **per event** and never holds a
//! connection. This is the API-Gateway / RoadRunner shape rather than the
//! Swoole shape:
//!
//! | | ePHPm | Swoole / ReactPHP |
//! |---|---|---|
//! | who owns the socket | the Rust reactor | a PHP process |
//! | cost of an idle connection | one registry entry + reactor memory | a PHP coroutine/process |
//! | what runs per message | one ordinary PHP request | userland event loop callback |
//!
//! Thousands of idle connections therefore cost no PHP at all. A message costs
//! exactly one PHP execution, on the same path an HTTP request takes — so
//! `open_basedir`, the per-vhost temp/session root, the per-site database
//! credentials, OPcache, and the crash guard all apply unchanged.
//!
//! # Lifecycle
//!
//! 1. **Upgrade.** [`Router::handle_inner`](crate::router::Router) spots an RFC
//!    6455 upgrade, resolves the vhost's `[server] websocket_files` entrypoint
//!    against the **already-resolved** document root, and 404s if none exists.
//! 2. **`connect`.** The entrypoint runs with `WS_EVENT=connect` **before** the
//!    handshake completes, using the real upgrade request — so it sees the
//!    cookies, query string and headers auth needs. A `2xx` accepts the
//!    upgrade; anything else is returned to the client verbatim and no socket
//!    is ever created. This is the only point at which an upgrade can be
//!    refused, which is why authentication belongs here.
//! 3. **`message`.** One PHP execution per inbound data frame, with the payload
//!    as the request body (`php://input`) and `WS_OPCODE` set to `text` or
//!    `binary`. Executions for one connection are strictly sequential: the
//!    socket is not read again until the handler returns.
//! 4. **`disconnect`.** Best-effort, after the socket is gone and the
//!    connection has already been removed from the registry — so
//!    `ephpm_ws_send()` to the departing connection correctly fails, and a
//!    broadcast no longer includes it. Not delivered if the process dies.
//!
//! # Site identity
//!
//! The tenant is pinned **once**, at upgrade time, from the canonical site key
//! `Router::resolve_site` produced (issue #293). Every subsequent event for
//! that connection carries the captured key; the `Host` header is never
//! consulted again. A request whose host matched no vhost has no key, so it
//! gets no registry scope and cannot open a WebSocket at all.
//!
//! # Transport
//!
//! HTTP/1.1 only, over plain TCP or TLS (both reach the same
//! `serve_connection`, which already uses `serve_connection_with_upgrades`).
//! HTTP/2 and HTTP/3 requests never carry the `Upgrade` mechanism this path
//! detects — RFC 8441 extended CONNECT is not implemented — and browsers open a
//! dedicated HTTP/1.1 connection for `ws:`/`wss:` regardless.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ephpm_config::WebSocketConfig;
use ephpm_ws::{Limits, Registry};
use futures::{SinkExt, StreamExt};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::Full;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use metrics::counter;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig as CodecConfig};

use crate::body::{self, ServerBody};
use crate::router::Router;

/// WebSocket close code for "the server is going away" — used when a
/// connection is dropped for exceeding
/// [`WebSocketConfig::idle_timeout_secs`].
const CLOSE_GOING_AWAY: u16 = 1001;

/// WebSocket close code for an internal error — used when a `message` event's
/// PHP execution exceeded `[server.timeouts] request`.
const CLOSE_INTERNAL_ERROR: u16 = 1011;

/// Everything the server needs to serve WebSockets, resolved once at startup.
pub struct WsRuntime {
    /// The site-scoped connection registry, shared with the PHP bridge.
    pub registry: Arc<Registry>,
    /// Codec bounds handed to every connection.
    codec: CodecConfig,
    /// Server-initiated ping period, or `None` when keepalive is disabled.
    ping_interval: Option<Duration>,
    /// How long a connection may receive nothing before it is closed, or
    /// `None` when the check is disabled.
    idle_timeout: Option<Duration>,
}

impl WsRuntime {
    /// Build the runtime from `[server.websocket]`.
    ///
    /// Returns `None` when the feature is disabled — the `Option` is what makes
    /// "off" cost exactly one `is_none()` on the request path, and what keeps
    /// the registry (and therefore the PHP bridge) uninstalled.
    #[must_use]
    pub fn new(config: &WebSocketConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }

        let codec = CodecConfig {
            max_message_size: Some(config.max_message_size),
            max_frame_size: Some(config.max_frame_size),
            // RFC 6455 requires client→server frames to be masked. Accepting
            // unmasked ones would let a proxy-confusion payload through, and
            // no conforming client needs it.
            accept_unmasked_frames: false,
            ..CodecConfig::default()
        };

        let registry = Arc::new(Registry::new(Limits {
            max_connections: config.max_connections,
            max_connections_per_site: config.max_connections_per_site,
            send_queue: config.send_queue,
            max_message_size: config.max_message_size,
        }));

        let ping_interval = non_zero_secs(config.ping_interval_secs);
        let idle_timeout = non_zero_secs(config.idle_timeout_secs);
        if let (Some(ping), Some(idle)) = (ping_interval, idle_timeout)
            && ping >= idle
        {
            tracing::warn!(
                ping_interval_secs = config.ping_interval_secs,
                idle_timeout_secs = config.idle_timeout_secs,
                "[server.websocket] ping_interval_secs >= idle_timeout_secs — idle \
                     connections will be closed before a ping can refresh them"
            );
        }

        tracing::info!(
            max_connections = config.max_connections,
            max_connections_per_site = config.max_connections_per_site,
            max_message_size = config.max_message_size,
            send_queue = config.send_queue,
            "native websocket support enabled (experimental)"
        );

        Some(Self { registry, codec, ping_interval, idle_timeout })
    }
}

/// `0` disables a duration knob; anything else is seconds.
fn non_zero_secs(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Whether this request is an RFC 6455 WebSocket upgrade.
///
/// Deliberately loose on the parts a client can get wrong (version, key) and
/// strict on the parts that decide *routing*: method, HTTP version, and the
/// `Connection: Upgrade` + `Upgrade: websocket` pair. A request that looks like
/// an upgrade but is malformed must still be routed here — so it gets a
/// diagnostic `400` instead of falling through and being served `index.php`.
pub fn is_upgrade_request<B>(req: &Request<B>) -> bool {
    if req.method() != Method::GET || req.version() != Version::HTTP_11 {
        return false;
    }
    if !header_contains_token(req.headers(), http::header::CONNECTION, "upgrade") {
        return false;
    }
    header_contains_token(req.headers(), http::header::UPGRADE, "websocket")
}

/// Case-insensitive comma-list token search (`Connection: keep-alive, Upgrade`).
fn header_contains_token(headers: &HeaderMap, name: http::header::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value
            .to_str()
            .is_ok_and(|v| v.split(',').any(|part| part.trim().eq_ignore_ascii_case(token)))
    })
}

/// The client's `Sec-WebSocket-Key`, when the handshake is well formed.
///
/// Returns `None` for a missing key or a `Sec-WebSocket-Version` other than
/// `13`, which the caller turns into a `400` carrying the version we support.
pub(crate) fn handshake_key(headers: &HeaderMap) -> Option<String> {
    let version = headers.get("sec-websocket-version")?.to_str().ok()?;
    if version.trim() != "13" {
        return None;
    }
    Some(headers.get("sec-websocket-key")?.to_str().ok()?.to_owned())
}

/// The `101 Switching Protocols` response completing the handshake.
pub(crate) fn switching_protocols(key: &str) -> Response<ServerBody> {
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::UPGRADE, "websocket")
        .header("sec-websocket-accept", accept)
        .body(body::buffered(Full::new(Bytes::new())))
        .expect("101 response builder")
}

/// A `400` that tells the client which WebSocket version we speak.
pub(crate) fn bad_handshake(reason: &'static str) -> Response<ServerBody> {
    counter!("ephpm_ws_handshake_rejected_total", "reason" => reason).increment(1);
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("sec-websocket-version", "13")
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body::buffered(Full::new(Bytes::from_static(b"400 Bad WebSocket Handshake"))))
        .expect("400 response builder")
}

/// Which lifecycle event a PHP execution represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsEventKind {
    /// Runs before the handshake completes; a non-2xx response refuses it.
    Connect,
    /// One inbound data frame.
    Message,
    /// Best-effort, after the socket is gone.
    Disconnect,
}

impl WsEventKind {
    /// The `$_SERVER['WS_EVENT']` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Message => "message",
            Self::Disconnect => "disconnect",
        }
    }
}

/// The per-event context injected into `$_SERVER`, and the marker that tells
/// `handle_php` this execution is a WebSocket event rather than an HTTP
/// request.
#[derive(Debug, Clone)]
pub struct WsEvent {
    /// `$_SERVER['WS_EVENT']`.
    pub kind: WsEventKind,
    /// `$_SERVER['WS_CONNECTION_ID']` — the capability token for this socket.
    pub connection_id: String,
    /// `$_SERVER['WS_OPCODE']`, `"text"` or `"binary"`. Only set for
    /// [`WsEventKind::Message`].
    pub opcode: Option<&'static str>,
}

impl WsEvent {
    /// The `$_SERVER` entries this event contributes.
    #[must_use]
    pub fn server_vars(&self) -> Vec<(String, String)> {
        let mut vars = vec![
            ("WS_EVENT".to_string(), self.kind.as_str().to_string()),
            ("WS_CONNECTION_ID".to_string(), self.connection_id.clone()),
        ];
        if let Some(opcode) = self.opcode {
            vars.push(("WS_OPCODE".to_string(), opcode.to_string()));
        }
        vars
    }
}

/// Everything a connection's session task needs to run PHP events after the
/// HTTP request that created it is long gone.
///
/// Captured once at upgrade time. In particular `site_key` and `site_scope` are
/// snapshots of the canonical identity `Router::resolve_site` derived for the
/// upgrade request — the session never re-derives a tenant from anything the
/// client sends over the socket.
pub struct WsSession {
    pub(crate) router: Arc<Router>,
    /// The canonical site key, pinned at upgrade time.
    ///
    /// This is the connection's whole tenant identity. Every event re-derives
    /// the per-site database, KV keyspace, OPcache vhost **and** the WebSocket
    /// registry scope from it (`Router::site_identities`), so all four keep
    /// naming the tenant that owned the upgrade — the `Host` header is never
    /// consulted again after the handshake.
    pub(crate) site_key: Option<String>,
    pub(crate) connection_id: String,
    pub(crate) script: PathBuf,
    pub(crate) document_root: PathBuf,
    pub(crate) remote_addr: SocketAddr,
    pub(crate) is_https: bool,
    /// The upgrade request's URI, replayed on every event so `$_SERVER`
    /// (`REQUEST_URI`, `QUERY_STRING`) is stable for the connection's life.
    pub(crate) uri: Uri,
    /// The upgrade request's headers, replayed on every event so cookies and
    /// `Origin` remain visible to `message` / `disconnect` handlers.
    pub(crate) headers: HeaderMap,
}

impl WsSession {
    /// Build the synthetic request for one event.
    ///
    /// Replays the upgrade request's URI and headers, with the frame payload as
    /// the body. The hop-by-hop upgrade headers are stripped: `$_SERVER` for a
    /// `message` event should not claim the request is still an upgrade, and a
    /// `Content-Length` from the original request would misdescribe this body.
    fn event_request(
        &self,
        payload: Bytes,
        content_type: Option<&'static str>,
    ) -> Request<Full<Bytes>> {
        let mut builder =
            Request::builder().method(Method::GET).uri(self.uri.clone()).version(Version::HTTP_11);
        if let Some(headers) = builder.headers_mut() {
            headers.clone_from(&self.headers);
            for name in [
                http::header::CONNECTION,
                http::header::UPGRADE,
                http::header::CONTENT_LENGTH,
                http::header::CONTENT_TYPE,
            ] {
                headers.remove(name);
            }
            headers.remove("sec-websocket-key");
            headers.remove("sec-websocket-version");
            headers.remove("sec-websocket-extensions");
            if let Some(ct) = content_type {
                headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static(ct));
            }
        }
        builder.body(Full::new(payload)).expect("websocket event request builder")
    }

    /// Run one event through the ordinary PHP request path.
    async fn dispatch(
        &self,
        kind: WsEventKind,
        payload: Bytes,
        opcode: Option<&'static str>,
    ) -> Response<ServerBody> {
        let content_type = match opcode {
            Some("binary") => Some("application/octet-stream"),
            Some(_) => Some("text/plain; charset=utf-8"),
            None => None,
        };
        let event = WsEvent { kind, connection_id: self.connection_id.clone(), opcode };
        counter!("ephpm_ws_events_total", "event" => kind.as_str()).increment(1);

        self.router
            .handle_php_event(
                self.event_request(payload, content_type),
                self.remote_addr,
                self.is_https,
                self.script.clone(),
                self.document_root.clone(),
                self.site_key.clone(),
                event,
            )
            .await
    }
}

/// Refuse an upgrade whose registry admission failed.
pub(crate) fn registry_full(err: ephpm_ws::RegisterError) -> Response<ServerBody> {
    counter!("ephpm_ws_handshake_rejected_total", "reason" => "capacity").increment(1);
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body::buffered(Full::new(Bytes::from(format!("503 {err}")))))
        .expect("503 response builder")
}

/// Take the upgrade handle hyper attached to the request, if this transport
/// has one. HTTP/2 and HTTP/3 requests never do.
pub(crate) fn take_upgrade<B>(req: &mut Request<B>) -> Option<OnUpgrade> {
    req.extensions_mut().remove::<OnUpgrade>()
}

/// Drive one connection's socket: outbound queue, inbound frames, keepalive.
pub(crate) async fn run_session(
    session: WsSession,
    upgrade: OnUpgrade,
    runtime: Arc<WsRuntime>,
    mut outbound: tokio::sync::mpsc::Receiver<ephpm_ws::OutFrame>,
    control: Arc<ephpm_ws::Control>,
    request_timeout: Duration,
) {
    let upgraded = match upgrade.await {
        Ok(upgraded) => upgraded,
        Err(e) => {
            tracing::debug!(%e, "websocket upgrade never completed");
            runtime.registry.unregister(&session.connection_id);
            return;
        }
    };

    let mut ws =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(runtime.codec))
            .await;

    // Housekeeping period: ping when keepalive is on, otherwise a half-idle
    // tick purely to notice a peer that has gone silent. One timer serves both
    // jobs — a ping is exactly the probe the idle check needs.
    let tick = runtime
        .ping_interval
        .or_else(|| runtime.idle_timeout.map(|idle| idle / 2))
        .unwrap_or(Duration::from_secs(3600));
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // the first tick completes immediately

    let mut last_rx = Instant::now();
    let mut closing: Option<u16> = None;

    loop {
        tokio::select! {
            // Shedding and explicit closes win over anything queued behind
            // them — a connection marked for 1013 must not first drain the
            // very backlog that overflowed.
            biased;

            code = control.closed() => {
                closing = Some(code);
                break;
            }

            frame = outbound.recv() => {
                let Some(frame) = frame else { break };
                let message = match frame {
                    ephpm_ws::OutFrame::Text(b) => match String::from_utf8(b.to_vec()) {
                        Ok(text) => Message::Text(text),
                        Err(_) => {
                            // A text frame must carry UTF-8 (RFC 6455 §5.6).
                            // Refusing here is better than writing a frame the
                            // client is obliged to close the connection over.
                            counter!("ephpm_ws_frames_rejected_total", "reason" => "invalid_utf8")
                                .increment(1);
                            continue;
                        }
                    },
                    ephpm_ws::OutFrame::Binary(b) => Message::Binary(b.to_vec()),
                };
                if let Err(e) = ws.send(message).await {
                    tracing::debug!(%e, "websocket write failed");
                    break;
                }
                counter!("ephpm_ws_frames_sent_total").increment(1);
            }

            _ = ticker.tick() => {
                if let Some(idle) = runtime.idle_timeout
                    && last_rx.elapsed() >= idle
                {
                    tracing::debug!(
                        conn = %session.connection_id,
                        idle_secs = idle.as_secs(),
                        "closing idle websocket connection"
                    );
                    closing = Some(CLOSE_GOING_AWAY);
                    break;
                }
                // A failed ping means the socket is already gone; stop rather
                // than waiting for the read half to notice.
                if runtime.ping_interval.is_some()
                    && ws.send(Message::Ping(Vec::new())).await.is_err()
                {
                    break;
                }
            }

            incoming = ws.next() => {
                let Some(incoming) = incoming else { break };
                let message = match incoming {
                    Ok(message) => message,
                    Err(e) => {
                        tracing::debug!(%e, "websocket read failed");
                        break;
                    }
                };
                last_rx = Instant::now();

                let (payload, opcode) = match message {
                    Message::Text(text) => (Bytes::from(text.into_bytes()), "text"),
                    Message::Binary(data) => (Bytes::from(data), "binary"),
                    Message::Ping(_) => {
                        // tungstenite queued the pong during the read; flush it
                        // now rather than waiting for the next outbound frame.
                        if ws.flush().await.is_err() {
                            break;
                        }
                        continue;
                    }
                    // A pong only had to refresh `last_rx`, which it just did.
                    // `Frame` is never yielded on the read path.
                    Message::Pong(_) | Message::Frame(_) => continue,
                    Message::Close(_) => break,
                };
                counter!("ephpm_ws_frames_received_total").increment(1);

                // Sequential by construction: the socket is not read again
                // until this handler returns, so one connection's messages
                // cannot execute out of order or concurrently.
                match tokio::time::timeout(
                    request_timeout,
                    session.dispatch(WsEventKind::Message, payload, Some(opcode)),
                )
                .await
                {
                    Ok(_response) => {}
                    Err(_) => {
                        // The blocking PHP execution cannot be cancelled, so
                        // continuing would risk a second dispatch overlapping
                        // the abandoned one. Close instead.
                        counter!("ephpm_ws_event_timeouts_total").increment(1);
                        tracing::warn!(
                            conn = %session.connection_id,
                            timeout_secs = request_timeout.as_secs(),
                            "websocket message handler exceeded [server.timeouts] request"
                        );
                        closing = Some(CLOSE_INTERNAL_ERROR);
                        break;
                    }
                }
            }
        }
    }

    // Best effort: the peer may already be gone.
    let close_frame = closing.map(|code| tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: code.into(),
        reason: std::borrow::Cow::Borrowed(""),
    });
    let _ = ws.send(Message::Close(close_frame)).await;
    let _ = ws.close(None).await;

    // Unregister BEFORE the disconnect event, so a handler that calls
    // `ephpm_ws_send($id)` on the departing connection correctly fails and a
    // broadcast no longer reaches it.
    runtime.registry.unregister(&session.connection_id);
    let _ = session.dispatch(WsEventKind::Disconnect, Bytes::new(), None).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upgrade_request() -> Request<()> {
        Request::builder()
            .method(Method::GET)
            .uri("/chat")
            .version(Version::HTTP_11)
            .header("connection", "keep-alive, Upgrade")
            .header("upgrade", "WebSocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(())
            .unwrap()
    }

    #[test]
    fn detects_an_upgrade_with_token_lists_and_mixed_case() {
        assert!(is_upgrade_request(&upgrade_request()));
    }

    #[test]
    fn an_ordinary_get_is_not_an_upgrade() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/index.php")
            .version(Version::HTTP_11)
            .body(())
            .unwrap();
        assert!(!is_upgrade_request(&req));
    }

    #[test]
    fn a_post_or_http2_upgrade_is_not_routed_here() {
        let mut req = upgrade_request();
        *req.method_mut() = Method::POST;
        assert!(!is_upgrade_request(&req), "only GET upgrades");

        let mut req = upgrade_request();
        *req.version_mut() = Version::HTTP_2;
        assert!(!is_upgrade_request(&req), "HTTP/2 has no Upgrade mechanism");
    }

    #[test]
    fn a_malformed_upgrade_still_routes_here_and_is_rejected_by_the_handshake() {
        // Routing must not depend on the version header — otherwise a client
        // sending version 8 would silently be served index.php.
        let mut req = upgrade_request();
        req.headers_mut().insert("sec-websocket-version", "8".parse().unwrap());
        assert!(is_upgrade_request(&req));
        assert!(handshake_key(req.headers()).is_none());

        let mut req = upgrade_request();
        req.headers_mut().remove("sec-websocket-key");
        assert!(is_upgrade_request(&req));
        assert!(handshake_key(req.headers()).is_none());
    }

    #[test]
    fn accept_key_matches_rfc_6455_section_1_3() {
        let key = handshake_key(upgrade_request().headers()).expect("well-formed handshake");
        let resp = switching_protocols(&key);
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(
            resp.headers().get("sec-websocket-accept").unwrap(),
            // The worked example from RFC 6455 §1.3.
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        );
    }

    #[test]
    fn the_runtime_is_absent_when_the_feature_is_off() {
        let config = WebSocketConfig::default();
        assert!(!config.enabled);
        assert!(WsRuntime::new(&config).is_none(), "disabled must cost an Option::is_none");
    }

    #[test]
    fn zero_durations_disable_their_timers() {
        let config = WebSocketConfig {
            enabled: true,
            ping_interval_secs: 0,
            idle_timeout_secs: 0,
            ..WebSocketConfig::default()
        };
        let runtime = WsRuntime::new(&config).expect("enabled");
        assert!(runtime.ping_interval.is_none());
        assert!(runtime.idle_timeout.is_none());
    }

    #[test]
    fn event_vars_carry_the_kind_the_id_and_the_opcode() {
        let event = WsEvent {
            kind: WsEventKind::Message,
            connection_id: "abc123".to_string(),
            opcode: Some("binary"),
        };
        let vars = event.server_vars();
        assert!(vars.contains(&("WS_EVENT".to_string(), "message".to_string())));
        assert!(vars.contains(&("WS_CONNECTION_ID".to_string(), "abc123".to_string())));
        assert!(vars.contains(&("WS_OPCODE".to_string(), "binary".to_string())));

        let connect = WsEvent {
            kind: WsEventKind::Connect,
            connection_id: "abc123".to_string(),
            opcode: None,
        };
        let vars = connect.server_vars();
        assert!(vars.contains(&("WS_EVENT".to_string(), "connect".to_string())));
        assert!(
            !vars.iter().any(|(k, _)| k == "WS_OPCODE"),
            "a connect event has no opcode to report"
        );
    }
}

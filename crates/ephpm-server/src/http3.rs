//! HTTP/3 (QUIC) listener.
//!
//! HTTP/3 runs over UDP and is served *alongside* the TCP listeners, never
//! instead of them. Everything above the transport is shared: an accepted h3
//! request is turned into a plain [`hyper::Request`] and handed to
//! [`Router::handle`] — the same entry point HTTP/1.1 and HTTP/2 use — so
//! `$_SERVER`, middleware, vhost resolution, static files, rate limiting and
//! the `ephpm_http_*` metrics behave identically regardless of transport.
//! There is deliberately no second request pipeline here.
//!
//! # Discovery
//!
//! Clients do not guess that HTTP/3 exists. They learn about it from an
//! `Alt-Svc: h3=":443"; ma=86400` header on the TCP (HTTPS) responses, which
//! [`Router`] emits when HTTP/3 is enabled. A client that never sees that
//! header — or a `curl` invocation without `--http3` — will keep using TCP
//! forever, which is the correct fallback behaviour, not a bug.
//!
//! # Certificates
//!
//! QUIC mandates TLS 1.3, and the certificate is baked into the QUIC endpoint
//! at bind time. Only static `[server.tls] cert`/`key` are supported; ACME is
//! a documented limitation (see [`build_endpoint`]).

use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use anyhow::Context as _;
use bytes::{Buf, Bytes};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};
use hyper::{Request, Response};
use metrics::{counter, gauge, histogram};
use quinn::crypto::rustls::QuicServerConfig;

use crate::body::ServerBody;
use crate::router::Router;

/// ALPN protocol identifier for HTTP/3 (RFC 9114).
const ALPN_H3: &[u8] = b"h3";

/// Build the QUIC endpoint that serves HTTP/3.
///
/// The rustls config comes from [`crate::tls::build_server_config`] — the
/// exact function the TCP HTTPS listener uses — so both transports present
/// the same certificate and, critically, resolve the same rustls crypto
/// provider. Only the ALPN list differs (`h3` here, `h2`/`http/1.1` there).
///
/// # ACME limitation
///
/// ACME-provisioned certificates are not supported on this path. `rustls-acme`
/// hands out certificates through a `ResolvesServerCert` that rotates
/// mid-process, whereas `quinn::ServerConfig` captures its crypto config when
/// the endpoint is created. Wiring rotation through
/// [`quinn::Endpoint::set_server_config`] is possible but is deliberately not
/// attempted here; callers must refuse to start HTTP/3 in ACME mode rather
/// than silently serving no h3 (see [`Http3Params::resolve`]).
///
/// # Errors
///
/// Returns an error if the cert/key cannot be loaded, if the resulting rustls
/// config has no QUIC-capable cipher suite, or if the UDP socket cannot be
/// bound.
pub fn build_endpoint(
    addr: SocketAddr,
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<quinn::Endpoint> {
    let crypto = crate::tls::build_server_config(cert_path, key_path, &[ALPN_H3.to_vec()])
        .context("failed to build the HTTP/3 TLS configuration")?;

    let quic_crypto = QuicServerConfig::try_from(crypto)
        .context("TLS configuration has no QUIC-capable cipher suite (TLS 1.3 required)")?;

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));

    quinn::Endpoint::server(server_config, addr)
        .with_context(|| format!("failed to bind the HTTP/3 (UDP) listener to {addr}"))
}

/// Everything the HTTP/3 listener needs, resolved from config at startup.
#[derive(Debug, Clone)]
pub struct Http3Params {
    /// UDP address the QUIC endpoint binds.
    pub listen: SocketAddr,
    /// Static certificate chain (PEM).
    pub cert: std::path::PathBuf,
    /// Static private key (PEM).
    pub key: std::path::PathBuf,
}

impl Http3Params {
    /// Resolve `[server.http3]` against the rest of the server config.
    ///
    /// Returns `Ok(None)` when HTTP/3 is simply switched off. Returns an error
    /// — deliberately fatal rather than a warning-and-continue — when HTTP/3
    /// is enabled but cannot run, so an operator who asked for HTTP/3 never
    /// ends up with a server that quietly serves only TCP.
    ///
    /// `https_addr` is the address that terminates TLS over TCP; when
    /// `[server.http3] listen` is absent the QUIC socket reuses it (same port,
    /// UDP instead of TCP), which is what browsers expect.
    ///
    /// # Errors
    ///
    /// Returns an error if HTTP/3 is enabled without TLS, with ACME TLS, or
    /// with an unparseable `listen` address.
    pub fn resolve(
        config: &ephpm_config::Config,
        https_addr: SocketAddr,
    ) -> anyhow::Result<Option<Self>> {
        let http3 = &config.server.http3;
        if !http3.enabled {
            return Ok(None);
        }

        let Some(tls) = config.server.tls.as_ref() else {
            anyhow::bail!(
                "[server.http3] enabled = true requires TLS: QUIC mandates TLS 1.3. \
                 Configure [server.tls] cert and key, or set enabled = false."
            );
        };

        if tls.is_acme() {
            anyhow::bail!(
                "[server.http3] enabled = true is not supported with ACME TLS yet. \
                 HTTP/3 requires a static [server.tls] cert/key; ACME support is \
                 planned. Set enabled = false to start with HTTPS over TCP only."
            );
        }

        let (Some(cert), Some(key)) = (tls.cert.as_ref(), tls.key.as_ref()) else {
            anyhow::bail!("[server.http3] enabled = true requires both [server.tls] cert and key");
        };

        let listen = match http3.listen.as_deref() {
            Some(raw) => raw
                .parse::<SocketAddr>()
                .with_context(|| format!("invalid [server.http3] listen address: {raw}"))?,
            None => https_addr,
        };

        Ok(Some(Self { listen, cert: cert.clone(), key: key.clone() }))
    }
}

/// Format the `Alt-Svc` header value that advertises this HTTP/3 endpoint.
///
/// Only the port is advertised (`h3=":443"`), never a host: RFC 7838 treats a
/// missing host as "same host as the origin", which is what we want — the
/// server has no reliable idea what authority the client used.
///
/// Returns `None` when `max_age` is 0, which is how an operator suppresses
/// advertisement entirely.
#[must_use]
pub fn alt_svc_value(port: u16, max_age: u64) -> Option<String> {
    if max_age == 0 {
        return None;
    }
    Some(format!("h3=\":{port}\"; ma={max_age}"))
}

/// Accept QUIC connections until `shutdown` resolves.
///
/// Each connection is handled on its own task; each request within a
/// connection gets a further task, mirroring how HTTP/2 streams are handled by
/// hyper on the TCP path.
pub async fn accept_loop(
    endpoint: quinn::Endpoint,
    router: Arc<Router>,
    in_flight: Arc<AtomicUsize>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    // `None` means the endpoint was closed.
                    break;
                };
                let router = Arc::clone(&router);
                let in_flight = Arc::clone(&in_flight);
                tokio::spawn(async move {
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    handle_connection(incoming, router).await;
                    in_flight.fetch_sub(1, Ordering::Relaxed);
                });
            }
            () = &mut shutdown => {
                tracing::info!("HTTP/3 listener stopping");
                break;
            }
        }
    }

    // `0` here is an HTTP/3 H3_NO_ERROR application close code.
    endpoint.close(quinn::VarInt::from_u32(0), b"server shutting down");
}

/// Drive one QUIC connection: complete the handshake, then serve requests.
async fn handle_connection(incoming: quinn::Incoming, router: Arc<Router>) {
    let remote_addr = incoming.remote_address();

    let connection = match incoming.await {
        Ok(conn) => conn,
        Err(err) => {
            // A failed handshake is a per-connection event (bad ALPN, client
            // gave up, version negotiation), never fatal to the listener.
            counter!("ephpm_http3_connection_errors_total", "stage" => "handshake").increment(1);
            tracing::debug!(%remote_addr, %err, "HTTP/3 handshake failed");
            return;
        }
    };

    counter!("ephpm_http3_connections_total").increment(1);
    gauge!("ephpm_http3_connections_active").increment(1.0);

    let result = serve_requests(connection, remote_addr, router).await;

    gauge!("ephpm_http3_connections_active").decrement(1.0);

    if let Err(err) = result {
        counter!("ephpm_http3_connection_errors_total", "stage" => "session").increment(1);
        tracing::debug!(%remote_addr, %err, "HTTP/3 connection error");
    }
}

/// Serve every request multiplexed over one established QUIC connection.
async fn serve_requests(
    connection: quinn::Connection,
    remote_addr: SocketAddr,
    router: Arc<Router>,
) -> anyhow::Result<()> {
    let mut h3 = h3::server::builder()
        .build(h3_quinn::Connection::new(connection))
        .await
        .context("HTTP/3 session setup failed")?;

    loop {
        match h3.accept().await {
            Ok(Some(resolver)) => {
                let router = Arc::clone(&router);
                tokio::spawn(async move {
                    if let Err(err) = handle_request(resolver, remote_addr, router).await {
                        tracing::debug!(%remote_addr, %err, "HTTP/3 request failed");
                    }
                });
            }
            // Clean end of the connection.
            Ok(None) => return Ok(()),
            Err(err) => return Err(anyhow::anyhow!(err)),
        }
    }
}

/// The receive half of a request stream, after `split()`.
type H3RecvStream = h3_quinn::RecvStream;
/// The send half of a request stream, after `split()`.
type H3SendStream = h3_quinn::SendStream<Bytes>;

/// Handle a single HTTP/3 request end to end.
///
/// The request is normalized into the shape the shared pipeline expects and
/// passed to [`Router::handle`] with `is_tls = true` (QUIC is always TLS).
async fn handle_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    remote_addr: SocketAddr,
    router: Arc<Router>,
) -> anyhow::Result<()> {
    let (request, stream) = resolver.resolve_request().await.context("malformed HTTP/3 request")?;

    counter!("ephpm_http3_requests_total").increment(1);
    let started = std::time::Instant::now();

    let (send, recv) = stream.split();

    // No request-side normalization is needed: h3 already stamps
    // `Version::HTTP_3` (so `$_SERVER['SERVER_PROTOCOL']` reads `HTTP/3.0`)
    // and treats connection-specific request fields as a stream error before
    // we ever see them. The response side is where h3 leaves work for us —
    // see `strip_forbidden_response_headers`.
    let (parts, ()) = request.into_parts();
    let request = Request::from_parts(parts, H3RequestBody::new(recv));

    let response = router
        .handle(request, remote_addr, true)
        .await
        .context("HTTP/3 request handling failed")?;

    let result = send_response(send, response).await;

    histogram!("ephpm_http3_request_duration_seconds").record(started.elapsed().as_secs_f64());

    result
}

/// Header fields RFC 9114 §4.2 forbids in HTTP/3 messages.
///
/// h3 rejects these on *requests* (a malformed request is a stream error), but
/// it does not filter *responses* — it QPACK-encodes whatever it is handed. A
/// PHP script calling `header('Connection: close')`, or a
/// `[server.response] headers` entry setting one of these, would therefore put
/// an illegal field on the wire and a strict client would reset the stream.
/// The TCP path has no such problem: hyper owns connection management there.
///
/// `TE` is deliberately absent: RFC 9114 permits it when the value is exactly
/// `trailers`, and ePHPm never generates it otherwise.
const FORBIDDEN_H3_RESPONSE_HEADERS: [hyper::header::HeaderName; 5] = [
    hyper::header::CONNECTION,
    hyper::header::TRANSFER_ENCODING,
    hyper::header::UPGRADE,
    hyper::header::HeaderName::from_static("keep-alive"),
    hyper::header::HeaderName::from_static("proxy-connection"),
];

/// Strip HTTP/3-illegal fields from a response produced by the shared pipeline.
///
/// Returns how many forbidden *names* were removed (`HeaderMap::remove` drops
/// every value bound to a name), so the caller can log a misbehaving
/// application rather than silently papering over it.
fn strip_forbidden_response_headers(headers: &mut hyper::HeaderMap) -> usize {
    FORBIDDEN_H3_RESPONSE_HEADERS.iter().filter(|name| headers.remove(*name).is_some()).count()
}

/// Write a finished [`ServerBody`] response out over an HTTP/3 stream.
///
/// Data frames are forwarded as they arrive rather than collected, so large
/// static files and streamed PHP output keep flat memory on h3 exactly as they
/// do on the TCP path.
async fn send_response(
    mut send: h3::server::RequestStream<H3SendStream, Bytes>,
    response: Response<ServerBody>,
) -> anyhow::Result<()> {
    let (mut parts, mut body) = response.into_parts();

    let removed = strip_forbidden_response_headers(&mut parts.headers);
    if removed > 0 {
        counter!("ephpm_http3_stripped_headers_total").increment(removed as u64);
        tracing::debug!(removed, "stripped connection-specific headers from an HTTP/3 response");
    }

    let head = Response::from_parts(parts, ());

    send.send_response(head).await.context("failed to send HTTP/3 response headers")?;

    while let Some(frame) = body.frame().await {
        let frame = frame.context("response body error")?;
        match frame.into_data() {
            Ok(data) => {
                if !data.is_empty() {
                    send.send_data(data).await.context("failed to send HTTP/3 response data")?;
                }
            }
            // A trailers frame; anything else is ignored the way hyper does.
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    send.send_trailers(trailers).await.context("failed to send HTTP/3 trailers")?;
                }
            }
        }
    }

    send.finish().await.context("failed to finish the HTTP/3 response stream")?;
    Ok(())
}

/// Adapts an HTTP/3 receive stream to [`hyper::body::Body`].
///
/// This is what lets HTTP/3 reuse the shared request pipeline unchanged: the
/// router's body handling (`Limited`, `collect`, frame-by-frame streaming into
/// a PHP worker) is already generic over `Body`, so it works on QUIC streams
/// without a parallel implementation. Nothing is buffered here — each QUIC
/// data chunk becomes one body frame.
pub struct H3RequestBody {
    stream: h3::server::RequestStream<H3RecvStream, Bytes>,
    /// Set once the peer signals end-of-stream, so later polls stay `None`.
    finished: bool,
}

impl H3RequestBody {
    fn new(stream: h3::server::RequestStream<H3RecvStream, Bytes>) -> Self {
        Self { stream, finished: false }
    }
}

/// Error surfaced when reading an HTTP/3 request body fails.
#[derive(Debug, thiserror::Error)]
#[error("HTTP/3 request body error: {0}")]
pub struct H3BodyError(String);

impl Body for H3RequestBody {
    type Data = Bytes;
    type Error = H3BodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // `H3RequestBody` holds no self-references and `poll_recv_data` takes
        // `&mut self`, so projecting through the pin is sound without any
        // pin-projection machinery.
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }

        match this.stream.poll_recv_data(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(Some(mut buf))) => {
                let remaining = buf.remaining();
                Poll::Ready(Some(Ok(Frame::data(buf.copy_to_bytes(remaining)))))
            }
            Poll::Ready(Ok(None)) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Err(err)) => {
                this.finished = true;
                Poll::Ready(Some(Err(H3BodyError(err.to_string()))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        // QUIC gives no framing-level length; `Content-Length`, when the client
        // sent one, is already enforced by the router's own body cap.
        SizeHint::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Load a `Config` from an inline TOML document.
    ///
    /// The `TempDir` is kept alive for the duration of the call only —
    /// `Config::load` reads the file eagerly.
    fn cfg_toml(body: &str) -> ephpm_config::Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ephpm.toml");
        std::fs::write(&path, body).expect("write config");
        ephpm_config::Config::load(&path).expect("load config")
    }

    fn https_addr() -> SocketAddr {
        "127.0.0.1:8443".parse().expect("addr")
    }

    // ── Alt-Svc ──────────────────────────────────────────────────────

    #[test]
    fn alt_svc_advertises_port_and_max_age() {
        assert_eq!(alt_svc_value(443, 86400).as_deref(), Some("h3=\":443\"; ma=86400"));
        assert_eq!(alt_svc_value(8443, 60).as_deref(), Some("h3=\":8443\"; ma=60"));
    }

    #[test]
    fn alt_svc_suppressed_when_max_age_is_zero() {
        assert_eq!(alt_svc_value(443, 0), None);
    }

    // ── Http3Params::resolve ─────────────────────────────────────────

    #[test]
    fn disabled_by_default_resolves_to_none() {
        let config = cfg_toml("[server]\nlisten = \"127.0.0.1:8080\"\n");
        assert!(
            Http3Params::resolve(&config, https_addr()).expect("resolve").is_none(),
            "http3 must be off unless explicitly enabled"
        );
    }

    #[test]
    fn enabled_without_tls_is_a_startup_error() {
        let config =
            cfg_toml("[server]\nlisten = \"127.0.0.1:8080\"\n\n[server.http3]\nenabled = true\n");
        let err = Http3Params::resolve(&config, https_addr())
            .expect_err("http3 without TLS must not start silently");
        let msg = format!("{err:#}");
        assert!(msg.contains("requires TLS"), "unexpected error: {msg}");
    }

    #[test]
    fn enabled_with_acme_tls_is_a_startup_error() {
        let config = cfg_toml(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n\
             [server.tls]\ndomains = [\"example.com\"]\n\n\
             [server.http3]\nenabled = true\n",
        );
        let err = Http3Params::resolve(&config, https_addr())
            .expect_err("http3 + ACME must fail loudly, not start without h3");
        let msg = format!("{err:#}");
        assert!(msg.contains("ACME"), "unexpected error: {msg}");
    }

    #[test]
    fn listen_defaults_to_the_https_address() {
        let config = cfg_toml(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n\
             [server.tls]\ncert = \"/tmp/c.pem\"\nkey = \"/tmp/k.pem\"\n\n\
             [server.http3]\nenabled = true\n",
        );
        let params = Http3Params::resolve(&config, https_addr()).expect("resolve").expect("some");
        assert_eq!(params.listen, https_addr());
        assert_eq!(params.cert, PathBuf::from("/tmp/c.pem"));
        assert_eq!(params.key, PathBuf::from("/tmp/k.pem"));
    }

    #[test]
    fn explicit_listen_overrides_the_derived_address() {
        let config = cfg_toml(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n\
             [server.tls]\ncert = \"/tmp/c.pem\"\nkey = \"/tmp/k.pem\"\n\n\
             [server.http3]\nenabled = true\nlisten = \"0.0.0.0:9443\"\n",
        );
        let params = Http3Params::resolve(&config, https_addr()).expect("resolve").expect("some");
        assert_eq!(params.listen, "0.0.0.0:9443".parse::<SocketAddr>().expect("addr"));
    }

    #[test]
    fn unparseable_listen_is_a_startup_error() {
        let config = cfg_toml(
            "[server]\nlisten = \"127.0.0.1:8080\"\n\n\
             [server.tls]\ncert = \"/tmp/c.pem\"\nkey = \"/tmp/k.pem\"\n\n\
             [server.http3]\nenabled = true\nlisten = \"not-an-address\"\n",
        );
        let err = Http3Params::resolve(&config, https_addr()).expect_err("bad listen must fail");
        assert!(format!("{err:#}").contains("invalid [server.http3] listen"));
    }

    // ── RFC 9114 §4.2 response-header normalization ──────────────────

    #[test]
    fn strips_connection_specific_response_headers() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(hyper::header::CONNECTION, "close".parse().expect("value"));
        headers.insert(hyper::header::TRANSFER_ENCODING, "chunked".parse().expect("value"));
        headers.insert(hyper::header::UPGRADE, "websocket".parse().expect("value"));
        headers.insert("keep-alive", "timeout=5".parse().expect("value"));
        headers.insert("proxy-connection", "close".parse().expect("value"));
        // ...and one that must survive.
        headers.insert(hyper::header::CONTENT_TYPE, "text/html".parse().expect("value"));

        let removed = strip_forbidden_response_headers(&mut headers);

        assert_eq!(removed, 5);
        for name in &FORBIDDEN_H3_RESPONSE_HEADERS {
            assert!(!headers.contains_key(name), "{name} must not reach an HTTP/3 client");
        }
        assert_eq!(
            headers.get(hyper::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("text/html"),
            "ordinary headers must be left alone"
        );
    }

    #[test]
    fn leaves_a_clean_response_untouched() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(hyper::header::CONTENT_LENGTH, "12".parse().expect("value"));
        headers.insert(hyper::header::ETAG, "\"abc\"".parse().expect("value"));

        assert_eq!(strip_forbidden_response_headers(&mut headers), 0);
        assert_eq!(headers.len(), 2);
    }

    /// A repeated forbidden field must go entirely, not just its first value.
    #[test]
    fn strips_every_value_of_a_repeated_forbidden_header() {
        let mut headers = hyper::HeaderMap::new();
        headers.append(hyper::header::CONNECTION, "close".parse().expect("value"));
        headers.append(hyper::header::CONNECTION, "keep-alive".parse().expect("value"));

        assert_eq!(strip_forbidden_response_headers(&mut headers), 1, "one name removed");
        assert!(
            !headers.contains_key(hyper::header::CONNECTION),
            "no value of a forbidden field may survive"
        );
        assert!(headers.is_empty());
    }

    // ── Endpoint construction ────────────────────────────────────────

    #[test]
    fn endpoint_build_fails_on_missing_cert() {
        crate::tls::tests_support::init_crypto();
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().join("key.pem");
        std::fs::write(&key, "irrelevant").expect("write");
        let err = build_endpoint(
            "127.0.0.1:0".parse().expect("addr"),
            &dir.path().join("nope.pem"),
            &key,
        )
        .expect_err("missing cert must fail");
        assert!(format!("{err:#}").contains("cannot open cert file"), "{err:#}");
    }

    #[test]
    fn endpoint_build_fails_on_garbage_cert_pem() {
        crate::tls::tests_support::init_crypto();
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert, key) = crate::tls::tests_support::generate_ec_cert(dir.path());
        std::fs::write(&cert, "-----BEGIN NOT A CERT-----\nzzz\n").expect("write");
        let err = build_endpoint("127.0.0.1:0".parse().expect("addr"), &cert, &key)
            .expect_err("garbage cert PEM must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no certificates found") || msg.contains("failed to parse"),
            "unexpected error: {msg}"
        );
    }

    /// Binding needs a tokio reactor: quinn registers the UDP socket with it.
    #[tokio::test]
    async fn endpoint_binds_a_udp_socket() {
        crate::tls::tests_support::init_crypto();
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert, key) = crate::tls::tests_support::generate_ec_cert(dir.path());

        // Port 0: let the OS pick, so the test never collides with anything.
        let endpoint = build_endpoint("127.0.0.1:0".parse().expect("addr"), &cert, &key)
            .expect("endpoint should build from a valid EC cert/key");

        let bound = endpoint.local_addr().expect("bound address");
        assert_ne!(bound.port(), 0, "the UDP socket must actually be bound");
        endpoint.close(quinn::VarInt::from_u32(0), b"test over");
    }

    /// The single-crypto-provider invariant, asserted rather than assumed.
    ///
    /// Since #241 the tree carries a single rustls provider (aws-lc-rs), so
    /// the two transports can no longer disagree by crate feature. They could
    /// still disagree by *code* — one of them naming a provider directly
    /// instead of going through `tls::build_server_config`. That function
    /// hands out one cached `Arc`, so pointer identity is a genuine check
    /// that HTTP/3 and HTTPS-over-TCP got the *same* provider instance, not
    /// merely two equivalent ones.
    #[test]
    fn http3_and_tcp_tls_share_one_crypto_provider() {
        crate::tls::tests_support::init_crypto();
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert, key) = crate::tls::tests_support::generate_ec_cert(dir.path());

        let h3_config = crate::tls::build_server_config(&cert, &key, &[ALPN_H3.to_vec()])
            .expect("h3 server config");
        let tcp_config =
            crate::tls::build_server_config(&cert, &key, &[b"h2".to_vec(), b"http/1.1".to_vec()])
                .expect("tcp server config");

        assert!(
            Arc::ptr_eq(h3_config.crypto_provider(), tcp_config.crypto_provider()),
            "HTTP/3 and TCP TLS resolved different rustls crypto providers"
        );
        // ...and the ALPN lists are the only thing that differs.
        assert_eq!(h3_config.alpn_protocols, vec![b"h3".to_vec()]);
        assert_eq!(tcp_config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }
}

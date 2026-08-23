//! End-to-end tests for the built-in reverse proxy (`[[server.proxy]]`) against
//! **real** backends over real TCP.
//!
//! Rather than boot the whole `serve()` stack (which needs config, KV, TLS, …),
//! these tests wrap [`ephpm_server::proxy::ProxyEngine`] in a minimal hyper
//! HTTP/1.1 edge server. That is enough to exercise everything the proxy path
//! actually does: real streaming through hyper's client, the `OnUpgrade`
//! machinery (a request served with `.with_upgrades()` carries it, exactly as it
//! does in `Router::handle_inner`), and the forwarded-header rewriting. The
//! matcher logic itself is unit-tested in `proxy.rs`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use ephpm_config::ProxyRuleConfig;
use ephpm_server::proxy::ProxyEngine;
use futures::{SinkExt, StreamExt};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

/// Build a rule with sane defaults for a given host/path/upstream.
fn rule(host: Option<&str>, path: &str, upstream: &str) -> ProxyRuleConfig {
    ProxyRuleConfig {
        host: host.map(str::to_string),
        path: path.to_string(),
        path_exact: false,
        upstream: upstream.to_string(),
        connect_timeout_secs: 2,
        read_timeout_secs: 5,
    }
}

/// A backend that echoes what it received as `key=value` lines, so a test can
/// assert exactly which `Host`/`X-Forwarded-*`/path/body reached the upstream.
async fn spawn_http_echo(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| async move {
                    let get = |name: &str| {
                        req.headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string()
                    };
                    let host = get("host");
                    let xff = get("x-forwarded-for");
                    let xfp = get("x-forwarded-proto");
                    let xfh = get("x-forwarded-host");
                    let method = req.method().to_string();
                    let path = req.uri().path().to_string();
                    let body = req
                        .into_body()
                        .collect()
                        .await
                        .map(http_body_util::Collected::to_bytes)
                        .unwrap_or_default();
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let out = format!(
                        "tag={tag}\nhost={host}\nxff={xff}\nxfp={xfp}\nxfh={xfh}\nmethod={method}\npath={path}\nbody={body}\n"
                    );
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(out))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    addr
}

/// A WebSocket echo backend: accepts the upgrade and echoes every frame.
async fn spawn_ws_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
                while let Some(Ok(msg)) = ws.next().await {
                    if msg.is_text() || msg.is_binary() {
                        if ws.send(msg).await.is_err() {
                            break;
                        }
                    } else if msg.is_close() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// The edge: a hyper HTTP/1.1 server that routes every request through the
/// engine, returning 404 when no rule matches (mirroring how the real router
/// falls through to local serving on a miss).
async fn spawn_edge(engine: ProxyEngine) -> SocketAddr {
    let engine = Arc::new(engine);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { break };
            let engine = engine.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let engine = engine.clone();
                    async move {
                        let host = req
                            .headers()
                            .get("host")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .split(':')
                            .next()
                            .unwrap_or("")
                            .trim_end_matches('.')
                            .to_ascii_lowercase();
                        let original_host = req
                            .headers()
                            .get("host")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let path = req.uri().path().to_string();
                        let resp = if let Some(idx) = engine.match_index(&host, &path) {
                            engine.forward(idx, req, &original_host, peer, false).await
                        } else {
                            Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .body(ephpm_server::body::buffered(Full::new(Bytes::from_static(
                                    b"no proxy rule",
                                ))))
                                .unwrap()
                        };
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    addr
}

/// Low-level HTTP/1.1 GET/POST with a fully controlled `Host` header (so the
/// host-routing rules can be exercised without DNS).
async fn send(
    edge: SocketAddr,
    host: &str,
    path: &str,
    body: &'static [u8],
) -> (StatusCode, String) {
    let stream = TcpStream::connect(edge).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let method = if body.is_empty() { "GET" } else { "POST" };
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", host)
        .body(Full::new(Bytes::from_static(body)))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn proxies_http_and_preserves_host_and_sets_forwarded_headers() {
    let backend = spawn_http_echo("A").await;
    let engine =
        ProxyEngine::new(&[rule(Some("app.example.com"), "/", &format!("http://{backend}"))])
            .expect("engine");
    let edge = spawn_edge(engine).await;

    let (status, body) = send(edge, "app.example.com", "/widgets?x=1", b"").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("tag=A"), "reached backend A:\n{body}");
    // Host is preserved end to end.
    assert!(body.contains("host=app.example.com"), "Host must be preserved:\n{body}");
    // X-Forwarded-* are set correctly.
    assert!(body.contains("xfp=http"), "X-Forwarded-Proto:\n{body}");
    assert!(body.contains("xfh=app.example.com"), "X-Forwarded-Host:\n{body}");
    assert!(body.contains("xff=127.0.0.1"), "X-Forwarded-For is the client IP:\n{body}");
    // The original path (and query) is forwarded unchanged (no v1 rewriting).
    assert!(body.contains("path=/widgets"), "path forwarded unchanged:\n{body}");
}

#[tokio::test]
async fn streams_request_body_to_the_upstream() {
    let backend = spawn_http_echo("A").await;
    let engine =
        ProxyEngine::new(&[rule(None, "/", &format!("http://{backend}"))]).expect("engine");
    let edge = spawn_edge(engine).await;

    let (status, body) = send(edge, "anything", "/upload", b"payload-bytes").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("method=POST"), "{body}");
    assert!(body.contains("body=payload-bytes"), "request body must reach the upstream:\n{body}");
}

#[tokio::test]
async fn routes_by_host_and_path_to_distinct_backends() {
    let a = spawn_http_echo("A").await;
    let b = spawn_http_echo("B").await;
    let engine = ProxyEngine::new(&[
        rule(Some("a.test"), "/", &format!("http://{a}")),
        rule(Some("b.test"), "/api", &format!("http://{b}")),
    ])
    .expect("engine");
    let edge = spawn_edge(engine).await;

    // Host a.test → backend A.
    let (_, body_a) = send(edge, "a.test", "/", b"").await;
    assert!(body_a.contains("tag=A"), "a.test must route to A:\n{body_a}");

    // Host b.test + path /api → backend B.
    let (_, body_b) = send(edge, "b.test", "/api/v1", b"").await;
    assert!(body_b.contains("tag=B"), "b.test/api must route to B:\n{body_b}");

    // Host b.test but a non-/api path matches no rule → 404 (falls through).
    let (status, _) = send(edge, "b.test", "/other", b"").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unmatched path must not be proxied");

    // Unknown host → no rule → 404.
    let (status, _) = send(edge, "c.test", "/", b"").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown host must not be proxied");
}

#[tokio::test]
async fn unreachable_upstream_returns_502_not_a_hang() {
    // A port nothing listens on. connect_timeout bounds this to ~2s.
    let engine = ProxyEngine::new(&[rule(None, "/", "http://127.0.0.1:1")]).expect("engine");
    let edge = spawn_edge(engine).await;

    let (status, body) = send(edge, "anything", "/", b"").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "unreachable upstream must 502");
    assert!(body.contains("502"), "502 body:\n{body}");
}

#[tokio::test]
async fn proxies_a_websocket_upgrade_end_to_end() {
    let backend = spawn_ws_echo().await;
    let engine =
        ProxyEngine::new(&[rule(None, "/", &format!("http://{backend}"))]).expect("engine");
    let edge = spawn_edge(engine).await;

    // `connect_async` is behind tokio-tungstenite's `connect` feature (not
    // enabled here), so connect the TCP stream ourselves and drive the client
    // handshake with `client_async`.
    let url = format!("ws://{edge}/socket");
    let stream = TcpStream::connect(edge).await.unwrap();
    let (mut ws, _resp) =
        tokio_tungstenite::client_async(url, stream).await.expect("ws connect via proxy");

    ws.send(tokio_tungstenite::tungstenite::Message::Text("hello through proxy".into()))
        .await
        .unwrap();
    let echoed = ws.next().await.expect("a reply").expect("no ws error");
    assert_eq!(echoed.into_text().unwrap(), "hello through proxy");

    ws.send(tokio_tungstenite::tungstenite::Message::Binary(vec![1, 2, 3, 4])).await.unwrap();
    let echoed = ws.next().await.expect("a reply").expect("no ws error");
    assert_eq!(echoed.into_data(), vec![1, 2, 3, 4]);
}

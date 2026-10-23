//! Raw-TCP tests for HTTP/1.x request framing (request smuggling).
//!
//! Each case writes literal request bytes to a real `serve_connection` (the
//! production listener path: hyper + `framing::reject` + the router) and reads
//! until the server closes. A rejection must be a `400`, the connection must be
//! closed by the server, and a request pipelined behind the malformed one must
//! never be answered — that last part is what makes it a smuggling test rather
//! than a status-code test.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use super::{ConnSettings, Router, serve_connection};

/// The request pipelined behind every malformed one. It names a file that
/// exists, so if it were ever parsed it would produce a recognisable `200`.
const SMUGGLED: &[u8] = b"GET /smuggled.txt HTTP/1.1\r\nHost: a\r\n\r\n";
/// Body of `smuggled.txt`.
const SMUGGLED_MARKER: &str = "SMUGGLED-REQUEST-WAS-SERVED";

/// Serve every accepted connection from one router over a docroot holding
/// `index.php` (so bodies are read) and `smuggled.txt`.
async fn spawn_server() -> SocketAddr {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("index.php"), "<?php echo 'ok';").expect("index.php");
    std::fs::write(dir.path().join("smuggled.txt"), SMUGGLED_MARKER).expect("smuggled.txt");

    ephpm_php::PhpRuntime::init().expect("stub runtime init");
    let config = ephpm_config::Config {
        server: ephpm_config::ServerConfig {
            document_root: dir.path().to_path_buf(),
            fallback: vec!["$uri".to_string(), "=404".to_string()],
            ..ephpm_config::ServerConfig::default()
        },
        php: ephpm_config::PhpConfig { concurrency: 2, ..ephpm_config::PhpConfig::default() },
        db: ephpm_config::DbConfig::default(),
        kv: ephpm_config::KvConfig::default(),
        cluster: ephpm_config::ClusterConfig::default(),
        middleware: Vec::new(),
        opcache: ephpm_config::OpcacheConfig::default(),
    };
    let store = ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default());
    let router = Arc::new(Router::new(&config, store, None, None, None, None, None));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _dir = dir;
        loop {
            let Ok((stream, remote)) = listener.accept().await else { return };
            let router = Arc::clone(&router);
            tokio::spawn(serve_connection(
                stream,
                router,
                remote,
                false,
                ConnSettings {
                    header_read_timeout: Duration::from_secs(10),
                    max_header_size: 8192,
                    idle_timeout: Duration::ZERO,
                    shutdown: tokio_util::sync::CancellationToken::new(),
                },
            ));
        }
    });
    addr
}

/// What came back on one connection.
struct Exchange {
    /// Everything the server wrote, lossily decoded.
    text: String,
    /// Whether the server closed the connection (EOF) within the deadline.
    closed: bool,
}

impl Exchange {
    /// Every response status line, wherever it starts (a body without a
    /// trailing newline runs straight into the next status line).
    fn status_lines(&self) -> Vec<&str> {
        self.text
            .match_indices("HTTP/1.")
            .map(|(i, _)| self.text[i..].lines().next().unwrap_or_default())
            .collect()
    }
}

/// Write `bytes`, then read until EOF or 3 s of silence.
async fn exchange(addr: SocketAddr, bytes: &[u8]) -> Exchange {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    // The server may close before reading everything; a write error is fine.
    let _ = stream.write_all(bytes).await;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let closed = loop {
        match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => break true,
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Err(_) => break false,
        }
    };
    Exchange { text: String::from_utf8_lossy(&out).into_owned(), closed }
}

/// `head` followed by a pipelined [`SMUGGLED`] request must yield exactly one
/// `400`, a server-side close, and no trace of the smuggled request.
async fn assert_rejected(addr: SocketAddr, head: &[u8], ours: bool) {
    let mut bytes = head.to_vec();
    bytes.extend_from_slice(SMUGGLED);
    let ex = exchange(addr, &bytes).await;
    let case = String::from_utf8_lossy(head);
    assert_eq!(ex.status_lines(), vec!["HTTP/1.1 400 Bad Request"], "case {case:?}:\n{}", ex.text);
    assert!(ex.closed, "server must close after a framing 400 — case {case:?}");
    assert!(!ex.text.contains(SMUGGLED_MARKER), "smuggled request was served — case {case:?}");
    if ours {
        assert!(
            ex.text.to_ascii_lowercase().contains("connection: close"),
            "ePHPm's own rejection must say Connection: close — case {case:?}:\n{}",
            ex.text
        );
    }
}

#[tokio::test]
async fn control_pipelined_valid_requests_are_both_served() {
    let addr = spawn_server().await;
    let mut bytes = b"GET /smuggled.txt HTTP/1.1\r\nHost: a\r\n\r\n".to_vec();
    bytes.extend_from_slice(b"GET /smuggled.txt HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    let ex = exchange(addr, &bytes).await;
    assert_eq!(ex.status_lines().len(), 2, "{}", ex.text);
    assert_eq!(ex.text.matches(SMUGGLED_MARKER).count(), 2);
}

#[tokio::test]
async fn content_length_with_transfer_encoding_is_rejected_and_closed() {
    let addr = spawn_server().await;
    // With CL first, hyper hands both headers to ePHPm, which refuses it.
    assert_rejected(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        true,
    )
    .await;
}

/// With `Transfer-Encoding` *first*, hyper discards the later `Content-Length`
/// before ePHPm can see it, so the request cannot be refused. It is framed by
/// TE (RFC 9112 §6.3), and the connection is closed after the response — the
/// condition the RFC attaches to TE-wins. The classic CL.TE payload below (a
/// front end honouring `Content-Length: 44` forwards the smuggled request as
/// part of this body) therefore never gets the smuggled request parsed.
#[tokio::test]
async fn transfer_encoding_first_then_content_length_is_closed_after_response() {
    let addr = spawn_server().await;
    let mut bytes = b"POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\nContent-Length: 44\r\n\r\n0\r\n\r\n".to_vec();
    bytes.extend_from_slice(SMUGGLED);
    let ex = exchange(addr, &bytes).await;
    assert_eq!(ex.status_lines().len(), 1, "exactly one response:\n{}", ex.text);
    assert!(ex.closed, "server must close after a TE-framed request");
    assert!(ex.text.to_ascii_lowercase().contains("connection: close"), "{}", ex.text);
    assert!(!ex.text.contains(SMUGGLED_MARKER), "smuggled request was served:\n{}", ex.text);
}

#[tokio::test]
async fn conflicting_content_lengths_are_rejected() {
    let addr = spawn_server().await;
    // hyper refuses these itself, before the request reaches ePHPm.
    assert_rejected(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabcd",
        false,
    )
    .await;
    assert_rejected(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nContent-Length: 3, 4\r\n\r\nabcd",
        false,
    )
    .await;
    assert_rejected(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nContent-Length: +3\r\n\r\nabc",
        false,
    )
    .await;
    // Identical duplicates are one length (RFC 9112 §6.3) and are served.
    let ex = exchange(
        addr,
        b"GET /smuggled.txt HTTP/1.1\r\nHost: a\r\nContent-Length: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(ex.status_lines(), vec!["HTTP/1.1 200 OK"], "{}", ex.text);
}

#[tokio::test]
async fn transfer_encoding_must_be_exactly_chunked() {
    let addr = spawn_server().await;
    // hyper itself refuses a TE whose last coding is not `chunked`.
    for te in ["chunked, identity", "identity", "xchunked", "chunked,"] {
        let head = format!(
            "POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: {te}\r\n\r\n0\r\n\r\n"
        );
        assert_rejected(addr, head.as_bytes(), false).await;
    }
    // These hyper accepts (last coding is `chunked`); ePHPm refuses them.
    for te in ["gzip, chunked", ", chunked", "chunked, chunked", "identity,chunked"] {
        let head = format!(
            "POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: {te}\r\n\r\n0\r\n\r\n"
        );
        assert_rejected(addr, head.as_bytes(), true).await;
    }
    // Split over two fields.
    assert_rejected(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        true,
    )
    .await;
}

#[tokio::test]
async fn transfer_encoding_header_syntax_tricks_are_rejected() {
    let addr = spawn_server().await;
    // obs-fold continuation line and a space before the colon: httparse
    // refuses both while parsing the head.
    assert_rejected(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n identity\r\n\r\n0\r\n\r\n",
        false,
    )
    .await;
    assert_rejected(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding : chunked\r\n\r\n0\r\n\r\n",
        false,
    )
    .await;
    // Surrounding whitespace is OWS, stripped by the parser: still `chunked`,
    // still valid — the body is read and the request served.
    let ex = exchange(
        addr,
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: \t chunked \t\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
    )
    .await;
    assert_eq!(ex.status_lines().len(), 1, "{}", ex.text);
    assert!(!ex.status_lines()[0].contains(" 400 "), "{}", ex.text);
}

#[tokio::test]
async fn transfer_encoding_on_http10_is_rejected() {
    let addr = spawn_server().await;
    let mut bytes =
        b"POST /index.php HTTP/1.0\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            .to_vec();
    bytes.extend_from_slice(SMUGGLED);
    let ex = exchange(addr, &bytes).await;
    assert_eq!(ex.status_lines().len(), 1, "{}", ex.text);
    assert!(ex.status_lines()[0].contains(" 400 "), "{}", ex.text);
    assert!(ex.closed);
    assert!(!ex.text.contains(SMUGGLED_MARKER));
}

#[tokio::test]
async fn malformed_chunk_syntax_is_400_not_413() {
    let addr = spawn_server().await;
    for body in [
        &b"zz\r\nabc\r\n0\r\n\r\n"[..],
        b"ffffffffffffffffff\r\nabc\r\n0\r\n\r\n",
        b"3;ext\nabc\r\n0\r\n\r\n",
        b"3\r\nabcXX0\r\n\r\n",
    ] {
        let mut head =
            b"POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        head.extend_from_slice(body);
        assert_rejected(addr, &head, true).await;
    }
}

#[tokio::test]
async fn host_rules_are_enforced() {
    let addr = spawn_server().await;
    assert_rejected(addr, b"GET /smuggled.txt HTTP/1.1\r\n\r\n", true).await;
    assert_rejected(addr, b"GET /smuggled.txt HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n", true).await;
    assert_rejected(addr, b"GET http://b/smuggled.txt HTTP/1.1\r\nHost: a\r\n\r\n", true).await;

    // A matching absolute-form target, and HTTP/1.0 without Host, are fine.
    let ex = exchange(
        addr,
        b"GET http://a/smuggled.txt HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(ex.status_lines(), vec!["HTTP/1.1 200 OK"], "{}", ex.text);
    let ex = exchange(addr, b"GET /smuggled.txt HTTP/1.0\r\n\r\n").await;
    assert_eq!(ex.status_lines().len(), 1, "{}", ex.text);
    assert!(ex.status_lines()[0].ends_with(" 200 OK"), "{}", ex.text);
}

#[tokio::test]
async fn valid_chunked_body_is_served_then_the_connection_closes() {
    let addr = spawn_server().await;
    let mut bytes =
        b"POST /index.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"
            .to_vec();
    bytes.extend_from_slice(SMUGGLED);
    let ex = exchange(addr, &bytes).await;
    assert_eq!(ex.status_lines(), vec!["HTTP/1.1 200 OK"], "{}", ex.text);
    assert!(ex.closed);
    assert!(!ex.text.contains(SMUGGLED_MARKER));
}

/// HTTP/2 cleartext (prior knowledge) to the same listener: hyper-util's
/// `auto::Builder` serves h2 when the connection opens with the h2 preface.
async fn h2_send(
    addr: SocketAddr,
    req: http::Request<http_body_util::Full<bytes::Bytes>>,
) -> Result<http::Response<hyper::body::Incoming>, hyper::Error> {
    let tcp = TcpStream::connect(addr).await.expect("connect");
    let (mut sender, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(tcp),
    )
    .await
    .expect("h2 handshake");
    tokio::spawn(conn);
    sender.send_request(req).await
}

#[tokio::test]
async fn http2_host_contradicting_authority_is_rejected() {
    let addr = spawn_server().await;
    let req = http::Request::builder()
        .uri("http://a/smuggled.txt")
        .header("host", "b")
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .expect("request");
    let resp = h2_send(addr, req).await.expect("response");
    assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
    assert!(resp.headers().get(http::header::CONNECTION).is_none(), "no Connection on h2");

    // Control: the same request without the contradicting Host is served.
    let req = http::Request::builder()
        .uri("http://a/smuggled.txt")
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .expect("request");
    assert_eq!(h2_send(addr, req).await.expect("response").status(), http::StatusCode::OK);
}

/// The `h2` crate checks `Content-Length` against the DATA frames and resets
/// the stream on a mismatch; the request never produces a `200`.
#[tokio::test]
async fn http2_content_length_mismatch_never_succeeds() {
    let addr = spawn_server().await;
    for (declared, sent) in [("10", &b"abc"[..]), ("2", b"abcdef")] {
        let req = http::Request::builder()
            .method("POST")
            .uri("http://a/index.php")
            .header("content-length", declared)
            .body(http_body_util::Full::new(bytes::Bytes::from_static(sent)))
            .expect("request");
        match h2_send(addr, req).await {
            Err(_) => {}
            Ok(resp) => {
                let status = resp.status();
                let body = http_body_util::BodyExt::collect(resp.into_body()).await;
                assert!(
                    body.is_err() || status.is_client_error(),
                    "declared {declared}, sent {}: got {status}",
                    sent.len()
                );
            }
        }
    }
}

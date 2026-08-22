//! End-to-end HTTP/3 tests: a real QUIC client over a real UDP socket.
//!
//! These are not mocks. Each test binds the production QUIC endpoint
//! ([`ephpm_server::http3::build_endpoint`]), runs the production accept loop
//! ([`ephpm_server::http3::accept_loop`]) against a real [`Router`], and drives
//! it with the `h3` client over loopback UDP — full TLS 1.3 handshake, ALPN
//! negotiation, QPACK header encoding, QUIC stream framing.
//!
//! # What these prove, and what they do not
//!
//! **Proven:** a third-party HTTP/3 client can complete a QUIC handshake
//! against ePHPm, that requests traverse the *same* [`Router::handle`] pipeline
//! HTTP/1.1 and HTTP/2 use (static files, PHP dispatch, body limits, `Alt-Svc`),
//! and that request bodies sent over QUIC streams reach that pipeline.
//!
//! **Not proven:** actual PHP execution. Unless the crate is built with
//! `PHP_SDK_PATH` set (`php_linked`), `ephpm-php` is compiled in stub mode and
//! the "PHP" response is its stub page. The PHP-route test below therefore
//! asserts that the request *reached PHP dispatch with the correctly resolved
//! script* — which is the part HTTP/3 is responsible for — rather than
//! asserting on PHP-generated output. `curl --http3` is also not exercised:
//! the CI container's curl is not built with HTTP/3 support.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use bytes::{Buf, Bytes};
use ephpm_config::{
    ClusterConfig, Config, DbConfig, KvConfig, PhpConfig, RequestConfig, ServerConfig,
};
use ephpm_kv::store::{Store, StoreConfig};
use ephpm_server::router::Router;
use http::{Method, Request, StatusCode};

/// Alt-Svc max-age the tests advertise, distinct from the default so an
/// accidental fallback to the default would fail the assertion.
const TEST_ALT_SVC_MAX_AGE: u64 = 4242;

// ── Fixtures ────────────────────────────────────────────────────────

/// A CA plus a `localhost` leaf signed by it.
///
/// A CA-signed chain (rather than a bare self-signed leaf plus a disabled
/// verifier) means the client below performs *real* certificate verification —
/// so a broken cert chain on the server fails these tests instead of sliding
/// through.
struct TestCa {
    /// PEM chain: leaf followed by the CA.
    cert_pem: String,
    /// PEM private key for the leaf.
    key_pem: String,
    /// DER of the CA, for the client's root store.
    ca_der: rustls::pki_types::CertificateDer<'static>,
}

fn issue_certs() -> TestCa {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "ephpm http3 test ca");
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign ca");

    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf_params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned()]).expect("leaf params");
    let leaf = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).expect("sign leaf");

    TestCa {
        cert_pem: format!("{}{}", leaf.pem(), ca_cert.pem()),
        key_pem: leaf_key.serialize_pem(),
        ca_der: ca_cert.der().clone(),
    }
}

/// Install the process-wide rustls provider through the same entry point the
/// server uses, so the QUIC client in these tests cannot end up on a
/// different provider than the endpoint it is dialling.
fn init_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(ephpm_server::tls::install_default_crypto_provider);
}

fn test_config(document_root: &std::path::Path, max_body_size: u64) -> Config {
    Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_owned(),
            document_root: document_root.to_path_buf(),
            index_files: vec!["index.php".to_owned(), "index.html".to_owned()],
            fallback: vec![
                "$uri".to_owned(),
                "$uri/".to_owned(),
                "/index.php?$query_string".to_owned(),
            ],
            request: RequestConfig { max_body_size, ..RequestConfig::default() },
            ..ServerConfig::default()
        },
        php: PhpConfig::default(),
        db: DbConfig::default(),
        kv: KvConfig::default(),
        cluster: ClusterConfig::default(),
        middleware: Vec::new(),
        opcache: ephpm_config::OpcacheConfig::default(),
    }
}

/// A running HTTP/3 server: bound endpoint plus the task serving it.
struct TestServer {
    addr: SocketAddr,
    ca_der: rustls::pki_types::CertificateDer<'static>,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Bind and start the production HTTP/3 accept loop on an ephemeral port.
    fn start(document_root: &std::path::Path, max_body_size: u64) -> Self {
        Self::start_with(test_config(document_root, max_body_size))
    }

    /// Same, with a caller-supplied config.
    fn start_with(config: Config) -> Self {
        init_crypto();
        let certs = issue_certs();
        let dir = tempfile::tempdir().expect("cert dir");
        let cert_path = dir.path().join("chain.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, &certs.cert_pem).expect("write chain");
        std::fs::write(&key_path, &certs.key_pem).expect("write key");

        let endpoint = ephpm_server::http3::build_endpoint(
            "127.0.0.1:0".parse().expect("addr"),
            &cert_path,
            &key_path,
        )
        .expect("bind quic endpoint");
        let addr = endpoint.local_addr().expect("local addr");

        let store: Arc<Store> = Store::new(StoreConfig::default());
        let router = Arc::new(
            Router::new(&config, store, None, None, None, None, None)
                .with_alt_svc(addr.port(), TEST_ALT_SVC_MAX_AGE),
        );

        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            ephpm_server::http3::accept_loop(
                endpoint,
                router,
                Arc::new(AtomicUsize::new(0)),
                async move {
                    let _ = rx.changed().await;
                },
            )
            .await;
        });

        // The temp dir holding the PEM files can go away now: quinn read them
        // at bind time.
        drop(dir);

        Self { addr, ca_der: certs.ca_der, shutdown: tx, task }
    }

    /// Open a real HTTP/3 connection and issue one request.
    async fn request(
        &self,
        request: Request<()>,
        body: Option<Bytes>,
    ) -> (http::Response<()>, Vec<u8>) {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca_der.clone()).expect("trust the test CA");

        let mut tls =
            rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];

        let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .expect("client crypto supports quic");
        let mut endpoint =
            quinn::Endpoint::client("127.0.0.1:0".parse().expect("addr")).expect("client endpoint");
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_client)));

        let connection =
            endpoint.connect(self.addr, "localhost").expect("connect").await.expect("handshake");

        let (mut driver, mut send_request) =
            h3::client::new(h3_quinn::Connection::new(connection)).await.expect("h3 client");

        // The driver future must be polled for the connection to make
        // progress; it resolves when the connection closes.
        let drive = tokio::spawn(async move {
            let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
        });

        let mut stream = send_request.send_request(request).await.expect("send request");
        if let Some(body) = body {
            stream.send_data(body).await.expect("send body");
        }
        stream.finish().await.expect("finish request");

        let response = stream.recv_response().await.expect("recv response");

        let mut bytes = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.expect("recv data") {
            let remaining = chunk.remaining();
            bytes.extend_from_slice(chunk.copy_to_bytes(remaining).as_ref());
        }

        drop(send_request);
        drive.abort();
        endpoint.wait_idle().await;

        (response, bytes)
    }

    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await;
    }

    fn uri(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.addr.port())
    }
}

// ── Tests ───────────────────────────────────────────────────────────

/// The headline end-to-end: a real HTTP/3 client fetches a static file and
/// gets its exact bytes back, having negotiated `h3` over TLS 1.3 on UDP.
#[tokio::test]
async fn serves_a_static_file_over_real_http3() {
    let dir = tempfile::tempdir().expect("docroot");
    std::fs::write(dir.path().join("hello.txt"), b"hello over quic\n").expect("write file");

    let server = TestServer::start(dir.path(), 10 * 1024 * 1024);

    let request = Request::builder()
        .method(Method::GET)
        .uri(server.uri("/hello.txt"))
        .body(())
        .expect("build request");
    let (response, body) = server.request(request, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, b"hello over quic\n");
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("text/plain"),
        "static-file content-type detection must work identically on HTTP/3"
    );

    server.stop().await;
}

/// `Alt-Svc` is how clients discover HTTP/3 at all, so it must actually appear
/// on responses — with the configured max-age, not the default.
#[tokio::test]
async fn advertises_alt_svc_with_the_configured_max_age() {
    let dir = tempfile::tempdir().expect("docroot");
    std::fs::write(dir.path().join("hello.txt"), b"x").expect("write file");

    let server = TestServer::start(dir.path(), 10 * 1024 * 1024);
    let expected = format!("h3=\":{}\"; ma={TEST_ALT_SVC_MAX_AGE}", server.addr.port());

    let request = Request::builder()
        .method(Method::GET)
        .uri(server.uri("/hello.txt"))
        .body(())
        .expect("build request");
    let (response, _) = server.request(request, None).await;

    assert_eq!(
        response.headers().get(http::header::ALT_SVC).and_then(|v| v.to_str().ok()),
        Some(expected.as_str())
    );

    server.stop().await;
}

/// An HTTP/3 request for a `.php` file must land in the shared PHP dispatch
/// path with the script resolved from the document root.
///
/// Without `php_linked` the response body is `ephpm-php`'s stub page, which
/// names the script it *would* have executed — so this asserts the routing and
/// hand-off HTTP/3 is responsible for, not PHP's own output. On a PHP-linked
/// build the same request executes PHP for real; the status assertion holds in
/// both cases.
#[tokio::test]
async fn routes_a_php_request_through_the_shared_pipeline() {
    let dir = tempfile::tempdir().expect("docroot");
    std::fs::write(dir.path().join("index.php"), "<?php echo \"php over quic\";").expect("write");

    // The router 500s when the PHP runtime was never booted; boot it so the
    // test exercises dispatch rather than the not-ready guard. In stub builds
    // this is a flag flip, in PHP-linked builds it starts the real SAPI.
    let _ = ephpm_php::PhpRuntime::init_with_ini_file(None);

    let server = TestServer::start(dir.path(), 10 * 1024 * 1024);

    let request = Request::builder()
        .method(Method::GET)
        .uri(server.uri("/index.php"))
        .body(())
        .expect("build request");
    let (response, body) = server.request(request, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("php over quic") || text.contains("index.php"),
        "the PHP handler ran and resolved the script; got: {text}"
    );

    server.stop().await;
}

/// Request bodies sent on a QUIC stream must flow through the shared body
/// pipeline — including its size cap.
///
/// This is the strongest transport-independent proof that `H3RequestBody` is
/// really feeding [`Router::handle`]: the 413 can only come from the router
/// having read more body bytes than `max_body_size` allows.
#[tokio::test]
async fn enforces_the_shared_body_limit_on_http3_uploads() {
    let dir = tempfile::tempdir().expect("docroot");
    std::fs::write(dir.path().join("index.php"), "<?php echo \"ok\";").expect("write");

    // 64-byte cap; the upload below is deliberately larger.
    let server = TestServer::start(dir.path(), 64);

    let request = Request::builder()
        .method(Method::POST)
        .uri(server.uri("/index.php"))
        .body(())
        .expect("build request");
    let (response, _) = server.request(request, Some(Bytes::from(vec![b'a'; 4096]))).await;

    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "an oversized HTTP/3 upload must hit the same 413 as an HTTP/1.1 one"
    );

    server.stop().await;
}

/// Connection-specific header fields are illegal in HTTP/3 (RFC 9114 §4.2)
/// and `h3` does not filter responses, so ePHPm must.
///
/// A `[server.response] headers` entry is the reproducible stand-in for a PHP
/// script calling `header('Connection: close')`: the shared pipeline adds it to
/// every response, and the HTTP/3 path must remove it before it reaches the
/// wire — while leaving an ordinary custom header alone.
#[tokio::test]
async fn strips_connection_specific_headers_before_the_wire() {
    let dir = tempfile::tempdir().expect("docroot");
    std::fs::write(dir.path().join("hello.txt"), b"x").expect("write file");

    let mut config = test_config(dir.path(), 10 * 1024 * 1024);
    config.server.response.headers = vec![
        ["Connection".to_owned(), "close".to_owned()],
        ["Keep-Alive".to_owned(), "timeout=5".to_owned()],
        ["X-Custom".to_owned(), "kept".to_owned()],
    ];

    let server = TestServer::start_with(config);

    let request = Request::builder()
        .method(Method::GET)
        .uri(server.uri("/hello.txt"))
        .body(())
        .expect("build request");
    let (response, _) = server.request(request, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get(http::header::CONNECTION).is_none(),
        "Connection is illegal in HTTP/3 and must not reach the client"
    );
    assert!(response.headers().get("keep-alive").is_none(), "Keep-Alive is illegal in HTTP/3");
    assert_eq!(
        response.headers().get("x-custom").and_then(|v| v.to_str().ok()),
        Some("kept"),
        "ordinary custom headers must still be delivered"
    );

    server.stop().await;
}

/// A client that does not offer the `h3` ALPN must be rejected at the
/// handshake — the listener speaks HTTP/3 and nothing else.
#[tokio::test]
async fn rejects_a_client_without_the_h3_alpn() {
    let dir = tempfile::tempdir().expect("docroot");
    let server = TestServer::start(dir.path(), 10 * 1024 * 1024);

    let mut roots = rustls::RootCertStore::empty();
    roots.add(server.ca_der.clone()).expect("trust the test CA");
    let mut tls =
        rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    tls.alpn_protocols = vec![b"not-h3".to_vec()];

    let quic_client =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("client crypto");
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("addr")).expect("client endpoint");
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_client)));

    let result = endpoint.connect(server.addr, "localhost").expect("connect").await;
    assert!(result.is_err(), "ALPN mismatch must fail the QUIC handshake");

    server.stop().await;
}

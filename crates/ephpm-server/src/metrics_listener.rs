//! The optional separate metrics listener (`[server.metrics] listen`).
//!
//! Many servers keep their observability endpoints off the public port:
//! Caddy's admin API on `localhost:2019`, Envoy's admin listener, Traefik's
//! metrics entrypoint, RoadRunner's metrics plugin address. With `listen` set,
//! ePHPm does the same: the metrics path, `/_ephpm/requests` and the KV
//! snapshot controls (`/_ephpm/kv/snapshot[/load]`) are served **only** here
//! (see [`Router::handle_metrics_listener`]) and not on the main listener. Health, readiness and primary probes stay on the main listener,
//! where load balancers and kubelets already look for them.
//!
//! Optionally HTTPS (`tls_cert`/`tls_key`), and optionally **mutual TLS**
//! (`client_ca`): the handshake then requires a client certificate signed by
//! that CA, so a connection that reaches HTTP at all is a verified one, and
//! that verification counts as the credential a multi-site node requires.
//!
//! A separate address limits **network exposure**. It is not tenant
//! isolation: on a multi-site node tenant PHP can connect to `127.0.0.1:9100`
//! as easily as the scraper can. Only the token or mTLS separates them.
//!
//! The listener is deliberately small: HTTP/1.1, a header-read timeout, no
//! middleware, no PHP, no static files. It is bound during startup (a bind
//! failure fails startup, like every other listener) and aborted at shutdown
//! — a half-finished scrape has nothing worth draining.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ephpm_config::MetricsConfig;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::router::Router;

/// How long a client may take to send its request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A bound, not-yet-serving metrics listener.
pub struct MetricsListener {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    /// Whether `tls` demands and verifies client certificates.
    mtls: bool,
}

impl MetricsListener {
    /// Bind `[server.metrics] listen` (and build its TLS acceptor) when set.
    ///
    /// # Errors
    ///
    /// Fails when the address does not parse, cannot be bound, or the TLS /
    /// client-CA files cannot be loaded — the same fail-fast contract as the
    /// main listeners, so a configured-but-absent metrics port is never
    /// silent.
    pub async fn bind(config: &MetricsConfig) -> anyhow::Result<Option<Self>> {
        let Some(listen) = &config.listen else {
            return Ok(None);
        };
        let addr: SocketAddr =
            listen.parse().with_context(|| format!("invalid [server.metrics] listen {listen}"))?;
        let tls = match (&config.tls_cert, &config.tls_key) {
            (Some(cert), Some(key)) => Some(
                crate::tls::build_metrics_tls_acceptor(cert, key, config.client_ca.as_deref())
                    .context("failed to build the [server.metrics] listener TLS config")?,
            ),
            _ => None,
        };
        let mtls = tls.is_some() && config.client_ca.is_some();
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind [server.metrics] listen {addr}"))?;
        tracing::info!(
            %addr,
            tls = tls.is_some(),
            mtls,
            "metrics listener — {} and /_ephpm/requests are served here only",
            config.path
        );
        Ok(Some(Self { listener, tls, mtls }))
    }

    /// The bound local address (useful when binding port `0` in tests).
    ///
    /// # Errors
    ///
    /// Propagates the socket's `local_addr` error.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Run the accept loop until the returned task is aborted.
    #[must_use]
    pub fn spawn(self, router: Arc<Router>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let (stream, peer) = match self.listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::debug!(error = %e, "metrics listener accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let router = Arc::clone(&router);
                let tls = self.tls.clone();
                let mtls = self.mtls;
                tokio::spawn(async move {
                    serve(stream, peer, tls, mtls, router).await;
                });
            }
        })
    }
}

/// Serve one connection: optional TLS handshake (which, with `client_ca`,
/// rejects any client without a valid certificate), then HTTP/1.1.
async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    tls: Option<TlsAcceptor>,
    mtls: bool,
    router: Arc<Router>,
) {
    let _ = stream.set_nodelay(true);
    match tls {
        None => serve_http(stream, peer, false, false, router).await,
        Some(acceptor) => match acceptor.accept(stream).await {
            // With a client verifier installed, rustls only completes the
            // handshake for a client whose certificate chains to client_ca.
            Ok(tls_stream) => serve_http(tls_stream, peer, true, mtls, router).await,
            Err(e) => tracing::debug!(%peer, error = %e, "metrics listener TLS handshake failed"),
        },
    }
}

async fn serve_http<I>(io: I, peer: SocketAddr, is_tls: bool, mtls: bool, router: Arc<Router>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let router = Arc::clone(&router);
        async move {
            Ok::<_, std::convert::Infallible>(
                router.handle_metrics_listener(req, peer, is_tls, mtls).await,
            )
        }
    });
    let result = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(TokioIo::new(io), service)
        .await;
    if let Err(e) = result {
        tracing::debug!(%peer, error = %e, "metrics listener connection error");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ephpm_config::Config;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    struct Pki {
        ca_pem: String,
        ca_der: CertificateDer<'static>,
        server_chain_pem: String,
        server_key_pem: String,
        /// (chain DER, key DER) of a client cert signed by the CA.
        good_client: (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>),
        /// A client cert signed by an unrelated CA.
        rogue_client: (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>),
    }

    fn ca(name: &str) -> (rcgen::Certificate, rcgen::KeyPair) {
        let key = rcgen::KeyPair::generate().expect("ca key");
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name.push(rcgen::DnType::CommonName, name);
        (params.self_signed(&key).expect("self-sign ca"), key)
    }

    fn client_cert(
        ca: &rcgen::Certificate,
        ca_key: &rcgen::KeyPair,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let key = rcgen::KeyPair::generate().expect("client key");
        let mut params =
            rcgen::CertificateParams::new(vec!["scraper".to_owned()]).expect("client params");
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let cert = params.signed_by(&key, ca, ca_key).expect("sign client");
        let key_der = PrivateKeyDer::try_from(key.serialize_der()).expect("client key der");
        (vec![cert.der().clone()], key_der)
    }

    fn pki() -> Pki {
        let (ca_cert, ca_key) = ca("ephpm metrics test ca");
        let server_key = rcgen::KeyPair::generate().expect("server key");
        let server = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .expect("server params")
            .signed_by(&server_key, &ca_cert, &ca_key)
            .expect("sign server");
        let good_client = client_cert(&ca_cert, &ca_key);
        let (rogue_ca, rogue_key) = ca("someone else");
        let rogue_client = client_cert(&rogue_ca, &rogue_key);
        Pki {
            ca_pem: ca_cert.pem(),
            ca_der: ca_cert.der().clone(),
            server_chain_pem: format!("{}{}", server.pem(), ca_cert.pem()),
            server_key_pem: server_key.serialize_pem(),
            good_client,
            rogue_client,
        }
    }

    fn router(dir: &Path, sites_dir: &Path) -> Arc<Router> {
        ephpm_php::PhpRuntime::init().expect("stub runtime init");
        let mut config = Config::default();
        config.server.document_root = dir.to_path_buf();
        config.php.concurrency = 1;
        config.server.metrics.enabled = true;
        config.server.metrics.listen = Some("127.0.0.1:0".to_owned());
        // Multi-site, no token: only a verified client certificate admits.
        config.server.sites_dir = Some(sites_dir.to_path_buf());
        let handle =
            metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder().handle();
        Arc::new(Router::new(
            &config,
            ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default()),
            None,
            Some(handle),
            None,
            None,
            None,
        ))
    }

    /// GET /metrics over TLS with an optional client identity. `Err` when the
    /// handshake (or the first read) fails.
    async fn scrape(
        addr: SocketAddr,
        ca: &CertificateDer<'static>,
        identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
    ) -> Result<String, String> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.clone()).expect("root");
        let builder = rustls::ClientConfig::builder_with_provider(crate::tls::crypto_provider())
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots);
        let config = match identity {
            Some((chain, key)) => builder.with_client_auth_cert(chain, key).expect("client auth"),
            None => builder.with_no_client_auth(),
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let name = ServerName::try_from("localhost").expect("name");
        let mut tls = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
        tls.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        // TLS 1.3 reports a rejected client certificate after the handshake,
        // on the first read.
        tls.read_to_end(&mut out).await.map_err(|e| e.to_string())?;
        if out.is_empty() {
            return Err("connection closed with no response".to_owned());
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// End to end over a real socket: the mTLS metrics listener serves a
    /// client whose certificate chains to `client_ca` — on a multi-site node
    /// with no token, where that certificate is the only credential — and
    /// refuses one with no certificate or a certificate from another CA at
    /// the TLS layer, before any HTTP is spoken.
    #[tokio::test]
    async fn mtls_listener_admits_only_ca_signed_clients() {
        crate::tls::tests_support::init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let sites = tempfile::tempdir().unwrap();
        let pki = pki();
        let cert = dir.path().join("server.pem");
        let key = dir.path().join("server.key");
        let ca = dir.path().join("ca.pem");
        std::fs::write(&cert, &pki.server_chain_pem).unwrap();
        std::fs::write(&key, &pki.server_key_pem).unwrap();
        std::fs::write(&ca, &pki.ca_pem).unwrap();

        let mcfg = MetricsConfig {
            enabled: true,
            listen: Some("127.0.0.1:0".to_owned()),
            tls_cert: Some(cert),
            tls_key: Some(key),
            client_ca: Some(ca),
            ..MetricsConfig::default()
        };
        let listener = MetricsListener::bind(&mcfg).await.expect("bind").expect("configured");
        let addr = listener.local_addr().unwrap();
        let task = listener.spawn(router(dir.path(), sites.path()));

        let ok = scrape(addr, &pki.ca_der, Some(pki.good_client)).await.expect("good client");
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");

        let none = scrape(addr, &pki.ca_der, None).await;
        assert!(none.is_err(), "no client certificate must fail the handshake: {none:?}");

        let rogue = scrape(addr, &pki.ca_der, Some(pki.rogue_client)).await;
        assert!(rogue.is_err(), "a certificate from another CA must fail: {rogue:?}");

        task.abort();
    }

    /// Plain-HTTP metrics listener on a multi-site node with no token: the
    /// endpoint answers 403 naming the setting, over the real socket.
    #[tokio::test]
    async fn plain_listener_on_multi_site_without_a_token_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let sites = tempfile::tempdir().unwrap();
        let mcfg = MetricsConfig {
            enabled: true,
            listen: Some("127.0.0.1:0".to_owned()),
            ..MetricsConfig::default()
        };
        let listener = MetricsListener::bind(&mcfg).await.expect("bind").expect("configured");
        let addr = listener.local_addr().unwrap();
        let task = listener.spawn(router(dir.path(), sites.path()));

        let mut tcp = TcpStream::connect(addr).await.unwrap();
        tcp.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        tcp.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        assert!(out.contains("[server.metrics] token"), "{out}");
        task.abort();
    }
}

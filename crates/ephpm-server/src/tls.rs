//! TLS setup for the HTTP server.
//!
//! Loads PEM certificate chains and private keys from disk and builds
//! a [`tokio_rustls::TlsAcceptor`] for wrapping TCP connections.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// Build a TLS acceptor from PEM-encoded cert and key files.
///
/// The certificate file should contain the full chain (leaf + intermediates).
/// The key file should contain a single private key in PKCS#8, RSA, or EC format.
///
/// # Errors
///
/// Returns an error if the files cannot be read, parsed, or if the cert/key
/// pair is invalid.
pub fn build_tls_acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("invalid TLS certificate/key pair")?;

    // Only advertise HTTP/1.1 — this server does not support h2 yet.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Load PEM-encoded certificates from a file.
fn load_certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let file =
        File::open(path).with_context(|| format!("cannot open cert file: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse PEM certificates from {}", path.display()))
}

/// Load a private key from a PEM file.
///
/// Supports PKCS#8, RSA, and EC key formats.
fn load_private_key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    let file =
        File::open(path).with_context(|| format!("cannot open key file: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("failed to parse private key from {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", path.display()))
}

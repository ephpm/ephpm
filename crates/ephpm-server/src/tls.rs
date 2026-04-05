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

    // Advertise HTTP/2 and HTTP/1.1 (preference order: h2 first).
    // Clients that support h2 will negotiate it; others fall back to http/1.1.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

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

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use super::*;

    static CRYPTO_INIT: Once = Once::new();

    fn init_crypto() {
        CRYPTO_INIT.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("install ring crypto provider");
        });
    }

    /// Generate a self-signed RSA cert+key pair using openssl CLI.
    fn generate_rsa_cert(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let status = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-keyout"])
            .arg(&key)
            .args(["-out"])
            .arg(&cert)
            .args(["-days", "1", "-nodes", "-subj", "/CN=localhost"])
            .output()
            .expect("openssl must be available");
        assert!(status.status.success(), "openssl cert generation failed");
        (cert, key)
    }

    /// Generate a self-signed EC cert+key pair using openssl CLI.
    fn generate_ec_cert(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = dir.join("ec-cert.pem");
        let key = dir.join("ec-key.pem");
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-keyout",
            ])
            .arg(&key)
            .args(["-out"])
            .arg(&cert)
            .args(["-days", "1", "-nodes", "-subj", "/CN=localhost"])
            .output()
            .expect("openssl must be available");
        assert!(status.status.success(), "openssl EC cert generation failed");
        (cert, key)
    }

    #[test]
    fn load_valid_rsa_cert_and_key() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = generate_rsa_cert(dir.path());
        assert!(build_tls_acceptor(&cert, &key).is_ok());
    }

    #[test]
    fn load_valid_ec_cert_and_key() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = generate_ec_cert(dir.path());
        assert!(build_tls_acceptor(&cert, &key).is_ok());
    }

    #[test]
    fn missing_cert_file_returns_error() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let (_, key) = generate_rsa_cert(dir.path());
        let err = build_tls_acceptor(Path::new("/nonexistent/cert.pem"), &key)
            .err()
            .expect("should fail with missing cert");
        let msg = format!("{err:#}");
        assert!(msg.contains("cannot open cert file"), "unexpected error: {msg}");
    }

    #[test]
    fn missing_key_file_returns_error() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = generate_rsa_cert(dir.path());
        let err = build_tls_acceptor(&cert, Path::new("/nonexistent/key.pem"))
            .err()
            .expect("should fail with missing key");
        let msg = format!("{err:#}");
        assert!(msg.contains("cannot open key file"), "unexpected error: {msg}");
    }

    #[test]
    fn invalid_cert_pem_returns_error() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("bad-cert.pem");
        let (_, key) = generate_rsa_cert(dir.path());
        std::fs::write(&cert, "not a real PEM certificate").unwrap();
        let err = build_tls_acceptor(&cert, &key).err().expect("should fail with invalid cert");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid TLS")
                || msg.contains("no private key")
                || msg.contains("certificate"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn invalid_key_pem_returns_error() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = generate_rsa_cert(dir.path());
        let key = dir.path().join("bad-key.pem");
        std::fs::write(&key, "not a real PEM key").unwrap();
        let err = build_tls_acceptor(&cert, &key).err().expect("should fail with invalid key");
        let msg = format!("{err:#}");
        assert!(msg.contains("no private key"), "unexpected error: {msg}");
    }

    #[test]
    fn mismatched_cert_key_returns_error() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = generate_rsa_cert(dir.path());
        let dir2 = tempfile::tempdir().unwrap();
        let (_, other_key) = generate_rsa_cert(dir2.path());
        let err = build_tls_acceptor(&cert, &other_key)
            .err()
            .expect("should fail with mismatched cert/key");
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid TLS"), "unexpected error: {msg}");
    }
}

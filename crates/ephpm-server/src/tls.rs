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
    // Advertise HTTP/2 and HTTP/1.1 (preference order: h2 first).
    // Clients that support h2 will negotiate it; others fall back to http/1.1.
    let config = build_server_config(cert_path, key_path, &[b"h2".to_vec(), b"http/1.1".to_vec()])?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Build a [`rustls::ServerConfig`] from PEM cert/key files with the given
/// ALPN protocol list.
///
/// Both transports go through this one function: the TCP listener asks for
/// `["h2", "http/1.1"]`, the QUIC listener asks for `["h3"]`. Sharing it is
/// what guarantees they agree on the crypto provider (see
/// [`crypto_provider`]) — "HTTP/3 negotiated a different provider than HTTPS"
/// is not representable.
///
/// # Errors
///
/// Returns an error if the files cannot be read or parsed, or if the
/// cert/key pair is invalid.
pub fn build_server_config(
    cert_path: &Path,
    key_path: &Path,
    alpn: &[Vec<u8>],
) -> anyhow::Result<ServerConfig> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let mut config = ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .context("crypto provider does not support the default TLS versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("invalid TLS certificate/key pair")?;

    config.alpn_protocols = alpn.to_vec();

    Ok(config)
}

/// Install this process's rustls crypto provider, exactly once.
///
/// Call this once at startup, before anything can build a rustls config.
/// It is idempotent and never panics: `install_default` returns `Err` when a
/// provider is already installed, and that is a normal outcome (a test
/// harness, an embedding host, or a second call), not a failure.
///
/// ePHPm's own configs do not depend on this — they name the provider
/// explicitly through `crypto_provider()`. It exists for the *third-party*
/// rustls users in the tree that call the bare `builder()` and therefore
/// consult the process default: `hyper-rustls` behind
/// `metrics-exporter-prometheus`, and `reqwest` in the integration tests.
/// Installing deliberately means those all land on the same provider ePHPm's
/// listeners use, instead of whichever one a crate feature happened to pick.
pub fn install_default_crypto_provider() {
    // `install_default` consumes a `CryptoProvider`; clone out of the shared
    // `Arc` so the installed default and `crypto_provider()` are the same
    // configuration.
    let _ = crypto_provider().as_ref().clone().install_default();
}

/// The rustls crypto provider this server uses: **aws-lc-rs**.
///
/// The provider is named explicitly here and `ServerConfig::builder()` is
/// never called. Why both halves of that matter:
///
/// rustls picks a provider from its crate features only when *exactly one*
/// of `ring` / `aws_lc_rs` is enabled. Cargo unions features across the whole
/// graph, so for a long time this binary had both on; in that state
/// `from_crate_features()` returns `None` rather than choosing, and the bare
/// builder panics at runtime:
///
/// > Could not automatically determine the process-level CryptoProvider from
/// > Rustls crate features.
///
/// That is not hypothetical — it made every `[server.tls]` static-certificate
/// startup abort with exit 101, in every release from v0.1.0 through v0.6.1,
/// while the ACME path kept working because `rustls-acme` names its provider.
/// #243 fixed the call site; **#241 removed the ambiguity itself** by
/// converging the tree on aws-lc-rs (see the `rustls` pin in the workspace
/// `Cargo.toml` for why aws-lc-rs and not ring).
///
/// Naming the provider is kept anyway, as defence in depth: it is a one-line
/// dependency change to re-introduce `rustls/ring`, and if that happens the
/// explicit name is the difference between a lint-visible duplicate and a
/// startup panic. `tests/tls_single_crypto_provider.rs` guards the feature
/// state; `tests/tls_provider_independence.rs` guards the call sites.
///
/// The `OnceLock` means every config built here shares one `Arc`, so "the
/// whole server uses one provider" is checkable by pointer identity — which
/// is exactly how the HTTP/3 tests assert that QUIC and HTTPS-over-TCP did
/// not diverge.
fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    static PROVIDER: std::sync::OnceLock<Arc<rustls::crypto::CryptoProvider>> =
        std::sync::OnceLock::new();
    Arc::clone(PROVIDER.get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider())))
}

/// Load PEM-encoded certificates from a file.
fn load_certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let file =
        File::open(path).with_context(|| format!("cannot open cert file: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse PEM certificates from {}", path.display()))?;
    // A PEM file with no CERTIFICATE block parses "successfully" into zero
    // certs; without this check the failure surfaces later as an opaque
    // rustls error instead of naming the file (or, for QUIC, as a handshake
    // that never completes).
    anyhow::ensure!(!certs.is_empty(), "no certificates found in {}", path.display());
    Ok(certs)
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

/// Certificate fixtures shared by the TLS and HTTP/3 test modules.
///
/// HTTP/3 has to build its endpoint from the *same* cert-loading path the TCP
/// listener uses, so its tests need the same fixtures; duplicating them would
/// let the two drift.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::Path;
    use std::sync::Once;

    static CRYPTO_INIT: Once = Once::new();

    /// Install the process-wide rustls crypto provider exactly once.
    ///
    /// Goes through the same entry point production startup uses, so a test
    /// can never install a *different* provider than the server would. The
    /// `Once` is belt-and-braces: `install_default_crypto_provider` is itself
    /// idempotent.
    pub(crate) fn init_crypto() {
        CRYPTO_INIT.call_once(super::install_default_crypto_provider);
    }

    /// Generate a self-signed RSA cert+key pair using rcgen.
    pub(crate) fn generate_rsa_cert(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
            .expect("generate RSA-2048 key pair");
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .expect("build cert params");
        let cert = params.self_signed(&key_pair).expect("self-sign RSA cert");
        std::fs::write(&cert_path, cert.pem()).expect("write RSA cert");
        std::fs::write(&key_path, key_pair.serialize_pem()).expect("write RSA key");
        (cert_path, key_path)
    }

    /// Generate a self-signed EC (P-256) cert+key pair using rcgen.
    pub(crate) fn generate_ec_cert(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert_path = dir.join("ec-cert.pem");
        let key_path = dir.join("ec-key.pem");
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .expect("generate ECDSA P-256 key pair");
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .expect("build cert params");
        let cert = params.self_signed(&key_pair).expect("self-sign EC cert");
        std::fs::write(&cert_path, cert.pem()).expect("write EC cert");
        std::fs::write(&key_path, key_pair.serialize_pem()).expect("write EC key");
        (cert_path, key_path)
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{generate_ec_cert, generate_rsa_cert, init_crypto};
    use super::*;

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

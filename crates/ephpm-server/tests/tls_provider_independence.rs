//! The TLS stack must not depend on a process-default rustls crypto provider.
//!
//! **This file must live in its own integration-test binary and must never
//! call `CryptoProvider::install_default()`.** That is the entire point.
//!
//! Both `ring` and `aws-lc-rs` end up compiled in — the workspace pins
//! `rustls` to `ring`, while `rustls-acme`, `rcgen` and `hyper-rustls` (via
//! `metrics-exporter-prometheus`) enable `aws-lc-rs`, and Cargo unions
//! features. In that configuration rustls' `from_crate_features()` returns
//! `None` rather than picking one, so `rustls::ServerConfig::builder()`
//! panics unless something already installed a provider:
//!
//! > Could not automatically determine the process-level CryptoProvider from
//! > Rustls crate features.
//!
//! `crates/ephpm-server/src/tls.rs` did exactly that, so every server
//! configured with a static `[server.tls]` cert/key aborted at startup with
//! exit 101 — reproduced on v0.6.1 and on main. It survived review and CI
//! because:
//!
//! - `rustls-acme` always calls `builder_with_provider`, so the ACME path was
//!   unaffected and kept working; and
//! - every unit test in `tls.rs` calls `install_default()` first, which masks
//!   the panic for the whole test binary.
//!
//! That second point is why this file is separate. Adding an
//! `install_default()` call anywhere in this binary, or folding these tests
//! into one that has such a call, silently disarms them.
//!
//! Provider consolidation is tracked in issue #241.

use std::path::{Path, PathBuf};

fn self_signed(dir: &Path) -> (PathBuf, PathBuf) {
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("key pair");
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
    let cert = params.self_signed(&key_pair).expect("self-sign");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem()).expect("write cert");
    std::fs::write(&key_path, key_pair.serialize_pem()).expect("write key");
    (cert_path, key_path)
}

/// Building the acceptor is what `serve()` does for `[server.tls]` cert/key.
/// Before the fix this panicked instead of returning.
#[test]
fn tcp_tls_acceptor_builds_without_an_installed_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key) = self_signed(dir.path());

    ephpm_server::tls::build_tls_acceptor(&cert, &key)
        .expect("static-cert TLS must not need a process-default crypto provider");
}

/// HTTP/3 builds its QUIC endpoint through the same `tls.rs` code path, so it
/// inherits the fix — asserted here rather than assumed.
#[tokio::test]
async fn http3_endpoint_builds_without_an_installed_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key) = self_signed(dir.path());

    let endpoint =
        ephpm_server::http3::build_endpoint("127.0.0.1:0".parse().expect("addr"), &cert, &key)
            .expect("HTTP/3 endpoint must not need a process-default crypto provider");
    endpoint.close(quinn::VarInt::from_u32(0), b"test over");
}

/// A valid PEM that simply contains no CERTIFICATE block must be rejected
/// against the offending file, not deferred into an opaque rustls error.
///
/// The case used here is the one operators actually hit: `cert` pointed at
/// the private key. That parses cleanly and yields zero certificates, so
/// without an explicit check it slips past `load_certs`.
#[test]
fn certificate_pem_with_no_certificate_is_reported_against_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_cert, key) = self_signed(dir.path());

    // `TlsAcceptor` is not `Debug`, so unwrap the Result by hand.
    let Err(err) = ephpm_server::tls::build_tls_acceptor(&key, &key) else {
        panic!("a PEM with no certificate must not be accepted");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("no certificates found"), "unexpected error: {msg}");
}

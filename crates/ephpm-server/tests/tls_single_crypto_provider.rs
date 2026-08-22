//! Exactly one rustls crypto provider may be compiled into this tree (#241).
//!
//! **This file must contain exactly one test, and it must live in its own
//! integration-test binary.** Both constraints are load-bearing — see
//! "Why it is alone in here" below.
//!
//! # What is being pinned
//!
//! `rustls` selects a provider from its crate features via
//! `CryptoProvider::from_crate_features()`, which returns `Some` only when
//! *exactly one* of `ring` / `aws_lc_rs` is enabled. With both on it returns
//! `None` — it does not pick a winner — and every bare
//! `ServerConfig::builder()` / `ClientConfig::builder()` in the process
//! panics:
//!
//! > Could not automatically determine the process-level CryptoProvider from
//! > Rustls crate features.
//!
//! Cargo unions features across the entire graph, so a single new dependency
//! (or one feature flip on an existing one) is enough to put the tree back in
//! that state. When it happened before, it made static-certificate
//! `[server.tls]` startup abort with exit 101 in every release from v0.1.0
//! through v0.6.1, and nothing caught it for thirteen tags.
//!
//! This test converts that from a runtime surprise into a test failure.
//!
//! # Why it is alone in here
//!
//! `ServerConfig::builder()` does not merely *read* the crate-feature
//! provider — on success it **installs it as the process default**. Any test
//! that runs afterwards in the same binary then sees a provider already
//! installed, which is exactly the state
//! `tests/tls_provider_independence.rs` exists to rule out. Adding a second
//! test here, or folding this one into another file, silently disarms
//! whatever shares the binary with it.
//!
//! # If this test fails
//!
//! Do not "fix" it by installing a provider first — that hides the problem
//! rather than reporting it. Find the new `rustls/ring` edge and remove it:
//!
//! ```text
//! cargo tree -i rustls@0.23 -e features | grep -n 'rustls feature "ring"' -A6
//! ```
//!
//! As of #241 the three edges that had to be steered were the workspace
//! `rustls` pin, the workspace `quinn` pin (`rustls-ring` →
//! `rustls-aws-lc-rs`), and `reqwest`'s `rustls-tls` dev-dependency feature
//! on `crates/ephpm`. `ring` still appears in `Cargo.lock` behind
//! `opensrv-mysql 0.7.0` → `tokio-rustls 0.25` → `rustls 0.22`, which is a
//! separate stack ePHPm cannot reach from here (litewire owns that edge, and
//! opensrv-mysql has had no release since 2024-02).

use rustls::crypto::CryptoProvider;

/// `from_crate_features()` must be able to pick a provider on its own.
///
/// Asserted through the public builder rather than the (private)
/// `from_crate_features` so this tests the real path a third-party crate
/// takes.
#[test]
fn exactly_one_crypto_provider_is_compiled_in() {
    assert!(
        CryptoProvider::get_default().is_none(),
        "nothing in this test binary may install a provider before this \
         assertion runs — see the module docs; the check below is meaningless \
         once a process default exists"
    );

    // Panics with "Could not automatically determine the process-level
    // CryptoProvider" if both `ring` and `aws_lc_rs` are enabled on rustls.
    let _ = rustls::ServerConfig::builder();

    // The provider it settled on is the one the rest of the tree names
    // explicitly. If these ever diverge, ePHPm's listeners and its
    // third-party rustls users (hyper-rustls, reqwest) are on different
    // crypto — which is the failure mode #241 set out to make impossible.
    let installed = CryptoProvider::get_default().expect("the builder installs what it selected");
    let expected = rustls::crypto::aws_lc_rs::default_provider();
    assert_eq!(
        installed.cipher_suites, expected.cipher_suites,
        "the crate-feature provider must be aws-lc-rs (see the workspace rustls pin)"
    );
}

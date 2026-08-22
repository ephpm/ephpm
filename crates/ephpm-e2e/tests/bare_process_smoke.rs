//! Bare-process fixture smoke test.
//!
//! Exercises [`ClusterFixture`] directly: spawns a 2-node ephpm cluster on
//! 127.0.0.1 and asserts both nodes serve `/index.php`. This is the only
//! coverage `ClusterFixture` has — the harness's own cluster fixture lives in
//! `xtask`, so without this suite the library one is compiled and never run.
//!
//! # How it is wired (issue #239)
//!
//! This suite manages its own topology, so `cargo xtask e2e` lists it in
//! `SELF_MANAGED_SUITES`: the harness spawns **no** node for it and hands it
//! `EPHPM_BINARY` plus `EPHPM_DOCROOT`. [`ClusterFixture`] reserves its own
//! loopback ports and writes its own config template, so it cannot collide
//! with — or perturb — the fixed-port fixtures the harness spawns later. That
//! is the same contract `turso_cdc` runs under, and the reason neither suite
//! touches the harness's shared node template (the #288/#295 lesson).
//!
//! Two bugs kept this dormant and broken before that:
//!
//! 1. Nothing set `EPHPM_BINARY` for it, so it silently `return`ed and
//!    reported `ok` — it had never once run.
//! 2. It derived its docroot from `CARGO_MANIFEST_DIR`. The harness execs
//!    **pre-built** test binaries, which inherit *xtask's* environment, so
//!    that resolved to `xtask/tests/docroot` — a path that does not exist.
//!    The docroot now comes from `EPHPM_DOCROOT`, with a fallback (below)
//!    for a direct `cargo test` run where no harness is involved.

use std::path::PathBuf;

use ephpm_e2e::{ClusterFixture, ephpm_binary_env};

/// Resolve the document root to serve.
///
/// Prefers `EPHPM_DOCROOT`, which `cargo xtask e2e` sets to the canonicalized
/// `tests/docroot`. Falls back to deriving it from `CARGO_MANIFEST_DIR` for a
/// direct `cargo test --manifest-path crates/ephpm-e2e/Cargo.toml` run, where
/// cargo sets that variable to *this* crate's directory and the workspace root
/// is two levels up. Under the harness the fallback is never used — and must
/// never be relied on, because there `CARGO_MANIFEST_DIR` belongs to xtask.
fn resolve_docroot() -> PathBuf {
    if let Some(dir) = std::env::var_os("EPHPM_DOCROOT") {
        return PathBuf::from(dir);
    }
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // crates/ephpm-e2e -> crates -> <workspace root>
    manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("ephpm-e2e manifest dir has a workspace root two levels up")
        .join("tests")
        .join("docroot")
}

#[tokio::test]
async fn two_node_cluster_serves_php() {
    let Some(binary) = ephpm_binary_env() else {
        // Deliberate, and the only skip left: a bare `cargo test` on a
        // checkout with no release build cannot spawn anything. `cargo xtask
        // e2e` always sets this, so a skip in CI means the harness wiring
        // regressed — hence the loud marker rather than a quiet `return`.
        eprintln!(
            "SKIP two_node_cluster_serves_php: EPHPM_BINARY is unset. \
             Under `cargo xtask e2e` this must never happen — check that \
             `bare_process_smoke` is still in SELF_MANAGED_SUITES."
        );
        return;
    };

    let docroot = resolve_docroot();
    assert!(
        docroot.join("index.php").is_file(),
        "docroot {} has no index.php — set EPHPM_DOCROOT to the tests/docroot to serve",
        docroot.display()
    );

    let fixture = ClusterFixture::start(&binary, &docroot, 2)
        .await
        .unwrap_or_else(|e| panic!("failed to start cluster fixture: {e}"));

    let client =
        reqwest::Client::builder().pool_max_idle_per_host(0).build().expect("reqwest client");

    for (i, base_url) in fixture.base_urls().iter().enumerate() {
        let url = format!("{base_url}/index.php");
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {url} on node {i} failed: {e}"));
        assert_eq!(resp.status().as_u16(), 200, "node {i} ({base_url}) returned {}", resp.status());
        let body = resp.text().await.expect("body");
        assert!(body.contains("ePHPm"), "node {i} response missing 'ePHPm' marker, got: {body}");
    }
}

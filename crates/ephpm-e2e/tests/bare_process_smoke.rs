//! Bare-process fixture smoke test.
//!
//! Exercises `ClusterFixture` directly: spawns a 2-node ephpm cluster on
//! 127.0.0.1 and asserts both nodes serve `/index.php`. Skips gracefully
//! when `EPHPM_BINARY` is unset so `cargo test --no-run` still succeeds on
//! machines without a built release binary.

use std::path::PathBuf;

use ephpm_e2e::{ClusterFixture, ephpm_binary_env};

#[tokio::test]
async fn two_node_cluster_serves_php() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping bare-process cluster smoke test");
        return;
    };

    // The docroot lives next to this test file; walk up from the manifest dir.
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let docroot = manifest_dir.join("tests").join("docroot");
    assert!(docroot.exists(), "docroot missing at {}", docroot.display());

    let fixture = ClusterFixture::start(&binary, &docroot, 2)
        .await
        .unwrap_or_else(|e| panic!("failed to start cluster fixture: {e}"));

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("reqwest client");

    for (i, base_url) in fixture.base_urls().iter().enumerate() {
        let url = format!("{base_url}/index.php");
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {url} on node {i} failed: {e}"));
        assert_eq!(
            resp.status().as_u16(),
            200,
            "node {i} ({base_url}) returned {}",
            resp.status()
        );
        let body = resp.text().await.expect("body");
        assert!(
            body.contains("ePHPm"),
            "node {i} response missing 'ePHPm' marker, got: {body}"
        );
    }
}

//! Virtual host e2e tests.
//!
//! Tests directory-based virtual hosting with lazy discovery.
//! Requires ephpm configured with `sites_dir` and a way to create
//! site directories at runtime.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)
//! - `EPHPM_SITES_DIR` — path to the sites directory on the ephpm host
//!   (must be writable by the test runner, e.g. shared volume in k8s)

use ephpm_e2e::required_env;
use std::path::PathBuf;

/// Get the sites directory path from env.
fn sites_dir() -> PathBuf {
    PathBuf::from(required_env("EPHPM_SITES_DIR"))
}

/// Make an HTTP request with a specific Host header.
async fn get_with_host(base_url: &str, host: &str, path: &str) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .get(format!("{base_url}{path}"))
        .header("Host", host)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} with Host: {host} failed: {e}"))
}

#[tokio::test]
async fn unknown_host_returns_fallback() {
    let base_url = required_env("EPHPM_URL");

    // Request with a host that has no site directory — should get the
    // fallback document_root response (200 from default site or 404).
    let resp = get_with_host(&base_url, "nonexistent-site.example.com", "/").await;

    // Either 200 (fallback docroot has an index) or 404 (no index in fallback).
    // Both are valid — the key is it's NOT a 500.
    let status = resp.status().as_u16();
    assert!(
        status == 200 || status == 404,
        "expected 200 or 404 for unknown host, got {status}"
    );
}

#[tokio::test]
async fn lazy_discovered_site_serves_content() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();
    let host = "lazy-test.preview.ephpm.dev";
    let site_dir = sites.join(host);

    // Ensure clean state.
    let _ = std::fs::remove_dir_all(&site_dir);

    // Verify site doesn't exist yet — should fallback.
    let resp = get_with_host(&base_url, host, "/").await;
    let before_status = resp.status().as_u16();
    assert!(
        before_status == 200 || before_status == 404,
        "expected fallback response before site exists, got {before_status}"
    );

    // Deploy: create the site directory with an index.html.
    std::fs::create_dir_all(&site_dir).expect("failed to create site directory");
    std::fs::write(
        site_dir.join("index.html"),
        "<html><body>lazy discovery works</body></html>",
    )
    .expect("failed to write index.html");

    // Request again — should now serve from the new site directory.
    let resp = get_with_host(&base_url, host, "/index.html").await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from lazily discovered site"
    );
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("lazy discovery works"),
        "expected content from new site directory, got: {body}"
    );

    // Teardown: remove the site directory.
    std::fs::remove_dir_all(&site_dir).expect("failed to remove site directory");

    // Request again — should fall back to default.
    let resp = get_with_host(&base_url, host, "/index.html").await;
    let after_status = resp.status().as_u16();
    assert_ne!(
        after_status, 200,
        "site should no longer serve after directory removal"
    );
}

#[tokio::test]
async fn multiple_sites_isolated() {
    let base_url = required_env("EPHPM_URL");
    let sites = sites_dir();

    let host_a = "site-a.preview.ephpm.dev";
    let host_b = "site-b.preview.ephpm.dev";
    let dir_a = sites.join(host_a);
    let dir_b = sites.join(host_b);

    // Clean state.
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);

    // Deploy two sites with different content.
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::write(dir_a.join("index.html"), "site A content").unwrap();

    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_b.join("index.html"), "site B content").unwrap();

    // Verify each site serves its own content.
    let resp_a = get_with_host(&base_url, host_a, "/index.html").await;
    assert_eq!(resp_a.status().as_u16(), 200);
    let body_a = resp_a.text().await.unwrap();
    assert!(body_a.contains("site A content"), "site A got: {body_a}");

    let resp_b = get_with_host(&base_url, host_b, "/index.html").await;
    assert_eq!(resp_b.status().as_u16(), 200);
    let body_b = resp_b.text().await.unwrap();
    assert!(body_b.contains("site B content"), "site B got: {body_b}");

    // Cleanup.
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

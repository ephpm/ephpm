//! Idle and header-read timeout configuration tests.
//!
//! Validates that ephpm accepts and operates correctly with explicit timeout
//! configuration. Since timing-sensitive tests are unreliable in CI, these
//! tests take a conservative approach:
//! - Verify the server starts and serves normally with explicit timeout values
//! - Verify PHP execution still works within the configured timeouts
//! - Verify static file serving is unaffected by timeout settings
//!
//! The actual timeout enforcement is covered by `timeouts.rs` (request timeout)
//! and `timeout_edge.rs` (recovery after timeout). This file focuses on the
//! `[server.timeouts]` idle and header_read config fields.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Verify the server starts and serves static files normally with explicit
/// idle and header_read timeout values configured in ephpm-test.toml.
#[tokio::test]
async fn static_file_served_with_timeout_config() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "static file must be served with explicit timeout config, got {}",
        resp.status()
    );
}

/// Verify PHP execution completes within the configured timeouts.
/// A fast PHP script should not be affected by idle/header_read timeouts.
#[tokio::test]
async fn php_execution_within_timeouts() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "PHP request must succeed within configured timeouts, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        !body.is_empty(),
        "PHP response must not be empty"
    );
}

/// Verify that multiple sequential requests on the same connection succeed.
/// With idle timeout set to 45s, keep-alive connections should remain open
/// between rapid sequential requests.
#[tokio::test]
async fn sequential_requests_within_idle_timeout() {
    let base_url = required_env("EPHPM_URL");

    // Use a single client to exercise keep-alive connections.
    let client = reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build reqwest client");

    for i in 0..5 {
        let url = format!("{base_url}/test.html");
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("request {i}: GET {url} failed: {e}"));

        assert_eq!(
            resp.status().as_u16(),
            200,
            "request {i}: sequential keep-alive request must succeed, got {}",
            resp.status()
        );

        // Consume the body to release the connection back to the pool.
        let _ = resp.bytes().await;
    }
}

/// Verify that a POST request with headers and body completes within the
/// header_read timeout. The configured header_read timeout (20s) is well
/// above what a normal request needs.
#[tokio::test]
async fn post_request_within_header_read_timeout() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("timeout_test=yes")
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "POST must complete within header_read timeout, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("timeout_test = yes"),
        "POST body must reach PHP within timeout config:\n{body}"
    );
}

/// Verify that the server handles concurrent requests correctly with
/// timeout config active. Timeouts should not interfere with parallel
/// request processing.
#[tokio::test]
async fn concurrent_requests_with_timeout_config() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    let mut handles = Vec::new();
    for i in 0..10 {
        let url = format!("{base_url}/test.html");
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let resp = c
                .get(&url)
                .send()
                .await
                .unwrap_or_else(|e| panic!("concurrent request {i}: GET {url} failed: {e}"));
            (i, resp.status().as_u16())
        }));
    }

    for handle in handles {
        let (i, status) = handle.await.expect("task panicked");
        assert_eq!(
            status, 200,
            "concurrent request {i}: must succeed with timeout config, got {status}"
        );
    }
}

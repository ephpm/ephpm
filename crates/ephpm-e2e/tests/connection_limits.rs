//! Connection limit smoke tests.
//!
//! Validates that the `[server.limits] max_connections` setting is accepted
//! by the server without breaking normal request handling.
//!
//! The test config sets `max_connections = 100` — high enough that normal
//! traffic never hits the limit, but non-zero so the code path that
//! initialises the `Limiter` is exercised.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Fire 5 concurrent GET requests and confirm every one succeeds.
///
/// With `max_connections = 100` and only 5 in-flight requests, none should
/// be rejected. This proves the limiter initialises correctly and the
/// connection-slot bookkeeping (acquire + release) works without leaking
/// slots on normal request completion.
#[tokio::test]
async fn concurrent_requests_under_limit_all_succeed() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    let mut handles = Vec::new();
    for i in 0..5 {
        let url = format!("{base_url}/index.php?conn_test={i}");
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let resp = c
                .get(&url)
                .send()
                .await
                .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
            (i, resp.status().as_u16())
        }));
    }

    for handle in handles {
        let (i, status) = handle.await.expect("task panicked");
        assert_eq!(
            status, 200,
            "request {i} should succeed with 200 under the connection limit, got {status}"
        );
    }
}

/// Verify the server returns standard response headers even when the
/// connection limiter is active. This catches regressions where the limiter
/// middleware accidentally strips response headers.
#[tokio::test]
async fn response_headers_intact_with_limiter_active() {
    let base_url = required_env("EPHPM_URL");

    let resp = reqwest::get(format!("{base_url}/test.html"))
        .await
        .unwrap_or_else(|e| panic!("GET /test.html failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);

    // The test config sets X-Frame-Options: DENY in [server.response] headers.
    // If the limiter interferes with the response pipeline this will be missing.
    let xfo = resp
        .headers()
        .get("x-frame-options")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        xfo, "DENY",
        "X-Frame-Options must still be present when the connection limiter is active"
    );
}

/// Sequential requests succeed repeatedly, proving connection slots are
/// properly released after each response completes.
#[tokio::test]
async fn slots_released_after_response() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // 10 sequential requests — if slots leak, a low limit would eventually
    // reject. With max_connections = 100 we have headroom, but the pattern
    // still exercises the release path.
    for i in 0..10 {
        let url = format!("{base_url}/index.php?seq={i}");
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {url} (seq {i}) failed: {e}"));
        assert_eq!(
            resp.status().as_u16(),
            200,
            "sequential request {i} must succeed, got {}",
            resp.status()
        );
        // Consume the body so the connection is fully returned to the pool.
        let _ = resp.bytes().await;
    }
}

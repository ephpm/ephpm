//! KV Unix socket configuration smoke tests.
//!
//! Validates that ephpm starts correctly when a Unix socket path is configured
//! for the KV RESP listener. Since the e2e runner cannot access the Unix socket
//! on the ephpm pod, these tests verify:
//! - The server starts without errors when `[kv.redis_compat] socket` is set
//! - PHP SAPI KV functions still work (they use in-process DashMap, not the socket)
//! - Normal HTTP requests are unaffected by the socket configuration
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Helper: call /kv.php with the given query string, return trimmed body.
async fn kv(base_url: &str, query: &str) -> (u16, String) {
    let url = format!("{base_url}/kv.php?{query}");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .expect("failed to read kv response body")
        .trim()
        .to_owned();
    (status, body)
}

/// Verify the server starts and serves requests normally with a Unix socket
/// configured for the KV RESP listener. The socket config is in ephpm-test.toml
/// under `[kv.redis_compat] socket = "/tmp/ephpm-kv.sock"`.
#[tokio::test]
async fn server_starts_with_kv_unix_socket_config() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "server must start and serve index.php with KV Unix socket configured, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("Hello from ePHPm"),
        "index.php must produce expected output when KV socket is configured:\n{body}"
    );
}

/// Verify that PHP SAPI KV functions (ephpm_kv_set/get) work correctly when
/// a Unix socket is configured. These functions use the in-process DashMap
/// directly and should be completely unaffected by the RESP socket config.
#[tokio::test]
async fn kv_sapi_functions_work_with_unix_socket_config() {
    let base_url = required_env("EPHPM_URL");

    // Use a unique key to avoid collisions with other KV tests.
    let key = "unix_sock_test_key";
    let val = "socket_config_ok";

    // Set via PHP SAPI function
    let (status, body) = kv(
        &base_url,
        &format!("op=set&key={key}&val={val}&ttl=0"),
    )
    .await;
    assert_eq!(status, 200, "kv set must return 200");
    assert_eq!(body, "ok", "kv set must return 'ok'");

    // Get via PHP SAPI function
    let (status, body) = kv(&base_url, &format!("op=get&key={key}")).await;
    assert_eq!(status, 200, "kv get must return 200");
    assert_eq!(
        body, val,
        "kv get must return the value set via SAPI function"
    );

    // Verify exists
    let (status, body) = kv(&base_url, &format!("op=exists&key={key}")).await;
    assert_eq!(status, 200);
    assert_eq!(body, "1", "key must exist after set");

    // Cleanup
    kv(&base_url, &format!("op=del&key={key}")).await;
}

/// Verify that KV TTL expiry works correctly with a Unix socket configured.
/// This exercises the DashMap + expiry logic independently of the RESP listener.
#[tokio::test(flavor = "current_thread")]
async fn kv_ttl_works_with_unix_socket_config() {
    let base_url = required_env("EPHPM_URL");

    let key = "unix_sock_ttl_key";

    // ephpm_kv_set takes the TTL in seconds (Redis convention), so the
    // shortest expiry we can request is 1 s. Sleep slightly longer so
    // scheduler jitter can't make the test flaky.
    let (status, _) = kv(
        &base_url,
        &format!("op=set&key={key}&val=ephemeral&ttl=1"),
    )
    .await;
    assert_eq!(status, 200);

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // Key should be gone
    let (status, body) = kv(&base_url, &format!("op=get&key={key}")).await;
    assert_eq!(status, 200);
    assert_eq!(
        body, "null",
        "key must expire after TTL even with Unix socket configured"
    );
}

//! PHP error recovery tests.
//!
//! These are the highest-risk gap in the test suite: if the zend_try/zend_catch
//! wrapper misbehaves, PHP fatal errors cause the server to hang or SIGSEGV
//! rather than returning a 500.  These tests ensure the server survives every
//! PHP-level failure mode and keeps accepting new requests afterwards.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Issue a request, assert it returns 500, and assert the *next* request to a
/// good endpoint still works — confirming the server recovered cleanly.
async fn assert_fatal_returns_500_and_server_recovers(url: &str, label: &str) {
    let resp = reqwest::get(url)
        .await
        .unwrap_or_else(|e| panic!("{label}: GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        500,
        "{label}: expected 500, got {} — server may have crashed or swallowed the error",
        resp.status()
    );

    // Consume the body so the connection is released cleanly.
    let _ = resp.bytes().await;
}

async fn assert_server_still_alive(base_url: &str, label: &str) {
    let url = format!("{base_url}/index.php");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("{label}: recovery check GET {url} failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{label}: server must still accept requests after PHP error, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn php_fatal_error_returns_500() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/fatal_error.php");

    assert_fatal_returns_500_and_server_recovers(&url, "fatal_error").await;
    assert_server_still_alive(&base_url, "fatal_error").await;
}

#[tokio::test]
async fn php_memory_limit_exceeded_returns_500() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/memory_hog.php");

    assert_fatal_returns_500_and_server_recovers(&url, "memory_limit").await;
    assert_server_still_alive(&base_url, "memory_limit").await;
}

#[tokio::test]
async fn php_syntax_error_returns_500() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/syntax_error.php");

    assert_fatal_returns_500_and_server_recovers(&url, "syntax_error").await;
    assert_server_still_alive(&base_url, "syntax_error").await;
}

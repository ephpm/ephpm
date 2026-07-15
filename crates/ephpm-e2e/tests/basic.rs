//! Basic ephpm integration tests.
//!
//! Validates core HTTP functionality:
//! - 404 responses for missing files
//! - PHP rendering for .php files
//! - Static file serving for .html files
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn missing_file_returns_404() {
    let base_url = required_env("EPHPM_URL");

    let url = format!("{base_url}/nonexistent.txt");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        404,
        "expected 404 for missing file, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn php_renders_correctly() {
    let base_url = required_env("EPHPM_URL");

    let url = format!("{base_url}/index.php");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from /index.php, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read response body");

    // Verify PHP code was executed
    assert!(
        body.contains("Hello from ePHPm"),
        "expected PHP output, got:\n{body}"
    );
}

#[tokio::test]
async fn static_file_serving() {
    let base_url = required_env("EPHPM_URL");

    let url = format!("{base_url}/test.html");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 for static file, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read response body");

    // Verify static content
    assert!(
        body.contains("Static Works"),
        "expected static HTML content, got:\n{body}"
    );
}

#[tokio::test]
async fn ob_buffer_and_shutdown_output_are_captured() {
    // Regression: fpm-mode capture ran before PHP flushed open ob_ buffers
    // or ran shutdown functions, so `ob_start(); echo ...` returned an
    // empty body and WordPress 7.0 rendered every page as 0 bytes.
    let base_url = required_env("EPHPM_URL");

    let url = format!("{base_url}/ob_shutdown.php");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read response body");
    assert!(
        body.contains("OB_BODY_"),
        "unclosed ob_start() content missing from body: {body:?}"
    );
    assert!(
        body.contains("SHUTDOWN_RAN"),
        "register_shutdown_function output missing from body: {body:?}"
    );
    assert!(
        body.contains("OB_BODY_SHUTDOWN_RAN"),
        "shutdown output must come after buffered body: {body:?}"
    );
}

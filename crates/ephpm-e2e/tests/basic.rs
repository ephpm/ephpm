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

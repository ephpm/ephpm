//! Compression threshold tests.
//!
//! Validates:
//! - Responses smaller than `compression_min_size` are NOT compressed
//! - Responses larger than `compression_min_size` ARE compressed
//!
//! The test config sets `compression_min_size = 1024` (1 KiB). reqwest is
//! configured with `default-features = false` so it does NOT auto-decompress,
//! meaning we receive raw bytes and can inspect `Content-Encoding` directly.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn small_response_is_not_compressed() {
    let base_url = required_env("EPHPM_URL");
    // test.html is a small static file (well under 1 KiB) — should not be compressed.
    let url = format!("{base_url}/test.html");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from /test.html, got {}",
        resp.status()
    );

    let encoding = resp
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        encoding.is_empty(),
        "small response (under compression_min_size=1024) must NOT have Content-Encoding, got: {encoding:?}"
    );
}

#[tokio::test]
async fn large_response_is_compressed() {
    let base_url = required_env("EPHPM_URL");
    // info.php generates a large phpinfo() page — well above 1 KiB.
    let url = format!("{base_url}/info.php");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from /info.php, got {}",
        resp.status()
    );

    let encoding = resp
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        encoding.contains("gzip"),
        "large response (above compression_min_size=1024) must have Content-Encoding: gzip, got: {encoding:?}"
    );

    let body = resp.bytes().await.expect("failed to read compressed body");
    assert!(
        !body.is_empty(),
        "compressed response body must not be empty"
    );
}

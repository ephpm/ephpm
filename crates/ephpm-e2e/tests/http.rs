//! HTTP protocol correctness tests.
//!
//! Validates:
//! - HEAD returns headers but no body
//! - POST body is parsed by PHP
//! - Static files get the correct Content-Type
//! - ETag / 304 Not Modified round-trip
//! - Gzip compression negotiation
//! - 413 Payload Too Large for oversized bodies
//! - Cache-Control header on static files
//! - X-Forwarded-For forwarded to PHP as $_SERVER var
//! - Fallback chain resolves / to index.php
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn head_request_has_no_body() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    let client = reqwest::Client::new();
    let resp = client
        .head(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("HEAD {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from HEAD /test.html, got {}",
        resp.status()
    );
    assert!(
        resp.headers().contains_key("content-length"),
        "HEAD response must include Content-Length header"
    );

    let body = resp.bytes().await.expect("failed to read HEAD body");
    assert!(
        body.is_empty(),
        "HEAD response body must be empty, got {} bytes",
        body.len()
    );
}

#[tokio::test]
async fn post_body_reaches_php() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("username=alice&score=42")
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from POST /test.php, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read response body");
    assert!(
        body.contains("username = alice"),
        "expected $_POST['username'] = alice in body:\n{body}"
    );
    assert!(
        body.contains("score = 42"),
        "expected $_POST['score'] = 42 in body:\n{body}"
    );
}

#[tokio::test]
async fn content_type_for_static_files() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    let url = format!("{base_url}/test.css");
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200, "expected 200 for test.css");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/css"),
        "expected text/css Content-Type for .css file, got: {ct}"
    );

    let url = format!("{base_url}/test.js");
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200, "expected 200 for test.js");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("javascript"),
        "expected application/javascript Content-Type for .js file, got: {ct}"
    );
}

#[tokio::test]
async fn etag_304_not_modified() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");
    let client = reqwest::Client::new();

    // First request — capture ETag
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200);
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .expect("server must return an ETag header for static files")
        .to_owned();

    // Second request with If-None-Match — expect 304
    let resp = client
        .get(&url)
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap_or_else(|e| panic!("conditional GET {url} failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        304,
        "expected 304 Not Modified with matching ETag '{etag}', got {}",
        resp.status()
    );
    let body = resp.bytes().await.expect("failed to read 304 body");
    assert!(
        body.is_empty(),
        "304 response must have an empty body, got {} bytes",
        body.len()
    );
}

#[tokio::test]
async fn gzip_response_is_compressed() {
    let base_url = required_env("EPHPM_URL");
    // info.php generates a large phpinfo() page — well above the 1 KiB compression threshold
    let url = format!("{base_url}/info.php");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let encoding = resp
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        encoding.contains("gzip"),
        "expected Content-Encoding: gzip when Accept-Encoding: gzip was sent, got: {encoding:?}"
    );
    // Body is raw compressed bytes — non-empty confirms data was sent
    let body = resp.bytes().await.expect("failed to read compressed body");
    assert!(!body.is_empty(), "compressed response body must not be empty");
}

#[tokio::test]
async fn request_body_too_large_returns_413() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    // Test config sets max_body_size = 1024; send ~2 KiB
    let big_body = "x=".to_owned() + &"a".repeat(2000);

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(big_body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        413,
        "expected 413 Payload Too Large for body exceeding max_body_size, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn cache_control_present_on_static_files() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let cc = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        !cc.is_empty(),
        "static files must include a Cache-Control header (set via [server.static] cache_control)"
    );
}

#[tokio::test]
async fn x_forwarded_for_header_reaches_php() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("X-Forwarded-For", "203.0.113.1")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("HTTP_X_FORWARDED_FOR = 203.0.113.1"),
        "X-Forwarded-For must be forwarded to PHP as HTTP_X_FORWARDED_FOR in $_SERVER:\n{body}"
    );
}

#[tokio::test]
async fn fallback_chain_serves_index_php() {
    let base_url = required_env("EPHPM_URL");
    // GET / — no file name; fallback chain must resolve to index.php
    let url = format!("{base_url}/");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "GET / must resolve via fallback chain to index.php, got {}",
        resp.status()
    );
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("Hello from ePHPm"),
        "fallback chain must serve index.php output for /:\n{body}"
    );
}

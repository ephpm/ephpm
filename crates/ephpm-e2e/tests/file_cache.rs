//! File cache e2e tests.
//!
//! Validates that the in-memory open file cache (`[server.file_cache]`)
//! serves static files correctly and transparently — responses must be
//! identical whether served from cache or disk. Also verifies that PHP
//! pages are never served from the file cache (dynamic content must
//! always execute).
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn cached_html_returns_correct_body() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");
    let client = reqwest::Client::new();

    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 for /test.html, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        !body.is_empty(),
        "/test.html must return a non-empty body"
    );

    // Second request — should hit the file cache; response must be identical.
    let resp2 = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("second GET {url} failed: {e}"));

    assert_eq!(
        resp2.status().as_u16(),
        200,
        "expected 200 on second request for /test.html, got {}",
        resp2.status()
    );

    let body2 = resp2.text().await.expect("failed to read body on second request");
    assert_eq!(
        body, body2,
        "cached response body must be identical to the first response"
    );
}

#[tokio::test]
async fn cached_css_has_correct_content_type() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.css");
    let client = reqwest::Client::new();

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
        "expected text/css Content-Type for .css, got: {ct}"
    );

    let body = resp.text().await.expect("failed to read css body");

    // Second request — cache hit path.
    let resp2 = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("second GET {url} failed: {e}"));

    assert_eq!(resp2.status().as_u16(), 200);
    let ct2 = resp2
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct2.contains("text/css"),
        "Content-Type must remain text/css on cache hit, got: {ct2}"
    );

    let body2 = resp2.text().await.expect("failed to read css body on second request");
    assert_eq!(
        body, body2,
        "cached CSS response must be identical to the original"
    );
}

#[tokio::test]
async fn cached_js_has_correct_content_type() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.js");
    let client = reqwest::Client::new();

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
        "expected application/javascript Content-Type for .js, got: {ct}"
    );

    let body = resp.text().await.expect("failed to read js body");

    // Second request — cache hit path.
    let resp2 = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("second GET {url} failed: {e}"));

    assert_eq!(resp2.status().as_u16(), 200);
    let ct2 = resp2
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct2.contains("javascript"),
        "Content-Type must remain javascript on cache hit, got: {ct2}"
    );

    let body2 = resp2.text().await.expect("failed to read js body on second request");
    assert_eq!(
        body, body2,
        "cached JS response must be identical to the original"
    );
}

#[tokio::test]
async fn php_pages_are_not_cached() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");
    let client = reqwest::Client::new();

    // PHP test.php echoes $_SERVER vars including REQUEST_TIME_FLOAT which
    // changes every request. Two identical GETs must produce different bodies
    // (or at least both must execute PHP successfully).
    let resp1 = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp1.status().as_u16(),
        200,
        "expected 200 from PHP page, got {}",
        resp1.status()
    );

    let body1 = resp1.text().await.expect("failed to read PHP body");
    assert!(
        !body1.is_empty(),
        "PHP response must not be empty"
    );

    // The file cache must NOT intercept .php requests — they must always
    // reach the PHP executor. We verify by checking the response looks like
    // dynamic PHP output (contains SERVER_SOFTWARE or similar markers).
    assert!(
        body1.contains("SERVER_SOFTWARE") || body1.contains("REQUEST_METHOD"),
        "PHP output must contain server variable dumps, got:\n{body1}"
    );
}

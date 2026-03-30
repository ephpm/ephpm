//! PHP ETag cache tests.
//!
//! Validates the PHP ETag cache feature (`[server.php_etag_cache]`):
//! - First PHP request returns 200 with an ETag header
//! - Repeat request with `If-None-Match` returns 304 (served from cache, no PHP)
//! - Mismatched ETag returns 200 (full response)
//! - POST requests bypass the cache
//! - Requests without `If-None-Match` always return 200
//! - Different query strings get independent cache entries
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)
//!
//! Requires `server.php_etag_cache.enabled = true` in the ephpm configuration.

use ephpm_e2e::required_env;

#[tokio::test]
async fn php_etag_first_request_returns_200_with_etag() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/etag_test.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "first request to PHP endpoint must return 200, got {}",
        resp.status()
    );

    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok());
    assert!(
        etag.is_some(),
        "PHP response must include an ETag header"
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("ETag test content"),
        "response body must contain expected PHP output, got: {body}"
    );
}

#[tokio::test]
async fn php_etag_matching_returns_304() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/etag_test.php");
    let client = reqwest::Client::new();

    // First request — get the ETag
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
        .expect("PHP response must include an ETag header")
        .to_owned();

    // Second request with matching If-None-Match — expect 304
    let resp = client
        .get(&url)
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap_or_else(|e| panic!("conditional GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        304,
        "request with matching ETag must return 304, got {} (etag: {etag})",
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
async fn php_etag_mismatched_returns_200() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/etag_test.php");
    let client = reqwest::Client::new();

    // Prime the cache
    let _ = client.get(&url).send().await;

    // Send a wrong ETag — expect full 200 response
    let resp = client
        .get(&url)
        .header("If-None-Match", "\"wrong-etag-value\"")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "request with mismatched ETag must return 200, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("ETag test content"),
        "mismatched ETag must return full PHP output, got: {body}"
    );
}

#[tokio::test]
async fn php_etag_post_requests_not_cached() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/etag_test.php");
    let client = reqwest::Client::new();

    // Prime the cache with a GET
    let resp = client.get(&url).send().await.unwrap();
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .expect("must have ETag")
        .to_owned();

    // POST with matching ETag — must NOT get 304 (POST is not cacheable)
    let resp = client
        .post(&url)
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "POST requests must not be served from ETag cache, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("ETag test content"),
        "POST must execute PHP and return full output, got: {body}"
    );
}

#[tokio::test]
async fn php_etag_no_if_none_match_returns_200() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/etag_test.php");
    let client = reqwest::Client::new();

    // Prime the cache
    let _ = client.get(&url).send().await;

    // Request without If-None-Match — must always return 200
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "request without If-None-Match must return 200 even when cache is primed, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn php_etag_different_query_strings_independent() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // GET with ?v=1 — cache the ETag
    let url_v1 = format!("{base_url}/etag_test.php?v=1");
    let resp = client.get(&url_v1).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let etag_v1 = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .expect("must have ETag")
        .to_owned();

    // Same ETag against a different query string — must NOT match
    let url_v2 = format!("{base_url}/etag_test.php?v=2");
    let resp = client
        .get(&url_v2)
        .header("If-None-Match", &etag_v1)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url_v2} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "ETag from ?v=1 must not match cache for ?v=2, got {}",
        resp.status()
    );
}

//! HTTP edge-case tests.
//!
//! Validates:
//! - Percent-encoded paths resolve correctly
//! - HEAD on static files includes Content-Length with empty body
//! - Very long query strings (~4KB) are accepted
//! - Multiple query parameters are preserved and reach PHP
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn percent_encoded_path_resolves() {
    let base_url = required_env("EPHPM_URL");
    // %2E = '.' — /test%2Ehtml should resolve to /test.html
    let url = format!("{base_url}/test%2Ehtml");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "percent-encoded path /test%%2Ehtml must resolve to /test.html, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        !body.is_empty(),
        "resolved static file must have content"
    );
}

#[tokio::test]
async fn head_static_file_has_content_length_empty_body() {
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
        "HEAD on static file must return 200, got {}",
        resp.status()
    );

    let cl = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    assert!(
        cl.is_some_and(|len| len > 0),
        "HEAD response must include a non-zero Content-Length, got {:?}",
        cl
    );

    let body = resp.bytes().await.expect("failed to read HEAD body");
    assert!(
        body.is_empty(),
        "HEAD response body must be empty, got {} bytes",
        body.len()
    );
}

#[tokio::test]
async fn long_query_string_accepted() {
    let base_url = required_env("EPHPM_URL");
    // Build a ~4KB query string
    let long_value = "x".repeat(4000);
    let url = format!("{base_url}/test.php?data={long_value}");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET with ~4KB query failed: {e}"));

    let status = resp.status().as_u16();
    assert!(
        status == 200,
        "~4KB query string should be accepted (200), got {status}"
    );

    let body = resp.text().await.expect("failed to read body");
    // test.php prints GET params — verify the long value arrived
    assert!(
        body.contains(&long_value[..40]),
        "PHP must receive the long query string value, but body does not contain it"
    );
}

#[tokio::test]
async fn multiple_query_params_preserved() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php?alpha=one&beta=two&gamma=three");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");

    assert!(
        body.contains("alpha = one"),
        "query param 'alpha' must reach PHP $_GET:\n{body}"
    );
    assert!(
        body.contains("beta = two"),
        "query param 'beta' must reach PHP $_GET:\n{body}"
    );
    assert!(
        body.contains("gamma = three"),
        "query param 'gamma' must reach PHP $_GET:\n{body}"
    );
}

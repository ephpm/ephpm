//! Extended PHP behaviour tests.
//!
//! Validates:
//! - Empty PHP output returns 200
//! - JSON content-type is propagated correctly
//! - Multiple Set-Cookie headers are preserved
//! - SERVER_SOFTWARE contains "ephpm"
//! - REQUEST_METHOD is correct for PUT and DELETE
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn empty_php_output_returns_200() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/empty.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "PHP script with no output must return 200, got {}",
        resp.status()
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.is_empty(),
        "empty.php must produce no output, got {} bytes: {body:?}",
        body.len()
    );
}

#[tokio::test]
async fn json_content_type_propagated() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/json_response.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/json"),
        "Content-Type must be application/json, got: {ct}"
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("\"status\":\"ok\"") || body.contains("\"status\": \"ok\""),
        "JSON body must contain expected data, got: {body}"
    );
}

#[tokio::test]
async fn multiple_set_cookie_headers_preserved() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/multi_cookie.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);

    let cookies: Vec<String> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(String::from)
        .collect();

    assert!(
        cookies.len() >= 3,
        "expected at least 3 Set-Cookie headers (session, theme, lang), got {}: {cookies:?}",
        cookies.len()
    );

    let all = cookies.join(" | ");
    assert!(
        all.contains("session=abc123"),
        "must include session cookie: {all}"
    );
    assert!(
        all.contains("theme=dark"),
        "must include theme cookie: {all}"
    );
    assert!(
        all.contains("lang=en"),
        "must include lang cookie: {all}"
    );
}

#[tokio::test]
async fn server_software_contains_ephpm() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/server_test.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");

    // server_test.php dumps $_SERVER — check that SERVER_SOFTWARE is present
    // and contains "ephpm". The exact format depends on the SAPI registration.
    let has_software = body.lines().any(|line| {
        line.contains("SERVER_SOFTWARE") && line.to_ascii_lowercase().contains("ephpm")
    });
    assert!(
        has_software,
        "$_SERVER['SERVER_SOFTWARE'] must contain 'ephpm':\n{body}"
    );
}

#[tokio::test]
async fn request_method_put() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    let client = reqwest::Client::new();
    let resp = client
        .put(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("PUT {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("REQUEST_METHOD: PUT"),
        "$_SERVER['REQUEST_METHOD'] must be PUT:\n{body}"
    );
}

#[tokio::test]
async fn request_method_delete() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    let client = reqwest::Client::new();
    let resp = client
        .delete(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("DELETE {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("REQUEST_METHOD: DELETE"),
        "$_SERVER['REQUEST_METHOD'] must be DELETE:\n{body}"
    );
}

//! PHP execution correctness tests.
//!
//! Validates:
//! - $_GET is populated from the query string
//! - Critical $_SERVER variables are set and non-empty
//! - echo before exit(0) is delivered to the client
//! - http_response_code() propagates to the HTTP status line
//! - Cookie header populates $_COOKIE
//! - php://input is readable for non-form POST bodies
//! - PHP header() calls appear in the HTTP response
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn query_string_available() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php?foo=bar&baz=qux");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("foo = bar"),
        "expected $_GET['foo'] = bar in body:\n{body}"
    );
    assert!(
        body.contains("baz = qux"),
        "expected $_GET['baz'] = qux in body:\n{body}"
    );
}

#[tokio::test]
async fn server_vars_populated() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");

    assert!(
        body.contains("REQUEST_METHOD: GET"),
        "$_SERVER['REQUEST_METHOD'] not set correctly:\n{body}"
    );
    assert!(
        body.contains("REQUEST_URI: /test.php"),
        "$_SERVER['REQUEST_URI'] not set correctly:\n{body}"
    );
    assert!(
        body.contains("DOCUMENT_ROOT: /var/www/html"),
        "$_SERVER['DOCUMENT_ROOT'] not set correctly:\n{body}"
    );

    let has_remote_addr = body.lines().any(|line| {
        line.starts_with("REMOTE_ADDR:") && line.len() > "REMOTE_ADDR: ".len()
    });
    assert!(
        has_remote_addr,
        "$_SERVER['REMOTE_ADDR'] missing or empty:\n{body}"
    );
}

#[tokio::test]
async fn php_exit_returns_output() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/exit_test.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "server must not crash when PHP calls exit(0), got {}",
        resp.status()
    );
    let body = resp.text().await.expect("failed to read body");
    assert!(
        body.contains("bye"),
        "output before exit(0) must be delivered to client, got:\n{body}"
    );
}

#[tokio::test]
async fn php_sets_custom_status() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/status_201.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        201,
        "http_response_code(201) must propagate to HTTP status line, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn cookie_header_populates_cookie_superglobal() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/server_test.php");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Cookie", "session=abc123; user=alice")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");
    // server_test.php calls var_export($_COOKIE)
    assert!(
        body.contains("'session' => 'abc123'"),
        "Cookie header must populate $_COOKIE['session']:\n{body}"
    );
    assert!(
        body.contains("'user' => 'alice'"),
        "Cookie header must populate $_COOKIE['user']:\n{body}"
    );
}

#[tokio::test]
async fn php_input_stream_readable() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/server_test.php");
    let payload = r#"{"action":"test","value":42}"#;

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("failed to read body");
    // server_test.php echoes file_get_contents('php://input')
    assert!(
        body.contains(payload),
        "php://input must contain the raw request body for non-form Content-Types:\n{body}"
    );
    // Non-form body must NOT be parsed into $_POST
    assert!(
        !body.contains("'action'"),
        "application/json body must not be parsed into $_POST:\n{body}"
    );
}

#[tokio::test]
async fn custom_response_header_reaches_client() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/custom_header.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    let val = resp
        .headers()
        .get("x-custom")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        val, "ok",
        "PHP header('X-Custom: ok') must appear in the HTTP response"
    );
}

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
    // DOCUMENT_ROOT is whatever `[server] document_root` points at, which
    // differs by environment: `/var/www/html` in the container image, the
    // repo's `tests/docroot` under the bare-process harness. The harness that
    // knows the path exports it as EXPECTED_DOCUMENT_ROOT; default to the
    // container path so the Kind harness (which does not set it) still passes.
    // Rather than assert an exact string (brittle across environments) we only
    // require the line to be present and non-empty, then match the expected
    // value when the harness provides it.
    let expected_docroot = std::env::var("EXPECTED_DOCUMENT_ROOT")
        .unwrap_or_else(|_| "/var/www/html".to_string());
    let docroot_line = body
        .lines()
        .find_map(|line| line.strip_prefix("DOCUMENT_ROOT: "))
        .unwrap_or_else(|| panic!("$_SERVER['DOCUMENT_ROOT'] line missing:\n{body}"));
    assert!(
        !docroot_line.trim().is_empty(),
        "$_SERVER['DOCUMENT_ROOT'] is empty:\n{body}"
    );
    assert_eq!(
        docroot_line.trim(),
        expected_docroot.trim_end_matches('/'),
        "$_SERVER['DOCUMENT_ROOT'] mismatch (expected {expected_docroot}):\n{body}"
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

// ── HTTP authentication $_SERVER variables (issue #386) ──────────────────
//
// `auth_vars.php` prints each key as either `NAME = [value]` or `NAME unset`,
// so these tests can tell "absent" from "present but empty" — a distinction
// php-src makes and applications branch on.

/// Fetch `auth_vars.php` with an explicit Authorization header. The header is
/// set verbatim rather than via `basic_auth()` so the exact bytes on the wire
/// are what the test says they are.
async fn auth_vars(header: Option<&str>) -> String {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/auth_vars.php");

    let mut req = reqwest::Client::new().get(&url);
    if let Some(value) = header {
        req = req.header("Authorization", value);
    }
    let resp = req
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(resp.status().as_u16(), 200);
    resp.text().await.expect("failed to read body")
}

/// The headline compatibility gap: an app reading `$_SERVER['PHP_AUTH_USER']`
/// must see the decoded credential, as it would under mod_php or PHP-FPM.
#[tokio::test]
async fn basic_auth_populates_php_auth_vars() {
    // base64("alice:s3cret")
    let body = auth_vars(Some("Basic YWxpY2U6czNjcmV0")).await;

    assert!(
        body.contains("PHP_AUTH_USER = [alice]"),
        "PHP_AUTH_USER must carry the decoded username:\n{body}"
    );
    assert!(
        body.contains("PHP_AUTH_PW = [s3cret]"),
        "PHP_AUTH_PW must carry the decoded password:\n{body}"
    );
    assert!(
        body.contains("AUTH_TYPE = [Basic]"),
        "AUTH_TYPE must report the scheme:\n{body}"
    );
    // The raw header stays available, as on stock SAPIs.
    assert!(
        body.contains("HTTP_AUTHORIZATION = [Basic YWxpY2U6czNjcmV0]"),
        "HTTP_AUTHORIZATION must survive alongside the derived vars:\n{body}"
    );
}

/// php-src registers PHP_AUTH_PW only for a non-empty password, so `bob:`
/// leaves the key unset rather than empty.
#[tokio::test]
async fn empty_password_leaves_php_auth_pw_unset() {
    // base64("bob:")
    let body = auth_vars(Some("Basic Ym9iOg==")).await;

    assert!(
        body.contains("PHP_AUTH_USER = [bob]"),
        "username must still be registered:\n{body}"
    );
    assert!(
        body.contains("PHP_AUTH_PW unset"),
        "an empty password must leave PHP_AUTH_PW unset, not empty:\n{body}"
    );
}

/// Non-Basic schemes get an AUTH_TYPE and nothing else; the token itself stays
/// readable through HTTP_AUTHORIZATION.
#[tokio::test]
async fn bearer_sets_auth_type_only() {
    let body = auth_vars(Some("Bearer some.jwt.token")).await;

    assert!(
        body.contains("AUTH_TYPE = [Bearer]"),
        "AUTH_TYPE must report a non-Basic scheme:\n{body}"
    );
    assert!(
        body.contains("PHP_AUTH_USER unset"),
        "Bearer must not produce PHP_AUTH_USER:\n{body}"
    );
    assert!(
        body.contains("PHP_AUTH_PW unset"),
        "Bearer must not produce PHP_AUTH_PW:\n{body}"
    );
}

/// Without an Authorization header none of the keys exist, so
/// `isset($_SERVER['PHP_AUTH_USER'])` is false — the check every HTTP Basic
/// challenge loop starts with.
#[tokio::test]
async fn no_authorization_header_leaves_all_auth_vars_unset() {
    let body = auth_vars(None).await;

    for key in ["AUTH_TYPE", "PHP_AUTH_USER", "PHP_AUTH_PW", "PHP_AUTH_DIGEST"] {
        assert!(
            body.contains(&format!("{key} unset")),
            "{key} must be absent without an Authorization header:\n{body}"
        );
    }
}

/// REMOTE_USER denotes a user the *server* authenticated. ePHPm validates
/// nothing here, so it must never be synthesised from a client-supplied
/// header — an app trusting it would otherwise accept any asserted identity.
#[tokio::test]
async fn remote_user_is_never_derived_from_the_header() {
    // base64("admin:hunter2")
    let body = auth_vars(Some("Basic YWRtaW46aHVudGVyMg==")).await;

    assert!(
        body.contains("REMOTE_USER unset"),
        "REMOTE_USER must never be derived from an unvalidated header:\n{body}"
    );
    assert!(
        body.contains("PHP_AUTH_USER = [admin]"),
        "sanity: the credential itself is still exposed:\n{body}"
    );
}

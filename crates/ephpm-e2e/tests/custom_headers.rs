//! Custom response headers tests.
//!
//! Validates that headers configured under `[server.response] headers`
//! are present on every response — both static files and PHP pages.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Headers configured in `tests/ephpm-test.toml` under `[server.response]`.
const EXPECTED_HEADERS: &[(&str, &str)] = &[
    ("x-frame-options", "DENY"),
    ("x-content-type-options", "nosniff"),
    (
        "strict-transport-security",
        "max-age=31536000; includeSubDomains",
    ),
];

#[tokio::test]
async fn custom_headers_on_static_file() {
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
        "expected 200 from GET /test.html, got {}",
        resp.status()
    );

    for (name, expected_value) in EXPECTED_HEADERS {
        let actual = resp
            .headers()
            .get(*name)
            .unwrap_or_else(|| panic!("missing custom response header '{name}' on static file"))
            .to_str()
            .unwrap_or_else(|e| panic!("header '{name}' has non-ASCII value: {e}"));
        assert_eq!(
            actual, *expected_value,
            "header '{name}' value mismatch on static file: expected '{expected_value}', got '{actual}'"
        );
    }
}

#[tokio::test]
async fn custom_headers_on_php_page() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 from GET /index.php, got {}",
        resp.status()
    );

    for (name, expected_value) in EXPECTED_HEADERS {
        let actual = resp
            .headers()
            .get(*name)
            .unwrap_or_else(|| panic!("missing custom response header '{name}' on PHP page"))
            .to_str()
            .unwrap_or_else(|e| panic!("header '{name}' has non-ASCII value: {e}"));
        assert_eq!(
            actual, *expected_value,
            "header '{name}' value mismatch on PHP page: expected '{expected_value}', got '{actual}'"
        );
    }
}

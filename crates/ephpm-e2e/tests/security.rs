//! Path and access control tests.
//!
//! Validates:
//! - Hidden files (dot-files) are blocked with 403
//! - PHP source is never served as plain text
//! - blocked_paths glob patterns return 403
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn dotfile_returns_403() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/.env");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        403,
        "hidden dot-files must be blocked with 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn php_source_not_exposed() {
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
    assert!(
        !body.contains("<?php"),
        "PHP source must not appear in response — server leaked raw .php file:\n{body}"
    );
}

#[tokio::test]
async fn blocked_path_pattern_returns_403() {
    let base_url = required_env("EPHPM_URL");
    // Test config sets blocked_paths = ["vendor/*"]; vendor/secret.php exists in docroot
    let url = format!("{base_url}/vendor/secret.php");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        403,
        "paths matching blocked_paths glob must return 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn path_traversal_is_blocked() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // URL-encoded traversal sequences that naive servers decode after resolving
    // the path, allowing escape from the docroot.
    let traversal_paths = [
        "/%2e%2e/etc/passwd",
        "/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/%2e%2e%2fetc%2fpasswd",
    ];

    for path in &traversal_paths {
        let url = format!("{base_url}{path}");
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

        let status = resp.status().as_u16();
        assert_ne!(
            status, 200,
            "path traversal '{path}' must not return 200"
        );

        let body = resp.text().await.expect("failed to read body");
        assert!(
            !body.contains("root:x:"),
            "path traversal '{path}' must not expose system file contents"
        );
    }
}

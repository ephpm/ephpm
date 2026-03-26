//! Validate that ephpm is serving PHP and reports the expected version.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)
//! - `EXPECTED_PHP_VERSION` — major.minor version to assert (e.g. `8.5`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn php_version_matches() {
    let base_url = required_env("EPHPM_URL");
    let expected_version = required_env("EXPECTED_PHP_VERSION");

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

    // Validate PHP version (e.g. "PHP Version: 8.5")
    let version_marker = format!("PHP Version: {expected_version}");
    assert!(
        body.contains(&version_marker),
        "expected body to contain \"{version_marker}\", got:\n{body}"
    );

    // Validate embedded SAPI
    assert!(
        body.contains("Server API: embed"),
        "expected embedded SAPI, got:\n{body}"
    );
}

#[tokio::test]
async fn health_check() {
    let base_url = required_env("EPHPM_URL");

    // Static file serving should work too
    let resp = reqwest::get(format!("{base_url}/"))
        .await
        .expect("GET / failed");

    // Should get some response (200 for index.php, or a listing)
    assert!(
        resp.status().is_success(),
        "expected success from /, got {}",
        resp.status()
    );
}

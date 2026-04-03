//! Hidden file edge-case tests.
//!
//! Validates:
//! - Hidden directory segments (e.g. `/.hidden/`) are blocked with 403
//! - `..` path traversal segments are not mistakenly treated as hidden files
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn hidden_directory_blocked() {
    let base_url = required_env("EPHPM_URL");
    // A request targeting a hidden directory segment must return 403,
    // regardless of whether the directory actually exists on disk.
    let url = format!("{base_url}/.hidden/file.txt");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        403,
        "hidden directory segment /.hidden/ must be blocked with 403, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn dot_dot_not_treated_as_hidden() {
    let base_url = required_env("EPHPM_URL");
    // /subdir/../test.html should resolve to /test.html via path normalization.
    // The ".." segment must NOT be treated as a hidden file (dot-file).
    let url = format!("{base_url}/subdir/../test.html");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    let status = resp.status().as_u16();
    assert_ne!(
        status, 403,
        "path with '..' must not be treated as a hidden file and return 403 — \
         '..' is traversal, not a dot-file"
    );
    // It should either resolve to test.html (200) or be rejected as traversal
    // (400/404). The key assertion is that it is NOT 403 "hidden file".
}

//! Hidden file edge-case tests for `.well-known` paths.
//!
//! Validates:
//! - `/.well-known/acme-challenge/test` is blocked by hidden-file rules
//!   because `.well-known` starts with a dot and ephpm has no special
//!   exception for it (unlike nginx/Apache which commonly allow it)
//! - `/.config/settings` is blocked as a hidden directory
//!
//! The default `hidden_files` mode is "deny" (403). These tests document
//! current behavior and verify the deny logic covers common dot-prefixed
//! paths that other servers sometimes exempt.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn well_known_path_blocked_by_hidden_files() {
    let base_url = required_env("EPHPM_URL");
    // `.well-known` is a dot-prefixed segment. ephpm does NOT have a special
    // exception for RFC 8615 well-known URIs — they are blocked like any
    // other hidden path when hidden_files = "deny" (the default).
    let url = format!("{base_url}/.well-known/acme-challenge/test-token");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        403,
        "/.well-known/ must be blocked by hidden-file rules (no exception exists), got {}",
        resp.status()
    );
}

#[tokio::test]
async fn hidden_config_directory_blocked() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/.config/settings");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        403,
        "/.config/ must be blocked by hidden-file rules, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn nested_hidden_segment_blocked() {
    let base_url = required_env("EPHPM_URL");
    // Hidden segment deep in the path — verifies the check inspects all segments.
    let url = format!("{base_url}/assets/.secret/logo.png");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        403,
        "nested hidden segment /assets/.secret/ must be blocked, got {}",
        resp.status()
    );
}

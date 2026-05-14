//! Hidden file edge-case tests for `.well-known` paths.
//!
//! Validates:
//! - `/.well-known/acme-challenge/<token>` is handled by the built-in ACME
//!   responder (404 when the token isn't registered) instead of being
//!   blocked by the generic hidden-file rule. ephpm intentionally exempts
//!   this path so Let's Encrypt HTTP-01 challenges work in clustered mode.
//! - `/.config/settings` is blocked as a hidden directory (403).
//!
//! The default `hidden_files` mode is "deny" (403). These tests document
//! that the deny logic still covers other dot-prefixed paths.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn well_known_acme_challenge_served_by_acme_handler() {
    let base_url = required_env("EPHPM_URL");
    // `.well-known/acme-challenge/*` is reserved for the built-in ACME
    // responder, which returns 404 when no challenge token is registered
    // (rather than 403 from the generic hidden-file rule). This is the
    // behavior nginx/Apache configs typically achieve via an explicit
    // allow rule, and is required for Let's Encrypt to function.
    let url = format!("{base_url}/.well-known/acme-challenge/test-token");

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        404,
        "/.well-known/acme-challenge/<unknown> must be 404 from the ACME handler, got {}",
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

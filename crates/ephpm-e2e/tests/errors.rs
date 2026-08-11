//! PHP error recovery tests.
//!
//! These are the highest-risk gap in the test suite: if the zend_try/zend_catch
//! wrapper misbehaves, PHP fatal errors cause the server to hang or SIGSEGV
//! rather than returning a 500.  These tests ensure the server survives every
//! PHP-level failure mode and keeps accepting new requests afterwards.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Issue a request, assert it returns 500, and assert the *next* request to a
/// good endpoint still works — confirming the server recovered cleanly.
async fn assert_fatal_returns_500_and_server_recovers(url: &str, label: &str) {
    let resp = reqwest::get(url)
        .await
        .unwrap_or_else(|e| panic!("{label}: GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        500,
        "{label}: expected 500, got {} — server may have crashed or swallowed the error",
        resp.status()
    );

    // Consume the body so the connection is released cleanly.
    let _ = resp.bytes().await;
}

async fn assert_server_still_alive(base_url: &str, label: &str) {
    let url = format!("{base_url}/index.php");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("{label}: recovery check GET {url} failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{label}: server must still accept requests after PHP error, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn php_fatal_error_returns_500() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/fatal_error.php");

    assert_fatal_returns_500_and_server_recovers(&url, "fatal_error").await;
    assert_server_still_alive(&base_url, "fatal_error").await;
}

#[tokio::test]
async fn php_memory_limit_exceeded_returns_500() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/memory_hog.php");

    assert_fatal_returns_500_and_server_recovers(&url, "memory_limit").await;
    assert_server_still_alive(&base_url, "memory_limit").await;
}

/// A script that bails out **after** producing output must not have that
/// output completed as a response.
///
/// This is the silent-corruption case: a bailout leaves a truncated prefix in
/// the SAPI capture buffer, and shipping it (at any status) hands a client half
/// a document. The fpm path buffers the whole response before it reaches the
/// wire, so the correct answer is always available: 500, partial body dropped,
/// PHP's headers dropped with it — they describe a response that was never
/// finished.
///
/// The assertions are deliberately specific. `status == 500` alone would pass
/// even if the truncated body were still being served.
#[tokio::test]
async fn php_bailout_discards_partial_output_and_returns_500() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/bailout_after_output.php");

    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let had_php_header = resp.headers().contains_key("x-bailout-fixture");
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(status, 500, "a bailed-out script must never report success; body was: {body}");
    assert!(
        !body.contains("BAILOUT-FIXTURE-PARTIAL-OUTPUT"),
        "the truncated prefix the script had already echoed must not be delivered; body: {body}"
    );
    assert!(
        !body.contains("BAILOUT-FIXTURE-UNREACHABLE"),
        "the script never got this far; body: {body}"
    );
    assert!(
        !had_php_header,
        "headers the script set describe a response it never finished — they must be dropped too"
    );

    assert_server_still_alive(&base_url, "bailout_after_output").await;
}

/// The other half of the contract, and the reason the test above cannot stand
/// alone: a script that does NOT bail out still gets its complete body and its
/// own headers. "Discard everything, always" would satisfy the test above.
#[tokio::test]
async fn normal_script_still_returns_200_with_a_complete_body() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/custom_header.php");

    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.headers().contains_key("x-custom"), "a healthy script's headers must survive");
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(body, "done", "a healthy script's body must be delivered in full");
}

#[tokio::test]
async fn php_syntax_error_returns_500() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/syntax_error.php");

    assert_fatal_returns_500_and_server_recovers(&url, "syntax_error").await;
    assert_server_still_alive(&base_url, "syntax_error").await;
}

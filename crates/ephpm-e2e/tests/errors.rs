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

/// Runaway recursion through an internal function must be a per-request fatal,
/// not a process kill (issue #116).
///
/// This is the strongest test in the suite, because it is the one failure mode
/// where the *server* — not the request — used to lose. ePHPm overrode PHP's
/// `zend_call_stack_init()` with a no-op on Linux, which left `EG(stack_limit)`
/// NULL and PHP's C-stack overflow guard permanently disabled. A
/// `do_blocks()`-shaped render (`array_map` / `preg_replace_callback` /
/// `apply_filters` re-entering userland once per nesting level, one C frame
/// each) then ran off the end of the thread stack and SIGSEGV'd, aborting the
/// whole process and every other tenant's in-flight request with it.
///
/// A regression therefore does NOT show up as a wrong status code — it shows up
/// as a transport error here and cascading failures in every other suite, which
/// is why both the crash request and the recovery check assert on their own.
#[tokio::test]
async fn deep_recursion_returns_500_and_the_server_survives() {
    let base_url = required_env("EPHPM_URL");
    // ~6x the measured ceiling (see `moderate_recursion_still_completes`), deep
    // enough that no plausible per-frame cost lets it through, shallow enough
    // that PHP can still render the resulting stack trace quickly.
    let url = format!("{base_url}/deep_recursion.php?depth=60000");

    let resp = reqwest::get(&url).await.unwrap_or_else(|e| {
        panic!(
            "GET {url} failed: {e} — a transport error here means the SERVER DIED: \
             PHP's C-stack guard is disabled again and the overflow was a SIGSEGV, \
             not a fatal (issue #116)"
        )
    });
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(
        status, 500,
        "runaway recursion must be answered 500 (PHP raises `Maximum call stack size \
         ... reached`); body was: {body}"
    );
    assert!(
        !body.contains("survived depth="),
        "the fixture must actually exhaust the stack — if it completed, the depth is \
         no longer deep enough to prove anything; body: {body}"
    );

    assert_server_still_alive(&base_url, "deep_recursion").await;
}

/// The companion to the test above: the ceiling PHP enforces must be roughly
/// php-fpm's, not a quarter of it.
///
/// Restoring the stack guard without also sizing the PHP threads would have
/// swapped a crash for a spurious `Maximum call stack size` on code that php-fpm
/// runs happily — a regression dressed as a fix. ePHPm gives every
/// PHP-executing thread `ephpm_php::PHP_THREAD_STACK` (8 MiB, matching a stock
/// `ulimit -s` main thread).
///
/// The depth is chosen from a measured sweep of this fixture (PHP 8.5.7 ZTS,
/// x86-64): it completes to ~10 000 levels on an 8 MiB stack and fatals by
/// 12 000, so the ceiling is ~750 bytes of C stack per level. 5 000 therefore
/// sits at roughly half the 8 MiB ceiling — comfortable margin for a different
/// PHP minor or VM — while still being about twice the ~2 600-level ceiling a
/// 2 MiB thread would have. That is what makes this a real assertion about the
/// thread stack size rather than a tautology.
#[tokio::test]
async fn moderate_recursion_still_completes() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/deep_recursion.php?depth=5000");

    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(
        status, 200,
        "5 000 levels fits in a php-fpm-sized stack; a 500 here means PHP threads \
         were left on Rust's 2 MiB default (issue #116). Body: {body}"
    );
    assert!(
        body.contains("survived depth=5000"),
        "the render must complete and report its depth; body: {body}"
    );
}

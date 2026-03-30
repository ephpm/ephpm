//! Timeout tests.
//!
//! Validates:
//! - PHP scripts exceeding the request timeout receive 504 Gateway Timeout
//! - Server recovers and handles subsequent requests normally
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn php_sleep_exceeding_timeout_returns_504() {
    let base_url = required_env("EPHPM_URL");
    // sleep.php sleeps for the given number of seconds.
    // The e2e config sets request timeout to a few seconds;
    // sleeping 30s should exceed it.
    let url = format!("{base_url}/sleep.php?seconds=30");

    let client = reqwest::Client::builder()
        // Client timeout must be longer than the server's request timeout
        // so we actually receive the 504 rather than a client-side timeout.
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("failed to build reqwest client");

    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        504,
        "PHP script sleeping beyond request timeout must return 504, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn server_recovers_after_timeout() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("failed to build reqwest client");

    // Trigger a timeout first
    let timeout_url = format!("{base_url}/sleep.php?seconds=30");
    let resp = client.get(&timeout_url).send().await;
    // We don't assert 504 here — the timeout might not fire in all configs.
    // The point is to stress the server, then verify recovery.
    drop(resp);

    // Follow-up request must succeed
    let ok_url = format!("{base_url}/test.html");
    let resp = reqwest::get(&ok_url)
        .await
        .unwrap_or_else(|e| panic!("GET {ok_url} failed after timeout: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "server must recover and serve requests after a timed-out PHP request, got {}",
        resp.status()
    );
}

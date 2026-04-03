//! Timeout edge-case tests.
//!
//! Validates:
//! - Server recovers cleanly after multiple consecutive PHP timeouts
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Trigger 3 consecutive PHP timeouts and verify the server recovers
/// after each one by successfully serving a normal request.
///
/// This stress-tests the timeout recovery mechanism — each timed-out
/// PHP execution must not leak resources or poison the worker pool.
#[tokio::test]
async fn server_recovers_after_multiple_timeouts() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::builder()
        // Client timeout must be longer than the server's request timeout
        // so we actually receive the 504 rather than a client-side timeout.
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("failed to build reqwest client");

    for round in 0..3 {
        // Trigger a timeout: sleep well beyond the configured request timeout.
        let timeout_url = format!("{base_url}/sleep.php?seconds=60");
        let resp = client
            .get(&timeout_url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("round {round}: timeout request failed: {e}"));

        assert_eq!(
            resp.status().as_u16(),
            504,
            "round {round}: PHP sleep beyond request timeout must return 504, got {}",
            resp.status()
        );

        // Verify the server recovers by serving a normal request.
        let ok_url = format!("{base_url}/test.html");
        let resp = client
            .get(&ok_url)
            .send()
            .await
            .unwrap_or_else(|e| {
                panic!("round {round}: recovery request GET {ok_url} failed: {e}")
            });

        assert_eq!(
            resp.status().as_u16(),
            200,
            "round {round}: server must recover after timeout and serve 200, got {}",
            resp.status()
        );
    }
}

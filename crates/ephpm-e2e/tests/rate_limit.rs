//! Rate limiting end-to-end tests.
//!
//! Validates:
//! - Rapid bursts of requests eventually trigger 429 Too Many Requests
//! - After the rate limit window resets, requests succeed again (200)
//!
//! The test config (`ephpm-test.toml`) sets `per_ip_rate = 50` and
//! `per_ip_burst = 10`, so after 10 requests the bucket is exhausted and
//! subsequent requests that outpace the 50/s refill rate are rejected.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test]
async fn burst_triggers_429_then_recovers() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    let client = reqwest::Client::new();

    // Send 40 requests as fast as possible. With per_ip_rate=50 and
    // per_ip_burst=10, the bucket drains after ~10 requests and subsequent
    // ones that outpace the 50/s refill should get 429.
    let total = 40;
    let mut statuses = Vec::with_capacity(total);

    for _ in 0..total {
        let resp = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
        statuses.push(resp.status().as_u16());
    }

    let ok_count = statuses.iter().filter(|&&s| s == 200).count();
    let rate_limited_count = statuses.iter().filter(|&&s| s == 429).count();

    assert!(
        rate_limited_count > 0,
        "expected at least one 429 Too Many Requests in {total} rapid requests, \
         but got {ok_count} 200s and 0 429s. All statuses: {statuses:?}"
    );

    // Sanity: the first few requests should have succeeded.
    assert!(
        ok_count > 0,
        "expected at least some 200 responses before rate limit kicks in, \
         but all {total} were rate-limited. Statuses: {statuses:?}"
    );

    // Wait for the token bucket to refill. At 50 req/s, 1 second gives
    // plenty of tokens back.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // After waiting, a single request should succeed again.
    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} (recovery) failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "expected 200 after rate limit window reset, got {}",
        resp.status()
    );
}

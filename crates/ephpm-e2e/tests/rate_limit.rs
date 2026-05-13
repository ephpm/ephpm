//! Rate limiting end-to-end tests.
//!
//! Validates:
//! - Rapid bursts of requests eventually trigger 429 Too Many Requests
//! - After the rate limit window resets, requests succeed again (200)
//!
//! The test config (`ephpm-test.toml`) sets `per_ip_rate = 500` and
//! `per_ip_burst = 100`. To reliably trip a 429 from a single pod IP we
//! fire requests concurrently — well past the burst — so they outpace the
//! 500/s refill before the responses come back.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_triggers_429_then_recovers() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    let client = reqwest::Client::new();

    // Fire enough concurrent requests to comfortably exceed per_ip_burst
    // (100) plus whatever the refill produces during the request window.
    // Concurrent dispatch is essential: sequential awaits are gated by RTT
    // and would never outpace a 500/s refill on a fast in-cluster network.
    let total = 500;
    let handles: Vec<_> = (0..total)
        .map(|_| {
            let client = client.clone();
            let url = url.clone();
            tokio::spawn(async move { client.get(&url).send().await })
        })
        .collect();

    let mut statuses = Vec::with_capacity(total);
    for handle in handles {
        let resp = handle
            .await
            .expect("rate-limit request task panicked")
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

    // Sanity: the first batch should have succeeded.
    assert!(
        ok_count > 0,
        "expected at least some 200 responses before rate limit kicks in, \
         but all {total} were rate-limited. Statuses: {statuses:?}"
    );

    // Wait long enough for the token bucket to fully refill. At 500 req/s
    // and a burst of 100, 1s is well past the refill window.
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

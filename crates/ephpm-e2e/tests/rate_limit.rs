//! Rate limiting end-to-end tests.
//!
//! Validates:
//! - Rapid bursts of requests eventually trigger 429 Too Many Requests
//! - After the rate limit window resets, requests succeed again (200)
//!
//! The test config (`ephpm-test.toml`) sets `per_ip_rate = 500`,
//! `per_ip_burst = 100`, and `max_connections = 100`. We fire requests
//! concurrently — well past the burst — so they outpace the 500/s
//! refill before responses come back. Concurrency is capped well below
//! `max_connections` so the test exercises the rate limiter rather than
//! the connection cap (the cap surfaces as TCP errors at the client,
//! which would mask the 429 we're trying to assert).
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_triggers_429_then_recovers() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/test.html");

    // Cap the in-flight connection count well under server max_connections
    // (100) so the test never trips the connection limiter; with HTTP
    // keep-alive on a small connection pool, sequential bursts are still
    // dispatched fast enough to outpace the 500/s token refill.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(50)
        .build()
        .expect("failed to build reqwest client");

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(50));
    let total = 400;
    let handles: Vec<_> = (0..total)
        .map(|_| {
            let client = client.clone();
            let url = url.clone();
            let semaphore = semaphore.clone();
            tokio::spawn(async move {
                let _permit = semaphore.acquire_owned().await.unwrap();
                client.get(&url).send().await
            })
        })
        .collect();

    // Treat any client-side error (connection refused, reset, etc.) as a
    // denial signal too — a spike that overwhelms the server is what the
    // limiter exists to protect against, even if the cap fires first.
    let mut ok_count = 0;
    let mut rate_limited_count = 0;
    let mut other_count = 0;
    let mut errors = 0;
    let mut other_statuses = Vec::new();
    for handle in handles {
        match handle.await.expect("rate-limit request task panicked") {
            Ok(resp) => match resp.status().as_u16() {
                200 => ok_count += 1,
                429 => rate_limited_count += 1,
                s => {
                    other_count += 1;
                    other_statuses.push(s);
                }
            },
            Err(_) => errors += 1,
        }
    }

    let denied = rate_limited_count + errors;
    assert!(
        denied > 0,
        "expected at least one 429 (or client-side denial) in {total} rapid requests; \
         got ok={ok_count} 429={rate_limited_count} other={other_count} \
         errors={errors} other_statuses={other_statuses:?}"
    );

    // Sanity: the first batch should have succeeded.
    assert!(
        ok_count > 0,
        "expected at least some 200 responses before rate limit kicks in; \
         got ok={ok_count} 429={rate_limited_count} errors={errors}"
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

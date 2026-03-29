//! Concurrency correctness tests.
//!
//! Validates that the PHP mutex / NTS serialisation layer handles multiple
//! simultaneous connections without deadlocking or returning wrong output,
//! and that the KV store produces consistent results under concurrent load.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

async fn kv(base_url: &str, query: &str) -> String {
    let url = format!("{base_url}/kv.php?{query}");
    reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"))
        .text()
        .await
        .expect("failed to read kv response body")
        .trim()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_php_requests_all_succeed() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");

    let handles: Vec<_> = (0..20)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move { reqwest::get(&url).await })
        })
        .collect();

    for (i, handle) in handles.into_iter().enumerate() {
        let resp = handle
            .await
            .unwrap_or_else(|e| panic!("task {i} panicked: {e}"))
            .unwrap_or_else(|e| panic!("request {i} failed: {e}"));

        assert_eq!(
            resp.status().as_u16(),
            200,
            "request {i} expected 200, got {}",
            resp.status()
        );

        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| panic!("request {i} body read failed: {e}"));
        assert!(
            body.contains("Hello from ePHPm"),
            "request {i} got unexpected body:\n{body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_kv_increments_are_consistent() {
    let base_url = required_env("EPHPM_URL");

    // Reset counter so the test is idempotent across runs.
    kv(&base_url, "op=del&key=concurrent_ctr").await;

    const N: u32 = 20;

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let base_url = base_url.clone();
            tokio::spawn(async move {
                let body = kv(&base_url, "op=incr&key=concurrent_ctr").await;
                body.parse::<u32>()
                    .unwrap_or_else(|_| panic!("task {i}: expected integer from incr, got: {body}"))
            })
        })
        .collect();

    let mut results: Vec<u32> = Vec::with_capacity(N as usize);
    for (i, handle) in handles.into_iter().enumerate() {
        let val = handle
            .await
            .unwrap_or_else(|e| panic!("task {i} panicked: {e}"));
        results.push(val);
    }

    // The final counter value must equal the number of increments.
    let final_val: u32 = kv(&base_url, "op=get&key=concurrent_ctr")
        .await
        .parse()
        .expect("final counter must be a valid integer");

    assert_eq!(
        final_val, N,
        "KV counter after {N} concurrent increments must be {N}, got {final_val} — \
         indicates lost updates or mutex corruption"
    );

    // Every returned value must be unique (1..=N) — no two requests got the same counter.
    results.sort_unstable();
    assert_eq!(
        results,
        (1..=N).collect::<Vec<_>>(),
        "each increment must return a unique value 1..={N}, got: {results:?}"
    );
}

//! Session locking regression tests.
//!
//! The ephpm session save handler takes a pessimistic per-session lock
//! (`session_lock:<sid>`, SETNX + TTL) in PS_READ and releases it in
//! PS_CLOSE, serializing concurrent requests that share a session id.
//! Without the lock, concurrent read-modify-write cycles on `$_SESSION`
//! lose updates.
//!
//! The fixture (`tests/docroot/session_counter.php`) reads a counter from
//! `$_SESSION`, sleeps 50ms to widen the race window, increments, writes,
//! and closes. With locking, N concurrent requests must land the counter
//! at exactly N.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;

/// Extract the `PHPSESSID=<sid>` pair from a response's `Set-Cookie` headers.
fn session_cookie(resp: &reqwest::Response) -> String {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("PHPSESSID="))
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned())
        .expect("init response must set a PHPSESSID cookie")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_session_increments_do_not_lose_updates() {
    let base_url = required_env("EPHPM_URL");
    let client = reqwest::Client::new();

    // Prime: create the session, zero the counter, capture the cookie so
    // every concurrent request rides the same session id.
    let init = client
        .get(format!("{base_url}/session_counter.php?mode=init"))
        .send()
        .await
        .expect("init request failed");
    assert_eq!(init.status().as_u16(), 200, "init must return 200");
    let cookie = session_cookie(&init);
    let init_body = init.text().await.expect("failed to read init body");
    assert_eq!(init_body.trim(), "0", "init must reset the counter to 0");

    const N: usize = 10;

    // Fan out N concurrent increments sharing the session cookie. Each one
    // holds the session (and its lock) across a 50ms sleep, so with locking
    // they serialize; without it they'd stomp each other's writes.
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let client = client.clone();
            let url = format!("{base_url}/session_counter.php?mode=incr");
            let cookie = cookie.clone();
            tokio::spawn(async move {
                let resp = client
                    .get(&url)
                    .header(reqwest::header::COOKIE, cookie)
                    .send()
                    .await
                    .unwrap_or_else(|e| panic!("incr request {i} failed: {e}"));
                assert_eq!(
                    resp.status().as_u16(),
                    200,
                    "incr request {i} expected 200, got {}",
                    resp.status()
                );
                let body = resp
                    .text()
                    .await
                    .unwrap_or_else(|e| panic!("incr request {i} body read failed: {e}"));
                body.trim()
                    .parse::<usize>()
                    .unwrap_or_else(|_| panic!("incr request {i}: expected integer, got: {body}"))
            })
        })
        .collect();

    let mut results = Vec::with_capacity(N);
    for (i, handle) in handles.into_iter().enumerate() {
        results.push(handle.await.unwrap_or_else(|e| panic!("task {i} panicked: {e}")));
    }

    // Final read must observe every increment — a value below N means a
    // concurrent request read a stale counter (lost update).
    let final_resp = client
        .get(format!("{base_url}/session_counter.php?mode=read"))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .expect("final read request failed");
    let final_val: usize = final_resp
        .text()
        .await
        .expect("failed to read final body")
        .trim()
        .parse()
        .expect("final counter must be a valid integer");

    assert_eq!(
        final_val, N,
        "session counter after {N} concurrent increments must be {N}, got {final_val} — \
         indicates lost updates (session locking not serializing requests)"
    );

    // With serialized increments every response value is unique: 1..=N.
    results.sort_unstable();
    assert_eq!(
        results,
        (1..=N).collect::<Vec<_>>(),
        "each increment must return a unique value 1..={N}, got: {results:?}"
    );
}

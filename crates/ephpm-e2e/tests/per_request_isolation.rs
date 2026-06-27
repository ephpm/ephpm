//! Per-request PHP state isolation.
//!
//! Regression test for ephpm/ephpm#101. Before that fix, `ephpm_execute_request`
//! reused a single PHP request for the lifetime of each worker thread and
//! never tore it down — user-defined functions, classes, constants, function
//! statics, static class properties, $GLOBALS, and the included-files list
//! all leaked from one HTTP request into the next on the same thread. Vanilla
//! WordPress rendered only the first request per worker (constants like
//! `WP_USE_THEMES` persisted and short-circuited `wp-blog-header.php`).
//!
//! The fixture (`tests/docroot/per_request_state.php`) emits four canaries —
//! a constant, a function-local static, a $GLOBALS counter, and a function-
//! existence check. On a correct SAPI every response is byte-identical;
//! under the leak, request #2+ on a warmed thread shows accumulated state.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use std::collections::HashSet;

use ephpm_e2e::required_env;

/// What a request to `per_request_state.php` MUST emit on a SAPI that
/// resets per-request state correctly. Any deviation means the leak is
/// back — most likely because `ephpm_execute_request` skipped the
/// `php_request_shutdown` + `php_request_startup` cycle.
const EXPECTED_CLEAN_RESPONSE: &str = "was_defined=false\n\
                                       static_counter=1\n\
                                       global_counter=1\n\
                                       func_existed=false\n";

async fn fetch_probe(base_url: &str) -> String {
    let url = format!("{base_url}/per_request_state.php");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert_eq!(
        resp.status().as_u16(),
        200,
        "probe must return 200 (got {})",
        resp.status()
    );
    resp.text()
        .await
        .expect("failed to read probe response body")
}

/// Sequential single-flight requests almost always reuse the freshest idle
/// worker thread (LIFO scheduling). Without the fix, request #2 onward
/// reports a leaked constant, an accumulating static, a growing $GLOBALS
/// counter, and `func_existed=true` — the test diff is unambiguous.
#[tokio::test]
async fn sequential_requests_see_fresh_per_request_state() {
    let base_url = required_env("EPHPM_URL");

    // 8 sequential requests: matches the manual reproduction in the
    // PR #101 body (req #1 worked, req #2-8 leaked).
    for i in 1..=8 {
        let body = fetch_probe(&base_url).await;
        assert_eq!(
            body, EXPECTED_CLEAN_RESPONSE,
            "request #{i} returned leaked state — every request must start \
             from a fresh per-request executor.\nGot:\n{body}\nExpected:\n{EXPECTED_CLEAN_RESPONSE}"
        );
    }
}

/// Even under concurrent load, every response must look like the first
/// request on a fresh thread. Without the fix, a pool of W warm worker
/// threads produces W "clean" responses (one per fresh thread) and N-W
/// leaked responses — collecting the response set surfaces this as a
/// non-singleton.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_requests_all_see_fresh_per_request_state() {
    let base_url = required_env("EPHPM_URL");

    const N: usize = 40;
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let base_url = base_url.clone();
            tokio::spawn(async move { fetch_probe(&base_url).await })
        })
        .collect();

    let mut responses: Vec<String> = Vec::with_capacity(N);
    for (i, h) in handles.into_iter().enumerate() {
        responses.push(
            h.await
                .unwrap_or_else(|e| panic!("request {i} panicked: {e}")),
        );
    }

    let unique: HashSet<&String> = responses.iter().collect();
    assert_eq!(
        unique.len(),
        1,
        "expected every response identical, got {} distinct bodies across {N} requests:\n{}",
        unique.len(),
        unique
            .iter()
            .enumerate()
            .map(|(i, b)| format!("--- variant {i} ---\n{b}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let only = responses.first().expect("must have at least one response");
    assert_eq!(
        only, EXPECTED_CLEAN_RESPONSE,
        "all responses agreed but disagreed with the clean baseline — \
         per-request reset may be partial.\nGot:\n{only}\nExpected:\n{EXPECTED_CLEAN_RESPONSE}"
    );
}

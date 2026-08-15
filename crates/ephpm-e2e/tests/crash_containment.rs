//! Stack-overflow crash containment (`[php] crash_containment`) e2e tests.
//!
//! Runs against a node the bare-process harness spawns with BOTH the pool engine
//! and containment on (see `ISOLATED_CONFIG_SUITES` /
//! `SingleNodeOptions::crash_containment`). This suite is the only one that
//! **deliberately crashes PHP**: `stack_overflow.php` drives the
//! `zend_object_std_dtor` ↔ `zend_objects_store_del` C recursion off the end of
//! its thread's stack. Without containment that `SIGSEGV` kills the whole
//! server; the point of every test here is that it does not.
//!
//! The node is throwaway — after a contained crash it carries abandoned TSRM
//! entries and skips PHP module shutdown at exit — which is why the suite gets
//! its own node and runs single-threaded.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the containment-enabled ephpm instance. Read via
//!   [`required_env`], so the suite FAILS (loudly) rather than skips when the
//!   harness did not provide a server (the fail-don't-skip convention, #244).

use std::time::Duration;

use ephpm_e2e::required_env;

/// Long enough for the deep free to actually run, short enough that a hung
/// request fails the test instead of hanging CI.
const CRASH_TIMEOUT: Duration = Duration::from_secs(60);

fn client() -> reqwest::Client {
    reqwest::Client::builder().timeout(CRASH_TIMEOUT).build().expect("build client")
}

/// Scrape `/metrics` and return the value of a single unlabelled sample, or the
/// summed value of every sample whose line starts with `name{`.
async fn metric(name: &str) -> f64 {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/metrics");
    let body = client()
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"))
        .text()
        .await
        .unwrap_or_default();

    body.lines()
        .filter(|line| {
            !line.starts_with('#')
                && (line.starts_with(&format!("{name} ")) || line.starts_with(&format!("{name}{{")))
        })
        .filter_map(|line| line.split_whitespace().next_back()?.parse::<f64>().ok())
        .sum()
}

/// Sum of `ephpm_fpm_pool_recycles_total` samples carrying `reason="poisoned"`.
async fn poisoned_recycles() -> f64 {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/metrics");
    let body = client()
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"))
        .text()
        .await
        .unwrap_or_default();

    body.lines()
        .filter(|line| {
            line.starts_with("ephpm_fpm_pool_recycles_total{") && line.contains("poisoned")
        })
        .filter_map(|line| line.split_whitespace().next_back()?.parse::<f64>().ok())
        .sum()
}

/// Ask the server to overflow a PHP thread's C stack.
async fn crash_request() -> reqwest::StatusCode {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/stack_overflow.php");
    client()
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| {
            panic!(
                "GET {url} failed: {e} — a transport error here usually means the \
                 SERVER DIED, i.e. the crash was not contained"
            )
        })
        .status()
}

/// A plain request that must keep working throughout.
async fn healthy_request() -> reqwest::StatusCode {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");
    client()
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e} — the server is gone"))
        .status()
}

/// The headline behaviour: a PHP C-stack overflow answers 500 and the server is
/// still alive and serving afterwards. Before containment this request took the
/// whole process — and every other tenant on it — down.
#[tokio::test]
async fn stack_overflow_returns_500_and_server_survives() {
    // Prove the server is up first, so a failure below cannot be blamed on a
    // node that never started.
    assert_eq!(healthy_request().await, 200, "the server must be healthy before the crash");

    let status = crash_request().await;
    assert_eq!(
        status, 500,
        "a contained C-stack overflow must be answered 500, not 200 and not a \
         dropped connection"
    );

    // The whole point: the process is still there.
    assert_eq!(
        healthy_request().await,
        200,
        "the server must still serve after containing a crash — if this fails with a \
         transport error the crash killed the process"
    );
}

/// Containment is observable, not incidental: the crash increments the contained
/// counter AND retires a pool thread (`reason="poisoned"` recycle). Both must
/// move, since delivering a 500 without retiring the thread would leave a
/// poisoned context in rotation.
#[tokio::test]
async fn contained_crash_is_counted_and_retires_a_thread() {
    let contained_before = metric("ephpm_fpm_pool_contained_crashes_total").await;
    let poisoned_before = poisoned_recycles().await;

    assert_eq!(crash_request().await, 500);

    // The retirement + respawn happens on the pool thread after the response is
    // sent, so give the counters a bounded moment to catch up.
    let mut contained_after = contained_before;
    let mut poisoned_after = poisoned_before;
    for _ in 0..40 {
        contained_after = metric("ephpm_fpm_pool_contained_crashes_total").await;
        poisoned_after = poisoned_recycles().await;
        if contained_after > contained_before && poisoned_after > poisoned_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    assert!(
        contained_after > contained_before,
        "ephpm_fpm_pool_contained_crashes_total must increment \
         ({contained_before} -> {contained_after})"
    );
    assert!(
        poisoned_after > poisoned_before,
        "the poisoned thread must be retired and replaced — \
         ephpm_fpm_pool_recycles_total{{reason=\"poisoned\"}} \
         ({poisoned_before} -> {poisoned_after})"
    );
}

/// The pool refills after a retirement: `ephpm_fpm_pool_size` is the target, and
/// once the replacement thread has registered the server serves at full
/// strength again. A pool that shrank on every crash would degrade into a
/// permanent outage after N crashes — the failure mode this suite exists to
/// rule out.
#[tokio::test]
async fn pool_refills_after_retirement() {
    let size = metric("ephpm_fpm_pool_size").await;
    assert!(size >= 1.0, "pool engine must be active, got size {size}");

    assert_eq!(crash_request().await, 500);

    // Concurrency after the retirement: fire more requests than the pool has
    // threads. If the retired thread were never replaced these would queue
    // behind a short pool and start timing out.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut handles = Vec::new();
    for _ in 0..20 {
        handles.push(tokio::spawn(healthy_request()));
    }
    for h in handles {
        let status = h.await.expect("request task panicked");
        assert_eq!(status, 200, "the pool must serve at full strength after a retirement");
    }
}

/// Blast radius: a crash on one thread must not damage a request in flight on
/// another. Start a slow request, crash while it is running, and require the
/// slow one to finish normally. This is the isolation claim — one tenant's
/// crash is one tenant's 500.
#[tokio::test]
async fn sibling_request_is_unaffected_by_a_concurrent_crash() {
    let base_url = required_env("EPHPM_URL");
    let slow_url = format!("{base_url}/sleep.php?seconds=3");
    let slow = tokio::spawn(async move {
        client().get(&slow_url).send().await.map(|r| (r.status().as_u16(), r.text()))
    });

    // Let the sleeper get onto a pool thread, then crash a different one.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(crash_request().await, 500);

    let (status, body) = slow
        .await
        .expect("slow request task panicked")
        .unwrap_or_else(|e| panic!("the sibling request died with a transport error: {e}"));
    assert_eq!(status, 200, "a concurrent crash must not disturb a sibling request");
    let body = body.await.unwrap_or_default();
    assert!(body.contains("slept"), "the sibling response must be complete, got: {body}");
}

/// Repeated crashes do not accumulate into an outage. Ten sequential crashes,
/// each answered 500, with the server healthy after every one — and healthy at
/// the end. This is the crash-storm case the respawn backoff exists for: it
/// slows replacement, it must not stop it.
#[tokio::test]
async fn repeated_crashes_do_not_wedge_the_server() {
    const CRASHES: usize = 10;

    for i in 0..CRASHES {
        assert_eq!(crash_request().await, 500, "crash #{i} must be contained and answered 500");
        assert_eq!(
            healthy_request().await,
            200,
            "the server must still serve after crash #{i} — a failure here means the \
             pool never recovered (or the process died)"
        );
    }

    let contained = metric("ephpm_fpm_pool_contained_crashes_total").await;
    assert!(
        contained >= CRASHES as f64,
        "every crash must have been contained, counter shows {contained} for {CRASHES} crashes"
    );
}

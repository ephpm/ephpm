//! Request-granularity load shedding (`[php] overload = "shed"`) e2e
//! tests — issue #301.
//!
//! Runs against a node the bare-process harness spawns with the pool engine
//! deliberately undersized: one PHP thread, one backlog slot, `shed_after_ms =
//! 0` (see `ISOLATED_CONFIG_SUITES` / `SingleNodeOptions::overload_shed`).
//! Saturation is therefore two concurrent slow requests, not a load generator.
//!
//! The property under test is one an in-process unit test cannot establish: that
//! the 503 **arrives at a client, over a real socket**. That is the #299 lesson
//! — a shed written outside the connection's normal response path can be lost to
//! the connection state machine, leaving the client with a hang instead of the
//! fast failure the shed was supposed to give it.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the shedding ephpm instance. Read via
//!   [`required_env`], so the suite FAILS (loudly) rather than skips when the
//!   harness did not provide a server (the fail-don't-skip convention, #244).

use std::time::{Duration, Instant};

use ephpm_e2e::required_env;

/// One saturating burst: `count` concurrent requests to a script that occupies
/// its PHP thread for `seconds`. Returns `(status, retry_after, body)` per
/// request, in completion order.
async fn saturating_burst(count: usize, seconds: f32) -> Vec<(u16, Option<String>, String)> {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/sleep.php?seconds={seconds}");

    let mut handles = Vec::new();
    for _ in 0..count {
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            // A generous client timeout on purpose: the bug this suite guards
            // against is exactly "the server never answers", and we want that
            // to show up as a missing 503, not as a client-side cancellation
            // that could be mistaken for one.
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("client");
            match client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let body = resp.text().await.unwrap_or_default();
                    (status, retry_after, body)
                }
                Err(e) => panic!("request failed outright (no HTTP response at all): {e}"),
            }
        }));
    }

    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.expect("request task panicked"));
    }
    out
}

/// Scrape one counter series from `/metrics`, summing every labelled sample
/// whose name matches. Returns 0 when the series has not been recorded yet
/// (a counter that has never fired is simply absent).
async fn metric_total(name: &str) -> f64 {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/metrics");
    let body = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"))
        .text()
        .await
        .unwrap_or_default();

    body.lines()
        .filter(|line| !line.starts_with('#') && line.starts_with(name))
        .filter_map(|line| line.split_whitespace().next_back()?.parse::<f64>().ok())
        .sum()
}

/// The headline behaviour of #301: an overloaded ePHPm answers. Before this,
/// every excess request queued and the client's own timeout was the only
/// terminator — an open-loop flood produced 200s and timeouts, and no error
/// status of any kind.
///
/// With one thread and one backlog slot, a burst of 8 concurrent 1-second
/// requests cannot fit: some must come back `503` immediately, carrying
/// `Retry-After` so a proxy knows to back off rather than hot-loop.
#[tokio::test]
async fn saturation_answers_503_with_retry_after() {
    let started = Instant::now();
    let results = saturating_burst(8, 1.0).await;

    let shed: Vec<_> = results.iter().filter(|(status, ..)| *status == 503).collect();
    let served = results.iter().filter(|(status, ..)| *status == 200).count();

    assert!(
        !shed.is_empty(),
        "a saturated server must shed, not queue — got statuses {:?}",
        results.iter().map(|(s, ..)| *s).collect::<Vec<_>>()
    );
    assert!(
        served >= 1,
        "shedding must not starve the requests that DO fit — got statuses {:?}",
        results.iter().map(|(s, ..)| *s).collect::<Vec<_>>()
    );

    for (_, retry_after, body) in &shed {
        assert_eq!(
            retry_after.as_deref(),
            Some("1"),
            "every overload 503 must tell the client when to retry"
        );
        assert!(
            body.contains("overloaded"),
            "an overload 503 must be distinguishable from a shutdown 503: {body}"
        );
    }

    // Shedding is only worth anything if it is *fast*. Each request holds its
    // thread for 1 s, so a queueing server would take at least `8 / 1 thread`
    // seconds to answer all eight; a shedding one answers the excess at once.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "shed responses must be immediate, not backpressured: burst took {:?}",
        started.elapsed()
    );
}

/// The shed is countable, so an operator can alert on it. `ephpm_php_shed_total`
/// is labelled by engine — `pool` here — and rises only for overload, never for
/// the pre-existing draining-pool 503.
#[tokio::test]
async fn shed_responses_are_counted_in_metrics() {
    let before = metric_total("ephpm_php_shed_total").await;
    let results = saturating_burst(8, 1.0).await;
    let shed = results.iter().filter(|(status, ..)| *status == 503).count();
    assert!(shed >= 1, "the burst must actually shed for this test to mean anything");

    let after = metric_total("ephpm_php_shed_total").await;
    #[allow(clippy::cast_precision_loss)]
    let shed_f = shed as f64;
    assert!(
        after >= before + shed_f,
        "every shed response must be counted: {before} -> {after} for {shed} shed requests"
    );

    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/metrics");
    let body = reqwest::get(&url).await.expect("metrics").text().await.unwrap_or_default();
    assert!(
        body.lines().any(|l| l.starts_with("ephpm_php_shed_total") && l.contains("pool")),
        "the shed counter must name the engine that shed:\n{body}"
    );
}

/// Shedding is admission control, not a latched failure: once the burst drains,
/// ordinary requests are served normally again. A server that stayed 503 after
/// a spike would be worse than the queueing it replaced.
#[tokio::test]
async fn server_recovers_after_the_burst() {
    let _ = saturating_burst(8, 1.0).await;

    // The in-flight sleeps finish within ~1 s; give the single thread room to
    // drain the one queued request too before calling recovery.
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/index.php");
    let mut last = 0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        last = reqwest::get(&url).await.map(|r| r.status().as_u16()).unwrap_or(0);
        if last == 200 {
            break;
        }
    }
    assert_eq!(last, 200, "the server must serve normally again once the burst drains");

    // And an idle server never sheds: sequential requests all succeed.
    for i in 0..5 {
        let status = reqwest::get(&url).await.expect("request").status().as_u16();
        assert_eq!(status, 200, "request {i} on an idle server must not be shed");
    }
}

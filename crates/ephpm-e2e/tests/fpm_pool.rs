//! Experimental FPM pool engine (`[php] fpm_engine = "pool"`) e2e tests.
//!
//! Runs against a node the bare-process harness spawns with the pool engine on
//! (see `ISOLATED_CONFIG_SUITES` / `SingleNodeOptions::fpm_engine_pool`). Proves
//! the dedicated FPM thread pool serves the FULL per-request path end to end —
//! real PHP plus the in-process `ephpm_db_*` bridge — and that the engine is
//! actually active (not silently falling back to `spawn_blocking`).
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the pool-engine ephpm instance. Read via
//!   [`required_env`], so the suite FAILS (loudly) rather than skips when the
//!   harness did not provide a server (the fail-don't-skip convention, #244).

use ephpm_e2e::required_env;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BridgeResponse {
    status: String,
    #[serde(default)]
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    affected_rows: Option<i64>,
    #[serde(default)]
    last_insert_id: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

async fn bridge_action(action: &str, params: &str) -> BridgeResponse {
    let base_url = required_env("EPHPM_URL");
    let sep = if params.is_empty() { "" } else { "&" };
    let url = format!("{base_url}/db_bridge_test.php?action={action}{sep}{params}");
    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "action={action}&{params} returned HTTP {status}: {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("failed to parse JSON for action={action}: {e}\nbody: {body}"))
}

/// The pool serves real PHP AND drives the in-process `ephpm_db_*` bridge:
/// CREATE + INSERT (bound params) + SELECT round-trips through a pool thread's
/// per-request litewire session. This is the whole point — the pool runs the
/// identical per-request path the `spawn_blocking` engine does.
#[tokio::test]
async fn pool_serves_php_and_db_bridge_roundtrip() {
    let setup = bridge_action("setup", "").await;
    assert_eq!(setup.status, "ok", "setup failed: {:?}", setup.message);

    let ins = bridge_action("insert", "name=pool&value=engine").await;
    assert_eq!(ins.status, "ok", "insert failed: {:?}", ins.message);
    assert_eq!(ins.affected_rows, Some(1), "INSERT must report 1 affected row");
    assert!(ins.last_insert_id.unwrap_or(0) >= 1, "last_insert_id must be >= 1");

    let q = bridge_action("query", "").await;
    assert_eq!(q.status, "ok", "query failed: {:?}", q.message);
    assert!(
        q.rows.iter().any(|r| r["name"] == "pool" && r["value"] == "engine"),
        "the row inserted through the pool must read back: {:?}",
        q.rows
    );

    let cleanup = bridge_action("cleanup", "").await;
    assert_eq!(cleanup.status, "ok", "cleanup failed: {:?}", cleanup.message);
}

/// The pool engine is genuinely ACTIVE, not a silent fallback to
/// `spawn_blocking`: `/metrics` exposes `ephpm_fpm_pool_size` with a value
/// >= 1. That gauge is set only by `FpmPool::spawn`, so its presence proves the
/// dedicated pool was built and PHP is being dispatched to it.
#[tokio::test]
async fn pool_engine_is_active_per_metrics() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/metrics");
    let body = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"))
        .text()
        .await
        .unwrap_or_default();

    let value = body
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("ephpm_fpm_pool_size")?;
            // Skip HELP/TYPE lines (they have a space then a word, not a number)
            // and match the metric sample line: `ephpm_fpm_pool_size <float>`.
            rest.trim().split_whitespace().next_back().and_then(|v| v.parse::<f64>().ok())
        })
        .unwrap_or_else(|| {
            panic!("ephpm_fpm_pool_size not found in /metrics — pool engine is not active:\n{body}")
        });

    assert!(value >= 1.0, "pool size must be >= 1 when the pool engine is on, got {value}");
}

/// The pool handles concurrency: 40 in-flight requests all complete with 200.
/// The pool size caps how many run at once; the rest queue (backpressure) and
/// still finish within the request timeout — none error.
#[tokio::test]
async fn pool_handles_concurrent_requests() {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/db_bridge_test.php?action=show_tables");

    let mut handles = Vec::new();
    for _ in 0..40 {
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            reqwest::get(&url).await.map(|r| r.status().as_u16()).unwrap_or(0)
        }));
    }

    for h in handles {
        let status = h.await.expect("request task panicked");
        assert_eq!(status, 200, "every concurrent pool request must return 200, got {status}");
    }
}

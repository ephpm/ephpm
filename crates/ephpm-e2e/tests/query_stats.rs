//! Query stats e2e tests.
//!
//! Verifies that SQL query digest metrics appear in `/metrics` after
//! executing queries through the litewire SQLite frontend.
//!
//! Requires:
//! - `EPHPM_URL` — base URL of the ephpm instance
//! - `[db.sqlite]` configured (litewire enabled)
//! - `[db.analysis] query_stats = true` (default)
//! - `[server.metrics] enabled = true`
//! - `sqlite_test.php` in docroot

use ephpm_e2e::required_env;

/// Fetch the Prometheus metrics endpoint.
async fn fetch_metrics() -> String {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/metrics");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "/metrics returned {}",
        resp.status()
    );

    resp.text().await.expect("failed to read /metrics body")
}

/// Execute a query through the SQLite test endpoint.
async fn run_sqlite_action(action: &str) {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/sqlite_test.php?action={action}");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    assert!(
        resp.status().is_success(),
        "sqlite_test.php?action={action} returned {}",
        resp.status()
    );
}

#[tokio::test]
async fn query_stats_appear_in_metrics_after_sql() {
    // Setup: create table and insert data (generates queries)
    run_sqlite_action("setup").await;
    run_sqlite_action("query").await;

    // Fetch metrics and check for query stats
    let metrics = fetch_metrics().await;

    assert!(
        metrics.contains("ephpm_query_duration_seconds"),
        "/metrics should contain ephpm_query_duration_seconds after SQL queries"
    );
    assert!(
        metrics.contains("ephpm_query_total"),
        "/metrics should contain ephpm_query_total after SQL queries"
    );

    // Cleanup
    run_sqlite_action("cleanup").await;
}

#[tokio::test]
async fn query_stats_track_active_digests() {
    run_sqlite_action("setup").await;
    run_sqlite_action("query").await;
    run_sqlite_action("cleanup").await;

    let metrics = fetch_metrics().await;

    assert!(
        metrics.contains("ephpm_query_active_digests"),
        "/metrics should contain ephpm_query_active_digests"
    );
}

#[tokio::test]
async fn query_stats_distinguish_query_and_mutation() {
    run_sqlite_action("setup").await; // mutations (CREATE TABLE, INSERT)
    run_sqlite_action("query").await; // query (SELECT)

    let metrics = fetch_metrics().await;

    assert!(
        metrics.contains("kind=\"query\""),
        "/metrics should contain kind=query label"
    );
    assert!(
        metrics.contains("kind=\"mutation\""),
        "/metrics should contain kind=mutation label"
    );

    run_sqlite_action("cleanup").await;
}

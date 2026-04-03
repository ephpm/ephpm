//! SQLite advanced edge-case tests via litewire.
//!
//! Validates:
//! - Concurrent SQLite writes from parallel HTTP requests
//! - Large result sets (bulk insert + SELECT all)
//! - Row count consistency after bulk operations
//!
//! Uses `sqlite_advanced_test.php` in docroot, which connects to litewire's
//! MySQL frontend backed by SQLite.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AdvancedResponse {
    status: String,
    #[serde(default)]
    count: Option<i64>,
    #[serde(default)]
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

/// Helper to call sqlite_advanced_test.php with an action and optional params.
async fn advanced_action(action: &str, extra_params: &str) -> AdvancedResponse {
    let base_url = required_env("EPHPM_URL");
    let url = if extra_params.is_empty() {
        format!("{base_url}/sqlite_advanced_test.php?action={action}")
    } else {
        format!("{base_url}/sqlite_advanced_test.php?action={action}&{extra_params}")
    };

    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    let status_code = resp.status().as_u16();
    assert_eq!(
        status_code, 200,
        "sqlite_advanced_test.php?action={action} returned {status_code}"
    );

    resp.json::<AdvancedResponse>()
        .await
        .unwrap_or_else(|e| panic!("failed to parse JSON from {url}: {e}"))
}

/// Bulk insert 50 rows into SQLite, then SELECT all and verify count matches.
#[tokio::test]
async fn sqlite_bulk_insert_and_select_all() {
    // Setup fresh table
    let resp = advanced_action("setup", "").await;
    assert_eq!(resp.status, "ok", "setup failed: {:?}", resp.message);

    // Bulk insert 50 rows
    let resp = advanced_action("bulk_insert", "count=50").await;
    assert_eq!(resp.status, "ok", "bulk_insert failed: {:?}", resp.message);
    assert_eq!(
        resp.count,
        Some(50),
        "bulk_insert should report count=50"
    );

    // Verify count via SELECT COUNT(*)
    let resp = advanced_action("count", "").await;
    assert_eq!(resp.status, "ok", "count failed: {:?}", resp.message);
    assert_eq!(
        resp.count,
        Some(50),
        "expected 50 rows after bulk insert, got {:?}",
        resp.count
    );

    // SELECT all rows and verify the result set
    let resp = advanced_action("select_all", "").await;
    assert_eq!(resp.status, "ok", "select_all failed: {:?}", resp.message);
    assert_eq!(
        resp.rows.len(),
        50,
        "expected 50 rows in SELECT result, got {}",
        resp.rows.len()
    );

    // Verify ordering and values
    for (i, row) in resp.rows.iter().enumerate() {
        let expected_id = (i + 1) as i64;
        let actual_id = row["id"]
            .as_i64()
            .or_else(|| row["id"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or_else(|| panic!("row {i}: id must be an integer, got {:?}", row["id"]));
        assert_eq!(
            actual_id, expected_id,
            "row {i}: expected id={expected_id}, got {actual_id}"
        );

        let expected_val = format!("bulk_value_{}", i + 1);
        assert_eq!(
            row["value"].as_str().unwrap_or(""),
            expected_val,
            "row {i}: value mismatch"
        );
    }

    // Cleanup
    let resp = advanced_action("cleanup", "").await;
    assert_eq!(resp.status, "ok", "cleanup failed: {:?}", resp.message);
}

/// Send 5 parallel INSERT requests to SQLite, then verify all rows exist.
/// SQLite serializes writes, so all requests should succeed without conflict.
#[tokio::test]
async fn sqlite_concurrent_writes() {
    let base_url = required_env("EPHPM_URL");

    // Setup fresh table
    let resp = advanced_action("setup", "").await;
    assert_eq!(resp.status, "ok", "setup failed: {:?}", resp.message);

    // Launch 5 concurrent INSERT requests with distinct IDs (1000..1004)
    let client = reqwest::Client::new();
    let mut handles = Vec::new();

    for i in 0..5 {
        let id = 1000 + i;
        let url = format!(
            "{base_url}/sqlite_advanced_test.php?action=concurrent_write&id={id}&value=concurrent_{id}"
        );
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let resp = c
                .get(&url)
                .send()
                .await
                .unwrap_or_else(|e| panic!("concurrent write id={id} failed: {e}"));

            let status = resp.status().as_u16();
            let body: AdvancedResponse = resp
                .json()
                .await
                .unwrap_or_else(|e| panic!("failed to parse response for id={id}: {e}"));
            (id, status, body)
        }));
    }

    // Wait for all concurrent writes to complete
    for handle in handles {
        let (id, status, body) = handle.await.expect("task panicked");
        assert_eq!(
            status, 200,
            "concurrent write id={id} returned {status}"
        );
        assert_eq!(
            body.status, "ok",
            "concurrent write id={id} failed: {:?}",
            body.message
        );
    }

    // Verify all 5 rows exist
    let resp = advanced_action("count", "").await;
    assert_eq!(resp.status, "ok", "count failed: {:?}", resp.message);
    assert_eq!(
        resp.count,
        Some(5),
        "expected 5 rows after concurrent writes, got {:?}",
        resp.count
    );

    // Verify each row individually
    let resp = advanced_action("select_all", "").await;
    assert_eq!(resp.status, "ok");
    assert_eq!(resp.rows.len(), 5, "expected 5 rows in result set");

    for i in 0..5 {
        let expected_id = 1000 + i as i64;
        let row = &resp.rows[i];
        let actual_id = row["id"]
            .as_i64()
            .or_else(|| row["id"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or_else(|| panic!("row {i}: id must be an integer, got {:?}", row["id"]));
        assert_eq!(
            actual_id, expected_id,
            "row {i}: expected id={expected_id}, got {actual_id}"
        );
        assert_eq!(
            row["value"].as_str().unwrap_or(""),
            format!("concurrent_{expected_id}"),
            "row {i}: value mismatch"
        );
    }

    // Cleanup
    let resp = advanced_action("cleanup", "").await;
    assert_eq!(resp.status, "ok", "cleanup failed: {:?}", resp.message);
}

/// Verify that large result sets are returned correctly through the
/// litewire MySQL protocol translation layer.
#[tokio::test]
async fn sqlite_large_result_set() {
    // Setup fresh table
    let resp = advanced_action("setup", "").await;
    assert_eq!(resp.status, "ok", "setup failed: {:?}", resp.message);

    // Insert 100 rows
    let resp = advanced_action("bulk_insert", "count=100").await;
    assert_eq!(resp.status, "ok", "bulk_insert failed: {:?}", resp.message);

    // SELECT all and verify count
    let resp = advanced_action("select_all", "").await;
    assert_eq!(resp.status, "ok", "select_all failed: {:?}", resp.message);
    assert_eq!(
        resp.rows.len(),
        100,
        "expected 100 rows in large result set, got {}",
        resp.rows.len()
    );

    // Spot-check first and last rows
    let first = &resp.rows[0];
    assert_eq!(
        first["id"]
            .as_i64()
            .or_else(|| first["id"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(-1),
        1,
        "first row id must be 1"
    );
    assert_eq!(
        first["value"].as_str().unwrap_or(""),
        "bulk_value_1",
        "first row value mismatch"
    );

    let last = &resp.rows[99];
    assert_eq!(
        last["id"]
            .as_i64()
            .or_else(|| last["id"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(-1),
        100,
        "last row id must be 100"
    );
    assert_eq!(
        last["value"].as_str().unwrap_or(""),
        "bulk_value_100",
        "last row value mismatch"
    );

    // Cleanup
    let resp = advanced_action("cleanup", "").await;
    assert_eq!(resp.status, "ok", "cleanup failed: {:?}", resp.message);
}

/// Verify setup is idempotent — calling setup drops and recreates the table.
#[tokio::test]
async fn sqlite_setup_is_idempotent() {
    // Setup twice in a row should not fail
    let resp = advanced_action("setup", "").await;
    assert_eq!(resp.status, "ok", "first setup failed: {:?}", resp.message);

    // Insert some data
    let resp = advanced_action("bulk_insert", "count=10").await;
    assert_eq!(resp.status, "ok");

    // Setup again — should drop and recreate, clearing data
    let resp = advanced_action("setup", "").await;
    assert_eq!(resp.status, "ok", "second setup failed: {:?}", resp.message);

    // Count should be 0 after fresh setup
    let resp = advanced_action("count", "").await;
    assert_eq!(resp.status, "ok");
    assert_eq!(
        resp.count,
        Some(0),
        "table should be empty after re-setup, got {:?}",
        resp.count
    );

    // Cleanup
    advanced_action("cleanup", "").await;
}

/// Verify cleanup is idempotent — DROP TABLE IF EXISTS should never fail.
#[tokio::test]
async fn sqlite_cleanup_is_idempotent() {
    let resp = advanced_action("cleanup", "").await;
    assert_eq!(resp.status, "ok", "first cleanup failed: {:?}", resp.message);

    let resp = advanced_action("cleanup", "").await;
    assert_eq!(resp.status, "ok", "second cleanup failed: {:?}", resp.message);
}

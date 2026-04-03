//! Read/write split e2e tests.
//!
//! Verifies that enabling `[db.read_write_split]` does not break
//! single-backend operation. When no replicas are configured, all
//! queries (reads and writes) should fall back to the primary.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RwSplitResponse {
    status: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    insert_id: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Call rw_split_test.php with the given action.
async fn rw_action(action: &str) -> RwSplitResponse {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/rw_split_test.php?action={action}");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "rw_split_test.php?action={action} returned {}",
        resp.status()
    );

    resp.json::<RwSplitResponse>()
        .await
        .unwrap_or_else(|e| panic!("failed to parse JSON from {url}: {e}"))
}

/// Call rw_split_test.php with action and extra query params.
async fn rw_action_with_params(action: &str, params: &str) -> RwSplitResponse {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/rw_split_test.php?action={action}&{params}");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "rw_split_test.php?action={action}&{params} returned {}",
        resp.status()
    );

    resp.json::<RwSplitResponse>()
        .await
        .unwrap_or_else(|e| panic!("failed to parse JSON from {url}: {e}"))
}

/// R/W split enabled: writes (CREATE TABLE + INSERT) and reads (SELECT)
/// both succeed on a single-backend setup.
#[tokio::test]
async fn rw_split_setup_creates_table_and_seeds() {
    // Cleanup any prior state
    rw_action("cleanup").await;

    let resp = rw_action("setup").await;
    assert_eq!(resp.status, "ok", "setup failed: {:?}", resp.message);
    assert_eq!(resp.action.as_deref(), Some("setup"));

    // Setup inserts a seed row and returns it
    assert_eq!(resp.rows.len(), 1, "expected 1 seed row, got {}", resp.rows.len());
    assert_eq!(resp.rows[0]["value"], "seed");

    // Cleanup
    rw_action("cleanup").await;
}

/// Pure read path works with R/W split enabled.
#[tokio::test]
async fn rw_split_read_returns_rows() {
    rw_action("cleanup").await;
    rw_action("setup").await;

    let resp = rw_action("read").await;
    assert_eq!(resp.status, "ok", "read failed: {:?}", resp.message);
    assert_eq!(resp.action.as_deref(), Some("read"));
    assert!(!resp.rows.is_empty(), "read should return at least the seed row");
    assert_eq!(resp.rows[0]["value"], "seed");

    rw_action("cleanup").await;
}

/// Pure write path works with R/W split enabled.
#[tokio::test]
async fn rw_split_write_inserts_row() {
    rw_action("cleanup").await;
    rw_action("setup").await;

    let resp = rw_action_with_params("write", "value=test_write").await;
    assert_eq!(resp.status, "ok", "write failed: {:?}", resp.message);
    assert!(resp.id.is_some(), "write should return an insert id");

    // Verify via read
    let resp = rw_action("read").await;
    assert_eq!(resp.status, "ok");
    assert_eq!(resp.rows.len(), 2, "expected 2 rows (seed + insert), got {}", resp.rows.len());

    let values: Vec<&str> =
        resp.rows.iter().filter_map(|r| r["value"].as_str()).collect();
    assert!(values.contains(&"test_write"), "inserted row not found in {values:?}");

    rw_action("cleanup").await;
}

/// Mixed write-then-read within a single request succeeds. This
/// exercises the sticky-after-write behavior — the read immediately
/// following a write must see the just-inserted data.
#[tokio::test]
async fn rw_split_mixed_write_then_read_is_consistent() {
    rw_action("cleanup").await;
    rw_action("setup").await;

    let resp = rw_action_with_params("mixed", "value=mixed_val").await;
    assert_eq!(resp.status, "ok", "mixed failed: {:?}", resp.message);
    assert_eq!(resp.action.as_deref(), Some("mixed"));
    assert!(resp.insert_id.is_some(), "mixed should return insert_id");

    // The returned rows must include the just-inserted value
    let values: Vec<&str> =
        resp.rows.iter().filter_map(|r| r["value"].as_str()).collect();
    assert!(
        values.contains(&"mixed_val"),
        "just-inserted row not found in immediate read: {values:?}"
    );

    rw_action("cleanup").await;
}

/// Cleanup is idempotent — dropping a non-existent table does not error.
#[tokio::test]
async fn rw_split_cleanup_is_idempotent() {
    let resp = rw_action("cleanup").await;
    assert_eq!(resp.status, "ok");

    // Second cleanup should also succeed
    let resp = rw_action("cleanup").await;
    assert_eq!(resp.status, "ok");
}

/// Read after cleanup fails (table does not exist).
#[tokio::test]
async fn rw_split_read_after_cleanup_fails() {
    rw_action("cleanup").await;

    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/rw_split_test.php?action=read");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        500,
        "read after cleanup should return 500"
    );

    let body: RwSplitResponse = resp.json().await.expect("failed to parse error JSON");
    assert_eq!(body.status, "error");
    assert!(
        body.message.as_ref().is_some_and(|m| m.contains("rw_test")),
        "error should mention the missing table, got: {:?}",
        body.message
    );
}

//! SQLite via litewire e2e tests.
//!
//! Tests the full stack: PHP (pdo_mysql) → litewire MySQL frontend → SQLite.
//! Requires ephpm configured with `[db.sqlite]` and `sqlite_test.php` in docroot.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance (e.g. `http://ephpm:8080`)

use ephpm_e2e::required_env;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SqliteResponse {
    status: String,
    #[serde(default)]
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Helper to call sqlite_test.php with an action.
async fn sqlite_action(action: &str) -> SqliteResponse {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/sqlite_test.php?action={action}");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "sqlite_test.php?action={action} returned {}",
        resp.status()
    );

    resp.json::<SqliteResponse>()
        .await
        .unwrap_or_else(|e| panic!("failed to parse JSON from {url}: {e}"))
}

/// Helper to call sqlite_test.php with query params.
async fn sqlite_action_with_params(action: &str, params: &str) -> SqliteResponse {
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/sqlite_test.php?action={action}&{params}");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        200,
        "sqlite_test.php?action={action}&{params} returned {}",
        resp.status()
    );

    resp.json::<SqliteResponse>()
        .await
        .unwrap_or_else(|e| panic!("failed to parse JSON from {url}: {e}"))
}

#[tokio::test]
async fn sqlite_create_table_and_insert() {
    // Setup: create table + insert test data
    let resp = sqlite_action("setup").await;
    assert_eq!(resp.status, "ok", "setup failed: {:?}", resp.message);

    // Query: verify the rows are there
    let resp = sqlite_action("query").await;
    assert_eq!(resp.status, "ok", "query failed: {:?}", resp.message);
    assert_eq!(resp.rows.len(), 2, "expected 2 rows, got {}", resp.rows.len());

    assert_eq!(resp.rows[0]["name"], "key1");
    assert_eq!(resp.rows[0]["value"], "hello");
    assert_eq!(resp.rows[1]["name"], "key2");
    assert_eq!(resp.rows[1]["value"], "world");

    // Cleanup
    let resp = sqlite_action("cleanup").await;
    assert_eq!(resp.status, "ok", "cleanup failed: {:?}", resp.message);
}

#[tokio::test]
async fn sqlite_insert_with_params() {
    // Setup table
    let resp = sqlite_action("setup").await;
    assert_eq!(resp.status, "ok");

    // Insert a new row via params
    let resp = sqlite_action_with_params("insert", "name=key3&value=testing").await;
    assert_eq!(resp.status, "ok", "insert failed: {:?}", resp.message);
    assert!(resp.id.is_some(), "insert should return an id");

    // Query and verify
    let resp = sqlite_action("query").await;
    assert_eq!(resp.status, "ok");
    assert_eq!(resp.rows.len(), 3, "expected 3 rows after insert, got {}", resp.rows.len());

    let last_row = &resp.rows[2];
    assert_eq!(last_row["name"], "key3");
    assert_eq!(last_row["value"], "testing");

    // Cleanup
    sqlite_action("cleanup").await;
}

#[tokio::test]
async fn sqlite_cleanup_is_idempotent() {
    // Cleanup when table doesn't exist should succeed (DROP IF EXISTS)
    let resp = sqlite_action("cleanup").await;
    assert_eq!(resp.status, "ok");
}

#[tokio::test]
async fn sqlite_query_after_cleanup_fails() {
    // Ensure table is gone
    sqlite_action("cleanup").await;

    // Query should fail — table doesn't exist
    let base_url = required_env("EPHPM_URL");
    let url = format!("{base_url}/sqlite_test.php?action=query");
    let resp = reqwest::get(&url)
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));

    assert_eq!(
        resp.status().as_u16(),
        500,
        "query after cleanup should return 500"
    );

    let body: SqliteResponse = resp.json().await.expect("failed to parse error JSON");
    assert_eq!(body.status, "error");
    assert!(
        body.message.as_ref().is_some_and(|m| m.contains("test_kv")),
        "error should mention the missing table, got: {:?}",
        body.message
    );
}

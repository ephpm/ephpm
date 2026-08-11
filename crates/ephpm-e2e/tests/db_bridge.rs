//! In-process `ephpm_db_*` bridge e2e tests.
//!
//! Exercises the native `ephpm_db_query()` / `ephpm_db_execute()` PHP
//! functions end to end over HTTP: PHP → C ops table → Rust db_bridge →
//! per-thread litewire Session → embedded SQLite. No PDO, no TCP hop —
//! this is the in-process lane, distinct from the `sqlite` suite's
//! pdo_mysql wire path. Requires ephpm configured with `[db.sqlite]` and
//! `db_bridge_test.php` in the docroot.
//!
//! The suite mutates shared DB state (it creates/drops `bridge_e2e`), so
//! `cargo xtask e2e` runs it as an ISOLATED_DB_SUITE: a fresh node with a
//! fresh database, single-threaded within the suite.
//!
//! Environment variables:
//! - `EPHPM_URL` — base URL of the ephpm instance. Read via
//!   [`required_env`], so the suite FAILS (loudly) rather than skips when
//!   the harness did not provide a server — the fail-don't-skip convention
//!   from #244.

use ephpm_e2e::required_env;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BridgeResponse {
    status: String,
    #[serde(default)]
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    tables: Vec<String>,
    #[serde(default)]
    affected_rows: Option<i64>,
    #[serde(default)]
    last_insert_id: Option<i64>,
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    leaked: Option<i64>,
    #[serde(default)]
    probe: Option<String>,
}

/// GET db_bridge_test.php with an action (+ extra query params), asserting
/// HTTP 200 and parsing the JSON body.
async fn bridge_action(action: &str, params: &str) -> BridgeResponse {
    let (status, body) = bridge_raw(action, params).await;
    assert_eq!(status, 200, "action={action}&{params} returned HTTP {status}: {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("failed to parse JSON for action={action}: {e}\nbody: {body}"))
}

/// GET db_bridge_test.php returning the raw (status, body) — for actions
/// that are expected to fail (the deliberate fatal).
async fn bridge_raw(action: &str, params: &str) -> (u16, String) {
    let base_url = required_env("EPHPM_URL");
    let sep = if params.is_empty() { "" } else { "&" };
    let url = format!("{base_url}/db_bridge_test.php?action={action}{sep}{params}");
    let resp = reqwest::get(&url).await.unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    (status, body)
}

async fn setup() {
    let resp = bridge_action("setup", "").await;
    assert_eq!(resp.status, "ok", "setup failed: {:?}", resp.message);
}

async fn cleanup() {
    let resp = bridge_action("cleanup", "").await;
    assert_eq!(resp.status, "ok", "cleanup failed: {:?}", resp.message);
}

/// CREATE TABLE + INSERT with bound params (checking affected_rows /
/// last_insert_id) + SELECT (checking row values and column-name keys).
#[tokio::test]
async fn create_insert_query_roundtrip() {
    setup().await;

    let ins = bridge_action("insert", "name=alice&value=hello").await;
    assert_eq!(ins.status, "ok", "insert failed: {:?}", ins.message);
    assert_eq!(ins.affected_rows, Some(1), "INSERT must report 1 affected row");
    let first_id = ins.last_insert_id.expect("insert must report last_insert_id");
    assert!(first_id >= 1, "last_insert_id must be >= 1, got {first_id}");

    let ins2 = bridge_action("insert", "name=bob&value=world").await;
    assert_eq!(ins2.affected_rows, Some(1));
    let second_id = ins2.last_insert_id.expect("second insert must report last_insert_id");
    assert!(second_id > first_id, "ids must advance: {first_id} then {second_id}");

    let q = bridge_action("query", "").await;
    assert_eq!(q.status, "ok", "query failed: {:?}", q.message);
    assert_eq!(q.rows.len(), 2, "expected 2 rows, got {:?}", q.rows);
    // Rows are assoc arrays keyed by column name, with native types.
    assert_eq!(q.rows[0]["id"], first_id, "row 0: {:?}", q.rows[0]);
    assert_eq!(q.rows[0]["name"], "alice");
    assert_eq!(q.rows[0]["value"], "hello");
    assert_eq!(q.rows[1]["name"], "bob");
    assert_eq!(q.rows[1]["value"], "world");

    cleanup().await;
}

/// SET NAMES is a dialect noop: OK result, rendered as an empty array.
#[tokio::test]
async fn set_names_is_a_noop_empty_result() {
    let resp = bridge_action("set_names", "").await;
    assert_eq!(resp.status, "ok", "set_names failed: {:?}", resp.message);
    assert!(resp.rows.is_empty(), "SET NAMES must return [], got {:?}", resp.rows);
}

/// SHOW TABLES runs the metadata emulation and lists the created table.
#[tokio::test]
async fn show_tables_lists_the_bridge_table() {
    setup().await;

    let resp = bridge_action("show_tables", "").await;
    assert_eq!(resp.status, "ok", "show_tables failed: {:?}", resp.message);
    assert!(
        resp.tables.iter().any(|t| t == "bridge_e2e"),
        "SHOW TABLES must list bridge_e2e, got {:?}",
        resp.tables
    );

    cleanup().await;
}

/// Invalid SQL surfaces as a PHP Exception with the mapped MySQL error:
/// code 1064, message starting with "SQLSTATE[42000]" (PDO convention).
#[tokio::test]
async fn bad_sql_throws_exception_1064() {
    let resp = bridge_action("bad_sql", "").await;
    assert_eq!(resp.status, "ok", "bad_sql script error: {:?}", resp.message);
    assert_eq!(resp.code, Some(1064), "exception code must be ER_PARSE_ERROR (1064)");
    let message = resp.message.expect("exception message must be reported");
    assert!(
        message.starts_with("SQLSTATE[42000]"),
        "message must start with SQLSTATE[42000], got: {message}"
    );
}

/// Fix-1 acceptance: a request that BEGINs + INSERTs and deliberately does
/// NOT commit must have its transaction rolled back at request end — the
/// uncommitted row is GONE for every later request, and the connection
/// stays healthy (no leaked write lock wedging autocommit writes). Covers
/// both the forgot-to-COMMIT return and the mid-transaction PHP fatal.
#[tokio::test]
async fn abandoned_transaction_is_rolled_back_between_requests() {
    setup().await;

    // Request 1: BEGIN + INSERT 'leaked', return without COMMIT.
    let leak = bridge_action("leak", "").await;
    assert_eq!(leak.status, "ok", "leak request failed: {:?}", leak.message);

    // Requests 2..: the uncommitted row must be gone and writes must work.
    // Checked several times so the assertions land on multiple PHP worker
    // threads — including, probabilistically, the one that ran the BEGIN.
    for i in 0..5 {
        let check = bridge_action("check", "").await;
        assert_eq!(check.status, "ok", "check {i} failed: {:?}", check.message);
        assert_eq!(
            check.leaked,
            Some(0),
            "check {i}: uncommitted 'leaked' row survived — rollback at request end missing"
        );
        assert_eq!(
            check.probe.as_deref(),
            Some("ok"),
            "check {i}: write probe unhealthy — a leaked transaction is holding the write lock"
        );
    }

    // Same leak via a mid-transaction PHP fatal (uncaught Error → 500).
    let (status, body) = bridge_raw("leak_fatal", "").await;
    assert_eq!(status, 500, "leak_fatal must end in a 500, got {status}: {body}");

    for i in 0..5 {
        let check = bridge_action("check", "").await;
        assert_eq!(check.leaked, Some(0), "check {i} after fatal: leaked row survived");
        assert_eq!(check.probe.as_deref(), Some("ok"), "check {i} after fatal: probe unhealthy");
    }

    cleanup().await;
}

//! End-to-end test: MySQL client -> litewire -> SQLite.
//!
//! Starts a litewire MySQL frontend on a random port, connects with
//! `mysql_async`, and exercises basic SQL operations through the full stack.

use std::net::SocketAddr;

use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder};
use tokio::net::TcpListener;

/// Find a free port by binding to :0.
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Start litewire with a MySQL frontend on the given port, backed by in-memory SQLite.
async fn start_litewire(port: u16) -> tokio::task::JoinHandle<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let backend = litewire::backend::Rusqlite::memory().unwrap();
    let config = litewire::litewire_mysql::MysqlFrontendConfig { listen: addr };
    let frontend =
        litewire::litewire_mysql::MysqlFrontend::new(config, std::sync::Arc::new(backend));

    tokio::spawn(async move {
        frontend.serve().await.unwrap();
    })
}

/// Connect to litewire's MySQL frontend.
async fn connect(port: u16) -> Conn {
    let opts: Opts = OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some(""))
        .db_name(Some("test"))
        .into();

    // Retry briefly to let the server start.
    for i in 0..20 {
        match Conn::new(opts.clone()).await {
            Ok(conn) => return conn,
            Err(_) if i < 19 => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            Err(e) => panic!("failed to connect after retries: {e}"),
        }
    }
    unreachable!()
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_test_writer()
        .try_init();
}

#[tokio::test]
async fn select_literal() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    let result: Vec<(i64,)> = conn.query("SELECT 1 + 2").await.unwrap();
    assert_eq!(result, vec![(3,)]);

    drop(conn);
}

#[tokio::test]
async fn create_table_insert_select() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .await
        .unwrap();

    conn.query_drop("INSERT INTO users (id, name) VALUES (1, 'Alice')")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO users (id, name) VALUES (2, 'Bob')")
        .await
        .unwrap();

    let rows: Vec<(i64, String)> = conn
        .query("SELECT id, name FROM users ORDER BY id")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, "Alice".into()), (2, "Bob".into())]);

    drop(conn);
}

#[tokio::test]
async fn now_function_translates() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    // NOW() should be translated to datetime('now') and return a timestamp string.
    let result: Vec<(String,)> = conn.query("SELECT NOW()").await.unwrap();
    assert_eq!(result.len(), 1);
    // Should look like "2024-01-15 12:34:56".
    assert!(
        result[0].0.contains('-'),
        "expected datetime string, got: {}",
        result[0].0
    );

    drop(conn);
}

#[tokio::test]
async fn boolean_literals() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    let result: Vec<(i64, i64)> = conn.query("SELECT TRUE, FALSE").await.unwrap();
    assert_eq!(result, vec![(1, 0)]);

    drop(conn);
}

#[tokio::test]
async fn show_tables() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE alpha (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    conn.query_drop("CREATE TABLE beta (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();

    let result: Vec<(String,)> = conn.query("SHOW TABLES").await.unwrap();
    let names: Vec<&str> = result.iter().map(|r| r.0.as_str()).collect();
    assert!(names.contains(&"alpha"), "got: {names:?}");
    assert!(names.contains(&"beta"), "got: {names:?}");

    drop(conn);
}

#[tokio::test]
async fn set_names_noop() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    // Should succeed without error (no-op).
    conn.query_drop("SET NAMES utf8mb4").await.unwrap();

    // Verify connection still works after.
    let result: Vec<(i64,)> = conn.query("SELECT 42").await.unwrap();
    assert_eq!(result, vec![(42,)]);

    drop(conn);
}

#[tokio::test]
async fn use_database_noop() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    // USE should succeed (SQLite has one database).
    conn.query_drop("SELECT 1").await.unwrap();
    // COM_INIT_DB is sent during connection setup via db_name("test").
    // If we got this far, it worked.

    drop(conn);
}

#[tokio::test]
async fn update_and_delete() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO items VALUES (1, 10)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO items VALUES (2, 20)")
        .await
        .unwrap();

    conn.query_drop("UPDATE items SET qty = 15 WHERE id = 1")
        .await
        .unwrap();

    let rows: Vec<(i64, i64)> = conn
        .query("SELECT id, qty FROM items ORDER BY id")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, 15), (2, 20)]);

    conn.query_drop("DELETE FROM items WHERE id = 2")
        .await
        .unwrap();

    let rows: Vec<(i64, i64)> = conn.query("SELECT id, qty FROM items").await.unwrap();
    assert_eq!(rows, vec![(1, 15)]);

    drop(conn);
}

#[tokio::test]
async fn multiple_connections() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;

    let mut conn1 = connect(port).await;
    conn1
        .query_drop("CREATE TABLE shared (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap();
    conn1
        .query_drop("INSERT INTO shared VALUES (1, 'from_conn1')")
        .await
        .unwrap();
    drop(conn1);

    // Second connection should see the data (same in-memory SQLite).
    let mut conn2 = connect(port).await;
    let rows: Vec<(i64, String)> = conn2
        .query("SELECT id, val FROM shared")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, "from_conn1".into())]);

    drop(conn2);
}

// ── Prepared statement tests ────────────────────────────────────────────────

#[tokio::test]
async fn prepared_select_with_param() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO users VALUES (1, 'Alice')")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO users VALUES (2, 'Bob')")
        .await
        .unwrap();

    // Prepared SELECT with parameter binding.
    let rows: Vec<(i64, String)> = conn
        .exec("SELECT id, name FROM users WHERE id = ?", (1_i64,))
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, "Alice".into())]);

    let rows: Vec<(i64, String)> = conn
        .exec("SELECT id, name FROM users WHERE id = ?", (2_i64,))
        .await
        .unwrap();
    assert_eq!(rows, vec![(2, "Bob".into())]);

    drop(conn);
}

#[tokio::test]
async fn prepared_insert() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, qty INTEGER)")
        .await
        .unwrap();

    // Prepared INSERT with parameters.
    conn.exec_drop(
        "INSERT INTO items (id, name, qty) VALUES (?, ?, ?)",
        (1_i64, "Widget", 10_i64),
    )
    .await
    .unwrap();

    conn.exec_drop(
        "INSERT INTO items (id, name, qty) VALUES (?, ?, ?)",
        (2_i64, "Gadget", 20_i64),
    )
    .await
    .unwrap();

    let rows: Vec<(i64, String, i64)> = conn
        .query("SELECT id, name, qty FROM items ORDER BY id")
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (1, "Widget".into(), 10),
            (2, "Gadget".into(), 20),
        ]
    );

    drop(conn);
}

#[tokio::test]
async fn prepared_update_and_delete() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap();
    conn.query_drop("INSERT INTO t VALUES (1, 'old')")
        .await
        .unwrap();

    conn.exec_drop("UPDATE t SET val = ? WHERE id = ?", ("new", 1_i64))
        .await
        .unwrap();

    let rows: Vec<(i64, String)> = conn.query("SELECT id, val FROM t").await.unwrap();
    assert_eq!(rows, vec![(1, "new".into())]);

    conn.exec_drop("DELETE FROM t WHERE id = ?", (1_i64,))
        .await
        .unwrap();

    let rows: Vec<(i64, String)> = conn.query("SELECT id, val FROM t").await.unwrap();
    assert!(rows.is_empty());

    drop(conn);
}

#[tokio::test]
async fn prepared_with_null_param() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap();

    conn.exec_drop(
        "INSERT INTO t (id, val) VALUES (?, ?)",
        (1_i64, Option::<String>::None),
    )
    .await
    .unwrap();

    let rows: Vec<(i64, Option<String>)> = conn.query("SELECT id, val FROM t").await.unwrap();
    assert_eq!(rows, vec![(1, None)]);

    drop(conn);
}

#[tokio::test]
async fn prepared_reuse_same_statement() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();

    // Execute the same prepared statement multiple times.
    let stmt = conn.prep("INSERT INTO t (id) VALUES (?)").await.unwrap();

    conn.exec_drop(&stmt, (1_i64,)).await.unwrap();
    conn.exec_drop(&stmt, (2_i64,)).await.unwrap();
    conn.exec_drop(&stmt, (3_i64,)).await.unwrap();

    // Drop the prepared statement (sends COM_STMT_CLOSE).
    drop(stmt);

    let rows: Vec<(i64,)> = conn.query("SELECT id FROM t ORDER BY id").await.unwrap();
    assert_eq!(rows, vec![(1,), (2,), (3,)]);

    drop(conn);
}

// ── Translation gap tests ───────────────────────────────────────────────────

#[tokio::test]
async fn on_duplicate_key_update() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)")
        .await
        .unwrap();

    conn.query_drop("INSERT INTO kv (k, v) VALUES ('a', 1)")
        .await
        .unwrap();

    // MySQL ON DUPLICATE KEY UPDATE -> SQLite ON CONFLICT DO UPDATE
    conn.query_drop(
        "INSERT INTO kv (k, v) VALUES ('a', 99) ON DUPLICATE KEY UPDATE v = 99",
    )
    .await
    .unwrap();

    let rows: Vec<(String, i64)> = conn
        .query("SELECT k, v FROM kv WHERE k = 'a'")
        .await
        .unwrap();
    assert_eq!(rows, vec![("a".into(), 99)]);

    // Insert new row (no conflict).
    conn.query_drop(
        "INSERT INTO kv (k, v) VALUES ('b', 2) ON DUPLICATE KEY UPDATE v = 2",
    )
    .await
    .unwrap();

    let rows: Vec<(String, i64)> = conn
        .query("SELECT k, v FROM kv ORDER BY k")
        .await
        .unwrap();
    assert_eq!(rows, vec![("a".into(), 99), ("b".into(), 2)]);

    drop(conn);
}

// ── Transaction tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn transaction_commit() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE txn_t (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap();

    conn.query_drop("BEGIN").await.unwrap();
    conn.query_drop("INSERT INTO txn_t VALUES (1, 'inside_txn')")
        .await
        .unwrap();
    conn.query_drop("COMMIT").await.unwrap();

    // Data should be visible after commit.
    let rows: Vec<(i64, String)> = conn
        .query("SELECT id, val FROM txn_t")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, "inside_txn".into())]);

    drop(conn);
}

#[tokio::test]
async fn transaction_rollback() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE txn_rb (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap();

    conn.query_drop("INSERT INTO txn_rb VALUES (1, 'before')")
        .await
        .unwrap();

    conn.query_drop("BEGIN").await.unwrap();
    conn.query_drop("INSERT INTO txn_rb VALUES (2, 'rolled_back')")
        .await
        .unwrap();
    conn.query_drop("ROLLBACK").await.unwrap();

    // Only the row inserted before the transaction should exist.
    let rows: Vec<(i64, String)> = conn
        .query("SELECT id, val FROM txn_rb ORDER BY id")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, "before".into())]);

    drop(conn);
}

#[tokio::test]
async fn transaction_atomicity() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE txn_atom (id INTEGER PRIMARY KEY, val INTEGER)")
        .await
        .unwrap();

    conn.query_drop("INSERT INTO txn_atom VALUES (1, 100)")
        .await
        .unwrap();

    // Begin a transaction, update, then rollback — value should remain 100.
    conn.query_drop("BEGIN").await.unwrap();
    conn.query_drop("UPDATE txn_atom SET val = 200 WHERE id = 1")
        .await
        .unwrap();
    conn.query_drop("ROLLBACK").await.unwrap();

    let rows: Vec<(i64, i64)> = conn
        .query("SELECT id, val FROM txn_atom")
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, 100)]);

    drop(conn);
}

#[tokio::test]
async fn information_schema_tables() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE users (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    conn.query_drop("CREATE TABLE posts (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();

    // Returns TABLE_NAME, TABLE_TYPE, TABLE_SCHEMA columns.
    let rows: Vec<(String, String, String)> = conn
        .query("SELECT TABLE_NAME FROM information_schema.tables")
        .await
        .unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert!(names.contains(&"users"), "got: {names:?}");
    assert!(names.contains(&"posts"), "got: {names:?}");

    drop(conn);
}

#[tokio::test]
async fn information_schema_columns() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT)")
        .await
        .unwrap();

    // Returns TABLE_NAME, COLUMN_NAME, IS_NULLABLE, DATA_TYPE, COLUMN_DEFAULT, ORDINAL_POSITION.
    let rows: Vec<mysql_async::Row> = conn
        .query("SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.columns WHERE TABLE_NAME = 'users'")
        .await
        .unwrap();
    assert!(rows.len() >= 3, "expected at least 3 columns, got {}", rows.len());
    // Check that column names are present in the result.
    let col_names: Vec<String> = rows
        .iter()
        .map(|r| r.get::<String, _>(1).unwrap_or_default())
        .collect();
    assert!(col_names.contains(&"id".to_string()), "got: {col_names:?}");
    assert!(col_names.contains(&"name".to_string()), "got: {col_names:?}");
    assert!(col_names.contains(&"email".to_string()), "got: {col_names:?}");

    drop(conn);
}

#[tokio::test]
async fn describe_table() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut conn = connect(port).await;

    conn.query_drop("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, qty INTEGER)")
        .await
        .unwrap();

    // DESCRIBE should return column info via PRAGMA table_info.
    let rows: Vec<mysql_async::Row> = conn.query("DESCRIBE items").await.unwrap();
    assert!(rows.len() >= 3, "expected at least 3 columns, got {}", rows.len());

    drop(conn);
}

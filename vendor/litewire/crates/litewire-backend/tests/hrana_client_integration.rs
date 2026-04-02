//! Integration tests for HranaClient against a real in-process Hrana server.
//!
//! These tests spin up an `HranaFrontend` backed by `Rusqlite::memory()`,
//! then connect an `HranaClient` and run queries end-to-end.

#![cfg(all(feature = "rusqlite", feature = "hrana-client"))]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use litewire_backend::{Backend, HranaClient, Rusqlite, Value};
use litewire_hrana::HranaFrontend;
use litewire_hrana::HranaFrontendConfig;

/// Start a Hrana server on a random port, return the client and server task handle.
async fn start_server() -> (HranaClient, tokio::task::JoinHandle<()>) {
    // Bind to port 0 to get a random available port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind");
    let addr: SocketAddr = listener.local_addr().unwrap();

    let backend = Rusqlite::memory().expect("failed to create in-memory SQLite");
    let shared: Arc<dyn Backend> = Arc::new(backend);

    let config = HranaFrontendConfig { listen: addr };
    let frontend = HranaFrontend::new(config, shared);

    // We can't reuse the listener from above since HranaFrontend binds its own.
    // Drop and let the frontend bind the same port — but this may race.
    // Instead, we'll use a different approach: start the server on port 0,
    // but we need the actual port. Let's just pick a port ourselves.
    drop(listener);

    let handle = tokio::spawn(async move {
        frontend.serve().await.expect("Hrana server failed");
    });

    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = HranaClient::new(&format!("http://{addr}"));
    (client, handle)
}

#[tokio::test]
async fn health_check() {
    let (client, _server) = start_server().await;
    client.health_check().await.expect("health check failed");
}

#[tokio::test]
async fn create_table_and_insert() {
    let (client, _server) = start_server().await;

    // Create table
    let result = client
        .execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", &[])
        .await
        .expect("CREATE TABLE failed");
    assert_eq!(result.affected_rows, 0);

    // Insert
    let result = client
        .execute(
            "INSERT INTO users (name) VALUES (?)",
            &[Value::Text("alice".into())],
        )
        .await
        .expect("INSERT failed");
    assert_eq!(result.affected_rows, 1);
    assert_eq!(result.last_insert_rowid, Some(1));

    // Insert another
    let result = client
        .execute(
            "INSERT INTO users (name) VALUES (?)",
            &[Value::Text("bob".into())],
        )
        .await
        .expect("INSERT failed");
    assert_eq!(result.affected_rows, 1);
    assert_eq!(result.last_insert_rowid, Some(2));
}

#[tokio::test]
async fn query_rows() {
    let (client, _server) = start_server().await;

    client
        .execute("CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT, price REAL)", &[])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO items (label, price) VALUES (?, ?)",
            &[Value::Text("widget".into()), Value::Float(9.99)],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO items (label, price) VALUES (?, ?)",
            &[Value::Text("gadget".into()), Value::Float(19.99)],
        )
        .await
        .unwrap();

    let rs = client
        .query("SELECT id, label, price FROM items ORDER BY id", &[])
        .await
        .expect("query failed");

    assert_eq!(rs.columns.len(), 3);
    assert_eq!(rs.columns[0].name, "id");
    assert_eq!(rs.columns[1].name, "label");
    assert_eq!(rs.columns[2].name, "price");

    assert_eq!(rs.rows.len(), 2);
    assert_eq!(rs.rows[0][0], Value::Integer(1));
    assert_eq!(rs.rows[0][1], Value::Text("widget".into()));
    assert_eq!(rs.rows[0][2], Value::Float(9.99));
    assert_eq!(rs.rows[1][0], Value::Integer(2));
    assert_eq!(rs.rows[1][1], Value::Text("gadget".into()));
}

#[tokio::test]
async fn query_with_params() {
    let (client, _server) = start_server().await;

    client
        .execute("CREATE TABLE kv (key TEXT, val TEXT)", &[])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO kv (key, val) VALUES (?, ?)",
            &[Value::Text("a".into()), Value::Text("1".into())],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO kv (key, val) VALUES (?, ?)",
            &[Value::Text("b".into()), Value::Text("2".into())],
        )
        .await
        .unwrap();

    let rs = client
        .query(
            "SELECT val FROM kv WHERE key = ?",
            &[Value::Text("b".into())],
        )
        .await
        .unwrap();

    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Text("2".into()));
}

#[tokio::test]
async fn query_empty_result() {
    let (client, _server) = start_server().await;

    client
        .execute("CREATE TABLE empty_test (id INTEGER)", &[])
        .await
        .unwrap();

    let rs = client
        .query("SELECT id FROM empty_test", &[])
        .await
        .unwrap();

    assert_eq!(rs.columns.len(), 1);
    assert!(rs.rows.is_empty());
}

#[tokio::test]
async fn blob_roundtrip() {
    let (client, _server) = start_server().await;

    client
        .execute("CREATE TABLE blobs (id INTEGER PRIMARY KEY, data BLOB)", &[])
        .await
        .unwrap();

    let blob_data = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
    client
        .execute(
            "INSERT INTO blobs (data) VALUES (?)",
            &[Value::Blob(blob_data.clone())],
        )
        .await
        .unwrap();

    let rs = client
        .query("SELECT data FROM blobs WHERE id = 1", &[])
        .await
        .unwrap();

    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Blob(blob_data));
}

#[tokio::test]
async fn null_values() {
    let (client, _server) = start_server().await;

    client
        .execute("CREATE TABLE nullable (id INTEGER, val TEXT)", &[])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO nullable (id, val) VALUES (?, ?)",
            &[Value::Integer(1), Value::Null],
        )
        .await
        .unwrap();

    let rs = client
        .query("SELECT val FROM nullable WHERE id = 1", &[])
        .await
        .unwrap();

    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Null);
}

#[tokio::test]
async fn sql_error_returns_backend_error() {
    let (client, _server) = start_server().await;

    let result = client
        .query("SELECT * FROM nonexistent_table", &[])
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("no such table") || err.contains("nonexistent"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn update_and_delete() {
    let (client, _server) = start_server().await;

    client
        .execute("CREATE TABLE counters (name TEXT, val INTEGER)", &[])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO counters VALUES ('hits', 0)",
            &[],
        )
        .await
        .unwrap();

    let result = client
        .execute(
            "UPDATE counters SET val = val + 1 WHERE name = 'hits'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(result.affected_rows, 1);

    let result = client
        .execute("DELETE FROM counters WHERE name = 'hits'", &[])
        .await
        .unwrap();
    assert_eq!(result.affected_rows, 1);

    let rs = client
        .query("SELECT COUNT(*) FROM counters", &[])
        .await
        .unwrap();
    assert_eq!(rs.rows[0][0], Value::Integer(0));
}

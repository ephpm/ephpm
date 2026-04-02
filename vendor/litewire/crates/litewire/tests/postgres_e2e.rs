//! End-to-end test: PostgreSQL client -> litewire -> SQLite.
//!
//! Starts a litewire PostgreSQL frontend on a random port, connects with
//! `tokio-postgres`, and exercises basic SQL operations through the full stack.

#![cfg(feature = "postgres")]

use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio_postgres::{Client, NoTls};

/// Find a free port by binding to :0.
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Start litewire with a PostgreSQL frontend on the given port, backed by in-memory SQLite.
async fn start_litewire(port: u16) -> tokio::task::JoinHandle<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let backend = litewire::backend::Rusqlite::memory().unwrap();
    let config = litewire::litewire_postgres::PostgresFrontendConfig { listen: addr };
    let frontend =
        litewire::litewire_postgres::PostgresFrontend::new(config, std::sync::Arc::new(backend));

    tokio::spawn(async move {
        frontend.serve().await.unwrap();
    })
}

/// Connect to litewire's PostgreSQL frontend with retry.
async fn connect(port: u16) -> Client {
    let connstr = format!("host=127.0.0.1 port={port} user=test dbname=test");

    for i in 0..20 {
        match tokio_postgres::connect(&connstr, NoTls).await {
            Ok((client, connection)) => {
                // Spawn the connection driver.
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        tracing::debug!("PG connection ended: {e}");
                    }
                });
                return client;
            }
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

// ── Simple query tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn select_literal() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    let rows = client.query("SELECT 1 + 2", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    let val: i64 = rows[0].get(0);
    assert_eq!(val, 3);
}

#[tokio::test]
async fn create_table_insert_select() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            &[],
        )
        .await
        .unwrap();

    client
        .execute("INSERT INTO users (id, name) VALUES (1, 'Alice')", &[])
        .await
        .unwrap();
    client
        .execute("INSERT INTO users (id, name) VALUES (2, 'Bob')", &[])
        .await
        .unwrap();

    let rows = client
        .query("SELECT id, name FROM users ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);

    let id: i64 = rows[0].get(0);
    let name: &str = rows[0].get(1);
    assert_eq!(id, 1);
    assert_eq!(name, "Alice");

    let id: i64 = rows[1].get(0);
    let name: &str = rows[1].get(1);
    assert_eq!(id, 2);
    assert_eq!(name, "Bob");
}

#[tokio::test]
async fn update_and_delete() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
            &[],
        )
        .await
        .unwrap();
    client
        .execute("INSERT INTO items VALUES (1, 10)", &[])
        .await
        .unwrap();
    client
        .execute("INSERT INTO items VALUES (2, 20)", &[])
        .await
        .unwrap();

    client
        .execute("UPDATE items SET qty = 15 WHERE id = 1", &[])
        .await
        .unwrap();

    let rows = client
        .query("SELECT id, qty FROM items ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let qty: i64 = rows[0].get(1);
    assert_eq!(qty, 15);

    client
        .execute("DELETE FROM items WHERE id = 2", &[])
        .await
        .unwrap();

    let rows = client
        .query("SELECT id, qty FROM items", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let id: i64 = rows[0].get(0);
    assert_eq!(id, 1);
}

#[tokio::test]
async fn boolean_literals() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    let rows = client.query("SELECT TRUE, FALSE", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    // SQLite translates TRUE/FALSE to 1/0.
    let t: i64 = rows[0].get(0);
    let f: i64 = rows[0].get(1);
    assert_eq!(t, 1);
    assert_eq!(f, 0);
}

#[tokio::test]
async fn now_function_translates() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    let rows = client.query("SELECT NOW()", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    let val: &str = rows[0].get(0);
    // Should look like "2024-01-15 12:34:56".
    assert!(
        val.contains('-'),
        "expected datetime string, got: {val}",
    );
}

#[tokio::test]
async fn set_names_noop() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    // SET NAMES should succeed as a no-op.
    client
        .execute("SET NAMES 'utf8mb4'", &[])
        .await
        .unwrap();

    // Connection still works after.
    let rows = client.query("SELECT 42", &[]).await.unwrap();
    let val: i64 = rows[0].get(0);
    assert_eq!(val, 42);
}

#[tokio::test]
async fn multiple_connections() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;

    let client1 = connect(port).await;
    client1
        .execute(
            "CREATE TABLE shared (id INTEGER PRIMARY KEY, val TEXT)",
            &[],
        )
        .await
        .unwrap();
    client1
        .execute("INSERT INTO shared VALUES (1, 'from_conn1')", &[])
        .await
        .unwrap();
    drop(client1);

    // Second connection should see the data.
    let client2 = connect(port).await;
    let rows = client2
        .query("SELECT id, val FROM shared", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let val: &str = rows[0].get(1);
    assert_eq!(val, "from_conn1");
}

#[tokio::test]
async fn empty_table_query() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute("CREATE TABLE empty_t (id INTEGER PRIMARY KEY, val TEXT)", &[])
        .await
        .unwrap();

    let rows = client.query("SELECT * FROM empty_t", &[]).await.unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn drop_table() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute("CREATE TABLE temp_t (id INTEGER PRIMARY KEY)", &[])
        .await
        .unwrap();
    client.execute("DROP TABLE temp_t", &[]).await.unwrap();

    // Selecting from dropped table should fail.
    let err = client.query("SELECT * FROM temp_t", &[]).await;
    assert!(err.is_err());
}

// ── PG-specific type translation tests ─────────────────────────────────────

#[tokio::test]
async fn pg_serial_to_integer() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    // SERIAL should be translated to INTEGER.
    client
        .execute(
            "CREATE TABLE auto_t (id SERIAL PRIMARY KEY, name TEXT)",
            &[],
        )
        .await
        .unwrap();

    client
        .execute("INSERT INTO auto_t (name) VALUES ('first')", &[])
        .await
        .unwrap();
    client
        .execute("INSERT INTO auto_t (name) VALUES ('second')", &[])
        .await
        .unwrap();

    let rows = client
        .query("SELECT id, name FROM auto_t ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let name: &str = rows[0].get(1);
    assert_eq!(name, "first");
}

#[tokio::test]
async fn pg_varchar_to_text() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute(
            "CREATE TABLE typed_t (name VARCHAR(255), bio TEXT, data BYTEA)",
            &[],
        )
        .await
        .unwrap();

    client
        .execute(
            "INSERT INTO typed_t (name, bio, data) VALUES ('test', 'a bio', 'raw')",
            &[],
        )
        .await
        .unwrap();

    let rows = client
        .query("SELECT name, bio FROM typed_t", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let name: &str = rows[0].get(0);
    assert_eq!(name, "test");
}

#[tokio::test]
async fn pg_boolean_column() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    // BOOLEAN should be translated to INTEGER in SQLite.
    client
        .execute(
            "CREATE TABLE flags (id INTEGER PRIMARY KEY, active BOOLEAN)",
            &[],
        )
        .await
        .unwrap();

    client
        .execute("INSERT INTO flags VALUES (1, TRUE)", &[])
        .await
        .unwrap();
    client
        .execute("INSERT INTO flags VALUES (2, FALSE)", &[])
        .await
        .unwrap();

    let rows = client
        .query("SELECT id, active FROM flags ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let active: i64 = rows[0].get(1);
    assert_eq!(active, 1);
    let inactive: i64 = rows[1].get(1);
    assert_eq!(inactive, 0);
}

// ── Multi-statement & edge cases ───────────────────────────────────────────

#[tokio::test]
async fn large_result_set() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute("CREATE TABLE big_t (id INTEGER PRIMARY KEY, val TEXT)", &[])
        .await
        .unwrap();

    // Insert 100 rows.
    for i in 0..100 {
        client
            .execute(
                &format!("INSERT INTO big_t VALUES ({i}, 'row_{i}')"),
                &[],
            )
            .await
            .unwrap();
    }

    let rows = client
        .query("SELECT id, val FROM big_t ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 100);

    let first_id: i64 = rows[0].get(0);
    let last_id: i64 = rows[99].get(0);
    assert_eq!(first_id, 0);
    assert_eq!(last_id, 99);
}

#[tokio::test]
async fn null_handling() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute(
            "CREATE TABLE nullable (id INTEGER PRIMARY KEY, val TEXT)",
            &[],
        )
        .await
        .unwrap();

    client
        .execute("INSERT INTO nullable VALUES (1, NULL)", &[])
        .await
        .unwrap();
    client
        .execute("INSERT INTO nullable VALUES (2, 'present')", &[])
        .await
        .unwrap();

    let rows = client
        .query("SELECT id, val FROM nullable ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);

    let val: Option<&str> = rows[0].get(1);
    assert!(val.is_none());

    let val: Option<&str> = rows[1].get(1);
    assert_eq!(val, Some("present"));
}

// ── Transaction tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn transaction_commit() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    // Use batch_execute (simple query protocol) for all transaction-related ops.
    client
        .batch_execute("CREATE TABLE txn_t (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap();

    client.batch_execute("BEGIN").await.unwrap();
    client
        .batch_execute("INSERT INTO txn_t VALUES (1, 'inside_txn')")
        .await
        .unwrap();
    client.batch_execute("COMMIT").await.unwrap();

    // Data should be visible after commit.
    let rows = client
        .query("SELECT id, val FROM txn_t", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let val: &str = rows[0].get(1);
    assert_eq!(val, "inside_txn");
}

#[tokio::test]
async fn transaction_rollback() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .batch_execute("CREATE TABLE txn_rb (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap();

    client
        .batch_execute("INSERT INTO txn_rb VALUES (1, 'before')")
        .await
        .unwrap();

    client.batch_execute("BEGIN").await.unwrap();
    client
        .batch_execute("INSERT INTO txn_rb VALUES (2, 'rolled_back')")
        .await
        .unwrap();
    client.batch_execute("ROLLBACK").await.unwrap();

    // Only the row inserted before the transaction should exist.
    let rows = client
        .query("SELECT id, val FROM txn_rb ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let val: &str = rows[0].get(1);
    assert_eq!(val, "before");
}

#[tokio::test]
async fn transaction_atomicity() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .batch_execute("CREATE TABLE txn_atom (id INTEGER PRIMARY KEY, val INTEGER)")
        .await
        .unwrap();

    client
        .batch_execute("INSERT INTO txn_atom VALUES (1, 100)")
        .await
        .unwrap();

    // Begin a transaction, update, then rollback — value should remain 100.
    client.batch_execute("BEGIN").await.unwrap();
    client
        .batch_execute("UPDATE txn_atom SET val = 200 WHERE id = 1")
        .await
        .unwrap();
    client.batch_execute("ROLLBACK").await.unwrap();

    let rows = client
        .query("SELECT id, val FROM txn_atom", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let val: i64 = rows[0].get(1);
    assert_eq!(val, 100);
}

#[tokio::test]
async fn float_values() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let client = connect(port).await;

    client
        .execute(
            "CREATE TABLE floats (id INTEGER PRIMARY KEY, val REAL)",
            &[],
        )
        .await
        .unwrap();

    client
        .execute("INSERT INTO floats VALUES (1, 3.14)", &[])
        .await
        .unwrap();

    let rows = client
        .query("SELECT val FROM floats WHERE id = 1", &[])
        .await
        .unwrap();
    let val: f64 = rows[0].get(0);
    assert!((val - 3.14).abs() < 0.001);
}

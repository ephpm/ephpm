//! End-to-end test: TDS (SQL Server) client -> litewire -> SQLite.
//!
//! Starts a litewire TDS frontend on a random port, connects with
//! `tiberius`, and exercises basic SQL operations through the full stack.

#![cfg(feature = "tds")]

use std::net::SocketAddr;

use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpListener;
use tokio_util::compat::TokioAsyncWriteCompatExt;

/// Find a free port by binding to :0.
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Start litewire with a TDS frontend on the given port, backed by in-memory SQLite.
async fn start_litewire(port: u16) -> tokio::task::JoinHandle<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let backend = litewire::backend::Rusqlite::memory().unwrap();
    let config = litewire::litewire_tds::TdsFrontendConfig { listen: addr };
    let frontend =
        litewire::litewire_tds::TdsFrontend::new(config, std::sync::Arc::new(backend));

    tokio::spawn(async move {
        frontend.serve().await.unwrap();
    })
}

/// Connect to litewire's TDS frontend with retry.
async fn connect(port: u16) -> Client<tokio_util::compat::Compat<tokio::net::TcpStream>> {
    let mut config = Config::new();
    config.host("127.0.0.1");
    config.port(port);
    config.authentication(AuthMethod::sql_server("sa", "password"));
    config.encryption(EncryptionLevel::NotSupported);
    config.trust_cert();

    for i in 0..20 {
        match tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await {
            Ok(tcp) => {
                tcp.set_nodelay(true).unwrap();
                match Client::connect(config.clone(), tcp.compat_write()).await {
                    Ok(client) => return client,
                    Err(_) if i < 19 => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        // Need a new config since we consumed it
                        config = Config::new();
                        config.host("127.0.0.1");
                        config.port(port);
                        config.authentication(AuthMethod::sql_server("sa", "password"));
                        config.encryption(EncryptionLevel::NotSupported);
                        config.trust_cert();
                    }
                    Err(e) => panic!("TDS connect failed after retries: {e}"),
                }
            }
            Err(_) if i < 19 => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => panic!("TCP connect failed after retries: {e}"),
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
    let mut client = connect(port).await;

    let stream = client.simple_query("SELECT 1 + 2").await.unwrap();
    let row = stream.into_row().await.unwrap().unwrap();
    // Expression results infer type from value — i64 for integers.
    let val: i64 = row.get(0).unwrap();
    assert_eq!(val, 3);
}

#[tokio::test]
async fn create_table_insert_select() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("INSERT INTO users (id, name) VALUES (1, 'Alice')")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO users (id, name) VALUES (2, 'Bob')")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT id, name FROM users ORDER BY id")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 2);

    let name: &str = rows[0].get(1).unwrap();
    assert_eq!(name, "Alice");

    let name: &str = rows[1].get(1).unwrap();
    assert_eq!(name, "Bob");
}

#[tokio::test]
async fn update_and_delete() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO items VALUES (1, 10)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO items VALUES (2, 20)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("UPDATE items SET qty = 15 WHERE id = 1")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT id, qty FROM items ORDER BY id")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 2);

    let qty: i64 = rows[0].get(1).unwrap();
    assert_eq!(qty, 15);

    client
        .simple_query("DELETE FROM items WHERE id = 2")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT id, qty FROM items")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn empty_table_query() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE empty_t (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT * FROM empty_t")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn drop_table() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE temp_t (id INTEGER PRIMARY KEY)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("DROP TABLE temp_t")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    // Selecting from dropped table should fail.
    let result = client.simple_query("SELECT * FROM temp_t").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn null_handling() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE nullable (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("INSERT INTO nullable VALUES (1, NULL)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO nullable VALUES (2, 'present')")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT id, val FROM nullable ORDER BY id")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 2);

    let val: Option<&str> = rows[0].get(1);
    assert!(val.is_none());

    let val: Option<&str> = rows[1].get(1);
    assert_eq!(val, Some("present"));
}

#[tokio::test]
async fn multiple_connections() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;

    {
        let mut client1 = connect(port).await;
        client1
            .simple_query("CREATE TABLE shared (id INTEGER PRIMARY KEY, val TEXT)")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
        client1
            .simple_query("INSERT INTO shared VALUES (1, 'from_conn1')")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
    }
    // client1 dropped

    let mut client2 = connect(port).await;
    let stream = client2
        .simple_query("SELECT id, val FROM shared")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 1);
    let val: &str = rows[0].get(1).unwrap();
    assert_eq!(val, "from_conn1");
}

#[tokio::test]
async fn large_result_set() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE big_t (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    // Insert 100 rows.
    for i in 0..100 {
        client
            .simple_query(&format!("INSERT INTO big_t VALUES ({i}, 'row_{i}')"))
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
    }

    let stream = client
        .simple_query("SELECT id, val FROM big_t ORDER BY id")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 100);
}

// ── Transaction tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn transaction_commit() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE txn_t (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("BEGIN TRANSACTION")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO txn_t VALUES (1, 'inside_txn')")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("COMMIT")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    // Data should be visible after commit.
    let stream = client
        .simple_query("SELECT id, val FROM txn_t")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 1);
    let val: &str = rows[0].get(1).unwrap();
    assert_eq!(val, "inside_txn");
}

#[tokio::test]
async fn transaction_rollback() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE txn_rb (id INTEGER PRIMARY KEY, val TEXT)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("INSERT INTO txn_rb VALUES (1, 'before')")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("BEGIN TRANSACTION")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO txn_rb VALUES (2, 'rolled_back')")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("ROLLBACK")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    // Only the row inserted before the transaction should exist.
    let stream = client
        .simple_query("SELECT id, val FROM txn_rb ORDER BY id")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 1);
    let val: &str = rows[0].get(1).unwrap();
    assert_eq!(val, "before");
}

#[tokio::test]
async fn transaction_atomicity() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE txn_atom (id INTEGER PRIMARY KEY, val INTEGER)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("INSERT INTO txn_atom VALUES (1, 100)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    // Begin a transaction, update, then rollback — value should remain 100.
    client
        .simple_query("BEGIN TRANSACTION")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("UPDATE txn_atom SET val = 200 WHERE id = 1")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    client
        .simple_query("ROLLBACK")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT id, val FROM txn_atom")
        .await
        .unwrap();
    let rows = stream.into_first_result().await.unwrap();
    assert_eq!(rows.len(), 1);
    let val: i64 = rows[0].get(1).unwrap();
    assert_eq!(val, 100);
}

// ── T-SQL specific translation tests ───────────────────────────────────────

#[tokio::test]
async fn getdate_translates() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    let stream = client.simple_query("SELECT GETDATE()").await.unwrap();
    let row = stream.into_row().await.unwrap().unwrap();
    let val: &str = row.get(0).unwrap();
    // Should look like a datetime string.
    assert!(
        val.contains('-'),
        "expected datetime string, got: {val}",
    );
}

#[tokio::test]
async fn integer_column_types() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE typed_ints (id INTEGER PRIMARY KEY, count INTEGER)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("INSERT INTO typed_ints VALUES (1, 42)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT id, count FROM typed_ints WHERE id = 1")
        .await
        .unwrap();
    let row = stream.into_row().await.unwrap().unwrap();
    let count: i64 = row.get(1).unwrap();
    assert_eq!(count, 42);
}

#[tokio::test]
async fn float_column_types() {
    init_tracing();
    let port = free_port().await;
    let _server = start_litewire(port).await;
    let mut client = connect(port).await;

    client
        .simple_query("CREATE TABLE typed_floats (id INTEGER PRIMARY KEY, val REAL)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    client
        .simple_query("INSERT INTO typed_floats VALUES (1, 3.14)")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();

    let stream = client
        .simple_query("SELECT val FROM typed_floats WHERE id = 1")
        .await
        .unwrap();
    let row = stream.into_row().await.unwrap().unwrap();
    let val: f64 = row.get(0).unwrap();
    assert!((val - 3.14).abs() < 0.001);
}

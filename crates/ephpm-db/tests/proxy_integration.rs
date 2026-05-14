//! Integration tests for the `MySQL` proxy.
//!
//! These tests require a running `MySQL` server. Set `MYSQL_TEST_URL` to a
//! connection string (e.g. `mysql://root:test@127.0.0.1:3306/test`) to enable
//! them. All tests are `#[ignore]` so they only run in nightly CI via
//! `cargo test -- --ignored`.

use std::time::Duration;

use ephpm_db::ResetStrategy;
use ephpm_db::mysql::{MySqlProxy, RwSplitParams};
use ephpm_db::pool::PoolConfig;
use mysql_async::prelude::*;

/// Read `MYSQL_TEST_URL` or return `None` (caller should skip).
fn mysql_url() -> Option<String> {
    std::env::var("MYSQL_TEST_URL").ok()
}

/// Build a default pool config suitable for tests.
fn test_pool_config() -> PoolConfig {
    PoolConfig {
        min_connections: 1,
        max_connections: 5,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        health_check_interval: Duration::from_secs(30),
    }
}

/// Boot a [`MySqlProxy`] on a random OS-assigned port and return the listen
/// address (e.g. `127.0.0.1:XXXXX`).
async fn start_proxy(
    backend_url: &str,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
) -> String {
    // Bind to port 0 so the OS assigns a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listen_addr = addr.to_string();

    // Build the proxy pointed at the real backend.
    let proxy = MySqlProxy::new(
        backend_url,
        &listen_addr,
        None,
        pool_config,
        reset_strategy,
        vec![],
        RwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
    )
    .await
    .expect("failed to create MySqlProxy");

    // Drop the listener we used for port discovery — the proxy will re-bind.
    drop(listener);

    // Give the OS a moment to release the port before the proxy re-binds.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Run the proxy in the background.
    tokio::spawn(async move {
        if let Err(e) = proxy.run().await {
            eprintln!("proxy stopped: {e}");
        }
    });

    // Wait for the proxy listener to be ready.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(&listen_addr).await.is_ok() {
            return listen_addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("proxy did not become ready at {listen_addr}");
}

/// Build a `mysql_async` connection opts that route through the proxy.
fn proxy_opts(proxy_addr: &str) -> mysql_async::Opts {
    mysql_async::OptsBuilder::default()
        .ip_or_hostname(proxy_addr.split(':').next().unwrap())
        .tcp_port(proxy_addr.split(':').nth(1).unwrap().parse().unwrap())
        .user(Some("ignored"))
        .pass(Some("ignored"))
        .db_name(Some("test"))
        .into()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — nightly CI only"]
async fn basic_query_roundtrip() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;
    let pool = mysql_async::Pool::new(proxy_opts(&addr));
    let mut conn = pool.get_conn().await.unwrap();

    // Create, insert, select, verify, drop.
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS _ephpm_test_roundtrip (id INT PRIMARY KEY, val VARCHAR(64))",
    )
    .await
    .unwrap();

    conn.query_drop("INSERT INTO _ephpm_test_roundtrip (id, val) VALUES (1, 'hello')")
        .await
        .unwrap();

    let rows: Vec<(i32, String)> =
        conn.query("SELECT id, val FROM _ephpm_test_roundtrip WHERE id = 1").await.unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, 1);
    assert_eq!(rows[0].1, "hello");

    conn.query_drop("DROP TABLE IF EXISTS _ephpm_test_roundtrip").await.unwrap();

    drop(conn);
    pool.disconnect().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — nightly CI only"]
async fn connection_pool_reuse() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let config = PoolConfig {
        min_connections: 2,
        max_connections: 5,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        health_check_interval: Duration::from_secs(30),
    };

    let addr = start_proxy(&url, config, ResetStrategy::Always).await;

    // Open 10 sequential connections, each running a trivial query then
    // disconnecting. If the pool reuses backends this should succeed without
    // exhausting the max_connections=5 limit.
    for i in 0..10 {
        let pool = mysql_async::Pool::new(proxy_opts(&addr));
        let mut conn = pool.get_conn().await.unwrap();
        let rows: Vec<(i32,)> = conn.query("SELECT 1").await.unwrap();
        assert_eq!(rows[0].0, 1, "iteration {i}");
        drop(conn);
        pool.disconnect().await.unwrap();
    }

    // If we got here without pool timeout errors, connections are being reused.
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — nightly CI only"]
async fn session_isolation() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    // Use Always reset so `COM_RESET_CONNECTION` fires between clients.
    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;

    // Client A: set a user variable.
    {
        let pool = mysql_async::Pool::new(proxy_opts(&addr));
        let mut conn = pool.get_conn().await.unwrap();
        conn.query_drop("SET @myvar = 42").await.unwrap();

        // Verify it's set within the same connection.
        let rows: Vec<(Option<i32>,)> = conn.query("SELECT @myvar").await.unwrap();
        assert_eq!(rows[0].0, Some(42));

        drop(conn);
        pool.disconnect().await.unwrap();
    }

    // Small delay to let the proxy finish the reset on the returned backend.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client B: the variable must be gone (`COM_RESET_CONNECTION` cleared it).
    {
        let pool = mysql_async::Pool::new(proxy_opts(&addr));
        let mut conn = pool.get_conn().await.unwrap();
        let rows: Vec<(Option<i32>,)> = conn.query("SELECT @myvar").await.unwrap();
        assert_eq!(rows[0].0, None, "@myvar should be NULL after reset");

        drop(conn);
        pool.disconnect().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — nightly CI only"]
async fn transaction_integrity() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;
    let pool = mysql_async::Pool::new(proxy_opts(&addr));
    let mut conn = pool.get_conn().await.unwrap();

    // Setup.
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS _ephpm_test_txn (id INT PRIMARY KEY, val VARCHAR(64))",
    )
    .await
    .unwrap();
    conn.query_drop("DELETE FROM _ephpm_test_txn").await.unwrap();

    // Begin transaction, insert, verify visible within txn.
    conn.query_drop("BEGIN").await.unwrap();
    conn.query_drop("INSERT INTO _ephpm_test_txn (id, val) VALUES (1, 'txn_data')").await.unwrap();

    let rows: Vec<(i32, String)> =
        conn.query("SELECT id, val FROM _ephpm_test_txn WHERE id = 1").await.unwrap();
    assert_eq!(rows.len(), 1, "row should be visible inside transaction");
    assert_eq!(rows[0].1, "txn_data");

    // Rollback — row should vanish.
    conn.query_drop("ROLLBACK").await.unwrap();

    let rows: Vec<(i32,)> =
        conn.query("SELECT id FROM _ephpm_test_txn WHERE id = 1").await.unwrap();
    assert!(rows.is_empty(), "row should not exist after ROLLBACK");

    // Cleanup.
    conn.query_drop("DROP TABLE IF EXISTS _ephpm_test_txn").await.unwrap();

    drop(conn);
    pool.disconnect().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — nightly CI only"]
async fn prepared_statement_lifecycle() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;
    let pool = mysql_async::Pool::new(proxy_opts(&addr));
    let mut conn = pool.get_conn().await.unwrap();

    // Setup a table with data.
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS _ephpm_test_ps (id INT PRIMARY KEY, name VARCHAR(64))",
    )
    .await
    .unwrap();
    conn.query_drop("DELETE FROM _ephpm_test_ps").await.unwrap();
    conn.query_drop("INSERT INTO _ephpm_test_ps (id, name) VALUES (1, 'alice'), (2, 'bob')")
        .await
        .unwrap();

    // Prepare, execute with parameter, verify result.
    let stmt = conn.prep("SELECT id, name FROM _ephpm_test_ps WHERE id = ?").await.unwrap();

    let rows: Vec<(i32, String)> = conn.exec(&stmt, (1,)).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "alice");

    // Execute same statement with different parameter.
    let rows: Vec<(i32, String)> = conn.exec(&stmt, (2,)).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "bob");

    // Close the statement (implicit on drop, but explicit via close too).
    conn.close(stmt).await.unwrap();

    // Cleanup.
    conn.query_drop("DROP TABLE IF EXISTS _ephpm_test_ps").await.unwrap();

    drop(conn);
    pool.disconnect().await.unwrap();
}

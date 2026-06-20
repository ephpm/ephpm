//! Integration tests for the `PostgreSQL` proxy.
//!
//! These tests require a running `PostgreSQL` server. Set `PG_TEST_URL` to a
//! connection string (e.g. `postgres://postgres:test@127.0.0.1:5433/test`) to
//! enable them. All tests are `#[ignore]` so they only run in nightly CI via
//! `cargo test -- --ignored`.
//!
//! These tests cover the analogous handshake-handling concerns that affected
//! the MySQL proxy in PR #91. The PG wire protocol has no capability
//! bitfield, so the exact MySQL "inherit-then-strip" bug class cannot
//! recur. However, the auth flow has its own framing pitfalls — these tests
//! were how we caught the SCRAM-SHA-256 off-by-one read in
//! `handle_backend_auth` that this PR fixes. They pin that behavior against
//! real servers across the two mainstream auth paths:
//!
//! - `postgres:17` — defaults to `scram-sha-256` (the modern path).
//! - `postgres:13` — configured with `md5` (legacy path still in the wild).
//!
//! Spin them up with:
//!
//! ```text
//! docker run -d --rm --name pgtest17 -e POSTGRES_PASSWORD=test \
//!     -e POSTGRES_DB=test -p 5433:5432 postgres:17
//! docker run -d --rm --name pgtest13 -e POSTGRES_PASSWORD=test \
//!     -e POSTGRES_DB=test -e POSTGRES_HOST_AUTH_METHOD=md5 \
//!     -p 5434:5432 postgres:13
//! PG_TEST_URL=postgres://postgres:test@127.0.0.1:5433/test \
//!     cargo test -p ephpm-db --test pg_proxy_integration -- --ignored
//! ```

use std::time::Duration;

use ephpm_db::ResetStrategy;
use ephpm_db::pool::PoolConfig;
use ephpm_db::postgres::{PgProxy, PgRwSplitParams};
use tokio_postgres::NoTls;

/// Read `PG_TEST_URL` or return `None` (caller should skip).
fn pg_url() -> Option<String> {
    std::env::var("PG_TEST_URL").ok()
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

/// Boot a [`PgProxy`] on a random OS-assigned port and return the listen
/// address (e.g. `127.0.0.1:XXXXX`).
async fn start_proxy(
    backend_url: &str,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listen_addr = addr.to_string();

    let proxy = PgProxy::new(
        backend_url,
        &listen_addr,
        pool_config,
        reset_strategy,
        vec![],
        PgRwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
    )
    .await
    .expect("failed to create PgProxy — backend handshake failed");

    drop(listener);
    tokio::time::sleep(Duration::from_millis(50)).await;

    tokio::spawn(async move {
        if let Err(e) = proxy.run().await {
            eprintln!("proxy stopped: {e}");
        }
    });

    for _ in 0..50 {
        if tokio::net::TcpStream::connect(&listen_addr).await.is_ok() {
            return listen_addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("proxy did not become ready at {listen_addr}");
}

/// Build a `tokio_postgres` config that routes through the proxy.
///
/// User/password are intentionally bogus because the proxy issues
/// `AuthenticationOk` without validating client creds (loopback only).
fn proxy_config(proxy_addr: &str) -> tokio_postgres::Config {
    let (host, port) = proxy_addr.split_once(':').unwrap();
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(host).port(port.parse().unwrap()).user("ignored").password("ignored").dbname("test");
    cfg
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Smoke test: the backend handshake completes against a real PG server.
/// If `pg_connect_and_handshake` ever desynchronizes the auth message
/// stream — e.g. consuming an extra message after SCRAM as the previous
/// implementation did — the proxy will fail to spawn and this test will
/// panic before `start_proxy` returns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PG_TEST_URL — nightly CI only"]
async fn backend_handshake_completes() {
    let Some(url) = pg_url() else {
        println!("PG_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;
    // If we got here, the backend handshake succeeded and the listener is up.
    let _ = tokio::net::TcpStream::connect(&addr).await.unwrap();
}

/// Round-trip a simple query through the proxy.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PG_TEST_URL — nightly CI only"]
async fn basic_query_roundtrip() {
    let Some(url) = pg_url() else {
        println!("PG_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;
    let (client, connection) = proxy_config(&addr).connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {e}");
        }
    });

    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS _ephpm_pg_roundtrip (id INT PRIMARY KEY, val TEXT)",
        )
        .await
        .unwrap();
    client
        .batch_execute("INSERT INTO _ephpm_pg_roundtrip (id, val) VALUES (1, 'hello')")
        .await
        .unwrap();

    let rows =
        client.query("SELECT id, val FROM _ephpm_pg_roundtrip WHERE id = 1", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>(0), 1);
    assert_eq!(rows[0].get::<_, &str>(1), "hello");

    client.batch_execute("DROP TABLE IF EXISTS _ephpm_pg_roundtrip").await.unwrap();
}

/// Open and close multiple connections; verify the pool reuses backends.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PG_TEST_URL — nightly CI only"]
async fn connection_pool_reuse() {
    let Some(url) = pg_url() else {
        println!("PG_TEST_URL not set — skipping");
        return;
    };

    let cfg = PoolConfig {
        min_connections: 2,
        max_connections: 5,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        health_check_interval: Duration::from_secs(30),
    };
    let addr = start_proxy(&url, cfg, ResetStrategy::Always).await;

    for i in 0..10 {
        let (client, connection) = proxy_config(&addr).connect(NoTls).await.unwrap();
        let handle = tokio::spawn(async move {
            let _ = connection.await;
        });
        let rows = client.query("SELECT 1::int", &[]).await.unwrap();
        assert_eq!(rows[0].get::<_, i32>(0), 1, "iteration {i}");
        drop(client);
        let _ = handle.await;
    }
}

/// Verify session state isolation across pooled connections. After
/// `DISCARD ALL` (issued by the proxy on Always reset), session-local
/// settings must not leak to the next client.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PG_TEST_URL — nightly CI only"]
async fn session_isolation() {
    let Some(url) = pg_url() else {
        println!("PG_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;

    // Client A: set a session-local GUC and a temp table.
    {
        let (client, connection) = proxy_config(&addr).connect(NoTls).await.unwrap();
        let h = tokio::spawn(async move {
            let _ = connection.await;
        });
        client.batch_execute("SET LOCAL search_path TO 'pg_temp'").await.unwrap();
        client.batch_execute("CREATE TEMP TABLE _ephpm_pg_session_leak (x INT)").await.unwrap();
        drop(client);
        let _ = h.await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client B: temp table from the previous session must be gone.
    {
        let (client, connection) = proxy_config(&addr).connect(NoTls).await.unwrap();
        let h = tokio::spawn(async move {
            let _ = connection.await;
        });
        // to_regclass returns NULL when the relation doesn't exist.
        let rows = client
            .query("SELECT to_regclass('pg_temp._ephpm_pg_session_leak')::text", &[])
            .await
            .unwrap();
        let val: Option<String> = rows[0].get(0);
        assert!(val.is_none(), "temp table should not leak across sessions after DISCARD ALL");
        drop(client);
        let _ = h.await;
    }
}

/// Verify BEGIN/INSERT/ROLLBACK semantics across the proxy.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PG_TEST_URL — nightly CI only"]
async fn transaction_integrity() {
    let Some(url) = pg_url() else {
        println!("PG_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;
    let (client, connection) = proxy_config(&addr).connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });

    client
        .batch_execute("CREATE TABLE IF NOT EXISTS _ephpm_pg_txn (id INT PRIMARY KEY, val TEXT)")
        .await
        .unwrap();
    client.batch_execute("DELETE FROM _ephpm_pg_txn").await.unwrap();

    client.batch_execute("BEGIN").await.unwrap();
    client.batch_execute("INSERT INTO _ephpm_pg_txn (id, val) VALUES (1, 'inside')").await.unwrap();

    let rows = client.query("SELECT id, val FROM _ephpm_pg_txn WHERE id = 1", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, &str>(1), "inside");

    client.batch_execute("ROLLBACK").await.unwrap();

    let rows = client.query("SELECT id FROM _ephpm_pg_txn WHERE id = 1", &[]).await.unwrap();
    assert!(rows.is_empty(), "row must vanish after ROLLBACK");

    client.batch_execute("DROP TABLE IF EXISTS _ephpm_pg_txn").await.unwrap();
}

/// Prepared statement lifecycle through the proxy (extended query protocol:
/// Parse/Bind/Execute/Sync). `tokio_postgres::query` always uses the extended
/// protocol, so this exercises that path implicitly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PG_TEST_URL — nightly CI only"]
async fn prepared_statement_lifecycle() {
    let Some(url) = pg_url() else {
        println!("PG_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Always).await;
    let (client, connection) = proxy_config(&addr).connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });

    client
        .batch_execute("CREATE TABLE IF NOT EXISTS _ephpm_pg_ps (id INT PRIMARY KEY, name TEXT)")
        .await
        .unwrap();
    client.batch_execute("DELETE FROM _ephpm_pg_ps").await.unwrap();
    client
        .batch_execute("INSERT INTO _ephpm_pg_ps (id, name) VALUES (1, 'alice'), (2, 'bob')")
        .await
        .unwrap();

    let stmt = client.prepare("SELECT id, name FROM _ephpm_pg_ps WHERE id = $1").await.unwrap();

    let rows = client.query(&stmt, &[&1_i32]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, &str>(1), "alice");

    let rows = client.query(&stmt, &[&2_i32]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, &str>(1), "bob");

    client.batch_execute("DROP TABLE IF EXISTS _ephpm_pg_ps").await.unwrap();
}

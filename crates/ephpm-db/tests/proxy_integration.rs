//! Integration tests for the `MySQL` proxy.
//!
//! These tests require a running `MySQL` server. Set `MYSQL_TEST_URL` to a
//! connection string (e.g. `mysql://root:test@127.0.0.1:3306/test`) to enable
//! them. All tests are `#[ignore]`, and CI runs them in
//! `.github/workflows/db-integration.yml` against a real `mysql:8.0` with its
//! **default** authentication plugin.
//!
//! Locally an unset `MYSQL_TEST_URL` skips; in CI it is a failure. See
//! `tests/common/mod.rs` for why.

use std::sync::Arc;
use std::time::Duration;

use ephpm_db::ResetStrategy;
use ephpm_db::mysql::{MySqlProxy, RwSplitParams};
use ephpm_db::pool::PoolConfig;
use mysql_async::prelude::*;

mod common;

/// Read `MYSQL_TEST_URL` or return `None` (caller should skip).
fn mysql_url() -> Option<String> {
    common::db_url("MYSQL_TEST_URL")
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
///
/// The listener is bound exactly once and handed to [`MySqlProxy::run_on`] —
/// never dropped and rebound. Dropping a `:0` listener and rebinding its port
/// races other test processes doing the same, and the old ready-poll would
/// then happily connect to *their* proxy (see `caching_sha2_auth.rs`). No
/// readiness poll is needed: the listener is live before this function
/// returns; early clients queue in the kernel accept backlog.
async fn start_proxy(
    backend_url: &str,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
) -> String {
    // Bind to port 0 so the OS assigns a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen_addr = listener.local_addr().unwrap().to_string();

    // Build the proxy pointed at the real backend.
    let proxy = MySqlProxy::new(
        backend_url,
        &listen_addr,
        None,
        pool_config,
        reset_strategy,
        vec![],
        RwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
        // Instrumented like production: these tests exercise the recording
        // tap points as a side effect of every forwarded statement.
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
    )
    .await
    .expect("failed to create MySqlProxy");

    // Run the proxy in the background on the listener bound above.
    tokio::spawn(async move {
        if let Err(e) = proxy.run_on(Arc::new(listener)).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    listen_addr
}

/// Boot a [`MySqlProxy`] with read/write splitting enabled and one replica
/// pointed at the same backend URL.
///
/// This is the combination that selects `proxy_routing_loop`, the per-command
/// path that frames responses and returns the backend to the pool after every
/// command. Nothing else reaches it.
async fn start_proxy_rw_split(
    backend_url: &str,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen_addr = listener.local_addr().unwrap().to_string();

    let proxy = MySqlProxy::new(
        backend_url,
        &listen_addr,
        None,
        pool_config,
        reset_strategy,
        vec![backend_url.to_string()],
        RwSplitParams { enabled: true, sticky_duration: Duration::from_secs(0) },
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
    )
    .await
    .expect("failed to create MySqlProxy");

    // Same single-bind pattern as `start_proxy` — see the comment there.
    tokio::spawn(async move {
        if let Err(e) = proxy.run_on(Arc::new(listener)).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    listen_addr
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
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
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
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
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
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
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
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
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

// ── Smart reset_strategy tests ───────────────────────────────────────────────
//
// These exercise the dirty-tracking half of `Smart`: the strategy is consulted
// once per session, when the backend goes back to the pool, and only a session
// the client dirtied pays for a `COM_RESET_CONNECTION`.
//
// They deliberately use `start_proxy` (no replicas), which is what every
// `[db.mysql]` deployment without `[db.read_write_split]` gets. On that path
// `Smart` and `Always` relay identical bytes through
// `proxy_bidirectional_sniff` and differ *only* in whether the reset is sent on
// return — see `MySqlProxy::handle_client`. Before #97 `Smart` alone selected
// `proxy_routing_loop`, which is what made it hang; `start_proxy_rw_split`
// below is now the only way to reach that per-command path.

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
async fn basic_query_roundtrip_smart() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Smart).await;
    let pool = mysql_async::Pool::new(proxy_opts(&addr));

    // Hard timeout so a hang reports as a test failure rather than waiting
    // for the per-test wallclock deadline.
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut conn = pool.get_conn().await.unwrap();

        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS _ephpm_smart_basic (id INT PRIMARY KEY, val VARCHAR(64))",
        )
        .await
        .unwrap();

        conn.query_drop("INSERT INTO _ephpm_smart_basic (id, val) VALUES (1, 'hello')")
            .await
            .unwrap();

        let rows: Vec<(i32, String)> =
            conn.query("SELECT id, val FROM _ephpm_smart_basic WHERE id = 1").await.unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[0].1, "hello");

        conn.query_drop("DROP TABLE IF EXISTS _ephpm_smart_basic").await.unwrap();

        drop(conn);
    })
    .await;

    pool.disconnect().await.unwrap();
    result.expect("Smart-strategy roundtrip hung (timed out)");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
async fn session_isolation_smart() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    // Smart strategy should still reset between clients when the session
    // became dirty (writes / SET commands).
    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Smart).await;

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        // Client A: set a user variable.
        {
            let pool = mysql_async::Pool::new(proxy_opts(&addr));
            let mut conn = pool.get_conn().await.unwrap();
            conn.query_drop("SET @myvar = 42").await.unwrap();

            let rows: Vec<(Option<i32>,)> = conn.query("SELECT @myvar").await.unwrap();
            assert_eq!(rows[0].0, Some(42));

            drop(conn);
            pool.disconnect().await.unwrap();
        }

        // Small delay to let the proxy finish the reset on the returned backend.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client B: variable must be cleared by COM_RESET_CONNECTION.
        {
            let pool = mysql_async::Pool::new(proxy_opts(&addr));
            let mut conn = pool.get_conn().await.unwrap();
            let rows: Vec<(Option<i32>,)> = conn.query("SELECT @myvar").await.unwrap();
            assert_eq!(
                rows[0].0, None,
                "@myvar should be NULL after Smart-strategy reset of dirty session"
            );

            drop(conn);
            pool.disconnect().await.unwrap();
        }
    })
    .await;

    result.expect("Smart-strategy session isolation hung (timed out)");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
async fn prepared_statement_lifecycle_smart() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let addr = start_proxy(&url, test_pool_config(), ResetStrategy::Smart).await;
    let pool = mysql_async::Pool::new(proxy_opts(&addr));

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut conn = pool.get_conn().await.unwrap();

        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS _ephpm_smart_ps (id INT PRIMARY KEY, name VARCHAR(64))",
        )
        .await
        .unwrap();
        conn.query_drop("DELETE FROM _ephpm_smart_ps").await.unwrap();
        conn.query_drop("INSERT INTO _ephpm_smart_ps (id, name) VALUES (1, 'alice'), (2, 'bob')")
            .await
            .unwrap();

        let stmt = conn.prep("SELECT id, name FROM _ephpm_smart_ps WHERE id = ?").await.unwrap();

        let rows: Vec<(i32, String)> = conn.exec(&stmt, (1,)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "alice");

        let rows: Vec<(i32, String)> = conn.exec(&stmt, (2,)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "bob");

        conn.close(stmt).await.unwrap();
        conn.query_drop("DROP TABLE IF EXISTS _ephpm_smart_ps").await.unwrap();

        drop(conn);
    })
    .await;

    pool.disconnect().await.unwrap();
    result.expect("Smart-strategy prepared-statement lifecycle hung (timed out)");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
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

// ── Multi-result responses (issue #223) ──────────────────────────────────────

/// A stored-procedure `CALL` returns two result sets followed by a terminating
/// `OK`. `forward_mysql_response` used to return after the first terminating
/// `EOF`, leaving the rest buffered on the *pooled* backend socket — so the
/// corruption outlived the session that caused it.
///
/// Three things are asserted against a real `MySQL` server, in the order they
/// break:
///
/// 1. both result sets reach the client;
/// 2. the next command *on the same client session* reads its own response;
/// 3. a *different* client session, drawing the same pooled backend, reads its
///    own response too.
///
/// `max_connections = 1` makes (3) load-bearing: there is exactly one backend
/// connection, so the second session provably gets the one the `CALL` ran on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
async fn multi_result_call_does_not_desync_the_pooled_backend() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let config = PoolConfig {
        min_connections: 1,
        max_connections: 1,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        health_check_interval: Duration::from_secs(30),
    };
    let addr = start_proxy_rw_split(&url, config, ResetStrategy::Smart).await;

    // A desync shows up as a hang, so bound the whole thing.
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        // Setup. `BEGIN … END` is scoped by the server's parser, so no
        // DELIMITER games are needed over the wire.
        {
            let pool = mysql_async::Pool::new(proxy_opts(&addr));
            let mut conn = pool.get_conn().await.unwrap();
            conn.query_drop("DROP PROCEDURE IF EXISTS _ephpm_two_results").await.unwrap();
            conn.query_drop(
                "CREATE PROCEDURE _ephpm_two_results() \
                 BEGIN SELECT 11 AS a; SELECT 22 AS b; END",
            )
            .await
            .unwrap();
            drop(conn);
            pool.disconnect().await.unwrap();
        }

        // Session A: the multi-result command, then a follow-up on the same
        // session.
        {
            let pool = mysql_async::Pool::new(proxy_opts(&addr));
            let mut conn = pool.get_conn().await.unwrap();

            let mut call = conn.query_iter("CALL _ephpm_two_results()").await.unwrap();
            let first: Vec<(i32,)> = call.collect().await.unwrap();
            assert_eq!(first, vec![(11,)], "the first result set must reach the client");
            let second: Vec<(i32,)> = call.collect().await.unwrap();
            assert_eq!(second, vec![(22,)], "the second result set must reach the client too");
            drop(call);

            let rows: Vec<(i32,)> = conn.query("SELECT 33").await.unwrap();
            assert_eq!(rows[0].0, 33, "the next command must read its own response");

            drop(conn);
            pool.disconnect().await.unwrap();
        }

        // Let the proxy finish parking the backend.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Session B: a different client session on the same pooled backend.
        {
            let pool = mysql_async::Pool::new(proxy_opts(&addr));
            let mut conn = pool.get_conn().await.unwrap();
            let rows: Vec<(i32,)> = conn.query("SELECT 44").await.unwrap();
            assert_eq!(
                rows[0].0, 44,
                "a pooled backend must not carry a previous session's result sets"
            );
            drop(conn);
            pool.disconnect().await.unwrap();
        }

        // Cleanup.
        {
            let pool = mysql_async::Pool::new(proxy_opts(&addr));
            let mut conn = pool.get_conn().await.unwrap();
            conn.query_drop("DROP PROCEDURE IF EXISTS _ephpm_two_results").await.unwrap();
            drop(conn);
            pool.disconnect().await.unwrap();
        }
    })
    .await;

    result.expect("a multi-result CALL desynchronised the proxy (timed out)");
}

// ── The WordPress bootstrap query on the Smart default (issues #97, #425) ────

/// `WordPress` opens with `SELECT @@max_allowed_packet, @@wait_timeout` — a
/// two-column, one-row result set — and before #97 that hung on the shipped
/// `reset_strategy = "smart"` default. `examples/wordpress-compose/ephpm.toml`
/// pinned `"always"` to dodge it and kept the warning for two months after the
/// fix landed; #425 is the ticket that removed it, and this is the test that
/// keeps the claim from having to be re-verified by hand.
///
/// The shape matters more than the query text. The original failure was
/// `forward_mysql_response` returning at the *intermediate* EOF that closes the
/// column-definition block, so the row packets stayed on the backend socket —
/// which only bites when a result set has both column definitions and rows.
/// `SELECT 1` does not exercise it; two columns and a row do.
///
/// `max_connections = 1` is load-bearing twice over: every session provably
/// draws the same backend, and the sessions alternate between the two `Smart`
/// return branches — a pure-read session (returned *without* a reset) and a
/// dirtied one (returned with `COM_RESET_CONNECTION`). Leftover bytes from
/// either branch would desynchronise the next session, and a desynchronised
/// session hangs rather than errors, so the whole body is bounded.
///
/// Scope, so nobody over-reads this: the client here is `mysql_async`, not
/// `pdo_mysql`. Nothing in CI drives the `[db.mysql]` proxy from PHP — the one
/// `pdo_mysql` e2e suite (`rw_split`) points at the embedded SQLite listener,
/// not a real MySQL backend — so the `pdo_mysql` half of #425 was verified by
/// hand (WordPress on `examples/wordpress-compose`) and is not regression-
/// guarded. What *is* guarded here is the wire shape that actually broke.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_TEST_URL — see .github/workflows/db-integration.yml"]
async fn wordpress_bootstrap_query_smart_survives_session_churn() {
    let Some(url) = mysql_url() else {
        println!("MYSQL_TEST_URL not set — skipping");
        return;
    };

    let config = PoolConfig {
        min_connections: 1,
        max_connections: 1,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(5),
        health_check_interval: Duration::from_secs(30),
    };
    let addr = start_proxy(&url, config, ResetStrategy::Smart).await;

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        for round in 0..6 {
            // A pure-read session: `Smart` returns this backend to the pool
            // *without* a reset, so anything left behind survives into the
            // next iteration.
            {
                let pool = mysql_async::Pool::new(proxy_opts(&addr));
                let mut conn = pool.get_conn().await.unwrap();

                let rows: Vec<(u64, u64)> =
                    conn.query("SELECT @@max_allowed_packet, @@wait_timeout").await.unwrap();
                assert_eq!(rows.len(), 1, "round {round}: expected exactly one row");
                assert!(rows[0].0 > 0, "round {round}: @@max_allowed_packet must be non-zero");
                assert!(rows[0].1 > 0, "round {round}: @@wait_timeout must be non-zero");

                // A second command on the same session proves the first
                // response was consumed to its last byte.
                let echo: Vec<(i32,)> = conn.query("SELECT 7").await.unwrap();
                assert_eq!(echo[0].0, 7, "round {round}: follow-up command read the wrong bytes");

                drop(conn);
                pool.disconnect().await.unwrap();
            }

            // A dirtied session on the same single backend: `Smart` resets this
            // one on return, which is the other half of the branch.
            {
                let pool = mysql_async::Pool::new(proxy_opts(&addr));
                let mut conn = pool.get_conn().await.unwrap();
                conn.query_drop(format!("SET @_ephpm_i425 = {round}")).await.unwrap();
                let rows: Vec<(Option<i64>,)> = conn.query("SELECT @_ephpm_i425").await.unwrap();
                assert_eq!(
                    rows[0].0,
                    Some(i64::from(round)),
                    "round {round}: session continuity broken — the backend was swapped mid-session"
                );
                drop(conn);
                pool.disconnect().await.unwrap();
            }

            // Let the proxy finish parking (and resetting) the backend before
            // the next round draws it again.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // The dirtied sessions above must not have leaked into a fresh one.
        let pool = mysql_async::Pool::new(proxy_opts(&addr));
        let mut conn = pool.get_conn().await.unwrap();
        let rows: Vec<(Option<i64>,)> = conn.query("SELECT @_ephpm_i425").await.unwrap();
        assert_eq!(
            rows[0].0, None,
            "@_ephpm_i425 leaked across sessions — Smart skipped a reset it owed"
        );
        drop(conn);
        pool.disconnect().await.unwrap();
    })
    .await;

    result.expect(
        "the WordPress bootstrap query hung on reset_strategy = \"smart\" (timed out) — \
         this is the #97 regression that #425 verified was gone",
    );
}

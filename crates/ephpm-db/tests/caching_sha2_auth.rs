//! End-to-end authentication tests against a **default-configured** `MySQL` 8.
//!
//! The point of this file is what it does *not* do: no
//! `ALTER USER ... IDENTIFIED WITH mysql_native_password`. Every other DB test
//! in this crate reaches for that ALTER, and it was exactly that workaround
//! which hid issue #234 — the proxy could not complete the
//! `caching_sha2_password` **full-auth** exchange, so it only ever worked
//! against accounts the server had already cached (or accounts downgraded to
//! the legacy plugin).
//!
//! ## Forcing the cache miss
//!
//! `caching_sha2_password` only runs full auth when the server has no cached
//! entry for the account. Two ways to guarantee that, both used here:
//!
//! - **A freshly created account.** The cache is populated only by a successful
//!   authentication, so an account created moments ago has no entry by
//!   construction. This is the deterministic path and needs no server restart.
//! - **`FLUSH PRIVILEGES`.** Documented to clear the password cache, so an
//!   *existing, already-cached* account is pushed back onto the full-auth path.
//!   This reproduces the shape of the bug report (first connect after a server
//!   restart) without restarting anything.
//!
//! ## Bootstrapping
//!
//! All setup SQL is issued **through a proxy**, not with `mysql_async` directly.
//! That is not stylistic: `mysql_async` (as built here, no TLS, no `rsa` crate
//! in the dependency graph) cannot perform the RSA full-auth exchange either.
//! It can only reach a `MySQL` 8 backend over fast auth or `mysql_native_password`
//! — the very limitation this test exists to rule out for our proxy. The proxy
//! is therefore both the thing under test and the only available admin client.
//!
//! Set `MYSQL_SHA2_TEST_URL` to an admin connection string against a stock
//! `MySQL` 8 (e.g. `mysql://root:secret@127.0.0.1:3306/mysql`) to enable these.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ephpm_db::ResetStrategy;
use ephpm_db::mysql::{MySqlProxy, RwSplitParams};
use ephpm_db::pool::PoolConfig;
use mysql_async::prelude::*;

mod common;

/// Admin URL for a stock `MySQL` 8, or `None` (caller should skip).
fn admin_url() -> Option<String> {
    common::db_url("MYSQL_SHA2_TEST_URL")
}

fn test_pool_config() -> PoolConfig {
    PoolConfig {
        min_connections: 1,
        max_connections: 3,
        idle_timeout: Duration::from_secs(60),
        max_lifetime: Duration::from_secs(300),
        pool_timeout: Duration::from_secs(10),
        health_check_interval: Duration::from_secs(30),
    }
}

/// Boot a [`MySqlProxy`] against `backend_url` on an OS-assigned port.
///
/// The listener is bound exactly once and handed to [`MySqlProxy::run_on`]
/// — never dropped and rebound. The old bind-drop-rebind dance raced other
/// test processes binding `127.0.0.1:0`: a stolen port made the proxy's
/// rebind fail while the ready-check connected to the *other* test's proxy,
/// so the session died mid-stream with `ConnectionReset` (the intermittent
/// `DB Integration` CI failure).
///
/// Panics with the proxy's own error text when the backend handshake fails
/// (`MySqlProxy::new` dials the backend), so a test fails fast instead of
/// hanging. No readiness poll is needed: the listener is live before this
/// function returns, and early clients queue in the kernel accept backlog
/// until `run_on` starts accepting.
async fn start_proxy(backend_url: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen_addr = listener.local_addr().unwrap().to_string();

    let proxy = MySqlProxy::new(
        backend_url,
        &listen_addr,
        None,
        test_pool_config(),
        ResetStrategy::Always,
        vec![],
        RwSplitParams { enabled: false, sticky_duration: Duration::from_secs(0) },
        ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
    )
    .await
    .expect("MySqlProxy::new failed — backend handshake did not complete");

    tokio::spawn(async move {
        if let Err(e) = proxy.run_on(Arc::new(listener)).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    listen_addr
}

/// `mysql_async` opts pointed at the proxy. The proxy accepts any client
/// credentials (loopback-only listener), so these are placeholders.
fn proxy_opts(proxy_addr: &str, db: &str) -> mysql_async::Opts {
    mysql_async::OptsBuilder::default()
        .ip_or_hostname(proxy_addr.split(':').next().unwrap())
        .tcp_port(proxy_addr.split(':').nth(1).unwrap().parse().unwrap())
        .user(Some("ignored"))
        .pass(Some("ignored"))
        .db_name(Some(db.to_string()))
        .into()
}

/// Replace the credentials in a `mysql://user:pass@host:port/db` URL.
fn with_credentials(url: &str, user: &str, password: &str, db: &str) -> String {
    let rest = url.strip_prefix("mysql://").expect("admin URL must start with mysql://");
    let host_and_db = rest.rsplit_once('@').map_or(rest, |(_creds, tail)| tail);
    let host = host_and_db.split('/').next().unwrap();
    format!("mysql://{user}:{password}@{host}/{db}")
}

/// Unique-per-run account name. `MySQL` caps usernames at 32 characters.
fn unique_user(tag: &str) -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("s2_{tag}_{pid}_{n}")
}

/// Create an account and a database for it, and return `(user, password, db)`.
///
/// `plugin` is `None` to take the **server default** — which on a stock
/// `MySQL` 8 is `caching_sha2_password`. That default is the whole point; the
/// caller asserts it rather than trusting it.
async fn create_account(
    admin: &mut mysql_async::Conn,
    tag: &str,
    plugin: Option<&str>,
) -> (String, String, String) {
    let user = unique_user(tag);
    let password = "Pa55word!sha2";
    let db = format!("{user}_db");

    admin.query_drop(format!("DROP USER IF EXISTS '{user}'@'%'")).await.unwrap();
    let identified = match plugin {
        Some(p) => format!("IDENTIFIED WITH {p} BY '{password}'"),
        None => format!("IDENTIFIED BY '{password}'"),
    };
    admin.query_drop(format!("CREATE USER '{user}'@'%' {identified}")).await.unwrap();
    admin.query_drop(format!("CREATE DATABASE IF NOT EXISTS `{db}`")).await.unwrap();
    admin.query_drop(format!("GRANT ALL ON `{db}`.* TO '{user}'@'%'")).await.unwrap();
    admin.query_drop("FLUSH PRIVILEGES").await.unwrap();

    (user, password.to_string(), db)
}

/// The authentication plugin the server actually recorded for `user`.
async fn account_plugin(admin: &mut mysql_async::Conn, user: &str) -> String {
    admin
        .query_first::<String, _>(format!(
            "SELECT plugin FROM mysql.user WHERE user = '{user}' AND host = '%'"
        ))
        .await
        .unwrap()
        .expect("account should exist")
}

/// Prove the connection is usable, not merely authenticated: round-trip a row
/// through DDL + DML + a read.
async fn assert_query_roundtrip(proxy_addr: &str, db: &str) {
    let pool = mysql_async::Pool::new(proxy_opts(proxy_addr, db));
    let mut conn = pool.get_conn().await.expect("client could not reach the proxy");

    conn.query_drop("CREATE TABLE IF NOT EXISTS auth_probe (id INT PRIMARY KEY, val VARCHAR(32))")
        .await
        .unwrap();
    conn.query_drop("DELETE FROM auth_probe").await.unwrap();
    conn.query_drop("INSERT INTO auth_probe (id, val) VALUES (1, 'authenticated')").await.unwrap();

    let val: Option<String> =
        conn.query_first("SELECT val FROM auth_probe WHERE id = 1").await.unwrap();
    assert_eq!(val.as_deref(), Some("authenticated"), "query through the proxy returned no row");

    drop(conn);
    pool.disconnect().await.unwrap();
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// The issue-#234 case: an account using the server's default plugin whose
/// password the server has never cached.
///
/// This is the **negative control target**. Revert `handle_caching_sha2_more_data`
/// to sending a NUL-terminated plaintext password and this test fails with
/// `caching_sha2 full auth error: Access denied for user ...` — the exact error
/// from the bug report. That failure is the proof the test reaches full auth
/// rather than silently riding the fast path.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_SHA2_TEST_URL — a stock MySQL 8"]
async fn full_auth_with_an_uncached_caching_sha2_account() {
    let Some(url) = admin_url() else {
        println!("MYSQL_SHA2_TEST_URL not set — skipping");
        return;
    };

    let admin_addr = start_proxy(&url).await;
    let admin_pool = mysql_async::Pool::new(proxy_opts(&admin_addr, "mysql"));
    let mut admin = admin_pool.get_conn().await.unwrap();

    let (user, password, db) = create_account(&mut admin, "full", None).await;

    // Guard against a vacuous pass: if this server was started with
    // default_authentication_plugin=mysql_native_password, the account below
    // would never exercise caching_sha2 at all and the test would prove nothing.
    let plugin = account_plugin(&mut admin, &user).await;
    assert_eq!(
        plugin, "caching_sha2_password",
        "server is not default-configured — this test only means something \
         against a stock MySQL 8 (got plugin '{plugin}')"
    );

    // First connect for this account ⇒ no cache entry ⇒ the server demands full
    // auth (auth-more-data 0x04) and the proxy must complete the RSA exchange.
    let addr = start_proxy(&with_credentials(&url, &user, &password, &db)).await;
    assert_query_roundtrip(&addr, &db).await;

    admin.query_drop(format!("DROP DATABASE IF EXISTS `{db}`")).await.unwrap();
    admin.query_drop(format!("DROP USER IF EXISTS '{user}'@'%'")).await.unwrap();
    drop(admin);
    admin_pool.disconnect().await.unwrap();
}

/// `FLUSH PRIVILEGES` clears the password cache, which pushes an
/// **already-cached** account back onto the full-auth path. This is the shape
/// of the reported failure — "works until the database restarts" — without
/// restarting the server.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_SHA2_TEST_URL — a stock MySQL 8"]
async fn full_auth_again_after_flush_privileges_evicts_the_cache() {
    let Some(url) = admin_url() else {
        println!("MYSQL_SHA2_TEST_URL not set — skipping");
        return;
    };

    let admin_addr = start_proxy(&url).await;
    let admin_pool = mysql_async::Pool::new(proxy_opts(&admin_addr, "mysql"));
    let mut admin = admin_pool.get_conn().await.unwrap();

    let (user, password, db) = create_account(&mut admin, "flush", None).await;
    assert_eq!(account_plugin(&mut admin, &user).await, "caching_sha2_password");

    let user_url = with_credentials(&url, &user, &password, &db);

    // Connect once to populate the server's cache for this account.
    let first = start_proxy(&user_url).await;
    assert_query_roundtrip(&first, &db).await;

    // Evict it.
    admin.query_drop("FLUSH PRIVILEGES").await.unwrap();

    // A brand-new proxy dials fresh connections, which now miss the cache again.
    let second = start_proxy(&user_url).await;
    assert_query_roundtrip(&second, &db).await;

    admin.query_drop(format!("DROP DATABASE IF EXISTS `{db}`")).await.unwrap();
    admin.query_drop(format!("DROP USER IF EXISTS '{user}'@'%'")).await.unwrap();
    drop(admin);
    admin_pool.disconnect().await.unwrap();
}

/// The common case must not regress: once the cache is warm the server answers
/// the initial token with `0x01 0x03` and no RSA exchange happens at all.
///
/// Warming necessarily goes through full auth first (nothing else in this
/// dependency graph can authenticate an uncached caching_sha2 account), so this
/// test depends on the fix rather than being independent of it — but it does
/// pin that the fast path still terminates correctly afterwards.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_SHA2_TEST_URL — a stock MySQL 8"]
async fn fast_auth_still_works_once_the_cache_is_warm() {
    let Some(url) = admin_url() else {
        println!("MYSQL_SHA2_TEST_URL not set — skipping");
        return;
    };

    let admin_addr = start_proxy(&url).await;
    let admin_pool = mysql_async::Pool::new(proxy_opts(&admin_addr, "mysql"));
    let mut admin = admin_pool.get_conn().await.unwrap();

    let (user, password, db) = create_account(&mut admin, "fast", None).await;
    let user_url = with_credentials(&url, &user, &password, &db);

    // Full auth caches the hash server-side...
    let first = start_proxy(&user_url).await;
    assert_query_roundtrip(&first, &db).await;

    // ...so these connects take the 0x03 fast path. No FLUSH PRIVILEGES between.
    for _ in 0..3 {
        let addr = start_proxy(&user_url).await;
        assert_query_roundtrip(&addr, &db).await;
    }

    admin.query_drop(format!("DROP DATABASE IF EXISTS `{db}`")).await.unwrap();
    admin.query_drop(format!("DROP USER IF EXISTS '{user}'@'%'")).await.unwrap();
    drop(admin);
    admin_pool.disconnect().await.unwrap();
}

/// Legacy accounts must keep working — the auth-switch branch is untouched by
/// this change but it is the path every other DB test in this crate relies on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MYSQL_SHA2_TEST_URL — a stock MySQL 8"]
async fn mysql_native_password_account_still_authenticates() {
    let Some(url) = admin_url() else {
        println!("MYSQL_SHA2_TEST_URL not set — skipping");
        return;
    };

    let admin_addr = start_proxy(&url).await;
    let admin_pool = mysql_async::Pool::new(proxy_opts(&admin_addr, "mysql"));
    let mut admin = admin_pool.get_conn().await.unwrap();

    // MySQL 8.4 removed mysql_native_password from the default plugin set; skip
    // rather than fail if the server will not create such an account.
    let Some((user, password, db)) = create_account_native(&mut admin, "native").await else {
        println!("server does not offer mysql_native_password — skipping");
        return;
    };

    assert_eq!(account_plugin(&mut admin, &user).await, "mysql_native_password");

    let addr = start_proxy(&with_credentials(&url, &user, &password, &db)).await;
    assert_query_roundtrip(&addr, &db).await;

    admin.query_drop(format!("DROP DATABASE IF EXISTS `{db}`")).await.unwrap();
    admin.query_drop(format!("DROP USER IF EXISTS '{user}'@'%'")).await.unwrap();
    drop(admin);
    admin_pool.disconnect().await.unwrap();
}

/// Like [`create_account`] but pinned to `mysql_native_password`, returning
/// `None` when the server does not have that plugin available (`MySQL` 8.4+
/// ships it disabled by default).
async fn create_account_native(
    admin: &mut mysql_async::Conn,
    tag: &str,
) -> Option<(String, String, String)> {
    let user = unique_user(tag);
    let password = "Pa55word!native";
    let db = format!("{user}_db");

    admin.query_drop(format!("DROP USER IF EXISTS '{user}'@'%'")).await.ok()?;
    admin
        .query_drop(format!(
            "CREATE USER '{user}'@'%' IDENTIFIED WITH mysql_native_password BY '{password}'"
        ))
        .await
        .ok()?;
    admin.query_drop(format!("CREATE DATABASE IF NOT EXISTS `{db}`")).await.ok()?;
    admin.query_drop(format!("GRANT ALL ON `{db}`.* TO '{user}'@'%'")).await.ok()?;
    admin.query_drop("FLUSH PRIVILEGES").await.ok()?;

    Some((user, password.to_string(), db))
}

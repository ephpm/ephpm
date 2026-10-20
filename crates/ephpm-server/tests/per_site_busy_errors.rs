//! A busy per-site database must be reported as busy — on every route.
//!
//! A 3-node per-site clustered stress run held one site's write lock with a
//! 3,000-row transaction. Every error of the run was on that site, and none of
//! them said "busy":
//!
//! * the `ephpm_db_*` bridge on the owner: `failed to open embedded database
//!   session: ... database is locked` — session *setup* needed the write lock;
//! * the bridge and `pdo_mysql` on the other nodes: `owner connection closed
//!   awaiting a reply from <owner>: early eof` — the owner hit the same setup
//!   failure and dropped the forwarding stream instead of answering;
//! * stock `pdo_mysql` on the owner: `[1698] Authenticate failed` — a busy
//!   database reported as a wrong password.
//!
//! These tests hold a real write transaction open on a real per-site clustered
//! (CDC-capturing) Turso database and drive each route against it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, maybe_start_cluster_channel, start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig};
use ephpm_server::site_backends::{SiteBackends, SiteOpenHook};
use ephpm_server::site_wire_auth::{SiteWireAuth, SiteWireRoute};
use ephpm_server::sql_forward::{RemoteProxyBackend, spawn_owner_sql_handler};
use litewire::backend::{Backend, BackendConn, BackendError, Value};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder};

const SITE: &str = "busy.test";

/// Comfortably below the 5 s busy timeout every per-site session uses: a step
/// that finishes inside this did not wait on the write lock.
const PROMPT: Duration = Duration::from_millis(2500);

fn stats() -> ephpm_query_stats::QueryStats {
    ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
        enabled: false,
        slow_query_threshold: Duration::from_secs(1),
        max_digests: 16,
        metric_label_series_max: 16,
    })
}

/// A per-site **clustered** registry: every site's wire factory captures its
/// writes into CDC, exactly as `wire_per_site_clustered_db` builds it.
fn clustered_registry(dir: &std::path::Path) -> SiteBackends {
    let hook: SiteOpenHook = Arc::new(|_site: &str| {});
    SiteBackends::new_clustered(
        dir.to_path_buf(),
        8,
        stats(),
        tokio::runtime::Handle::current(),
        hook,
    )
    .expect("clustered per-site registry")
}

/// Open `SITE`, create a table, and leave a session holding the database's
/// write lock inside an open 3,000-row transaction — the stress run's shape.
async fn hold_write_lock(registry: &SiteBackends) -> Box<dyn BackendConn> {
    let backend = registry.backend_for(SITE).await.expect("open the site");
    let setup = backend.connect().await.expect("setup session");
    setup
        .execute("CREATE TABLE ledger (id INTEGER PRIMARY KEY, v TEXT)", &[])
        .await
        .expect("create table");
    drop(setup);

    let writer = backend.connect().await.expect("writer session");
    writer.execute("BEGIN", &[]).await.expect("begin");
    for i in 0..3000 {
        writer
            .execute("INSERT INTO ledger (v) VALUES (?)", &[Value::Text(format!("bulk-{i}"))])
            .await
            .expect("bulk insert");
    }
    writer
}

fn count(rs: &litewire::backend::ResultSet) -> i64 {
    match rs.rows[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("expected an integer count, got {other:?}"),
    }
}

/// Route 1, the owner's own sessions (`ephpm_db_*` bridge and the owner side
/// of forwarding both open one per request/stream): opening a session and
/// reading must not wait on another connection's write lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_a_session_does_not_wait_on_a_held_write_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = clustered_registry(dir.path());
    let writer = hold_write_lock(&registry).await;

    let backend = registry.backend_for(SITE).await.expect("resolve the site");
    let started = Instant::now();
    let session = backend.connect().await.expect(
        "opening a session must not need the write lock (was: failed to open embedded \
         database session: database is locked)",
    );
    let rs = session.query("SELECT COUNT(*) FROM ledger", &[]).await.expect("read");
    assert_eq!(count(&rs), 0, "the open transaction is invisible to a new reader");
    assert!(started.elapsed() < PROMPT, "open + read waited {:?}", started.elapsed());

    writer.execute("COMMIT", &[]).await.expect("commit");
}

// ── Route 2: forwarding (`sql/<site>`) ───────────────────────────────────────

/// One node's channel with the production owner-side `sql/` handler, and a
/// forwarding proxy pointed back at it — the non-owner's view of the owner.
async fn start_owner(
    registry: SiteBackends,
) -> (RemoteProxyBackend, Vec<tokio::task::JoinHandle<()>>) {
    let gossip = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: gossip.to_string(),
        secret: "per-site-busy-errors-secret".to_string(),
        node_id: "owner".to_string(),
        cluster_id: "per-site-busy-errors".to_string(),
        ..ClusterConfig::default()
    };
    let cluster = Arc::new(start_gossip(&cluster_cfg).await.expect("gossip"));
    let channel: ChannelHandle = maybe_start_cluster_channel(
        &ClusterChannelConfig { listen: Some("127.0.0.1:0".to_string()), secret: None },
        &cluster_cfg.secret,
        &cluster,
        ChannelFeatureFlags { cdc: true },
    )
    .await
    .expect("channel start")
    .expect("channel bound");

    let mut handles = Vec::new();
    spawn_owner_sql_handler(
        &channel,
        Arc::clone(&cluster),
        cluster.self_node().id,
        registry,
        &mut handles,
    );
    let proxy = RemoteProxyBackend::new(channel.clone(), channel.listen_addr(), SITE);
    // Keep the gossip handle and channel alive with the handler tasks.
    handles.push(tokio::spawn(async move {
        let _keep = (cluster, channel);
        std::future::pending::<()>().await;
    }));
    (proxy, handles)
}

/// A forwarded statement that fails on the owner comes back as the owner's
/// SQL error — `database is locked`, which the client maps to 1205 — not as a
/// torn stream; and the forwarding session keeps working once the lock is
/// released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_owner_answers_forwarded_statements_with_its_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = clustered_registry(dir.path());
    let writer = hold_write_lock(&registry).await;
    let (proxy, _handles) = start_owner(registry).await;

    let conn = proxy.connect().await.expect("proxy session");
    // Reads are forwarded and served while the lock is held.
    let started = Instant::now();
    let rs = conn.query("SELECT COUNT(*) FROM ledger", &[]).await.expect("forwarded read");
    assert_eq!(count(&rs), 0);
    assert!(started.elapsed() < PROMPT, "forwarded read waited {:?}", started.elapsed());

    // A write genuinely needs the lock: it fails with the owner's busy error.
    let err = conn
        .execute("INSERT INTO ledger (v) VALUES ('late')", &[])
        .await
        .expect_err("the write cannot get past the held lock");
    match &err {
        BackendError::Sqlite(m) => {
            assert!(m.contains("database is locked"), "expected the owner's busy error, got: {m}");
        }
        BackendError::Other(m) => {
            panic!("expected the owner's SQL error, got a transport error: {m}")
        }
    }
    assert!(
        !err.to_string().contains("owner connection closed"),
        "the owner must answer, not drop the stream: {err}"
    );

    // Same forwarding session, after the lock is released.
    writer.execute("COMMIT", &[]).await.expect("commit");
    conn.execute("INSERT INTO ledger (v) VALUES ('after')", &[])
        .await
        .expect("the same forwarded session writes once the lock is free");
    let rs = conn.query("SELECT COUNT(*) FROM ledger", &[]).await.expect("forwarded read");
    assert_eq!(count(&rs), 3001);
}

/// When the owner cannot open the site's session at all, the forwarding client
/// gets the owner's reason, not `early eof` — and the stream stays usable, so
/// the next statement retries the open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_that_cannot_open_the_site_says_why() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A directory where the site's database file should be: opening it fails.
    std::fs::create_dir(dir.path().join(format!("{SITE}.db"))).expect("block the db path");
    let registry = clustered_registry(dir.path());
    let (proxy, _handles) = start_owner(registry).await;

    let conn = proxy.connect().await.expect("proxy session");
    for attempt in 0..2 {
        let err =
            conn.query("SELECT 1", &[]).await.expect_err("the site's database cannot be opened");
        let msg = err.to_string();
        assert!(
            !msg.contains("early eof") && !msg.contains("owner connection closed"),
            "attempt {attempt}: the owner must report why, not drop the stream: {msg}"
        );
        assert!(
            msg.contains(SITE) && msg.contains("open"),
            "attempt {attempt}: the error must name the site and the failed open: {msg}"
        );
    }
}

// ── Route 3: stock `pdo_mysql` on the per-site wire listener ─────────────────

async fn start_wire(registry: SiteBackends) -> (SiteWireAuth, u16) {
    let auth = SiteWireAuth::new(registry).expect("credentials");
    let port =
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let server = litewire::LiteWire::with_authenticator(auth.as_authenticator())
        .mysql(&format!("127.0.0.1:{port}"));
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    (auth, port)
}

async fn wire_connect(port: u16, user: &str, pass: &str) -> Result<Conn, mysql_async::Error> {
    let opts: Opts = OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some(user))
        .pass(Some(pass))
        // PDO's `dbname=`: the handshake names a database.
        .db_name(Some("app"))
        .into();
    let mut last = None;
    for _ in 0..40 {
        match Conn::new(opts.clone()).await {
            Err(mysql_async::Error::Io(e)) => {
                last = Some(mysql_async::Error::Io(e));
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            other => return other,
        }
    }
    Err(last.expect("at least one attempt"))
}

/// A `pdo_mysql` client with valid credentials connects and reads while the
/// site's write lock is held; its write fails as a lock wait (1205), never as
/// `1698 Authenticate failed`. A wrong password is still 1698.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_site_is_never_an_authentication_failure_on_the_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = clustered_registry(dir.path());
    let writer = hold_write_lock(&registry).await;
    let (auth, port) = start_wire(registry).await;

    let started = Instant::now();
    let mut conn = wire_connect(port, SITE, &auth.password_for(SITE))
        .await
        .expect("valid credentials connect while the site is busy (was: 1698 Authenticate failed)");
    let n: Vec<i64> = conn.query("SELECT COUNT(*) FROM ledger").await.expect("read");
    assert_eq!(n, vec![0]);
    assert!(started.elapsed() < PROMPT, "connect + read waited {:?}", started.elapsed());

    let err = conn
        .query_drop("INSERT INTO ledger (v) VALUES ('late')")
        .await
        .expect_err("the write cannot get past the held lock");
    match &err {
        mysql_async::Error::Server(e) => {
            assert_eq!(e.code, 1205, "a busy database is a lock wait: {}", e.message);
            assert!(e.message.contains("database is locked"), "{}", e.message);
        }
        other => panic!("expected a server error, got {other}"),
    }

    let err = wire_connect(port, SITE, "wrong-password").await.expect_err("wrong password");
    match &err {
        mysql_async::Error::Server(e) => assert_eq!(e.code, 1698, "{}", e.message),
        other => panic!("expected a server error, got {other}"),
    }

    writer.execute("COMMIT", &[]).await.expect("commit");
    conn.query_drop("INSERT INTO ledger (v) VALUES ('after')").await.expect("write once free");
}

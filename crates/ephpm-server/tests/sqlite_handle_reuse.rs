//! Does litewire's handle reuse actually engage under ePHPm's workload?
//!
//! `start_db_proxies` builds the single-node SQLite backend with
//! `Rusqlite::builder(path).handle_reuse(16)` and logs `handle_reuse=true` at
//! startup — but the startup log only proves the feature was *requested*.
//! litewire's own A/B measured ~400 µs saved per connect+10-query cycle, while
//! ePHPm's end-to-end read p50 never moved, and the recorded hypothesis was
//! that handles are being DISCARDED on the wire frontend's disconnect path
//! rather than returned to the free-list.
//!
//! litewire exposes the only observability that can settle it —
//! `Rusqlite::reuse_stats()` (`hits`/`misses`/`returned`/`discarded`/
//! `expired`/`idle`) — but ePHPm cannot reach it: the `Rusqlite` is moved into
//! `TrackedBackend`, which is moved into `LiteWire::new`, which erases it to
//! `Arc<dyn Backend>`. This file rebuilds the same stack with a probe handle
//! retained, and drives it with a real `mysql_async` client connecting and
//! disconnecting exactly the way `pdo_mysql` does — one connection per
//! "request", `COM_QUIT` at the end.
//!
//! The distinction that matters, and why counting is better than timing here:
//! a discard means the pool is broken, whereas a miss with `discarded == 0`
//! means the handle was fine and simply had not been parked yet — the park is
//! asynchronous (`RusqliteConn::drop` only posts `EndSession`; the session's
//! own worker thread performs the hygiene pass and parks afterwards). Those
//! two have completely different fixes and are indistinguishable from a
//! latency measurement.

use std::sync::Arc;
use std::time::Duration;

use litewire::backend::rusqlite_backend::ReuseStats;
use litewire::backend::{Backend, BackendConn, BackendError, Rusqlite};
use mysql_async::prelude::Queryable;

/// Shares one `Rusqlite` between litewire (which takes it by value and erases
/// it) and the test (which needs `reuse_stats()`).
///
/// `Backend` is not implemented for `Arc<T>`, so a bare clone will not do —
/// this newtype forwards the one required method. It is exactly the shape
/// ePHPm would need in `start_db_proxies` to surface these counters in
/// production.
struct SharedRusqlite(Arc<Rusqlite>);

#[async_trait::async_trait]
impl Backend for SharedRusqlite {
    async fn connect(&self) -> Result<Box<dyn BackendConn>, BackendError> {
        self.0.connect().await
    }
}

/// Bind an ephemeral loopback port and hand back the number.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Spin up the ePHPm single-node stack (same builder call as
/// `start_db_proxies`) behind a MySQL wire frontend, returning the probe
/// handle and the port.
async fn start_stack(db_path: &str) -> (Arc<Rusqlite>, u16) {
    // Mirrors crates/ephpm-server/src/lib.rs — keep the idle cap in sync.
    let backend = Arc::new(Rusqlite::builder(db_path).handle_reuse(16).build().expect("open db"));
    let port = free_port().await;
    let addr = format!("127.0.0.1:{port}");

    let serve_backend = SharedRusqlite(Arc::clone(&backend));
    tokio::spawn(async move {
        let _ = litewire::LiteWire::new(serve_backend).mysql(&addr).serve().await;
    });

    // Wait for the listener rather than sleeping a fixed amount.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (backend, port)
}

fn opts(port: u16) -> mysql_async::Opts {
    mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("ephpm"))
        .db_name(Some("ephpm"))
        .prefer_socket(false)
        .into()
}

/// One "PHP request": open a connection, run `queries` statements, disconnect
/// cleanly (`COM_QUIT`) — the exact lifecycle of a `pdo_mysql` script.
async fn one_session(port: u16, queries: usize) {
    let mut conn = mysql_async::Conn::new(opts(port)).await.expect("connect");
    for _ in 0..queries {
        let rows: Vec<i64> =
            conn.query("SELECT id FROM reuse_probe WHERE id = 1").await.expect("query");
        assert_eq!(rows, vec![1]);
    }
    conn.disconnect().await.expect("COM_QUIT");
}

/// Park is asynchronous — `RusqliteConn::drop` only posts `EndSession` and the
/// worker parks later — so a one-shot read right after a disconnect
/// undercounts. Poll until the books balance, or give up and report what we
/// saw.
async fn settled_stats(backend: &Rusqlite, expect_endings: u64) -> ReuseStats {
    let mut last = backend.reuse_stats().expect("handle_reuse enabled");
    for _ in 0..200 {
        if last.returned + last.discarded >= expect_endings {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        last = backend.reuse_stats().expect("handle_reuse enabled");
    }
    last
}

async fn seed(backend: &Rusqlite) {
    let conn = backend.connect().await.expect("seed connect");
    conn.execute("CREATE TABLE IF NOT EXISTS reuse_probe (id INTEGER PRIMARY KEY)", &[])
        .await
        .expect("create");
    conn.execute("INSERT OR IGNORE INTO reuse_probe (id) VALUES (1)", &[]).await.expect("insert");
}

/// Sequential connect-per-request, the shape of a single-threaded PHP
/// workload: the free-list must actually serve the connections.
///
/// Asserted rather than merely reported, because a silent regression to
/// "reuse is configured but never hits" is precisely what went unnoticed: the
/// startup log says `handle_reuse=true` either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_sessions_hit_the_free_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("reuse.db");
    let (backend, port) = start_stack(db.to_str().expect("utf8 path")).await;
    seed(&backend).await;

    let sessions = 30_u64;
    for _ in 0..sessions {
        one_session(port, 10).await;
    }

    // `seed` opened one session of its own, hence the +1 on endings.
    let stats = settled_stats(&backend, sessions + 1).await;
    eprintln!("sequential: {stats:?}");

    assert_eq!(
        stats.discarded, 0,
        "sessions are being DISCARDED rather than returned to the free-list — \
         the pool's hygiene pass is rejecting handles after an ordinary \
         connect/query/COM_QUIT cycle: {stats:?}"
    );
    assert!(
        stats.hits > 0,
        "handle reuse is enabled but no connection was ever served from the \
         free-list: {stats:?}"
    );
    // The first connection has nothing to reuse, and the park is asynchronous
    // so an occasional connect outruns it. A *systematic* miss rate is the
    // real signal: it would mean the park never wins the race, which costs the
    // same WAL-index attach that reuse exists to avoid. Stated as a ratio
    // rather than a count so the threshold does not drift with `sessions`.
    assert!(
        stats.misses * 4 <= stats.hits,
        "connect-per-request is missing the free-list too often ({} misses vs \
         {} hits) even though nothing was discarded — the asynchronous park is \
         losing the race with the next connect: {stats:?}",
        stats.misses,
        stats.hits
    );
}

/// Concurrent sessions, the shape of a real HTTP workload.
///
/// A handful of discards is legitimate here and is NOT the dirty-handle
/// failure: because the park is asynchronous, a wave of concurrent connects
/// finds the previous wave not yet parked, misses, and spawns extra workers.
/// Those extras park too, so the free-list drifts up to its 16 cap and the
/// over-cap arrivals are retired. What must not happen is discards *scaling*
/// with session count — that would mean the hygiene pass is rejecting handles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_do_not_force_discards() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("reuse.db");
    let (backend, port) = start_stack(db.to_str().expect("utf8 path")).await;
    seed(&backend).await;

    let waves = 5_u64;
    let width = 8_u64;
    for _ in 0..waves {
        let mut set = Vec::new();
        for _ in 0..width {
            set.push(tokio::spawn(async move { one_session(port, 5).await }));
        }
        for h in set {
            h.await.expect("session task");
        }
    }

    let stats = settled_stats(&backend, waves * width + 1).await;
    eprintln!("concurrent: {stats:?}");

    assert!(
        stats.discarded * 8 <= stats.returned,
        "discards are scaling with load ({} discarded vs {} returned) — that is \
         the hygiene pass rejecting handles, not the idle cap: {stats:?}",
        stats.discarded,
        stats.returned
    );
    assert!(stats.hits > 0, "no free-list hits under concurrency: {stats:?}");
}

/// An abrupt TCP close (client vanishes without `COM_QUIT`) must be treated
/// the same as a clean disconnect. This is the case the #221 DB-proxy defect
/// family made worth checking explicitly: there, a mishandled `COM_QUIT`
/// poisoned pooled connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abrupt_disconnect_still_returns_the_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("reuse.db");
    let (backend, port) = start_stack(db.to_str().expect("utf8 path")).await;
    seed(&backend).await;

    let before = backend.reuse_stats().expect("handle_reuse enabled");

    let sessions = 10_u64;
    for _ in 0..sessions {
        let mut conn = mysql_async::Conn::new(opts(port)).await.expect("connect");
        let _: Vec<i64> =
            conn.query("SELECT id FROM reuse_probe WHERE id = 1").await.expect("query");
        // Drop without `disconnect()`: the socket closes under the server.
        drop(conn);
    }

    let stats = settled_stats(&backend, before.returned + before.discarded + sessions).await;
    eprintln!("abrupt: {stats:?}");

    assert_eq!(
        stats.discarded, 0,
        "an abrupt client disconnect must not poison the handle: {stats:?}"
    );
}

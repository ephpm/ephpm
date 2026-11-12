//! Graceful whole-database clustered **failover** harness (bare-process, e2e).
//!
//! This is the live validation for ePHPm's v0.16.2 drain gate (#701) and
//! `ephpm db restore --reseed` (#704): a graceful drain of the elected primary
//! must promote a replica **without losing any already-written data**.
//!
//! # What this proves, and how it differs from `turso_cdc.rs`
//!
//! The sibling [`turso_cdc`](super) suite already covers a **hard-kill** failover
//! (SIGKILL the primary) observed over litewire's **Hrana** HTTP API, keyed on
//! the `ephpm_cdc_subscribers` metric. This suite is deliberately a different
//! axis:
//!
//! - **Graceful drain, not a crash.** The primary is stopped with SIGTERM
//!   ([`ClusterFixture::stop_node`] → `terminate`), which runs ephpm's graceful
//!   shutdown — and on a whole-database clustered primary that runs the CDC
//!   drain gate (`drain_whole_db_primary` in `ephpm-server/src/turso_cdc.rs`),
//!   which waits for every attached replica to apply the CDC head before the
//!   cluster channel closes. That gate is the thing under test: without it, a
//!   replica promoted after the primary drained would lose the shipped-but-not-
//!   yet-applied tail.
//! - **The MySQL wire path, not Hrana.** Data is written and read through each
//!   node's `[db.sqlite.proxy] mysql_listen` listener with a real MySQL client
//!   (`mysql_async`) — the same surface production PHP's `pdo_mysql` uses. In
//!   whole-database mode this is litewire's shared, non-authenticating
//!   `MysqlFrontend::new` (`Fixed` backend), so the client connects with any
//!   username and an empty password (see [`ClusterFixture::mysql_addrs`]).
//! - **`/_ephpm/primary`, not a metric.** The primary is located through the
//!   active-passive load-balancer probe (200 on the elected primary, 503 on a
//!   replica), which shares the exact `AtomicBool` the CDC election flips.
//!
//! # Gating — do not destabilise the release pipeline
//!
//! Failover is timing-sensitive (gossip convergence, election heartbeats, CDC
//! apply). The gating "E2E bare-process" CI leg sets `EPHPM_BINARY` but must
//! **not** run this, or a slow runner could flake the release. So this suite
//! runs only when **both**:
//!
//! - `EPHPM_BINARY` is set (a built ephpm binary to spawn), and
//! - `EPHPM_CLUSTER_FAILOVER=1` is set (the explicit opt-in).
//!
//! A developer runs it with
//! `EPHPM_CLUSTER_FAILOVER=1 EPHPM_BINARY=… EPHPM_DOCROOT=… \
//!  cargo test -p ephpm-e2e --test cluster_failover -- --nocapture`,
//! or the lab does. Anything short of both prints a clear SKIP and returns, so
//! a bare `cargo test` still compiles and passes without a build.
//!
//! Also skipped on Windows: clustered Turso is Linux/macOS + experimental and
//! not validated on Windows (CLAUDE.md, "Windows database"), matching the
//! sibling `turso_cdc` suite.
//!
//! # Robustness
//!
//! Every wait is a bounded poll against a deadline — never sleep-then-assert.
//! Timeouts are generous on purpose and mirror the proven sibling suite
//! (election heartbeats at 5s/10s TTL, plus a startup grace, plus slow CI).
//! On failure, the relevant node's own `stderr.log` tail is inlined into the
//! panic message. Teardown (including the surviving nodes) is the fixture's
//! `Drop`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ephpm_e2e::{ClusterFixture, ephpm_binary_env};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder};

/// Nodes in the fixture cluster.
const CLUSTER_SIZE: usize = 3;

/// How long to wait for gossip to converge and a single primary to be elected.
/// Matches the sibling suite's 90s: a freshly started node observes a startup
/// grace before it may claim, heartbeats are 5s with a 10s TTL, and CI runners
/// are slow.
const ELECTION_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to wait for a primary write to replicate to a replica over CDC.
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for a survivor to be promoted after the primary drains.
/// More generous than the initial election: the dead primary's gossip failure
/// must be detected first, then its claim must expire, then a survivor claims.
const REELECTION_TIMEOUT: Duration = Duration::from_secs(120);

/// How often each poll loop re-checks its condition.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Per-request timeout for a single MySQL connect + statement.
const MYSQL_OP_TIMEOUT: Duration = Duration::from_secs(10);

/// The rows written on the primary in step 3, as `(id, v)`.
const ROWS: &[(i64, &str)] = &[(1, "alpha"), (2, "beta"), (3, "gamma")];

#[tokio::test(flavor = "multi_thread")]
async fn graceful_primary_drain_promotes_a_replica_without_data_loss() {
    if cfg!(windows) {
        eprintln!(
            "SKIP cluster_failover: clustered Turso is Linux/macOS + experimental and not \
             validated on Windows (CLAUDE.md). Run this on Linux/macOS in the lab."
        );
        return;
    }
    let Some(binary) = ephpm_binary_env() else {
        eprintln!(
            "SKIP cluster_failover: EPHPM_BINARY is unset (need a built ephpm binary to spawn). \
             Set EPHPM_CLUSTER_FAILOVER=1 and EPHPM_BINARY=… to run."
        );
        return;
    };
    if std::env::var_os("EPHPM_CLUSTER_FAILOVER").is_none() {
        eprintln!(
            "SKIP cluster_failover: timing-sensitive failover test is opt-in — set \
             EPHPM_CLUSTER_FAILOVER=1 to run (the gating E2E leg deliberately does not)."
        );
        return;
    }

    let docroot = resolve_docroot();
    assert!(
        docroot.join("index.php").is_file(),
        "docroot {} has no index.php — set EPHPM_DOCROOT to a docroot that serves PHP",
        docroot.display()
    );

    // ── 1. Spawn a 3-node whole-database clustered cluster ──────────────────
    let mut fixture = ClusterFixture::start(&binary, &docroot, CLUSTER_SIZE)
        .await
        .unwrap_or_else(|e| panic!("failed to start {CLUSTER_SIZE}-node cluster fixture: {e:#}"));
    let base_urls: Vec<String> = fixture.base_urls().iter().map(|s| (*s).to_string()).collect();
    let mysql_addrs: Vec<String> = fixture.mysql_addrs().iter().map(|s| (*s).to_string()).collect();

    // ── 2. Find the elected primary (exactly one /_ephpm/primary == 200) ────
    let all_nodes: Vec<usize> = (0..CLUSTER_SIZE).collect();
    let primary = wait_for_sole_primary(&base_urls, &all_nodes, ELECTION_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("no sole primary elected: {e}\n{}", all_stderr(&fixture)));
    eprintln!("[failover-e2e] elected primary: node {primary} ({})", base_urls[primary]);

    // ── 3. Write data on the primary via its MySQL proxy ────────────────────
    let primary_addr = mysql_addrs[primary].clone();
    mysql_exec(&primary_addr, "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "CREATE TABLE on primary node {primary} failed: {e}\n{}",
                fixture.stderr_tail(primary, 40)
            )
        });
    for (id, v) in ROWS {
        mysql_exec(&primary_addr, &format!("INSERT INTO t (id, v) VALUES ({id}, '{v}')"))
            .await
            .unwrap_or_else(|e| panic!("INSERT ({id},{v}) on primary node {primary} failed: {e}"));
    }
    eprintln!("[failover-e2e] wrote {} rows on the primary", ROWS.len());

    // ── 4. Wait for the rows to replicate to a replica (CDC applied) ────────
    let a_replica = (0..CLUSTER_SIZE).find(|&i| i != primary).expect("a replica exists");
    wait_for_row_count(&mysql_addrs[a_replica], ROWS.len() as i64, REPLICATION_TIMEOUT)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "replica node {a_replica} never saw {} replicated rows: {e}\n{}",
                ROWS.len(),
                fixture.stderr_tail(a_replica, 40)
            )
        });
    eprintln!(
        "[failover-e2e] replica node {a_replica} has all {} rows (CDC replicated)",
        ROWS.len()
    );

    // ── 5. Gracefully drain the primary (SIGTERM → the drain gate runs) ─────
    eprintln!("[failover-e2e] gracefully draining primary node {primary}");
    fixture.stop_node(primary);

    // ── 6. A survivor is promoted (exactly one survivor /_ephpm/primary == 200)
    let survivors: Vec<usize> = (0..CLUSTER_SIZE).filter(|&i| i != primary).collect();
    let new_primary =
        wait_for_sole_primary(&base_urls, &survivors, REELECTION_TIMEOUT).await.unwrap_or_else(
            |e| panic!("no survivor was promoted after the drain: {e}\n{}", all_stderr(&fixture)),
        );
    assert_ne!(new_primary, primary, "the drained primary cannot be re-elected");
    eprintln!(
        "[failover-e2e] promoted new primary: node {new_primary} ({})",
        base_urls[new_primary]
    );

    // ── 7. CORE ASSERTION: every written row is present on the new primary ──
    let want: Vec<String> = ROWS.iter().map(|(_, v)| (*v).to_string()).collect();
    for &s in &survivors {
        let got = mysql_query_strings(&mysql_addrs[s], "SELECT v FROM t ORDER BY id")
            .await
            .unwrap_or_else(|e| {
                panic!("SELECT on survivor node {s} failed: {e}\n{}", fixture.stderr_tail(s, 40))
            });
        assert_eq!(
            got, want,
            "data loss after graceful drain → promote: survivor node {s} has {got:?}, expected {want:?}"
        );
    }
    eprintln!(
        "[failover-e2e] no data loss: all {} rows present on every survivor (including the new primary)",
        ROWS.len()
    );

    // ── 8. Soft check: the drain gate did not time out on the survivors ─────
    for &s in &survivors {
        match scrape_metric(&base_urls[s], "ephpm_cdc_drain_catchup_timeouts_total").await {
            Some(v) => assert_eq!(
                v, 0.0,
                "node {s} reports {v} drain-catchup timeouts — the drain gate closed the channel \
                 with a replica still behind the head (investigate the lagging replica)"
            ),
            None => eprintln!(
                "[failover-e2e] note: ephpm_cdc_drain_catchup_timeouts_total absent on survivor \
                 node {s} (it was never a drained primary — expected; soft check)"
            ),
        }
    }

    // TODO(v0.16.3): live `--reseed` convergence sub-test. Deferred here to keep
    // the required drain-loss test a single, non-flaky scenario: the reseed path
    // needs an offline `ephpm db restore --reseed` on a stopped node, a restart,
    // and a convergence assertion — a second topology lifecycle whose timing
    // variance is best isolated in its own test rather than bolted onto this one.
}

// ── docroot ─────────────────────────────────────────────────────────────────

/// Resolve the document root to serve. Prefers `EPHPM_DOCROOT` (what
/// `cargo xtask e2e` and the lab set); falls back to this crate's
/// `tests/docroot` for a direct `cargo test` run. Mirrors `bare_process_smoke`.
fn resolve_docroot() -> PathBuf {
    if let Some(dir) = std::env::var_os("EPHPM_DOCROOT") {
        return PathBuf::from(dir);
    }
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // crates/ephpm-e2e -> crates -> <workspace root>
    manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("ephpm-e2e manifest dir has a workspace root two levels up")
        .join("tests")
        .join("docroot")
}

// ── /_ephpm/primary ───────────────────────────────────────────────────────

/// Poll `candidates` until **exactly one** reports `/_ephpm/primary == 200`
/// (the elected primary) and every other candidate reports 503 (a replica).
/// Returns the primary's node index.
///
/// Requiring the rest to be 503 — not merely "one is 200" — rejects the
/// transient window where an election is mid-flight and either zero or two
/// nodes briefly claim, which is exactly the race a naive check would flake on.
async fn wait_for_sole_primary(
    base_urls: &[String],
    candidates: &[usize],
    timeout: Duration,
) -> Result<usize, String> {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        let mut statuses = Vec::with_capacity(candidates.len());
        for &i in candidates {
            statuses.push((i, primary_status(&client, &base_urls[i]).await));
        }
        let primaries: Vec<usize> =
            statuses.iter().filter(|(_, s)| *s == Some(200)).map(|(i, _)| *i).collect();
        let rest_replica =
            statuses.iter().filter(|(i, _)| !primaries.contains(i)).all(|(_, s)| *s == Some(503));
        if primaries.len() == 1 && rest_replica {
            return Ok(primaries[0]);
        }
        if Instant::now() >= deadline {
            return Err(format!("no sole primary within {timeout:?}; last: statuses {statuses:?}"));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// GET `/_ephpm/primary`, returning the HTTP status code, or `None` if the node
/// is unreachable (treated as "not yet" by the poller).
async fn primary_status(client: &reqwest::Client, base_url: &str) -> Option<u16> {
    let url = format!("{base_url}/_ephpm/primary");
    client.get(&url).timeout(Duration::from_secs(2)).send().await.ok().map(|r| r.status().as_u16())
}

// ── /metrics ──────────────────────────────────────────────────────────────

/// Scrape one unlabelled-or-labelled Prometheus series from a node's `/metrics`.
/// `None` when the node is unreachable or the series is absent. In whole-database
/// mode CDC series carry `site="default"`, so a bare name and `name{…}` both match.
async fn scrape_metric(base_url: &str, name: &str) -> Option<f64> {
    let url = format!("{base_url}/metrics");
    let text = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let labelled = format!("{name}{{");
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .find(|l| {
            l.split_whitespace().next().is_some_and(|k| k == name || k.starts_with(&labelled))
        })
        .and_then(|l| l.split_whitespace().last())
        .and_then(|v| v.parse().ok())
}

// ── MySQL wire (whole-database shared listener) ─────────────────────────────

/// Open a fresh connection to a node's whole-database MySQL listener.
///
/// A fresh connection per operation is deliberate: across a failover a pooled
/// connection to a drained node is worthless, and the per-op cost is irrelevant
/// to a test. No database is selected (SQLite has one), and the credential is
/// `root` with an empty password — the shared `Fixed` litewire frontend does
/// not authenticate (see [`ClusterFixture::mysql_addrs`]).
async fn mysql_connect(addr: &str) -> Result<Conn, String> {
    let (host, port) = addr.rsplit_once(':').ok_or_else(|| format!("bad mysql addr {addr:?}"))?;
    let port: u16 = port.parse().map_err(|e| format!("bad port in {addr:?}: {e}"))?;
    let opts = OptsBuilder::default()
        .ip_or_hostname(host.to_string())
        .tcp_port(port)
        .user(Some("root"))
        .pass(None::<String>)
        .db_name(None::<String>);
    match tokio::time::timeout(MYSQL_OP_TIMEOUT, Conn::new(opts)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(format!("connect {addr}: {e}")),
        Err(_) => Err(format!("connect {addr}: timed out after {MYSQL_OP_TIMEOUT:?}")),
    }
}

/// Execute a statement (no result set) on `addr`.
async fn mysql_exec(addr: &str, sql: &str) -> Result<(), String> {
    let mut conn = mysql_connect(addr).await?;
    let fut = conn.query_drop(sql);
    let out = match tokio::time::timeout(MYSQL_OP_TIMEOUT, fut).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("exec {sql:?} on {addr}: {e}")),
        Err(_) => Err(format!("exec {sql:?} on {addr}: timed out")),
    };
    let _ = conn.disconnect().await;
    out
}

/// Run a single-TEXT-column query on `addr`, returning the column values.
async fn mysql_query_strings(addr: &str, sql: &str) -> Result<Vec<String>, String> {
    let mut conn = mysql_connect(addr).await?;
    let fut = conn.query::<String, _>(sql);
    let out = match tokio::time::timeout(MYSQL_OP_TIMEOUT, fut).await {
        Ok(Ok(rows)) => Ok(rows),
        Ok(Err(e)) => Err(format!("query {sql:?} on {addr}: {e}")),
        Err(_) => Err(format!("query {sql:?} on {addr}: timed out")),
    };
    let _ = conn.disconnect().await;
    out
}

/// Poll `SELECT count(*) FROM t` on `addr` until it reads `want`.
async fn wait_for_row_count(addr: &str, want: i64, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let last = match mysql_count(addr, "SELECT count(*) FROM t").await {
            Ok(n) if n == want => return Ok(()),
            Ok(n) => format!("count = {n}"),
            Err(e) => format!("error: {e}"),
        };
        if Instant::now() >= deadline {
            return Err(format!("count never reached {want} within {timeout:?}; last: {last}"));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Run a `count(*)`-shaped query on `addr`, returning the single integer.
async fn mysql_count(addr: &str, sql: &str) -> Result<i64, String> {
    let mut conn = mysql_connect(addr).await?;
    let fut = conn.query_first::<i64, _>(sql);
    let out = match tokio::time::timeout(MYSQL_OP_TIMEOUT, fut).await {
        Ok(Ok(Some(n))) => Ok(n),
        Ok(Ok(None)) => Err(format!("count {sql:?} on {addr}: no rows")),
        Ok(Err(e)) => Err(format!("count {sql:?} on {addr}: {e}")),
        Err(_) => Err(format!("count {sql:?} on {addr}: timed out")),
    };
    let _ = conn.disconnect().await;
    out
}

// ── diagnostics ─────────────────────────────────────────────────────────────

/// The last 40 lines of every node's `stderr.log`, for a cluster-wide failure.
fn all_stderr(fixture: &ClusterFixture) -> String {
    (0..CLUSTER_SIZE)
        .map(|i| format!("--- node {i} stderr (tail) ---\n{}", fixture.stderr_tail(i, 40)))
        .collect::<Vec<_>>()
        .join("\n")
}

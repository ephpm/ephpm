//! Graceful whole-database clustered **failover** harness (bare-process, e2e).
//!
//! This is the live validation for ePHPm's v0.16.2 drain gate (#701) and
//! `ephpm db restore --reseed` (#704): a graceful drain of the elected primary
//! must promote a replica **without losing any already-written data**. Three
//! scenarios run here:
//!
//! - [`graceful_primary_drain_promotes_a_replica_without_data_loss`] — the drain
//!   gate: a SIGTERM drain of the primary promotes a survivor with no loss.
//! - [`reseed_converges_a_running_cluster`] — #707 (c): `ephpm db restore
//!   --reseed` converges a running cluster onto the restored data, each replica
//!   discarding its own copy. (Offline install correctness is unit-tested in the
//!   `ephpm` crate; this proves the live end-to-end convergence.)
//! - [`hard_kill_primary_promotes_a_replica_without_corrupting_replicated_data`]
//!   — #732: a SIGKILL of the primary (no graceful drain) still promotes a
//!   survivor with the already-replicated data intact and fresh writes accepted
//!   only once the new primary's apply pipeline has settled.
//!
//! # What this proves, and how it differs from `turso_cdc.rs`
//!
//! The sibling [`turso_cdc`](super) suite already covers a **hard-kill** failover
//! (SIGKILL the primary) observed over litewire's **Hrana** HTTP API, keyed on
//! the `ephpm_cdc_subscribers` metric. This suite holds three MySQL-wire
//! scenarios on a different axis:
//!
//! - **Graceful drain, not a crash**
//!   ([`graceful_primary_drain_promotes_a_replica_without_data_loss`]). The
//!   primary is stopped with SIGTERM ([`ClusterFixture::stop_node`] →
//!   `terminate`), which runs ephpm's graceful shutdown — and on a
//!   whole-database clustered primary that runs the CDC drain gate
//!   (`drain_whole_db_primary` in `ephpm-server/src/turso_cdc.rs`), which waits
//!   for every attached replica to apply the CDC head before the cluster channel
//!   closes. That gate is the thing under test: without it, a replica promoted
//!   after the primary drained would lose the shipped-but-not-yet-applied tail.
//! - **Abrupt kill, not a drain**
//!   ([`hard_kill_primary_promotes_a_replica_without_corrupting_replicated_data`]).
//!   The primary is SIGKILLed ([`ClusterFixture::kill_node`]) with **no**
//!   graceful shutdown, so the drain gate never runs. This is the case the
//!   promotion-side crash-safety gate (`turso_cdc::await_promotion_settle`, #706)
//!   covers: the newly-elected primary must not advertise itself writable until
//!   its own apply pipeline has quiesced, so the data that *had* already
//!   replicated is intact and a fresh write lands cleanly on top of a settled
//!   state. (A tail that only the dead primary held is lost — that is the
//!   by-design cost of an abrupt kill, which this test does not contradict: it
//!   writes, then waits for replication to **every** survivor before the kill.)
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

    // Live `--reseed` convergence is validated by
    // `reseed_converges_a_running_cluster` below — a separate scenario, so this
    // required drain-loss test stays a single, non-flaky lifecycle.
}

/// The rows the reseed source carries (`reseed_converges_a_running_cluster`): the
/// restore holds *only* these, and the cluster must converge on exactly them.
const RESEED_ROWS: &[(i64, &str)] = &[(1, "alpha"), (2, "beta"), (3, "gamma")];

/// The rows written *after* the reseed source is captured: present on every node
/// at reseed time, they must be **discarded** when each node adopts the restore.
const POST_SNAPSHOT_ROWS: &[(i64, &str)] = &[(4, "delta"), (5, "epsilon"), (6, "zeta")];

/// **#707 (c): live whole-database `--reseed` convergence.**
///
/// Proves that `ephpm db restore --reseed` converges a *running* whole-database
/// cluster onto the restored data — not just that the offline install works
/// (that is unit-tested in `ephpm`). The scenario deliberately makes the restore
/// data differ from what every node holds at reseed time, so a node that kept its
/// own copy (the bug `--reseed` fixes) would be caught:
///
/// 1. Write `RESEED_ROWS` and let them replicate to every node.
/// 2. Stop node 0 and `ephpm db dump` it → a snapshot holding **only** those rows.
/// 3. With the survivors still up, write `POST_SNAPSHOT_ROWS`; every live node now
///    holds `RESEED_ROWS + POST_SNAPSHOT_ROWS`.
/// 4. Stop every node. `ephpm db restore --reseed` the dump onto node 0 (the
///    lowest ordinal — the node that re-elects primary and advertises the reseed
///    generation). Start node 0 first, then the others.
/// 5. Assert every node converges on **exactly `RESEED_ROWS`** — each replica saw
///    the newer reseed generation, force-discarded its `+POST_SNAPSHOT_ROWS` copy
///    and cold re-bootstrapped from the restored primary.
///
/// Same gating and skips as the drain test; isolated as its own `#[test]` because
/// it drives a second, slower topology lifecycle (stop-all → restore → start-all).
#[tokio::test(flavor = "multi_thread")]
async fn reseed_converges_a_running_cluster() {
    if cfg!(windows) {
        eprintln!(
            "SKIP cluster_failover reseed: clustered Turso is Linux/macOS + experimental and not \
             validated on Windows (CLAUDE.md)."
        );
        return;
    }
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("SKIP cluster_failover reseed: EPHPM_BINARY is unset.");
        return;
    };
    if std::env::var_os("EPHPM_CLUSTER_FAILOVER").is_none() {
        eprintln!(
            "SKIP cluster_failover reseed: timing-sensitive test is opt-in — set \
             EPHPM_CLUSTER_FAILOVER=1 to run."
        );
        return;
    }

    let docroot = resolve_docroot();
    assert!(docroot.join("index.php").is_file(), "docroot {} has no index.php", docroot.display());

    // ── 1. Spawn the cluster; write RESEED_ROWS on the primary ──────────────
    let mut fixture = ClusterFixture::start(&binary, &docroot, CLUSTER_SIZE)
        .await
        .unwrap_or_else(|e| panic!("failed to start {CLUSTER_SIZE}-node cluster fixture: {e:#}"));
    let base_urls: Vec<String> = fixture.base_urls().iter().map(|s| (*s).to_string()).collect();
    let mysql_addrs: Vec<String> = fixture.mysql_addrs().iter().map(|s| (*s).to_string()).collect();
    let all_nodes: Vec<usize> = (0..CLUSTER_SIZE).collect();

    let primary = wait_for_sole_primary(&base_urls, &all_nodes, ELECTION_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("no sole primary elected: {e}\n{}", all_stderr(&fixture)));
    mysql_exec(
        &mysql_addrs[primary],
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)",
    )
    .await
    .unwrap_or_else(|e| panic!("CREATE TABLE failed: {e}\n{}", fixture.stderr_tail(primary, 40)));
    for (id, v) in RESEED_ROWS {
        mysql_exec(&mysql_addrs[primary], &format!("INSERT INTO t (id, v) VALUES ({id}, '{v}')"))
            .await
            .unwrap_or_else(|e| panic!("INSERT ({id},{v}) failed: {e}"));
    }

    // Wait for the reseed rows on EVERY node, so node 0's own file holds them
    // before we dump it (node 0 may be the primary or a replica).
    for &n in &all_nodes {
        wait_for_row_count(&mysql_addrs[n], RESEED_ROWS.len() as i64, REPLICATION_TIMEOUT)
            .await
            .unwrap_or_else(|e| {
                panic!("node {n} never saw the reseed rows: {e}\n{}", fixture.stderr_tail(n, 40))
            });
    }
    eprintln!("[reseed-e2e] every node has the {} reseed rows", RESEED_ROWS.len());

    // ── 2. Stop node 0 and dump it (the snapshot holds only RESEED_ROWS) ─────
    let reseed_node = 0;
    fixture.stop_node(reseed_node);
    let source = fixture.node_dir(reseed_node).expect("node 0 dir").join("reseed-source.db.gz");
    let source_arg = source.to_string_lossy().into_owned();
    let out = fixture
        .run_cli(reseed_node, &["db", "dump", "--site", "default", "--out", &source_arg])
        .expect("spawn ephpm db dump");
    assert!(out.status.success(), "ephpm db dump failed: {}", String::from_utf8_lossy(&out.stderr));
    eprintln!("[reseed-e2e] dumped node 0 → {}", source.display());

    // ── 3. Write POST_SNAPSHOT_ROWS on a survivor so every live node diverges ─
    let survivors: Vec<usize> = (1..CLUSTER_SIZE).collect();
    let survivor_primary = wait_for_sole_primary(&base_urls, &survivors, REELECTION_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("no survivor primary after stopping node 0: {e}"));
    for (id, v) in POST_SNAPSHOT_ROWS {
        mysql_exec(
            &mysql_addrs[survivor_primary],
            &format!("INSERT INTO t (id, v) VALUES ({id}, '{v}')"),
        )
        .await
        .unwrap_or_else(|e| panic!("INSERT post-snapshot ({id},{v}) failed: {e}"));
    }
    let total = (RESEED_ROWS.len() + POST_SNAPSHOT_ROWS.len()) as i64;
    for &n in &survivors {
        wait_for_row_count(&mysql_addrs[n], total, REPLICATION_TIMEOUT)
            .await
            .unwrap_or_else(|e| panic!("survivor {n} never saw the post-snapshot rows: {e}"));
    }
    eprintln!("[reseed-e2e] survivors now hold all {total} rows (reseed + post-snapshot)");

    // ── 4. Stop every node; reseed node 0 from the dump; restart all ────────
    for &n in &survivors {
        fixture.stop_node(n);
    }
    let out = fixture
        .run_cli(
            reseed_node,
            &["db", "restore", "--site", "default", "--from", &source_arg, "--reseed", "--yes"],
        )
        .expect("spawn ephpm db restore --reseed");
    assert!(
        out.status.success(),
        "ephpm db restore --reseed failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("[reseed-e2e] restored --reseed onto node 0; restarting the cluster (node 0 first)");

    // Node 0 first so it re-elects primary and advertises the reseed generation.
    fixture
        .restart_node(reseed_node)
        .await
        .unwrap_or_else(|e| panic!("restart node 0 failed: {e:#}\n{}", fixture.stderr_tail(0, 40)));
    for &n in &survivors {
        fixture.restart_node(n).await.unwrap_or_else(|e| {
            panic!("restart node {n} failed: {e:#}\n{}", fixture.stderr_tail(n, 40))
        });
    }

    let new_primary =
        wait_for_sole_primary(&base_urls, &all_nodes, REELECTION_TIMEOUT).await.unwrap_or_else(
            |e| panic!("no sole primary after the reseed restart: {e}\n{}", all_stderr(&fixture)),
        );
    eprintln!(
        "[reseed-e2e] cluster re-elected a primary (node {new_primary}); awaiting convergence"
    );

    // ── 5. CORE ASSERTION: every node converges on EXACTLY the reseed rows ──
    // The reseed rows are 3; the pre-reseed copies held 6. Converging back to a
    // count of 3 is the discard — the restore replaced each node's `+post` copy.
    let want: Vec<String> = RESEED_ROWS.iter().map(|(_, v)| (*v).to_string()).collect();
    for &n in &all_nodes {
        wait_for_row_count(&mysql_addrs[n], RESEED_ROWS.len() as i64, REELECTION_TIMEOUT)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "node {n} never converged on the {} reseeded rows (still holds its pre-reseed \
                     copy?): {e}\n{}",
                    RESEED_ROWS.len(),
                    fixture.stderr_tail(n, 60)
                )
            });
        let got = mysql_query_strings(&mysql_addrs[n], "SELECT v FROM t ORDER BY id")
            .await
            .unwrap_or_else(|e| {
                panic!("SELECT on node {n} failed: {e}\n{}", fixture.stderr_tail(n, 40))
            });
        assert_eq!(
            got, want,
            "reseed did not converge: node {n} has {got:?}, expected exactly the reseeded {want:?} \
             (the post-snapshot rows must have been discarded)"
        );
    }
    eprintln!(
        "[reseed-e2e] CONVERGED: every node holds exactly the {} reseeded rows; the \
         post-snapshot rows were discarded",
        RESEED_ROWS.len()
    );
}

/// Abrupt-kill (SIGKILL) failover — the promotion-side crash-safety gate (#706).
///
/// The drain gate cannot cover a crash: SIGKILL runs none of ephpm's shutdown.
/// What this proves is the *promotion* side. We write, then wait for the rows to
/// replicate to **every** survivor, then SIGKILL the primary. A survivor is
/// auto-promoted; the promotion gate holds it non-writable (`/_ephpm/primary`
/// non-200) until its apply pipeline has quiesced. Once promoted we assert:
///
/// 1. every already-replicated row is intact on every survivor (the crash did
///    not corrupt or lose data that had replicated), and
/// 2. a fresh write to the new primary commits and replicates to the other
///    survivor — i.e. the promoted node accepts writes cleanly on top of a
///    settled state, rather than on a tail it was still applying.
///
/// This deliberately does **not** assert recovery of a tail that only the dead
/// primary held: that loss is the documented cost of an abrupt kill.
#[tokio::test(flavor = "multi_thread")]
async fn hard_kill_primary_promotes_a_replica_without_corrupting_replicated_data() {
    if cfg!(windows) {
        eprintln!(
            "SKIP cluster_failover (hard kill): clustered Turso is Linux/macOS + experimental \
             and not validated on Windows (CLAUDE.md). Run this on Linux/macOS in the lab."
        );
        return;
    }
    let Some(binary) = ephpm_binary_env() else {
        eprintln!(
            "SKIP cluster_failover (hard kill): EPHPM_BINARY is unset. Set \
             EPHPM_CLUSTER_FAILOVER=1 and EPHPM_BINARY=… to run."
        );
        return;
    };
    if std::env::var_os("EPHPM_CLUSTER_FAILOVER").is_none() {
        eprintln!(
            "SKIP cluster_failover (hard kill): timing-sensitive failover test is opt-in — set \
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

    // ── 2. Find the elected primary ─────────────────────────────────────────
    let all_nodes: Vec<usize> = (0..CLUSTER_SIZE).collect();
    let primary = wait_for_sole_primary(&base_urls, &all_nodes, ELECTION_TIMEOUT)
        .await
        .unwrap_or_else(|e| panic!("no sole primary elected: {e}\n{}", all_stderr(&fixture)));
    eprintln!("[hardkill-e2e] elected primary: node {primary} ({})", base_urls[primary]);

    // ── 3. Write data on the primary ────────────────────────────────────────
    let primary_addr = mysql_addrs[primary].clone();
    mysql_exec(&primary_addr, "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)")
        .await
        .unwrap_or_else(|e| {
            panic!("CREATE TABLE on primary node {primary} failed: {e}\n{}", fixture.stderr_tail(primary, 40))
        });
    for (id, v) in ROWS {
        mysql_exec(&primary_addr, &format!("INSERT INTO t (id, v) VALUES ({id}, '{v}')"))
            .await
            .unwrap_or_else(|e| panic!("INSERT ({id},{v}) on primary node {primary} failed: {e}"));
    }
    eprintln!("[hardkill-e2e] wrote {} rows on the primary", ROWS.len());

    // ── 4. Wait for the rows to replicate to EVERY survivor ─────────────────
    // Crucial: the data must be on both replicas BEFORE the kill, so the test
    // asserts "no corruption of replicated data", never "recovery of a tail only
    // the dead node held" (which an abrupt kill legitimately loses).
    let survivors: Vec<usize> = (0..CLUSTER_SIZE).filter(|&i| i != primary).collect();
    for &s in &survivors {
        wait_for_row_count(&mysql_addrs[s], ROWS.len() as i64, REPLICATION_TIMEOUT)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "replica node {s} never saw {} replicated rows before the kill: {e}\n{}",
                    ROWS.len(),
                    fixture.stderr_tail(s, 40)
                )
            });
    }
    eprintln!("[hardkill-e2e] all {} rows replicated to every survivor", ROWS.len());

    // ── 5. SIGKILL the primary (no graceful shutdown; the drain gate is skipped)
    eprintln!("[hardkill-e2e] SIGKILLing primary node {primary}");
    fixture.kill_node(primary);

    // ── 6. A survivor is promoted ────────────────────────────────────────────
    let new_primary =
        wait_for_sole_primary(&base_urls, &survivors, REELECTION_TIMEOUT).await.unwrap_or_else(
            |e| panic!("no survivor was promoted after the kill: {e}\n{}", all_stderr(&fixture)),
        );
    assert_ne!(new_primary, primary, "the killed primary cannot be re-elected");
    eprintln!("[hardkill-e2e] promoted new primary: node {new_primary} ({})", base_urls[new_primary]);

    // ── 7. Replicated data is intact on every survivor ──────────────────────
    let want: Vec<String> = ROWS.iter().map(|(_, v)| (*v).to_string()).collect();
    for &s in &survivors {
        let got = mysql_query_strings(&mysql_addrs[s], "SELECT v FROM t ORDER BY id")
            .await
            .unwrap_or_else(|e| {
                panic!("SELECT on survivor node {s} failed: {e}\n{}", fixture.stderr_tail(s, 40))
            });
        assert_eq!(
            got, want,
            "replicated data corrupted/lost after hard kill → promote: survivor node {s} has \
             {got:?}, expected {want:?}"
        );
    }

    // ── 8. The promoted primary accepts a fresh write that replicates ────────
    // The crux of the promotion gate: once the new primary advertises writable,
    // a write must land on a settled state and ship to the other survivor.
    mysql_exec(&mysql_addrs[new_primary], "INSERT INTO t (id, v) VALUES (4, 'delta')")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "post-promotion INSERT on new primary node {new_primary} failed: {e}\n{}",
                fixture.stderr_tail(new_primary, 40)
            )
        });
    let other = survivors.iter().copied().find(|&s| s != new_primary).expect("a second survivor");
    wait_for_row_count(&mysql_addrs[other], (ROWS.len() + 1) as i64, REPLICATION_TIMEOUT)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "post-promotion write never replicated to survivor node {other}: {e}\n{}",
                fixture.stderr_tail(other, 40)
            )
        });
    eprintln!("[hardkill-e2e] post-promotion write replicated to node {other}; promotion is clean");
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

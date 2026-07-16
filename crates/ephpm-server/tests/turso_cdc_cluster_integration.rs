//! Cross-node CDC replication integration test — the shape of the
//! test that would have caught #186's Bugs 1 and 2.
//!
//! # What this proves
//!
//! Runs two INDEPENDENT ephpm cluster nodes in the same test process
//! (each with its own gossip stack, cluster channel, election, Turso
//! DB file). The nodes join the same cluster via a gossip seed and
//! elect one primary. The primary publishes its address into the
//! gossip election KV in the exact format `sqlite_election.rs` uses
//! (`http://addr`). The replica reads it, parses it, resolves the
//! channel `advertise_addr`, dials the primary's channel, and applies
//! a CREATE TABLE + INSERT.
//!
//! # Why this test would have caught the bugs
//!
//! The existing `turso_cdc_e2e.rs` test wires primary and replica
//! together with a hand-constructed `SocketAddr` — it never exercised
//! the `sqlite_election.rs` → `turso_cdc.rs::elected_to_role` path
//! that is the actual bug surface. Both bugs #186 shipped
//! (`http://` prefix breaks SocketAddr parse; `0.0.0.0` advertise
//! breaks remote dial) are silent under a single-hostname in-process
//! wiring — they only fire when the address genuinely round-trips
//! through the election and gets dialed as a routable IP by a
//! separate process.
//!
//! # Cross-container proxy: distinct loopback endpoints
//!
//! Full subprocess-per-node against a Podman fixture is what the
//! `github.com/ephpm/turso-cluster-e2e` repo does; that test is heavy
//! and gated behind `EPHPM_E2E_CLUSTER` in the harness. This test
//! stays in-process but uses distinct loopback endpoints for each
//! node (unique ports, `127.0.0.1` throughout) so the two nodes truly
//! act as remote peers to each other from the code's point of view.
//! On Linux we could use `127.0.0.2`; port-only isolation is enough
//! to make the bugs bite (both bugs fire when the replica's
//! `dial(addr)` uses a different socket than the primary's `bind`).
//!
//! # Local-only gate
//!
//! Even without a full container fixture this test spins up two
//! gossip stacks and waits for cluster convergence, which can be
//! flaky on shared CI runners. Gated behind `EPHPM_CLUSTER_INTEG=1`
//! — set the env var to run it. It is expected to pass locally on
//! developer machines and against the fixed branch.

use std::sync::Arc;
use std::time::Duration;

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, ClusterHandle, ElectedRole, IncomingStream, SqliteElection,
    maybe_start_cluster_channel, start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig};
use litewire::backend::{Backend, Value};
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::{CdcRow, CdcTailer, TxnBatch, apply_batch};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;

const CDC_STREAM_TYPE: &str = "cdc/default";
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
enum Frame {
    Batch { rows: Vec<WireCdcRow> },
    Ping,
}

#[derive(Serialize, Deserialize)]
struct WireCdcRow {
    change_id: i64,
    change_txn_id: Option<i64>,
    change_type: i64,
    table_name: Option<String>,
    id: Option<i64>,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    updates: Option<Vec<u8>>,
}

impl From<&CdcRow> for WireCdcRow {
    fn from(r: &CdcRow) -> Self {
        Self {
            change_id: r.change_id,
            change_txn_id: r.change_txn_id,
            change_type: r.change_type,
            table_name: r.table_name.clone(),
            id: r.id,
            before: r.before.clone(),
            after: r.after.clone(),
            updates: r.updates.clone(),
        }
    }
}

impl From<WireCdcRow> for CdcRow {
    fn from(w: WireCdcRow) -> Self {
        Self {
            change_id: w.change_id,
            change_txn_id: w.change_txn_id,
            change_type: w.change_type,
            table_name: w.table_name,
            id: w.id,
            before: w.before,
            after: w.after,
            updates: w.updates,
        }
    }
}

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) -> anyhow::Result<()> {
    let json = serde_json::to_vec(frame)?;
    let len = u32::try_from(json.len())?;
    anyhow::ensure!(len <= MAX_FRAME_LEN);
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&json).await?;
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Frame> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(len <= MAX_FRAME_LEN);
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Reserve a free port by binding a UDP socket and returning the
/// address it grabbed. Immediately drop the socket so gossip can
/// re-bind. Racy in principle; fine in practice on a dev machine.
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Bring up one ephpm-shaped node: gossip + cluster channel +
/// election. Returns everything you need to drive its role.
struct Node {
    cluster: Arc<ClusterHandle>,
    channel: ChannelHandle,
    election: SqliteElection,
    _gossip_port: u16,
}

async fn bring_up_node(node_id: &str, seed: Option<&str>) -> Node {
    let gossip_port = free_udp_port();
    let join = seed.map(|s| vec![s.to_string()]).unwrap_or_default();
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: format!("127.0.0.1:{gossip_port}"),
        join,
        secret: "cluster-integ-secret".to_string(),
        node_id: node_id.to_string(),
        cluster_id: "cdc-integ".to_string(),
        ..ClusterConfig::default()
    };
    let cluster = Arc::new(start_gossip(&cluster_cfg).await.expect("gossip start"));

    let channel = maybe_start_cluster_channel(
        &ClusterChannelConfig::default(),
        &cluster_cfg.secret,
        &cluster,
        ChannelFeatureFlags { cdc: true },
    )
    .await
    .expect("channel start")
    .expect("channel bound");

    // The election is what publishes the elected-primary KV entry in
    // the *exact* format the bug lives in (`http://addr`). We use
    // the channel's `advertise_addr()` — not `listen_addr()` — which
    // is Bug 2's fix on the caller side.
    let advertise = channel.advertise_addr().expect("advertise resolved (loopback bind)");
    let election = SqliteElection::new(Arc::clone(&cluster), advertise.to_string());

    Node { cluster, channel, election, _gossip_port: gossip_port }
}

/// Wait for both nodes to see each other in the cluster.
async fn await_cluster_size(nodes: &[&Node], expected: usize, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let mut all_ok = true;
        for n in nodes {
            if n.cluster.live_node_count().await < expected {
                all_ok = false;
                break;
            }
        }
        if all_ok {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn count_rows(backend: &Arc<Turso>, table: &str) -> i64 {
    let rs = backend.query(&format!("SELECT COUNT(*) FROM \"{table}\""), &[]).await;
    match rs {
        Ok(rs) => match &rs.rows[0][0] {
            Value::Integer(i) => *i,
            v => panic!("count non-integer: {v:?}"),
        },
        Err(_) => -1,
    }
}

async fn eventually_async<F, Fut>(mut check: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check().await
}

/// Wait for the election watcher to publish a non-empty primary URL
/// (i.e. gossip has propagated the elected-primary claim). Returns
/// the string exactly as `elected_to_role` would receive it, so
/// callers can assert on its shape.
async fn wait_for_non_empty_replica_url(
    rx: &mut tokio::sync::watch::Receiver<ElectedRole>,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let role = rx.borrow().clone();
            if let ElectedRole::Replica { primary_grpc_url } = &role {
                if !primary_grpc_url.is_empty() {
                    return Some(primary_grpc_url.clone());
                }
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        // Wake either on a watch change or on a short sleep, whichever comes first.
        let _ = tokio::time::timeout(Duration::from_millis(250), rx.changed()).await;
    }
}

/// Drive the primary side: tail loop + accept subscribers on the
/// channel. Returns the join handles so the caller keeps them alive.
async fn drive_primary_role(
    mgmt: Arc<Turso>,
    channel: &ChannelHandle,
) -> Vec<tokio::task::JoinHandle<()>> {
    let (tx, _rx0) = broadcast::channel::<Arc<TxnBatch>>(1024);
    let mut cdc_streams = channel.register_exact(CDC_STREAM_TYPE);
    let tx_for_subs = tx.clone();
    let dispatch = tokio::spawn(async move {
        while let Some(incoming) = cdc_streams.recv().await {
            let mut rx = tx_for_subs.subscribe();
            let IncomingStream { mut stream, .. } = incoming;
            tokio::spawn(async move {
                while let Ok(batch) = rx.recv().await {
                    let frame =
                        Frame::Batch { rows: batch.rows.iter().map(WireCdcRow::from).collect() };
                    if write_frame(&mut stream, &frame).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let tail = tokio::spawn(async move {
        let mut tailer = CdcTailer::new(&mgmt, 0);
        loop {
            match tailer.poll_batch().await {
                Ok(Some(batch)) => {
                    let _ = tx.send(Arc::new(batch));
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });

    vec![dispatch, tail]
}

/// Drive the replica side: dial the channel with what the election
/// told us the primary address is. Deliberately takes the
/// `ElectedRole::Replica` variant and re-runs the SAME parse the
/// production code does, to catch Bug 1 for real.
async fn drive_replica_role(
    mgmt: Arc<Turso>,
    elected: ElectedRole,
    channel: ChannelHandle,
) -> tokio::task::JoinHandle<()> {
    // Invoke the PRODUCTION `parse_primary_addr` — if it rejects
    // the URL form we fail loudly, which was Bug 1. Using the real
    // function (not a test-only copy) is the whole point: if
    // somebody tightens the parse back up to `SocketAddr::from_str`
    // in the future, this test fails, which is what the "missing
    // test" would have done at PR-review time.
    let primary_addr = match elected {
        ElectedRole::Replica { primary_grpc_url } => {
            ephpm_server::turso_cdc::parse_primary_addr(&primary_grpc_url)
                .expect("elected primary address must parse (regression: Bug 1)")
        }
        ElectedRole::Primary => panic!("expected Replica variant"),
    };

    let apply_conn = mgmt.raw_connection().unwrap();
    tokio::spawn(async move {
        loop {
            match channel.dial(primary_addr, CDC_STREAM_TYPE).await {
                Ok(mut stream) => loop {
                    match read_frame(&mut stream).await {
                        Ok(Frame::Batch { rows }) => {
                            let batch =
                                TxnBatch { rows: rows.into_iter().map(CdcRow::from).collect() };
                            if let Err(e) = apply_batch(&apply_conn, &batch).await {
                                eprintln!("apply_batch: {e:#}");
                            }
                        }
                        Ok(Frame::Ping) => {}
                        Err(_) => break,
                    }
                },
                Err(e) => {
                    eprintln!("dial({primary_addr}) failed: {e:#}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    })
}

/// HEADLINE: two independent cluster nodes, real election over
/// gossip, real channel dial, DDL + DML on the primary lands on the
/// replica.
///
/// **This test is gated behind `EPHPM_CLUSTER_INTEG=1`** — it spins
/// up two gossip stacks and waits for convergence, which is not
/// suitable for every CI runner but is trivially runnable on a dev
/// machine (`EPHPM_CLUSTER_INTEG=1 cargo test -p ephpm-server
/// two_node_cross_endpoint_cdc_replication`). See the module doc for
/// context.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_node_cross_endpoint_cdc_replication_via_real_election() {
    if std::env::var("EPHPM_CLUSTER_INTEG").ok().as_deref() != Some("1") {
        eprintln!("SKIP: set EPHPM_CLUSTER_INTEG=1 to run this test");
        return;
    }

    // Node A is brought up first, no seed. Node B joins A via
    // `join = ["127.0.0.1:<A gossip port>"]` — gossip converges,
    // both nodes see each other, election picks a primary.
    let node_a = bring_up_node("ephpm-a", None).await;
    let seed_addr = format!("127.0.0.1:{}", node_a._gossip_port);
    let node_b = bring_up_node("ephpm-b", Some(&seed_addr)).await;

    assert!(
        await_cluster_size(&[&node_a, &node_b], 2, Duration::from_secs(10)).await,
        "cluster did not converge to 2 nodes in 10s"
    );

    // Kick both elections. Lowest-id (`ephpm-a`) will win.
    let initial_a = node_a.election.determine_initial_role().await;
    let a_rx = node_a.election.watch_role();
    let mut b_rx = node_b.election.watch_role();
    tokio::spawn(node_a.election.run());
    tokio::spawn(node_b.election.run());

    // Verify A is primary; wait for B's watcher to observe the
    // published claim. The election heartbeat is 5s and the initial
    // determine_initial_role races the peer publish; we wait up to
    // 20s for gossip to propagate + the election tick to fire on B.
    assert!(matches!(initial_a, ElectedRole::Primary), "A must be primary, got {initial_a:?}");

    let primary_url = wait_for_non_empty_replica_url(&mut b_rx, Duration::from_secs(20))
        .await
        .expect("B must observe primary URL via election within 20s");
    assert!(
        primary_url.starts_with("http://"),
        "election KV format regression: expected 'http://addr' scheme, got {primary_url:?}"
    );
    assert!(
        !primary_url.contains("0.0.0.0") && !primary_url.contains("[::]"),
        "advertise_addr regression (Bug 2): primary published wildcard IP {primary_url:?}"
    );

    // Now wire up the actual CDC datapath using the elected role.
    let primary_file = tempfile::NamedTempFile::new().unwrap();
    let replica_file = tempfile::NamedTempFile::new().unwrap();

    let primary_wire = Arc::new(
        Turso::builder(primary_file.path().to_str().unwrap())
            .enable_cdc_on_connect(true)
            .build()
            .await
            .unwrap(),
    );
    let primary_mgmt = Arc::new(Turso::open(primary_file.path().to_str().unwrap()).await.unwrap());
    let replica_wire = Arc::new(Turso::open(replica_file.path().to_str().unwrap()).await.unwrap());
    let replica_mgmt = Arc::new(Turso::open(replica_file.path().to_str().unwrap()).await.unwrap());

    let _primary_handles = drive_primary_role(Arc::clone(&primary_mgmt), &node_a.channel).await;

    // Drive the replica with the ACTUAL elected role (URL form).
    let _replica_handle = drive_replica_role(
        Arc::clone(&replica_mgmt),
        ElectedRole::Replica { primary_grpc_url: primary_url.clone() },
        node_b.channel.clone(),
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let session = primary_wire.connect().await.unwrap();
    session
        .execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL)", &[])
        .await
        .unwrap();
    session.execute("INSERT INTO posts VALUES (1, 'from-election')", &[]).await.unwrap();

    let converged = eventually_async(
        || {
            let rw = Arc::clone(&replica_wire);
            async move { count_rows(&rw, "posts").await == 1 }
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        converged,
        "replica did not converge — is the CDC channel dial reaching the primary at {primary_url}?"
    );

    let rs = replica_wire.query("SELECT id, title FROM posts", &[]).await.unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Integer(1));
    assert_eq!(rs.rows[0][1], Value::Text("from-election".into()));

    // Keep node handles alive to end of test — chitchat needs the
    // handle held while gossip is heartbeating.
    let _ = (a_rx, node_a.cluster, node_b.cluster);
}

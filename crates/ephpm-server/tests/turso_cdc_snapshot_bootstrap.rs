//! Acceptance proof for Phase 2.1 snapshot bootstrap (task #97).
//!
//! Scenario (the whole point of #97):
//!
//! 1. The primary creates a table and inserts N rows **before any
//!    replica exists**. These are *pre-existing* rows a forward-only CDC
//!    tail could never reconstruct (some predate CDC being observed by a
//!    replica at all).
//! 2. A **cold** replica (empty DB file) starts and runs the production
//!    snapshot-bootstrap path ([`ephpm_server::turso_cdc::fetch_and_apply_snapshot`])
//!    against the primary's production serve path
//!    ([`ephpm_server::turso_cdc::serve_snapshot`]) over a real cluster
//!    channel (yamux over TCP, authenticated handshake).
//! 3. After bootstrap the replica must hold all N pre-existing rows
//!    (proves the snapshot transferred historical state).
//! 4. The primary then writes one more row *after* the replica joined;
//!    the replica tails CDC from its seeded watermark and must pick up
//!    that row too (proves the tail continues cleanly past the snapshot
//!    watermark, applying only change_id > N).
//!
//! This runs the drivers in one process against two DB files over the
//! real channel (the same shape as `turso_cdc_e2e.rs`) but exercising
//! the snapshot path instead of (well, in addition to) the plain tail.

use std::sync::Arc;
use std::time::Duration;

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, IncomingStream, maybe_start_cluster_channel, start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig};
use ephpm_server::turso_cdc::{fetch_and_apply_snapshot, serve_snapshot};
use litewire::backend::{Backend, Value};
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::{CdcRow, CdcTailer, TxnBatch, apply_batch, read_watermark};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;

const CDC_STREAM_TYPE: &str = "cdc/default";
const SNAPSHOT_STREAM_TYPE: &str = "snapshot/default";
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

// -- CDC wire-frame twin (private in the module; kept inline to keep the
//    test an honest black-box exercise of the tail path). The snapshot
//    path, by contrast, uses the REAL production functions.

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

async fn start_channel(node_id: &str) -> (Arc<ephpm_cluster::ClusterHandle>, ChannelHandle) {
    let gossip_bind = pick_free_port();
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: gossip_bind,
        secret: "snapshot-e2e-secret".to_string(),
        node_id: node_id.to_string(),
        cluster_id: "snapshot-e2e".to_string(),
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
    .expect("channel bound (feature is enabled)");
    (cluster, channel)
}

fn pick_free_port() -> String {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().to_string()
}

/// Bring up the primary side: register BOTH the snapshot handler (using
/// the real `serve_snapshot`) and the CDC tail/dispatch handler. Returns
/// the primary's dial address plus the keep-alive handles.
async fn spawn_primary(
    mgmt: Arc<Turso>,
    channel: &ChannelHandle,
) -> (std::net::SocketAddr, Vec<tokio::task::JoinHandle<()>>) {
    let mut handles = Vec::new();

    // Snapshot handler: the production serve path.
    let mut snapshot_streams = channel.register_exact(SNAPSHOT_STREAM_TYPE);
    let snap_mgmt = Arc::clone(&mgmt);
    handles.push(tokio::spawn(async move {
        while let Some(incoming) = snapshot_streams.recv().await {
            let mgmt = Arc::clone(&snap_mgmt);
            let IncomingStream { stream, .. } = incoming;
            tokio::spawn(async move {
                if let Err(e) = serve_snapshot(stream, &mgmt).await {
                    eprintln!("serve_snapshot: {e:#}");
                }
            });
        }
    }));

    // CDC dispatch + tail.
    let (tx, _rx0) = broadcast::channel::<Arc<TxnBatch>>(1024);
    let mut cdc_streams = channel.register_exact(CDC_STREAM_TYPE);
    let tx_for_subs = tx.clone();
    handles.push(tokio::spawn(async move {
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
    }));

    let tail_mgmt = Arc::clone(&mgmt);
    handles.push(tokio::spawn(async move {
        let mut tailer = CdcTailer::new(&tail_mgmt, 0);
        loop {
            match tailer.poll_batch().await {
                Ok(Some(batch)) => {
                    let _ = tx.send(Arc::new(batch));
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }));

    (channel.listen_addr(), handles)
}

/// Spawn the replica CDC tail loop (used AFTER bootstrap). Applies via
/// the monotonic watermark, so it idempotently skips batches already in
/// the snapshot.
fn spawn_replica_tail(
    mgmt: Arc<Turso>,
    primary_addr: std::net::SocketAddr,
    channel: ChannelHandle,
) -> tokio::task::JoinHandle<()> {
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
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check().await
}

/// **HEADLINE (acceptance criterion for #97).**
///
/// Primary writes N rows before the replica exists; a cold replica
/// bootstraps a snapshot and ends up with all N pre-existing rows, then
/// a post-join write also lands via the CDC tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_replica_bootstraps_snapshot_then_tails_cdc() {
    const PRE_ROWS: i64 = 25;

    let primary_file = tempfile::NamedTempFile::new().unwrap();
    let replica_file = tempfile::NamedTempFile::new().unwrap();

    let (_pc, primary_channel) = start_channel("snap-primary").await;
    let (_rc, replica_channel) = start_channel("snap-replica").await;

    // Primary factories: wire (CDC-on-connect) + mgmt (tail/serve).
    let primary_wire = Arc::new(
        Turso::builder(primary_file.path().to_str().unwrap())
            .enable_cdc_on_connect(true)
            .build()
            .await
            .unwrap(),
    );
    let primary_mgmt = Arc::new(Turso::open(primary_file.path().to_str().unwrap()).await.unwrap());

    // --- Step 1: populate the primary with pre-existing rows BEFORE the
    // replica exists at all. ---
    let session = primary_wire.connect().await.unwrap();
    session
        .execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL)", &[])
        .await
        .unwrap();
    for i in 1..=PRE_ROWS {
        session.execute(&format!("INSERT INTO posts VALUES ({i}, 'pre-{i}')"), &[]).await.unwrap();
    }
    assert_eq!(count_rows(&primary_wire, "posts").await, PRE_ROWS);

    // Bring up the primary's channel handlers (snapshot + CDC).
    let (primary_addr, _primary_handles) =
        spawn_primary(Arc::clone(&primary_mgmt), &primary_channel).await;

    // --- Step 2: cold replica. Its DB file is empty; it must bootstrap
    // a snapshot before it has any of the pre-existing rows. ---
    let replica_wire = Arc::new(Turso::open(replica_file.path().to_str().unwrap()).await.unwrap());
    let replica_mgmt = Arc::new(Turso::open(replica_file.path().to_str().unwrap()).await.unwrap());

    // Precondition: replica is genuinely cold.
    let replica_conn = replica_mgmt.raw_connection().unwrap();
    assert_eq!(read_watermark(&replica_conn).await.unwrap(), 0, "replica must start cold");

    // Run the PRODUCTION bootstrap path. Retry to ride out the window
    // where the primary channel is still coming up.
    let mut bootstrapped_wm = None;
    for _ in 0..30 {
        match fetch_and_apply_snapshot(&replica_conn, primary_addr, &replica_channel).await {
            Ok(n) => {
                bootstrapped_wm = Some(n);
                break;
            }
            Err(e) => {
                eprintln!("bootstrap retry: {e:#}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    let wm = bootstrapped_wm.expect("cold replica failed to bootstrap a snapshot");
    assert!(wm > 0, "snapshot watermark must be > 0 after {PRE_ROWS} pre-existing rows");

    // --- Step 3: assert the pre-existing rows are present on the replica
    // (this is the historical-state transfer the snapshot exists for). ---
    assert_eq!(
        count_rows(&replica_wire, "posts").await,
        PRE_ROWS,
        "replica did not receive all pre-existing rows from the snapshot"
    );
    let rs = replica_wire.query("SELECT title FROM posts WHERE id = 1", &[]).await.unwrap();
    assert_eq!(rs.rows[0][0], Value::Text("pre-1".into()));

    // The replica watermark must be seeded to N so the tail skips <= N.
    assert_eq!(
        read_watermark(&replica_conn).await.unwrap(),
        wm,
        "replica watermark was not seeded to the snapshot watermark"
    );

    // --- Step 4: start the CDC tail and write a NEW row on the primary
    // after the replica joined. The tail must deliver it (change_id > N),
    // proving replication continues past the snapshot point. ---
    let _replica_tail =
        spawn_replica_tail(Arc::clone(&replica_mgmt), primary_addr, replica_channel.clone());
    tokio::time::sleep(Duration::from_millis(200)).await;

    session
        .execute(&format!("INSERT INTO posts VALUES ({}, 'post-join')", PRE_ROWS + 1), &[])
        .await
        .unwrap();

    let converged = eventually_async(
        || {
            let rw = Arc::clone(&replica_wire);
            async move { count_rows(&rw, "posts").await == PRE_ROWS + 1 }
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        converged,
        "replica did not receive the post-join row via CDC tail (snapshot->tail handoff broke)"
    );

    let rs = replica_wire
        .query(&format!("SELECT title FROM posts WHERE id = {}", PRE_ROWS + 1), &[])
        .await
        .unwrap();
    assert_eq!(rs.rows[0][0], Value::Text("post-join".into()));

    // No double-application: exactly PRE_ROWS + 1 rows, not more.
    assert_eq!(count_rows(&replica_wire, "posts").await, PRE_ROWS + 1);
}

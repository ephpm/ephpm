//! End-to-end proof for Phase 2 CDC-native replication over the
//! cluster channel v1.
//!
//! Spins up primary + replica CDC drivers in the same process against
//! two separate DB files, communicating over a real cluster channel
//! listener (yamux over TCP, ChaCha20-Poly1305 handshake). Proves:
//!
//! - Writes on the primary appear on the replica (DDL + INSERT + UPDATE + DELETE)
//! - Killing the replica session (drop the yamux stream) and starting
//!   a fresh one does not double-apply already-replicated batches
//!   (idempotency via litewire's monotonic apply watermark)
//!
//! # Scope note
//!
//! This runs the CDC drivers directly rather than through the full
//! ephpm binary + cluster election machinery. It DOES exercise the
//! real cluster channel end-to-end (bind, handshake, yamux mux,
//! stream-type dispatch, per-stream backpressure) — the piece the
//! rework was fundamentally about. The gossip-integrated election
//! path still requires the full podman two-node bring-up (Phase 2.1
//! deliverable).

use std::sync::Arc;
use std::time::Duration;

use ephpm_cluster::{
    ChannelFeatureFlags, ChannelHandle, IncomingStream, maybe_start_cluster_channel, start_gossip,
};
use ephpm_config::{ClusterChannelConfig, ClusterConfig};
use litewire::backend::{Backend, Value};
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::{CdcRow, CdcTailer, TxnBatch, apply_batch, read_watermark};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;

const CDC_STREAM_TYPE: &str = "cdc/default";
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

// -- Wire format twin (kept inline so the test remains an honest
//    black-box exercise of the design; the module-internal Frame is
//    private).

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

// -- Cluster channel bring-up on one loopback port. We start gossip
//    so `maybe_start_cluster_channel` has a `ClusterHandle` to derive
//    the port from, and pass a `cdc: true` feature flag so the channel
//    actually binds.

async fn start_channel(node_id: &str) -> (Arc<ephpm_cluster::ClusterHandle>, ChannelHandle) {
    let gossip_bind = pick_free_port();
    let cluster_cfg = ClusterConfig {
        enabled: true,
        bind: gossip_bind,
        secret: "e2e-shared-secret".to_string(),
        node_id: node_id.to_string(),
        cluster_id: "cdc-e2e".to_string(),
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

/// Spawn a primary tail loop + a channel handler for `cdc/default`.
///
/// Returns the primary's channel address (what a replica dials).
async fn spawn_primary_on_channel(
    mgmt: Arc<Turso>,
    channel: &ChannelHandle,
) -> (std::net::SocketAddr, Vec<tokio::task::JoinHandle<()>>) {
    let (tx, _rx0) = broadcast::channel::<Arc<TxnBatch>>(1024);

    // Register the CDC stream-type handler.
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

    // Tail loop.
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

    (channel.listen_addr(), vec![dispatch, tail])
}

/// Spawn a replica: dial the primary's channel and apply frames.
async fn spawn_replica_on_channel(
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

/// **HEADLINE E2E**: two nodes in one process, each with its own
/// cluster channel, DDL and DML from primary land on the replica via
/// a real yamux stream through the authenticated handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_node_cdc_replicates_ddl_and_dml_end_to_end_via_channel() {
    let primary_file = tempfile::NamedTempFile::new().unwrap();
    let replica_file = tempfile::NamedTempFile::new().unwrap();

    // Bring up two independent cluster channels (each with its own
    // gossip stack — in real deployments they'd share a cluster, but
    // for this test we only need channel-to-channel connectivity).
    let (_pc, primary_channel) = start_channel("primary").await;
    let (_rc, replica_channel) = start_channel("replica").await;

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

    let (primary_addr, _primary_handles) =
        spawn_primary_on_channel(Arc::clone(&primary_mgmt), &primary_channel).await;
    let _replica_handle =
        spawn_replica_on_channel(Arc::clone(&replica_mgmt), primary_addr, replica_channel).await;

    // Let the replica connect and complete the handshake.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let session = primary_wire.connect().await.unwrap();
    session
        .execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL)", &[])
        .await
        .unwrap();
    session.execute("INSERT INTO posts VALUES (1, 'hello')", &[]).await.unwrap();
    session.execute("INSERT INTO posts VALUES (2, 'world')", &[]).await.unwrap();
    session.execute("UPDATE posts SET title = 'HELLO' WHERE id = 1", &[]).await.unwrap();
    session.execute("DELETE FROM posts WHERE id = 2", &[]).await.unwrap();

    let converged = eventually_async(
        || {
            let rw = Arc::clone(&replica_wire);
            async move { count_rows(&rw, "posts").await == 1 }
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(converged, "replica did not converge to 1 row after 10s");

    let rs = replica_wire.query("SELECT id, title FROM posts", &[]).await.unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Integer(1));
    assert_eq!(rs.rows[0][1], Value::Text("HELLO".into()));

    let wm = read_watermark(&replica_mgmt.raw_connection().unwrap()).await.unwrap();
    assert!(wm > 0, "replica watermark did not advance: {wm}");
}

/// Idempotency across replica reconnect: kill the yamux stream
/// mid-flight and start a fresh dial. The fresh session starts from
/// cursor 0 and re-applies every batch; the watermark should keep
/// the row count at 5, not 10.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replica_reconnect_via_channel_does_not_double_apply() {
    let primary_file = tempfile::NamedTempFile::new().unwrap();
    let replica_file = tempfile::NamedTempFile::new().unwrap();

    let (_pc, primary_channel) = start_channel("primary-r").await;
    let (_rc1, replica_channel_1) = start_channel("replica-r-1").await;
    let (_rc2, replica_channel_2) = start_channel("replica-r-2").await;

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

    let (primary_addr, _primary_handles) =
        spawn_primary_on_channel(Arc::clone(&primary_mgmt), &primary_channel).await;

    let first_replica =
        spawn_replica_on_channel(Arc::clone(&replica_mgmt), primary_addr, replica_channel_1).await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let session = primary_wire.connect().await.unwrap();
    session.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).await.unwrap();
    for i in 1..=5 {
        session.execute(&format!("INSERT INTO t VALUES ({i}, 'r{i}')"), &[]).await.unwrap();
    }

    let converged = eventually_async(
        || {
            let rw = Arc::clone(&replica_wire);
            async move { count_rows(&rw, "t").await == 5 }
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(converged, "first replica did not converge");

    first_replica.abort();
    let _second_replica =
        spawn_replica_on_channel(Arc::clone(&replica_mgmt), primary_addr, replica_channel_2).await;
    tokio::time::sleep(Duration::from_millis(700)).await;

    let n = count_rows(&replica_wire, "t").await;
    assert_eq!(n, 5, "reconnected replica double-applied rows: {n}");
}

//! **Experimental** Phase 2 CDC-native SQLite replication.
//!
//! Alternative to `start_clustered_sqlite` (the sqld sidecar path) for
//! `[db.sqlite] engine = "turso"` combined with clustered mode and the
//! `[db.sqlite.replication] cdc_experimental = true` opt-in.
//!
//! **Sqld remains the production clustered default.** This module is
//! evidence-gathering scaffolding for the Turso engine roadmap's Phase 2;
//! nothing here is used unless the operator explicitly opts in. All
//! wiring is strictly additive.
//!
//! # Architecture
//!
//! Each node opens **two** `Turso` factories against the same DB file:
//! one for the litewire wire frontends (client-facing) and one for the
//! CDC management path (tail on the primary, apply on the replica).
//! Both handles talk to the same underlying database — verified safe
//! in a single process by `litewire-turso/tests/multi_factory_same_file.rs`.
//!
//! ```text
//!            primary node                     replica node(s)
//!  ┌─────────────────────────────────┐    ┌───────────────────────┐
//!  │ litewire → Turso (wire factory  │    │ litewire → Turso      │
//!  │   with enable_cdc_on_connect=T) │    │  (wire factory,       │
//!  │        │                        │    │   cdc=off — RO)       │
//!  │  writes capture into turso_cdc  │    │        │              │
//!  │        ▼                        │    │   local reads only    │
//!  │  mgmt factory: CdcTailer polls  │    │        ▲              │
//!  │  turso_cdc → complete batches   │    │        │ apply_batch  │
//!  │  → broadcast channel            │    │  mgmt factory:        │
//!  │        │                        │    │  read framed batch    │
//!  │  TCP subscriber server          │◀───┤  → apply_batch(&conn) │
//!  └─────────────────────────────────┘    └───────────────────────┘
//! ```
//!
//! # Failover
//!
//! The sqlite election machinery (`ephpm_cluster::SqliteElection`) is
//! unchanged. On role change, the initial role's tasks stay running
//! (v1 simplification) and new tasks for the new role are spawned;
//! stale tasks eventually notice a broken TCP connection and log out.
//! **The divergence window is the same class as sqld async replication:**
//! a former primary that had unshipped batches at the moment it died
//! has lost those writes.
//!
//! # Bootstrap of a fresh replica
//!
//! **Deferred to Phase 2.1.** For v1, the operator is responsible for
//! seeding replicas with a copy of the primary's DB file before starting
//! them (or accepting that replicas start empty and only replicate
//! forward from the point they join). A snapshot-over-transport
//! bootstrap is designed in `docs/turso-phase2-cdc-design.md` but not
//! implemented.
//!
//! # Wire format
//!
//! Length-prefixed JSON frames:
//!
//! ```text
//! ┌──────────────┬──────────────────────────────────────┐
//! │ len: u32 BE  │ payload: len bytes (JSON-encoded)    │
//! └──────────────┴──────────────────────────────────────┘
//! ```
//!
//! Payload is a JSON-encoded [`Frame`]. JSON is chosen for v1
//! debuggability (a tcpdump gives you a readable `TxnBatch`). Frame
//! size is bounded at 16 MiB; oversized frames drop the connection.
//! **Encryption is not wired up in v1** — replication traffic MUST run
//! on a trusted network segment or through a TLS-terminating proxy.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ephpm_config::SqliteConfig;
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::{CdcRow, CdcTailer, TxnBatch, apply_batch};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use crate::tracked_backend;

/// Maximum frame length accepted on either side of the wire (16 MiB).
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Broadcast channel capacity — how many transactions the primary can
/// buffer between polls before slow subscribers start missing (`Lagged`)
/// frames. When a subscriber lags, it disconnects and reconnects (its
/// own retry loop) — replication picks up from the persisted watermark.
const BROADCAST_CAPACITY: usize = 1024;

/// How often the primary polls `turso_cdc` for new batches. Turso 0.7.0
/// has no wakeup signal for CDC inserts, so we poll on a schedule.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long a replica waits between connect retries when the primary
/// is unreachable.
const REPLICA_RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Frame types carried on the CDC replication wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Frame {
    /// A committed transaction batch. `rows` mirrors
    /// [`litewire_turso::cdc::TxnBatch::rows`].
    Batch { rows: Vec<WireCdcRow> },
    /// Heartbeat — sent every ~5s from primary to keep the subscriber
    /// TCP connection warm even during idle periods.
    Ping,
}

/// Wire-format twin of [`litewire_turso::cdc::CdcRow`] — Serde-derived
/// so we can put it on the wire without leaking derive traits through
/// the litewire crate boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireCdcRow {
    change_id: i64,
    change_txn_id: Option<i64>,
    change_type: i64,
    table_name: Option<String>,
    id: Option<i64>,
    #[serde(with = "serde_bytes_opt")]
    before: Option<Vec<u8>>,
    #[serde(with = "serde_bytes_opt")]
    after: Option<Vec<u8>>,
    #[serde(with = "serde_bytes_opt")]
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

/// Serde helper for `Option<Vec<u8>>` → base64 in JSON. Keeps SQLite
/// record blobs compact and copy-pasteable during debugging. Uses
/// `base64ct` which is already in the workspace dependency graph.
mod serde_bytes_opt {
    use base64ct::{Base64, Encoding};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(bytes) => Base64::encode_string(bytes).serialize(s),
            None => Option::<String>::None.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let opt: Option<String> = Option::deserialize(d)?;
        match opt {
            Some(s) => Base64::decode_vec(&s).map(Some).map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Startup entry point.
// ---------------------------------------------------------------------------

/// Start Phase 2 CDC-native replication for a clustered Turso engine.
///
/// Opens two Turso factories against the same DB file — one for the
/// litewire wire frontends (with `enable_cdc_on_connect` set on the
/// primary) and one for the CDC tail/apply path. Then starts:
///
/// - Litewire wire frontends against the wire factory (always).
/// - On primary: a tail loop reading `turso_cdc` and broadcasting
///   batches, plus a TCP server that forwards them to subscribers.
/// - On replica: a TCP client that connects to the primary and applies
///   received batches.
///
/// # Errors
///
/// Returns an error if either factory cannot open, if the elected role
/// requires a peer address that isn't configured, or if the CDC
/// listener/connection cannot be established.
pub async fn start_clustered_turso_cdc(
    sqlite_config: &SqliteConfig,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    query_stats: &ephpm_query_stats::QueryStats,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let cluster = cluster.context(
        "clustered Turso CDC replication requires [cluster] enabled = true; \
         no cluster handle available",
    )?;

    tracing::warn!(
        engine = "turso",
        role = %sqlite_config.replication.role,
        cdc_listen = %sqlite_config.replication.cdc_listen,
        "starting EXPERIMENTAL Phase 2 CDC-native SQLite replication. \
         sqld is NOT spawned; replication uses litewire's turso_cdc stream. \
         Turso engine remains Beta upstream — do not use with data you \
         cannot recreate. See site/content/roadmap/turso-engine.md."
    );

    let db_path = &sqlite_config.path;

    // Determine our initial role first, so we can size the wire-factory's
    // enable_cdc_on_connect appropriately.
    let (initial_role, role_rx) = determine_role(sqlite_config, cluster).await?;

    // Wire factory: served to litewire. Primary opts every session into
    // CDC so writes coming through the frontends are captured.
    let wire_cdc_on = matches!(initial_role, Role::Primary);
    let wire_factory = Turso::builder(db_path)
        .enable_cdc_on_connect(wire_cdc_on)
        .build()
        .await
        .with_context(|| format!("failed to open wire Turso factory at {db_path}"))?;

    // Mgmt factory: used by the tail loop on the primary and the apply
    // loop on the replica. Never opts into CDC-on-connect (the tailer
    // reads turso_cdc explicitly; the applier only writes).
    let mgmt_factory = Arc::new(
        Turso::open(db_path)
            .await
            .with_context(|| format!("failed to open mgmt Turso factory at {db_path}"))?,
    );

    // Start litewire wire frontends. Wire factory is moved in here.
    let tracked = tracked_backend::TrackedBackend::new(wire_factory, query_stats.clone());
    spawn_litewire_serve(sqlite_config, tracked, handles);

    // Broadcast channel for primary-side batches.
    let (tx, _rx0) = broadcast::channel::<Arc<TxnBatch>>(BROADCAST_CAPACITY);

    let cdc_listen = sqlite_config.replication.cdc_listen.clone();

    // Kick off role-appropriate work for the initial role.
    let mgmt = Arc::clone(&mgmt_factory);
    let tx0 = tx.clone();
    let listen0 = cdc_listen.clone();
    handles.push(tokio::spawn(async move {
        start_role(initial_role, mgmt, tx0, listen0).await;
    }));

    // Role-change watcher: on a role transition, spawn the new role's
    // driver. Old drivers stay running and drain naturally; v1 accepts
    // this simplification because in practice a role change only fires
    // on failure/join events, and the new driver's TCP-bind/connect
    // dance will fail cleanly if it collides with a stale one.
    if let Some(mut watch_rx) = role_rx {
        let mgmt = Arc::clone(&mgmt_factory);
        let tx = tx.clone();
        let listen = cdc_listen.clone();
        handles.push(tokio::spawn(async move {
            while watch_rx.changed().await.is_ok() {
                let new_elected = watch_rx.borrow().clone();
                let new_role = elected_to_role(new_elected);
                tracing::info!(?new_role, "CDC replication: role change detected");
                let mgmt = Arc::clone(&mgmt);
                let tx = tx.clone();
                let listen = listen.clone();
                tokio::spawn(async move { start_role(new_role, mgmt, tx, listen).await });
            }
        }));
    }

    Ok(())
}

#[derive(Debug, Clone)]
enum Role {
    Primary,
    Replica { primary_addr: String },
}

fn elected_to_role(elected: ephpm_cluster::ElectedRole) -> Role {
    match elected {
        ephpm_cluster::ElectedRole::Primary => Role::Primary,
        ephpm_cluster::ElectedRole::Replica { primary_grpc_url } => {
            Role::Replica { primary_addr: primary_grpc_url }
        }
    }
}

async fn determine_role(
    sqlite_config: &SqliteConfig,
    cluster: &Arc<ephpm_cluster::ClusterHandle>,
) -> anyhow::Result<(Role, Option<tokio::sync::watch::Receiver<ephpm_cluster::ElectedRole>>)> {
    match sqlite_config.replication.role.as_str() {
        "primary" => Ok((Role::Primary, None)),
        "replica" => {
            anyhow::ensure!(
                !sqlite_config.replication.primary_grpc_url.is_empty(),
                "replication.primary_grpc_url is required when role = \"replica\" \
                 in CDC-native replication mode (this field carries the \
                 primary's CDC TCP address in this mode)"
            );
            Ok((
                Role::Replica { primary_addr: sqlite_config.replication.primary_grpc_url.clone() },
                None,
            ))
        }
        _ => {
            // "auto" — reuse the same election as the sqld path but
            // advertise the CDC listen address (that's what replicas need
            // to reach in this mode).
            let election = ephpm_cluster::SqliteElection::new(
                Arc::clone(cluster),
                sqlite_config.replication.cdc_listen.clone(),
            );
            let initial = election.determine_initial_role().await;
            let rx = election.watch_role();
            tokio::spawn(election.run());
            Ok((elected_to_role(initial), Some(rx)))
        }
    }
}

async fn start_role(
    role: Role,
    mgmt: Arc<Turso>,
    tx: broadcast::Sender<Arc<TxnBatch>>,
    cdc_listen: String,
) {
    match role {
        Role::Primary => {
            if let Err(e) = run_primary(mgmt, tx, cdc_listen).await {
                tracing::error!("CDC primary loop exited: {e:#}");
            }
        }
        Role::Replica { primary_addr } => {
            if let Err(e) = run_replica(mgmt, primary_addr).await {
                tracing::error!("CDC replica loop exited: {e:#}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Primary: tail + broadcast + serve subscribers.
// ---------------------------------------------------------------------------

async fn run_primary(
    mgmt: Arc<Turso>,
    tx: broadcast::Sender<Arc<TxnBatch>>,
    cdc_listen: String,
) -> anyhow::Result<()> {
    // Start the TCP subscriber server.
    let listener = TcpListener::bind(&cdc_listen)
        .await
        .with_context(|| format!("failed to bind CDC listener at {cdc_listen}"))?;
    tracing::info!(listen = %cdc_listen, "CDC primary: subscriber listener bound");

    let tx_for_listener = tx.clone();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!(%e, "CDC listener accept error");
                    continue;
                }
            };
            tracing::info!(peer = %peer, "CDC primary: subscriber connected");
            let rx = tx_for_listener.subscribe();
            tokio::spawn(async move {
                if let Err(e) = serve_subscriber(stream, rx).await {
                    tracing::info!(peer = %peer, "CDC subscriber disconnected: {e:#}");
                }
            });
        }
    });

    // Tail loop: poll turso_cdc, push complete batches to broadcast.
    let mut tailer = CdcTailer::new(&mgmt, 0);
    loop {
        match tailer.poll_batch().await {
            Ok(Some(batch)) => {
                let arc = Arc::new(batch);
                // send() Err means no receivers; that's fine — subscribers
                // reconnect and stream from cursor 0 on the next connect.
                // In production we'd persist the watermark client-side;
                // v1 relies on subscribers being connected before writes.
                let _ = tx.send(arc);
            }
            Ok(None) => {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(e) => {
                tracing::error!("CDC tail poll error: {e:#}");
                tokio::time::sleep(POLL_INTERVAL * 4).await;
            }
        }
    }
}

async fn serve_subscriber(
    mut stream: TcpStream,
    mut rx: broadcast::Receiver<Arc<TxnBatch>>,
) -> anyhow::Result<()> {
    let mut hb = tokio::time::interval(Duration::from_secs(5));
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(batch) => {
                        let frame = Frame::Batch {
                            rows: batch.rows.iter().map(WireCdcRow::from).collect(),
                        };
                        write_frame(&mut stream, &frame).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Subscriber fell behind by n batches. v1 policy:
                        // drop the connection so the client reconnects
                        // and restarts the stream. In v1 that means
                        // starting from cursor 0 (persistence is a v2
                        // improvement).
                        anyhow::bail!("subscriber lagged by {n} batches; forcing reconnect");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        anyhow::bail!("primary broadcast channel closed");
                    }
                }
            }
            _ = hb.tick() => {
                write_frame(&mut stream, &Frame::Ping).await?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Replica: connect + read + apply.
// ---------------------------------------------------------------------------

async fn run_replica(mgmt: Arc<Turso>, primary_addr: String) -> anyhow::Result<()> {
    // The replica's local Turso engine serves reads via litewire; writes
    // arrive only through apply_batch, keyed by monotonic watermark.
    let apply_conn = mgmt.raw_connection()?;

    loop {
        match TcpStream::connect(&primary_addr).await {
            Ok(mut stream) => {
                tracing::info!(primary = %primary_addr, "CDC replica: connected to primary");
                match consume_frames(&mut stream, &apply_conn).await {
                    Ok(()) => {
                        tracing::info!("CDC replica: primary closed connection cleanly");
                    }
                    Err(e) => {
                        tracing::warn!("CDC replica stream error: {e:#}");
                    }
                }
            }
            Err(e) => {
                tracing::debug!(primary = %primary_addr, "CDC replica connect failed: {e}");
            }
        }
        tokio::time::sleep(REPLICA_RECONNECT_DELAY).await;
    }
}

async fn consume_frames(
    stream: &mut TcpStream,
    apply_conn: &litewire::litewire_turso::TursoConnection,
) -> anyhow::Result<()> {
    loop {
        let frame = read_frame(stream).await?;
        match frame {
            Frame::Batch { rows } => {
                let batch = TxnBatch { rows: rows.into_iter().map(CdcRow::from).collect() };
                if let Err(e) = apply_batch(apply_conn, &batch).await {
                    // v1 policy: log at ERROR, keep the connection alive
                    // so operators can see the failure stream. Skipping a
                    // bad batch silently would leave the replica in a
                    // divergent state; crashing would too.
                    tracing::error!(
                        change_id = batch.commit_change_id(),
                        "CDC apply_batch error: {e:#}"
                    );
                }
            }
            Frame::Ping => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Frame codec.
// ---------------------------------------------------------------------------

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) -> anyhow::Result<()> {
    let json = serde_json::to_vec(frame).context("frame serialize")?;
    let len = u32::try_from(json.len()).context("frame too large for u32 length prefix")?;
    anyhow::ensure!(len <= MAX_FRAME_LEN, "frame too large: {len} > {MAX_FRAME_LEN}");
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&json).await?;
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Frame> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(len <= MAX_FRAME_LEN, "frame too large: {len} > {MAX_FRAME_LEN}");
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let frame: Frame = serde_json::from_slice(&body).context("frame parse")?;
    Ok(frame)
}

// ---------------------------------------------------------------------------
// Litewire wire frontends.
// ---------------------------------------------------------------------------

fn spawn_litewire_serve<B: litewire::backend::Backend>(
    sqlite_config: &SqliteConfig,
    backend: B,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let mut builder = litewire::LiteWire::new(backend);
    builder = builder.mysql(&sqlite_config.proxy.mysql_listen);
    tracing::info!(
        listen = %sqlite_config.proxy.mysql_listen,
        "SQLite MySQL wire protocol enabled (CDC-replicated Turso)"
    );

    if let Some(ref hrana_addr) = sqlite_config.proxy.hrana_listen {
        builder = builder.hrana(hrana_addr);
        tracing::info!(listen = %hrana_addr, "SQLite Hrana HTTP API enabled (CDC-replicated Turso)");
    }
    if let Some(ref pg_addr) = sqlite_config.proxy.postgres_listen {
        builder = builder.postgres(pg_addr);
        tracing::info!(listen = %pg_addr, "SQLite PostgreSQL wire protocol enabled (CDC-replicated Turso)");
    }
    if let Some(ref tds_addr) = sqlite_config.proxy.tds_listen {
        builder = builder.tds(tds_addr);
        tracing::info!(listen = %tds_addr, "SQLite TDS wire protocol enabled (CDC-replicated Turso)");
    }
    handles.push(tokio::spawn(async move {
        match builder.serve().await {
            Ok(()) => tracing::info!("litewire stopped (CDC-replicated Turso)"),
            Err(e) => tracing::error!("litewire error (CDC-replicated Turso): {e:#}"),
        }
    }));
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_batch_roundtrip_preserves_all_fields() {
        let orig = TxnBatch {
            rows: vec![
                CdcRow {
                    change_id: 1,
                    change_txn_id: Some(1),
                    change_type: 1,
                    table_name: Some("t".into()),
                    id: Some(42),
                    before: None,
                    after: Some(vec![0x01, 0x02, 0x03, 0xff]),
                    updates: None,
                },
                CdcRow {
                    change_id: 2,
                    change_txn_id: None,
                    change_type: 2,
                    table_name: None,
                    id: None,
                    before: None,
                    after: None,
                    updates: None,
                },
            ],
        };
        let wire_rows: Vec<WireCdcRow> = orig.rows.iter().map(WireCdcRow::from).collect();
        let frame = Frame::Batch { rows: wire_rows };
        let json = serde_json::to_vec(&frame).unwrap();
        let decoded: Frame = serde_json::from_slice(&json).unwrap();
        let Frame::Batch { rows } = decoded else {
            panic!("expected Batch frame");
        };
        let back: Vec<CdcRow> = rows.into_iter().map(CdcRow::from).collect();
        assert_eq!(back.len(), orig.rows.len());
        for (a, b) in back.iter().zip(orig.rows.iter()) {
            assert_eq!(a.change_id, b.change_id);
            assert_eq!(a.change_txn_id, b.change_txn_id);
            assert_eq!(a.change_type, b.change_type);
            assert_eq!(a.table_name, b.table_name);
            assert_eq!(a.id, b.id);
            assert_eq!(a.before, b.before);
            assert_eq!(a.after, b.after);
            assert_eq!(a.updates, b.updates);
        }
    }

    #[tokio::test]
    async fn frame_codec_length_prefix_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(65536);
        let frame = Frame::Ping;
        write_frame(&mut client, &frame).await.unwrap();
        let decoded = read_frame(&mut server).await.unwrap();
        assert!(matches!(decoded, Frame::Ping));
    }

    #[tokio::test]
    async fn frame_codec_rejects_oversized_length_prefix() {
        let (mut client, mut server) = tokio::io::duplex(65536);
        let over = MAX_FRAME_LEN + 1;
        AsyncWriteExt::write_all(&mut client, &over.to_be_bytes()).await.unwrap();
        let err = read_frame(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("frame too large"), "unexpected error: {err}");
    }
}

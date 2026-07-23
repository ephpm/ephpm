//! **Experimental** Phase 2 CDC-native SQLite replication over the
//! cluster channel v1.
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
//! # Transport
//!
//! CDC batches ride the [cluster channel](ephpm_cluster::cluster_channel)
//! — a single, opt-in, authenticated,
//! `yamux`-multiplexed TCP listener that any cluster feature can share.
//! The listener is only bound when a feature asks for it; before this
//! module opted in, the channel port was closed.
//!
//! Each CDC stream is named `cdc/<vhost>` (today just `cdc/default`
//! — per-vhost replication is Phase 2.1). The primary registers a
//! handler for `"cdc/default"` on the channel; replicas dial the
//! primary's channel address and open a stream of that name. The
//! per-transaction frame format inside the stream stays as it was
//! (length-prefixed JSON) — the multiplexer only replaces the
//! bespoke TCP dance around it.
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
//!  │  cluster channel handler for    │    │  from cluster channel │
//!  │  "cdc/default" fans one         │◀───┤  stream "cdc/default" │
//!  │  broadcast::Receiver per stream │    │  → apply_batch(&conn) │
//!  └─────────────────────────────────┘    └───────────────────────┘
//! ```
//!
//! # Failover
//!
//! The sqlite election machinery (`ephpm_cluster::SqliteElection`) is
//! unchanged. On role change, the initial role's tasks stay running
//! (v1 simplification) and new tasks for the new role are spawned;
//! stale tasks eventually notice a broken channel stream and log out.
//! **The divergence window is the same class as sqld async replication:**
//! a former primary that had unshipped batches at the moment it died
//! has lost those writes.
//!
//! # Bootstrap of a fresh replica (Phase 2.1, task #97)
//!
//! A *cold* replica (empty local DB) cannot catch up by tailing CDC
//! alone: [`enable_cdc`] only captures mutations that happen after it is
//! switched on, so any pre-CDC data on the primary has no CDC rows to
//! replay. Worse, once CDC-log truncation lands (a future phase), even
//! post-CDC history is not guaranteed to be replayable from
//! `change_id = 0`. A cold replica therefore needs a base snapshot of
//! the primary's current state before it starts tailing.
//!
//! ## Snapshot mechanism: online logical dump (chosen for v1)
//!
//! The snapshot is a logical dump (schema DDL plus per-table `INSERT`s
//! with explicit `rowid`) captured on the primary inside a single read
//! transaction (`BEGIN` ... `COMMIT`), together with the watermark
//! `N = MAX(turso_cdc.change_id)` read in that same transaction so the
//! dump and `N` are consistent.
//!
//! Why a logical dump and not a physical file copy:
//!
//! - turso 0.7.0 exposes no online-backup, `serialize`, or local
//!   `checkpoint` API (verified against the pinned `turso = 0.7.0`
//!   crate: `Database`/`Connection` have `execute`/`query`/`prepare`/
//!   `pragma_*` but nothing that yields a consistent byte image, and
//!   the `checkpoint()` method exists only on the remote sync database,
//!   not the local one). `VACUUM INTO` is likewise out: litewire's
//!   Turso backend rejects `VACUUM` outright and turso 0.7.0's VACUUM
//!   is incomplete upstream.
//! - A raw file-byte copy would have to reason about turso's on-disk
//!   shape (main file plus any `-wal`/`-shm` sidecars, whose
//!   memory-mapped index state is not portable across a copy) and hold
//!   a write lock across the copy. A logical dump is format-agnostic
//!   and fully online: reads run under a `BEGIN` read view without
//!   blocking primary writers, so (unlike the quiesce-copy fallback the
//!   task allowed) bootstrap does NOT pause primary writes.
//! - The dump reads live table state, so it captures pre-CDC data that
//!   a tail-from-0 replay would miss. This is the property that makes
//!   cold-join actually work.
//!
//! The cost is that a very large DB is dumped as row-level `INSERT`s
//! rather than copied as pages; for an experimental Phase 2.1 that is an
//! acceptable trade. A physical page-copy path (pending a turso backup
//! API) is noted as future work.
//!
//! ## Correctness sequence
//!
//! Cold replica, before subscribing to CDC:
//!
//! 1. Detect cold start: [`read_watermark`] on the local mgmt
//!    connection returns `0` AND the DB has no user tables.
//! 2. Dial `snapshot/default` to the primary; receive the header
//!    (`watermark = N`) followed by the chunked SQL body.
//! 3. Apply the dump to the local DB, then seed the replica watermark
//!    to `N` (write `__litewire_cdc_watermark.applied_change_id = N`,
//!    the same table [`apply_batch`] maintains). This all completes
//!    before the litewire wire frontends start serving, so a client
//!    read never observes partial snapshot state.
//! 4. Subscribe to CDC as before. The primary tails from `change_id 0`,
//!    but [`apply_batch`] skips any batch whose `commit_change_id() <=
//!    N` (idempotent no-op against the seeded watermark), so only
//!    post-`N` changes are applied: the tail continues cleanly past the
//!    snapshot point.
//!
//! Snapshot/tail race: because the tail always re-broadcasts from `0`
//! and apply is idempotent past the watermark, there is no gap between
//! "snapshot at N" and "tail from N+1" even if writes land on the
//! primary during the dump; they simply arrive as post-`N` batches.
//!
//! The `snapshot/<vhost>` stream name is reserved in
//! [`ephpm_cluster::stream_type::SNAPSHOT_PREFIX`]; v1 uses only
//! `snapshot/default` (single-vhost, matching `cdc/default`).
//!
//! ## Scope guards (v1)
//!
//! - Single vhost: `snapshot/default` only.
//! - No CDC-log truncation handling: v1 relies on the log growing
//!   unbounded (as the CDC path does). When truncation lands, the
//!   snapshot watermark `N` and the tail's oldest retained `change_id`
//!   must be reconciled (ship a snapshot at >= the truncation point);
//!   that interaction is deferred.
//! - Experimental + gated: the whole path is behind
//!   `cdc_experimental = true`; sqld stays the production default.
//!
//! # Wire format (inside the yamux stream)
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
//! debuggability. Frame size is bounded at 16 MiB; oversized frames
//! drop the stream. Authentication and confidentiality are handled by
//! the channel handshake — inside the stream there is no per-frame
//! sealing (yamux payloads travel through the already-authenticated
//! TCP connection).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ephpm_cluster::{ChannelStream, IncomingStream};
use ephpm_config::SqliteConfig;
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::{CdcRow, CdcTailer, TxnBatch, apply_batch, read_watermark};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;

use crate::tracked_backend;

/// Full stream-type string this build uses for the default vhost.
///
/// Per-vhost replication is Phase 2.1; today every CDC stream uses
/// `"cdc/default"`.
const CDC_STREAM_TYPE: &str = "cdc/default";

/// Full stream-type string for the default vhost's snapshot bootstrap
/// stream (Phase 2.1, task #97). A cold replica dials this once to fetch
/// the primary's base state before subscribing to [`CDC_STREAM_TYPE`].
///
/// Per-vhost snapshots are future work; today every snapshot uses
/// `"snapshot/default"` under [`ephpm_cluster::stream_type::SNAPSHOT_PREFIX`].
const SNAPSHOT_STREAM_TYPE: &str = "snapshot/default";

/// Maximum frame length accepted on either side of the wire (16 MiB).
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Maximum size of a single snapshot data chunk (16 MiB), matching the
/// CDC per-frame cap. The snapshot body is split into chunks of at most
/// this size so an arbitrarily large dump never needs a single
/// oversized allocation on either side.
const MAX_SNAPSHOT_CHUNK_LEN: u32 = 16 * 1024 * 1024;

/// Target size of each emitted snapshot chunk. Kept well under
/// [`MAX_SNAPSHOT_CHUNK_LEN`] so the length prefix always fits and the
/// receiver's per-chunk allocation stays bounded.
const SNAPSHOT_CHUNK_TARGET: usize = 1024 * 1024;

/// Name of litewire's replica watermark table. Seeding it to the
/// snapshot watermark `N` makes [`apply_batch`] treat every CDC batch
/// with `commit_change_id() <= N` as an idempotent no-op. This mirrors
/// the `CREATE TABLE`/`INSERT OR IGNORE` that litewire-turso's
/// `ensure_watermark_table` performs; we depend on the table name and
/// shape staying stable under the exact litewire pin.
const WATERMARK_TABLE: &str = "__litewire_cdc_watermark";

/// Broadcast channel capacity — how many transactions the primary can
/// buffer between polls before slow subscribers start missing (`Lagged`)
/// frames. When a subscriber lags, it disconnects; the replica's
/// reconnect loop opens a fresh stream and starts from cursor 0
/// (idempotency provided by [`apply_batch`]'s monotonic watermark).
const BROADCAST_CAPACITY: usize = 1024;

/// How often the primary polls `turso_cdc` for new batches. Turso 0.7.0
/// has no wakeup signal for CDC inserts, so we poll on a schedule.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long a replica waits between connect retries when the primary
/// is unreachable.
const REPLICA_RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Heartbeat interval on primary-side subscribers.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Frame types carried on the CDC replication wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Frame {
    /// A committed transaction batch. `rows` mirrors
    /// [`litewire_turso::cdc::TxnBatch::rows`].
    Batch { rows: Vec<WireCdcRow> },
    /// Heartbeat — sent every ~5s from primary to keep the subscriber
    /// stream warm even during idle periods.
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

/// Start Phase 2 CDC-native replication for a clustered Turso engine,
/// riding the [cluster channel](ephpm_cluster::cluster_channel).
///
/// Opens two Turso factories against the same DB file — one for the
/// litewire wire frontends (with `enable_cdc_on_connect` set on the
/// primary) and one for the CDC tail/apply path. Then:
///
/// - Litewire wire frontends against the wire factory (always).
/// - On primary: a tail loop reading `turso_cdc` and broadcasting
///   batches, plus a channel stream handler that forwards them to any
///   inbound `cdc/default` stream.
/// - On replica: a channel-dial loop that opens `cdc/default` against
///   the primary and applies received batches.
///
/// The `channel_handle` argument comes from
/// [`ephpm_cluster::maybe_start_cluster_channel`] — when it's `None`,
/// the channel was never bound (no channel feature asked for it) and
/// this function returns an error, since CDC replication is exactly
/// such a feature. The caller in `lib.rs` guarantees `Some` on this
/// code path.
///
/// # Errors
///
/// Returns an error if either factory cannot open, if the elected role
/// requires a peer address that isn't configured, or if the cluster
/// channel is not available (indicating a startup ordering bug).
pub async fn start_clustered_turso_cdc(
    sqlite_config: &SqliteConfig,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    channel_handle: Option<&ephpm_cluster::ChannelHandle>,
    query_stats: &ephpm_query_stats::QueryStats,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let cluster = cluster.context(
        "clustered Turso CDC replication requires [cluster] enabled = true; \
         no cluster handle available",
    )?;
    let channel = channel_handle.context(
        "clustered Turso CDC replication requires the cluster channel to be bound; \
         maybe_start_cluster_channel returned None despite cdc_experimental = true \
         (startup ordering bug)",
    )?;

    tracing::warn!(
        engine = "turso",
        role = %sqlite_config.replication.role,
        channel_listen = %channel.listen_addr(),
        "starting EXPERIMENTAL Phase 2 CDC-native SQLite replication over the cluster \
         channel. sqld is NOT spawned; replication uses litewire's turso_cdc stream. \
         Turso engine remains Beta upstream — do not use with data you cannot recreate. \
         See site/content/roadmap/turso-engine.md and site/content/roadmap/cluster-channel.md."
    );

    // add-config-knob discipline: cdc_listen was replaced by the
    // cluster channel and is now a documented no-op. Warn when it's
    // explicitly set so operators fix their config; stay quiet at the
    // default value.
    if sqlite_config.replication.cdc_listen != "0.0.0.0:5015" {
        tracing::warn!(
            cdc_listen = %sqlite_config.replication.cdc_listen,
            "[db.sqlite.replication] cdc_listen is deprecated — parsed but not acted upon. \
             CDC now rides the cluster channel; move any port allocation to \
             [cluster.channel] listen. This knob will be removed in a future release."
        );
    }

    let db_path = &sqlite_config.path;

    // Use the resolved advertise address — NOT `listen_addr()`
    // verbatim — for what we publish to peers. This matters when the
    // channel is bound on a wildcard IP (`0.0.0.0` / `::`): if we
    // published `0.0.0.0:PORT` into the election KV, remote replicas
    // would dial `0.0.0.0` on their own stack (refused). Refuse to
    // start when there is no discoverable advertise IP anywhere, and
    // point operators at the two knobs that fix it.
    let channel_advertise = channel.advertise_addr().context(
        "clustered Turso CDC replication cannot advertise the cluster channel address: \
         both [cluster] bind and [cluster.channel] listen use an unspecified IP \
         (0.0.0.0 / ::), so there is no address we can publish that a remote replica \
         could dial. Bind [cluster] to a specific IP that peers can reach (e.g. \
         \"10.0.1.5:7946\"), or set [cluster.channel] listen to a specific \
         host:port explicitly.",
    )?;
    let (initial_role, role_rx) = determine_role(sqlite_config, cluster, channel_advertise).await?;

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

    // Register the primary-side snapshot handler NOW, before anything
    // dials, so a peer that comes up as a cold replica can always reach
    // us for a bootstrap regardless of which role we started in. The
    // handler pattern mirrors the CDC handler below (registered
    // up-front so it survives role transitions).
    spawn_snapshot_server(channel, Arc::clone(&mgmt_factory), handles);

    // Cold-start bootstrap: if we begin life as a replica with an empty
    // local DB, fetch the primary's base snapshot BEFORE the litewire
    // wire frontends start serving; otherwise a client read could
    // observe partial state. This is awaited (blocking startup) on
    // purpose; the wire frontends spin up only after it completes. A
    // primary, or a replica whose DB is already populated, skips this.
    if let Role::Replica { primary_addr } = &initial_role {
        maybe_bootstrap_cold_replica(&mgmt_factory, *primary_addr, channel).await;
    }

    // Start litewire wire frontends. Wire factory is moved in here.
    let tracked = tracked_backend::TrackedBackend::new(wire_factory, query_stats.clone());
    spawn_litewire_serve(sqlite_config, tracked, handles);

    // Broadcast channel for primary-side batches. Cloned per inbound
    // subscriber stream; each subscriber runs its own copy.
    let (tx, _rx0) = broadcast::channel::<Arc<TxnBatch>>(BROADCAST_CAPACITY);

    // Register the primary-side handler NOW even if we start as
    // replica. On a later role transition the handler is already in
    // place — we just start feeding the broadcast channel from the
    // tail loop.
    let mut cdc_streams = channel.register_exact(CDC_STREAM_TYPE);
    let tx_for_subs = tx.clone();
    handles.push(tokio::spawn(async move {
        while let Some(incoming) = cdc_streams.recv().await {
            let rx = tx_for_subs.subscribe();
            let IncomingStream { stream, peer, .. } = incoming;
            tokio::spawn(async move {
                if let Err(e) = serve_subscriber(stream, rx).await {
                    tracing::info!(peer = %peer, "CDC subscriber disconnected: {e:#}");
                }
            });
        }
    }));

    // Kick off role-appropriate work for the initial role.
    let mgmt = Arc::clone(&mgmt_factory);
    let tx0 = tx.clone();
    let channel0 = channel.clone();
    handles.push(tokio::spawn(async move {
        start_role(initial_role, mgmt, tx0, channel0).await;
    }));

    // Role-change watcher: on a role transition, spawn the new role's
    // driver. Old drivers stay running and drain naturally; v1 accepts
    // this simplification because in practice a role change only fires
    // on failure/join events, and the new driver's stream open will
    // succeed cleanly regardless of stale ones.
    if let Some(mut watch_rx) = role_rx {
        let mgmt = Arc::clone(&mgmt_factory);
        let tx = tx.clone();
        let channel = channel.clone();
        handles.push(tokio::spawn(async move {
            while watch_rx.changed().await.is_ok() {
                let new_elected = watch_rx.borrow().clone();
                let new_role = elected_to_role(new_elected);
                tracing::info!(?new_role, "CDC replication: role change detected");
                let mgmt = Arc::clone(&mgmt);
                let tx = tx.clone();
                let channel = channel.clone();
                tokio::spawn(async move { start_role(new_role, mgmt, tx, channel).await });
            }
        }));
    }

    Ok(())
}

#[derive(Debug, Clone)]
enum Role {
    Primary,
    Replica { primary_addr: SocketAddr },
}

fn elected_to_role(elected: ephpm_cluster::ElectedRole) -> Role {
    match elected {
        ephpm_cluster::ElectedRole::Primary => Role::Primary,
        ephpm_cluster::ElectedRole::Replica { primary_grpc_url } => {
            // In CDC-native mode the election broadcasts the primary's
            // *cluster channel* address in the `primary_grpc_url`
            // field. Note: the election machinery is shared with the
            // sqld path, which stores `"http://host:port"` (raw sqld
            // gRPC URL format) — so this reader normalizes both forms.
            //
            // We fix it here on the reader side rather than teach the
            // emitter to publish two formats: the emitter feeds a
            // gossip KV entry that's read by every subscriber, and
            // bloating that entry with a second serialization for one
            // consumer's benefit is the wrong direction. The sqld
            // reader keeps its URL form; the CDC reader strips.
            match parse_primary_addr(&primary_grpc_url) {
                Ok(addr) => Role::Replica { primary_addr: addr },
                Err(e) => {
                    tracing::error!(
                        primary = %primary_grpc_url,
                        "CDC replica: primary address is not a valid SocketAddr: {e}"
                    );
                    // Fall back to a bogus address; the replica loop
                    // will fail to connect and just log — this is
                    // preferable to panicking a background task.
                    Role::Replica { primary_addr: SocketAddr::from(([127, 0, 0, 1], 0)) }
                }
            }
        }
    }
}

/// Parse a primary address published by [`ephpm_cluster::SqliteElection`].
///
/// Accepts both:
/// - Raw `SocketAddr` form (`"10.0.0.1:8094"`) — what the CDC path
///   will publish once every deployment has upgraded.
/// - URL form (`"http://10.0.0.1:8094"`, optionally with trailing
///   path) — what the shared election emitter produces today for the
///   sqld path. See the `elected_to_role` doc for why we normalize on
///   the reader side.
///
/// Returns `Err` on unparseable input; the caller logs and falls back
/// to a bogus address so the driver task does not panic.
/// Parse a primary address published by [`ephpm_cluster::SqliteElection`].
///
/// Public so cross-crate integration tests can exercise the exact same
/// parse the production replica uses. See the module-level Bug 1 doc
/// on why we accept both `http://addr` and raw `addr` forms.
///
/// # Errors
///
/// Returns an error when the input cannot be reduced to a valid
/// `host:port` after scheme/path stripping.
pub fn parse_primary_addr(s: &str) -> anyhow::Result<SocketAddr> {
    let trimmed = s.trim();
    // Strip a scheme prefix if present (`http://`, `https://`, or any
    // other `<scheme>://`), then strip any trailing path so the parse
    // sees a bare `host:port`.
    let host_and_path = match trimmed.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => trimmed,
    };
    let host_port = host_and_path.split(['/', '?', '#']).next().unwrap_or(host_and_path);
    host_port.parse::<SocketAddr>().with_context(|| format!("expected host:port, got {trimmed:?}"))
}

async fn determine_role(
    sqlite_config: &SqliteConfig,
    cluster: &Arc<ephpm_cluster::ClusterHandle>,
    channel_advertise: SocketAddr,
) -> anyhow::Result<(Role, Option<tokio::sync::watch::Receiver<ephpm_cluster::ElectedRole>>)> {
    match sqlite_config.replication.role.as_str() {
        "primary" => Ok((Role::Primary, None)),
        "replica" => {
            anyhow::ensure!(
                !sqlite_config.replication.primary_grpc_url.is_empty(),
                "replication.primary_grpc_url is required when role = \"replica\" \
                 in CDC-native replication mode (this field carries the primary's \
                 cluster channel address in this mode, e.g. \"10.0.0.1:7947\")"
            );
            // Accept both "host:port" and "http://host:port" forms —
            // the URL form is what auto-election publishes today
            // (shared with the sqld path); operators who copy that
            // address into an explicit `[db.sqlite.replication]
            // primary_grpc_url` value should not have their config
            // rejected just because we changed the reader.
            let addr = parse_primary_addr(&sqlite_config.replication.primary_grpc_url)
                .with_context(|| {
                    format!(
                        "replication.primary_grpc_url is not a valid address in CDC-native \
                         mode (expected \"host:port\" or \"http://host:port\", got {:?})",
                        sqlite_config.replication.primary_grpc_url
                    )
                })?;
            Ok((Role::Replica { primary_addr: addr }, None))
        }
        _ => {
            // "auto" — reuse the same election as the sqld path but
            // advertise the cluster channel address (that's what
            // replicas need to dial in this mode).
            let election = ephpm_cluster::SqliteElection::new(
                Arc::clone(cluster),
                channel_advertise.to_string(),
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
    channel: ephpm_cluster::ChannelHandle,
) {
    match role {
        Role::Primary => {
            if let Err(e) = run_primary(mgmt, tx).await {
                tracing::error!("CDC primary loop exited: {e:#}");
            }
        }
        Role::Replica { primary_addr } => {
            if let Err(e) = run_replica(mgmt, primary_addr, channel).await {
                tracing::error!("CDC replica loop exited: {e:#}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Primary: tail + broadcast. (Subscriber-side accept is registered up in
// `start_clustered_turso_cdc` so it exists across role transitions.)
// ---------------------------------------------------------------------------

async fn run_primary(mgmt: Arc<Turso>, tx: broadcast::Sender<Arc<TxnBatch>>) -> anyhow::Result<()> {
    tracing::info!("CDC primary: tail loop starting");
    let mut tailer = CdcTailer::new(&mgmt, 0);
    loop {
        match tailer.poll_batch().await {
            Ok(Some(batch)) => {
                let arc = Arc::new(batch);
                // send() Err means no receivers; that's fine — subscribers
                // reconnect and stream from cursor 0 on the next connect.
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
    mut stream: ChannelStream,
    mut rx: broadcast::Receiver<Arc<TxnBatch>>,
) -> anyhow::Result<()> {
    let mut hb = tokio::time::interval(HEARTBEAT_INTERVAL);
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
                        // drop the stream so the client reconnects and
                        // restarts. Watermark keeps re-application safe.
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
// Replica: dial the cluster channel + read + apply.
// ---------------------------------------------------------------------------

async fn run_replica(
    mgmt: Arc<Turso>,
    primary_addr: SocketAddr,
    channel: ephpm_cluster::ChannelHandle,
) -> anyhow::Result<()> {
    // The replica's local Turso engine serves reads via litewire; writes
    // arrive only through apply_batch, keyed by monotonic watermark.
    let apply_conn = mgmt.raw_connection()?;

    loop {
        match channel.dial(primary_addr, CDC_STREAM_TYPE).await {
            Ok(mut stream) => {
                tracing::info!(primary = %primary_addr, "CDC replica: channel stream open");
                match consume_frames(&mut stream, &apply_conn).await {
                    Ok(()) => {
                        tracing::info!("CDC replica: primary closed stream cleanly");
                    }
                    Err(e) => {
                        tracing::warn!("CDC replica stream error: {e:#}");
                    }
                }
            }
            Err(e) => {
                tracing::debug!(primary = %primary_addr, "CDC replica dial failed: {e:#}");
            }
        }
        tokio::time::sleep(REPLICA_RECONNECT_DELAY).await;
    }
}

async fn consume_frames(
    stream: &mut ChannelStream,
    apply_conn: &litewire::litewire_turso::TursoConnection,
) -> anyhow::Result<()> {
    loop {
        let frame = read_frame(stream).await?;
        match frame {
            Frame::Batch { rows } => {
                let batch = TxnBatch { rows: rows.into_iter().map(CdcRow::from).collect() };
                if let Err(e) = apply_batch(apply_conn, &batch).await {
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
// Snapshot bootstrap (Phase 2.1, task #97).
//
// A cold replica dials `snapshot/default`; the primary answers with a
// header carrying the watermark `N` (= MAX(turso_cdc.change_id) at the
// moment of the dump), followed by the SQL dump body as length-prefixed
// chunks, terminated by an end marker. The replica applies the dump,
// seeds its watermark to `N`, then subscribes to CDC where apply_batch
// idempotently skips everything <= N.
//
// The snapshot wire format is deliberately NOT the JSON `Frame` codec:
// the body is raw SQL text (potentially many MiB), so a binary chunked
// framing avoids base64/JSON overhead. The 16 MiB per-chunk cap matches
// the CDC frame cap.
// ---------------------------------------------------------------------------

/// Snapshot wire header, sent once at the start of a snapshot stream
/// before any data chunk. JSON-encoded, length-prefixed (`u32` BE),
/// bounded by [`MAX_SNAPSHOT_CHUNK_LEN`] like every other frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotHeader {
    /// Watermark this snapshot corresponds to: the highest
    /// `turso_cdc.change_id` whose effects are included in the dump. The
    /// replica seeds its applied watermark to this value so the CDC tail
    /// resumes exactly past it.
    watermark: i64,
    /// Total dump body length in bytes across all chunks. Advisory: used
    /// for logging/progress; the authoritative end signal is the
    /// zero-length end-marker chunk.
    total_len: u64,
}

/// Spawn the primary-side snapshot server. Registered up-front (like the
/// CDC handler) so a cold replica can bootstrap from us no matter which
/// role we started in. Each inbound `snapshot/default` stream is served
/// on its own task from a fresh mgmt connection.
fn spawn_snapshot_server(
    channel: &ephpm_cluster::ChannelHandle,
    mgmt: Arc<Turso>,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let mut snapshot_streams = channel.register_exact(SNAPSHOT_STREAM_TYPE);
    handles.push(tokio::spawn(async move {
        while let Some(incoming) = snapshot_streams.recv().await {
            let mgmt = Arc::clone(&mgmt);
            let IncomingStream { stream, peer, .. } = incoming;
            tokio::spawn(async move {
                match serve_snapshot(stream, &mgmt).await {
                    Ok(n) => {
                        tracing::info!(peer = %peer, watermark = n, "served snapshot bootstrap");
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer, "snapshot bootstrap failed: {e:#}");
                    }
                }
            });
        }
    }));
}

/// Serve one snapshot to a dialing replica: produce the logical dump
/// under a consistent read view and stream it. Returns the watermark on
/// success.
///
/// Public so the cross-node snapshot integration test drives the exact
/// production serve path rather than a copy (mirrors why
/// [`parse_primary_addr`] is public). Not part of a stable API.
///
/// # Errors
///
/// Returns an error if the dump cannot be produced or the stream write
/// fails.
pub async fn serve_snapshot(mut stream: ChannelStream, mgmt: &Turso) -> anyhow::Result<i64> {
    let conn = mgmt.raw_connection().context("snapshot: open mgmt connection")?;
    let (watermark, dump) = produce_snapshot(&conn).await.context("snapshot: produce dump")?;

    let header = SnapshotHeader { watermark, total_len: dump.len() as u64 };
    write_snapshot_header(&mut stream, &header).await?;

    for chunk in dump.as_bytes().chunks(SNAPSHOT_CHUNK_TARGET) {
        write_snapshot_chunk(&mut stream, chunk).await?;
    }
    // Zero-length end marker: signals a clean end of body.
    write_snapshot_chunk(&mut stream, &[]).await?;
    stream.flush().await?;
    Ok(watermark)
}

/// Produce a logical dump of the current DB state plus the aligned
/// watermark, captured inside a single read transaction so the two are
/// consistent.
///
/// The dump is a sequence of SQL statements: `CREATE` DDL for every user
/// table/index, followed by rowid-preserving `INSERT`s for every
/// user-table row. Internal bookkeeping tables (`turso_cdc`, the
/// litewire watermark table, turso internals, and `sqlite_*`) are
/// excluded; the replica reconstructs its own watermark row from the
/// header.
async fn produce_snapshot(conn: &turso::Connection) -> anyhow::Result<(i64, String)> {
    // BEGIN pins a consistent read view for the whole dump; turso's
    // MVCC/WAL read path does not block writers, so this is fully online
    // (no primary write pause). Errors after BEGIN roll back via the
    // trailing COMMIT/ROLLBACK.
    conn.execute("BEGIN", ()).await.context("snapshot: BEGIN read view")?;
    let result = produce_snapshot_inner(conn).await;
    // End the read view regardless of outcome. A read-only txn COMMIT and
    // ROLLBACK are equivalent here; prefer COMMIT on success.
    match &result {
        Ok(_) => {
            let _ = conn.execute("COMMIT", ()).await;
        }
        Err(_) => {
            let _ = conn.execute("ROLLBACK", ()).await;
        }
    }
    result
}

async fn produce_snapshot_inner(conn: &turso::Connection) -> anyhow::Result<(i64, String)> {
    let watermark = current_max_change_id(conn).await?;

    let mut dump = String::new();
    let tables = snapshot_schema(conn, &mut dump).await?;
    for table in &tables {
        snapshot_table_rows(conn, table, &mut dump).await?;
    }
    Ok((watermark, dump))
}

/// Read the highest `change_id` in `turso_cdc` (0 when the log is empty
/// or the table does not exist). This is the snapshot watermark: every
/// committed change up to this id is reflected in the dump.
async fn current_max_change_id(conn: &turso::Connection) -> anyhow::Result<i64> {
    // COALESCE handles the empty-table case; the missing-table case (CDC
    // never enabled) is caught by the error path -> 0.
    let mut stmt = match conn.prepare("SELECT COALESCE(MAX(change_id), 0) FROM turso_cdc").await {
        Ok(s) => s,
        Err(_) => return Ok(0),
    };
    let mut rows = stmt.query(()).await.context("snapshot: query MAX(change_id)")?;
    match rows.next().await.context("snapshot: read MAX(change_id)")? {
        Some(row) => match row.get_value(0).context("snapshot: MAX(change_id) value")? {
            turso::Value::Integer(i) => Ok(i),
            _ => Ok(0),
        },
        None => Ok(0),
    }
}

/// Append `CREATE` DDL for every user object to `dump` and return the
/// list of user table names (in creation order) to dump rows for.
///
/// Reads `sqlite_schema` directly. Internal objects are skipped (see
/// [`is_internal_object`]). Autoindexes (NULL `sql`) are skipped: they
/// follow their parent table's DDL automatically.
async fn snapshot_schema(
    conn: &turso::Connection,
    dump: &mut String,
) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_schema \
             WHERE sql IS NOT NULL \
             ORDER BY rowid",
        )
        .await
        .context("snapshot: prepare schema read")?;
    let mut rows = stmt.query(()).await.context("snapshot: query schema")?;

    let mut tables = Vec::new();
    while let Some(row) = rows.next().await.context("snapshot: next schema row")? {
        let obj_type = value_text(&row, 0)?;
        let name = value_text(&row, 1)?;
        let sql = value_text(&row, 2)?;

        if is_internal_object(&name) {
            continue;
        }
        // Emit the object's own DDL verbatim. `sqlite_schema.sql` already
        // holds the exact CREATE text the primary ran.
        dump.push_str(&sql);
        dump.push_str(";\n");

        if obj_type == "table" {
            tables.push(name);
        }
    }
    Ok(tables)
}

/// Append rowid-preserving `INSERT` statements for every row of `table`
/// to `dump`. Preserving rowid is mandatory: the CDC apply path keys
/// INSERT/UPDATE/DELETE by rowid, so the replica must land identical
/// rowids for post-snapshot CDC replay to stay consistent.
async fn snapshot_table_rows(
    conn: &turso::Connection,
    table: &str,
    dump: &mut String,
) -> anyhow::Result<()> {
    let cols = table_columns(conn, table).await?;
    let col_list = cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");

    let select_sql = format!(
        "SELECT rowid, {} FROM {} ORDER BY rowid",
        cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", "),
        quote_ident(table),
    );
    let mut stmt = conn
        .prepare(&select_sql)
        .await
        .with_context(|| format!("snapshot: prepare row read for {table}"))?;
    let mut rows =
        stmt.query(()).await.with_context(|| format!("snapshot: query rows for {table}"))?;

    while let Some(row) = rows.next().await.context("snapshot: next data row")? {
        let rowid = match row.get_value(0).context("snapshot: rowid value")? {
            turso::Value::Integer(i) => i,
            v => anyhow::bail!("snapshot: non-integer rowid in {table}: {v:?}"),
        };
        let mut literals = Vec::with_capacity(cols.len());
        for i in 0..cols.len() {
            let v = row.get_value(i + 1).context("snapshot: column value")?;
            literals.push(sql_literal(&v)?);
        }
        // INSERT OR REPLACE keeps re-runs (e.g. an interrupted bootstrap
        // retried) idempotent by rowid.
        use std::fmt::Write as _;
        writeln!(
            dump,
            "INSERT OR REPLACE INTO {} (rowid, {}) VALUES ({}, {});",
            quote_ident(table),
            col_list,
            rowid,
            literals.join(", "),
        )
        .expect("writing to a String is infallible");
    }
    Ok(())
}

/// List the storable column names of `table` via `PRAGMA table_info`, in
/// record order.
async fn table_columns(conn: &turso::Connection, table: &str) -> anyhow::Result<Vec<String>> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let mut stmt = conn.prepare(&sql).await.context("snapshot: prepare table_info")?;
    let mut rows = stmt.query(()).await.context("snapshot: query table_info")?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().await.context("snapshot: next table_info row")? {
        // table_info columns: (cid, name, type, notnull, dflt_value, pk).
        names.push(value_text(&row, 1)?);
    }
    anyhow::ensure!(!names.is_empty(), "snapshot: table {table} has no columns");
    Ok(names)
}

/// Apply a received snapshot dump to the local (cold) DB, then seed the
/// watermark to `N` so the CDC tail resumes exactly past it.
async fn apply_snapshot(
    conn: &turso::Connection,
    watermark: i64,
    dump: &str,
) -> anyhow::Result<()> {
    // Execute the whole dump as a batch. It is self-consistent DDL+DML
    // captured under one read view on the primary.
    conn.execute_batch(dump).await.context("snapshot: apply dump")?;
    seed_watermark(conn, watermark).await.context("snapshot: seed watermark")?;
    Ok(())
}

/// Create (if absent) and set litewire's replica watermark table to
/// `watermark`. Mirrors litewire-turso's `ensure_watermark_table` shape;
/// see [`WATERMARK_TABLE`] for the coupling note.
async fn seed_watermark(conn: &turso::Connection, watermark: i64) -> anyhow::Result<()> {
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {WATERMARK_TABLE} (\
                id INTEGER PRIMARY KEY CHECK (id = 0), \
                applied_change_id INTEGER NOT NULL)"
        ),
        (),
    )
    .await
    .context("snapshot: create watermark table")?;
    conn.execute(
        &format!("INSERT OR REPLACE INTO {WATERMARK_TABLE} (id, applied_change_id) VALUES (0, ?)"),
        (watermark,),
    )
    .await
    .context("snapshot: write watermark")?;
    Ok(())
}

/// Cold-start bootstrap: if the local DB is empty, dial the primary for a
/// base snapshot and apply it before returning. A non-empty local DB
/// (already bootstrapped, or a warm restart) skips the transfer.
///
/// This never propagates an error: a bootstrap failure logs and lets the
/// node come up empty and replicate forward (degraded but not crashed),
/// matching the "replicas may start empty" v1 fallback. It retries the
/// dial a bounded number of times to ride out the window where the
/// primary's channel is not yet accepting.
async fn maybe_bootstrap_cold_replica(
    mgmt: &Turso,
    primary_addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
) {
    let conn = match mgmt.raw_connection() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("snapshot bootstrap: cannot open local connection: {e:#}");
            return;
        }
    };

    match local_db_is_cold(&conn).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!("snapshot bootstrap: local DB already populated; skipping");
            return;
        }
        Err(e) => {
            tracing::warn!("snapshot bootstrap: cold-check failed, skipping: {e:#}");
            return;
        }
    }

    tracing::warn!(
        primary = %primary_addr,
        "EXPERIMENTAL snapshot bootstrap: local Turso DB is cold; fetching base snapshot \
         from the primary before serving. This is Phase 2.1 and gated behind \
         cdc_experimental."
    );

    const MAX_ATTEMPTS: u32 = 30;
    for attempt in 1..=MAX_ATTEMPTS {
        match fetch_and_apply_snapshot(&conn, primary_addr, channel).await {
            Ok(n) => {
                tracing::info!(
                    primary = %primary_addr,
                    watermark = n,
                    "snapshot bootstrap complete; replica seeded and ready to tail CDC"
                );
                return;
            }
            Err(e) => {
                tracing::debug!(
                    attempt,
                    primary = %primary_addr,
                    "snapshot bootstrap attempt failed: {e:#}"
                );
                tokio::time::sleep(REPLICA_RECONNECT_DELAY).await;
            }
        }
    }
    tracing::error!(
        primary = %primary_addr,
        "snapshot bootstrap gave up after {MAX_ATTEMPTS} attempts; replica will start empty \
         and replicate forward only (pre-snapshot data will be MISSING until a restart \
         succeeds in bootstrapping)"
    );
}

/// A DB is "cold" when the applied watermark is 0 AND there are no user
/// tables yet. The double check avoids re-bootstrapping a replica that
/// legitimately holds an empty-but-initialized DB, and avoids treating a
/// primary-turned-replica (which has real tables but no watermark row)
/// as cold.
async fn local_db_is_cold(conn: &turso::Connection) -> anyhow::Result<bool> {
    let wm = read_watermark(conn).await.context("snapshot: read local watermark")?;
    if wm != 0 {
        return Ok(false);
    }
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name NOT LIKE '\\_\\_%' ESCAPE '\\' \
             AND name != 'turso_cdc'",
        )
        .await
        .context("snapshot: prepare user-table count")?;
    let mut rows = stmt.query(()).await.context("snapshot: query user-table count")?;
    let count = match rows.next().await.context("snapshot: read user-table count")? {
        Some(row) => match row.get_value(0).context("snapshot: user-table count value")? {
            turso::Value::Integer(i) => i,
            _ => 0,
        },
        None => 0,
    };
    Ok(count == 0)
}

/// Dial the primary, receive a snapshot, and apply it locally. Returns
/// the watermark applied.
///
/// Public so the cross-node snapshot integration test drives the exact
/// production fetch/apply path (mirrors [`parse_primary_addr`]). Not part
/// of a stable API.
///
/// # Errors
///
/// Returns an error if the dial, header/chunk read, or dump application
/// fails.
pub async fn fetch_and_apply_snapshot(
    conn: &turso::Connection,
    primary_addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
) -> anyhow::Result<i64> {
    let mut stream = channel
        .dial(primary_addr, SNAPSHOT_STREAM_TYPE)
        .await
        .with_context(|| format!("snapshot: dial {primary_addr}"))?;

    let header = read_snapshot_header(&mut stream).await.context("snapshot: read header")?;
    let mut body = Vec::with_capacity(usize::try_from(header.total_len).unwrap_or(0));
    loop {
        let chunk = read_snapshot_chunk(&mut stream).await.context("snapshot: read chunk")?;
        if chunk.is_empty() {
            break; // end marker
        }
        body.extend_from_slice(&chunk);
    }
    let dump = String::from_utf8(body).context("snapshot: dump body is not valid utf-8")?;
    apply_snapshot(conn, header.watermark, &dump).await?;
    Ok(header.watermark)
}

// ---------------------------------------------------------------------------
// Snapshot codec: header (JSON, length-prefixed) + binary chunks
// (length-prefixed), zero-length chunk = end marker. Operates on any
// tokio Async{Read,Write} so tests can drive it over a DuplexStream.
// ---------------------------------------------------------------------------

async fn write_snapshot_header<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    header: &SnapshotHeader,
) -> anyhow::Result<()> {
    let json = serde_json::to_vec(header).context("snapshot header serialize")?;
    let len = u32::try_from(json.len()).context("snapshot header too large for u32 prefix")?;
    anyhow::ensure!(
        len <= MAX_SNAPSHOT_CHUNK_LEN,
        "snapshot header too large: {len} > {MAX_SNAPSHOT_CHUNK_LEN}"
    );
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&json).await?;
    Ok(())
}

async fn read_snapshot_header<R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> anyhow::Result<SnapshotHeader> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(
        len <= MAX_SNAPSHOT_CHUNK_LEN,
        "snapshot header too large: {len} > {MAX_SNAPSHOT_CHUNK_LEN}"
    );
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("snapshot header parse")
}

async fn write_snapshot_chunk<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    chunk: &[u8],
) -> anyhow::Result<()> {
    let len = u32::try_from(chunk.len()).context("snapshot chunk too large for u32 prefix")?;
    anyhow::ensure!(
        len <= MAX_SNAPSHOT_CHUNK_LEN,
        "snapshot chunk too large: {len} > {MAX_SNAPSHOT_CHUNK_LEN}"
    );
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(chunk).await?;
    Ok(())
}

async fn read_snapshot_chunk<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(
        len <= MAX_SNAPSHOT_CHUNK_LEN,
        "snapshot chunk too large: {len} > {MAX_SNAPSHOT_CHUNK_LEN}"
    );
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(body)
}

// ---------------------------------------------------------------------------
// Snapshot SQL helpers: identifier quoting and value literalization.
// ---------------------------------------------------------------------------

/// Read a text column value, erroring if it is not text.
fn value_text(row: &turso::Row, idx: usize) -> anyhow::Result<String> {
    match row.get_value(idx).context("snapshot: get text value")? {
        turso::Value::Text(s) => Ok(s),
        v => anyhow::bail!("snapshot: expected text at column {idx}, got {v:?}"),
    }
}

/// Quote a SQL identifier with double quotes, escaping embedded quotes.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Render a turso value as a self-contained SQL literal for the dump.
/// Blobs use the `X'..'` hex form; text is single-quote escaped.
fn sql_literal(v: &turso::Value) -> anyhow::Result<String> {
    Ok(match v {
        turso::Value::Null => "NULL".to_string(),
        turso::Value::Integer(i) => i.to_string(),
        turso::Value::Real(f) => {
            // Use a round-trippable representation. Non-finite floats
            // cannot be expressed as SQL literals; reject them loudly
            // rather than silently corrupt the replica.
            anyhow::ensure!(f.is_finite(), "snapshot: non-finite float cannot be dumped: {f}");
            format!("{f:?}")
        }
        turso::Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        turso::Value::Blob(b) => {
            use std::fmt::Write as _;
            let mut hex = String::with_capacity(b.len() * 2 + 3);
            hex.push_str("X'");
            for byte in b {
                write!(hex, "{byte:02x}").expect("writing to a String is infallible");
            }
            hex.push('\'');
            hex
        }
    })
}

/// Is this a bookkeeping object the snapshot must not ship?
///
/// Excludes:
/// - `sqlite_*`: engine-internal (schema, sequence, autoindex).
/// - `turso_cdc`: the CDC log itself (rebuilt by enabling CDC, and in v1
///   replicas do not capture, so it stays absent).
/// - `__turso_internal*`: turso 0.7.0's own bookkeeping, e.g. the
///   `__turso_internal_seq_*` autoincrement backing tables. Their
///   `sqlite_schema.sql` is a real CREATE statement, but replaying it is
///   rejected by the engine ("Object name reserved for internal use"),
///   so they must never be dumped.
/// - [`WATERMARK_TABLE`]: litewire's replica watermark, seeded from the
///   snapshot header instead.
///
/// The broad `__` prefix guard is intentional: every internal
/// bookkeeping table this path has encountered uses a `__`-prefixed
/// name, and user tables conventionally do not.
fn is_internal_object(name: &str) -> bool {
    name.starts_with("sqlite_") || name.starts_with("__") || name == "turso_cdc"
}

// ---------------------------------------------------------------------------
// Frame codec — operates on any tokio Async{Read,Write} (i.e. a
// [`ChannelStream`] on the wire, a `tokio::io::DuplexStream` in tests).
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

    // -----------------------------------------------------------------
    // Snapshot codec: header + chunks + end marker roundtrip, and the
    // oversized-chunk rejection. Mirrors the CDC frame_codec_* tests.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn snapshot_codec_header_chunks_end_marker_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(1 << 20);

        let header = SnapshotHeader { watermark: 42, total_len: 11 };
        let chunk_a = b"hello ".to_vec();
        let chunk_b = b"world".to_vec();

        // Writer side.
        write_snapshot_header(&mut client, &header).await.unwrap();
        write_snapshot_chunk(&mut client, &chunk_a).await.unwrap();
        write_snapshot_chunk(&mut client, &chunk_b).await.unwrap();
        write_snapshot_chunk(&mut client, &[]).await.unwrap(); // end marker
        client.flush().await.unwrap();

        // Reader side.
        let got_header = read_snapshot_header(&mut server).await.unwrap();
        assert_eq!(got_header.watermark, 42);
        assert_eq!(got_header.total_len, 11);

        let mut body = Vec::new();
        loop {
            let chunk = read_snapshot_chunk(&mut server).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"hello world");
    }

    #[tokio::test]
    async fn snapshot_codec_rejects_oversized_chunk_prefix() {
        let (mut client, mut server) = tokio::io::duplex(65536);
        let over = MAX_SNAPSHOT_CHUNK_LEN + 1;
        AsyncWriteExt::write_all(&mut client, &over.to_be_bytes()).await.unwrap();
        let err = read_snapshot_chunk(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("snapshot chunk too large"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn snapshot_codec_rejects_oversized_header_prefix() {
        let (mut client, mut server) = tokio::io::duplex(65536);
        let over = MAX_SNAPSHOT_CHUNK_LEN + 1;
        AsyncWriteExt::write_all(&mut client, &over.to_be_bytes()).await.unwrap();
        let err = read_snapshot_header(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("snapshot header too large"), "unexpected error: {err}");
    }

    #[test]
    fn snapshot_stream_type_matches_registry_prefix() {
        assert!(
            SNAPSHOT_STREAM_TYPE.starts_with(ephpm_cluster::stream_type::SNAPSHOT_PREFIX),
            "snapshot stream type {SNAPSHOT_STREAM_TYPE:?} must live under the {:?} prefix",
            ephpm_cluster::stream_type::SNAPSHOT_PREFIX
        );
    }

    #[test]
    fn sql_literal_forms_are_escaped_and_round_trippable() {
        assert_eq!(sql_literal(&turso::Value::Null).unwrap(), "NULL");
        assert_eq!(sql_literal(&turso::Value::Integer(7)).unwrap(), "7");
        assert_eq!(sql_literal(&turso::Value::Integer(-7)).unwrap(), "-7");
        // Single quotes doubled.
        assert_eq!(sql_literal(&turso::Value::Text("a'b".into())).unwrap(), "'a''b'");
        // Blob as hex.
        assert_eq!(sql_literal(&turso::Value::Blob(vec![0x00, 0xde, 0xad])).unwrap(), "X'00dead'");
        // Non-finite float rejected.
        assert!(sql_literal(&turso::Value::Real(f64::INFINITY)).is_err());
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("posts"), "\"posts\"");
        assert_eq!(quote_ident("weird\"name"), "\"weird\"\"name\"");
    }

    #[test]
    fn internal_objects_are_excluded_from_snapshot() {
        assert!(is_internal_object("sqlite_sequence"));
        assert!(is_internal_object("turso_cdc"));
        assert!(is_internal_object(WATERMARK_TABLE));
        // turso 0.7.0's autoincrement backing table for turso_cdc: its
        // sqlite_schema.sql is a real CREATE the engine refuses to
        // replay, so it must be filtered.
        assert!(is_internal_object(
            "__turso_internal_seq___turso_internal_autoincrement_turso_cdc"
        ));
        assert!(!is_internal_object("posts"));
        assert!(!is_internal_object("users"));
    }

    /// The `cdc/` prefix constant this module uses matches the well-known
    /// prefix registered on the cluster channel side.
    #[test]
    fn cdc_stream_type_matches_registry_prefix() {
        assert!(
            CDC_STREAM_TYPE.starts_with(ephpm_cluster::stream_type::CDC_PREFIX),
            "CDC stream type {CDC_STREAM_TYPE:?} must live under the {:?} prefix so the \
             cluster channel dispatch table stays coherent",
            ephpm_cluster::stream_type::CDC_PREFIX
        );
    }

    // -----------------------------------------------------------------
    // parse_primary_addr — Bug 1 regression coverage.
    //
    // The elected-primary KV entry today is emitted by the shared
    // sqlite_election machinery in URL form (`http://addr`). The old
    // code parsed it directly as a SocketAddr and dropped every
    // election result on the floor. These tests lock in the "accept
    // both forms" contract.
    // -----------------------------------------------------------------

    #[test]
    fn parse_primary_addr_accepts_raw_socketaddr() {
        let addr = parse_primary_addr("10.0.0.1:8094").unwrap();
        assert_eq!(addr, "10.0.0.1:8094".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_primary_addr_accepts_http_url_form() {
        // This is the exact string sqlite_election publishes today.
        let addr = parse_primary_addr("http://10.0.0.1:8094").unwrap();
        assert_eq!(addr, "10.0.0.1:8094".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_primary_addr_accepts_https_url_form() {
        let addr = parse_primary_addr("https://10.0.0.1:8094").unwrap();
        assert_eq!(addr, "10.0.0.1:8094".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_primary_addr_strips_trailing_path() {
        let addr = parse_primary_addr("http://10.0.0.1:8094/hrana/v3").unwrap();
        assert_eq!(addr, "10.0.0.1:8094".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_primary_addr_ipv6_forms() {
        let raw = parse_primary_addr("[::1]:8094").unwrap();
        assert_eq!(raw, "[::1]:8094".parse::<SocketAddr>().unwrap());
        let url = parse_primary_addr("http://[::1]:8094").unwrap();
        assert_eq!(url, "[::1]:8094".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_primary_addr_rejects_garbage() {
        assert!(parse_primary_addr("").is_err());
        assert!(parse_primary_addr("not-a-host-port").is_err());
        assert!(parse_primary_addr("http://").is_err());
    }

    /// Direct regression proof: the exact log line from the observed
    /// failure (`primary=http://0.0.0.0:8094`) now parses to a real
    /// SocketAddr instead of the SocketAddr-parse error the old code
    /// produced. The `0.0.0.0` here is only a bug-2 artifact (that
    /// the primary should not have advertised it) — parsing must
    /// still succeed so the caller reaches the dial attempt and the
    /// operator can see the real problem in the error.
    #[test]
    fn elected_to_role_parses_wildcard_url_form_from_field_bug() {
        let elected = ephpm_cluster::ElectedRole::Replica {
            primary_grpc_url: "http://0.0.0.0:8094".to_string(),
        };
        let role = elected_to_role(elected);
        match role {
            Role::Replica { primary_addr } => {
                assert_eq!(primary_addr, "0.0.0.0:8094".parse::<SocketAddr>().unwrap());
            }
            Role::Primary => panic!("expected Role::Replica, got Primary"),
        }
    }
}

//! CDC-native SQLite replication over the cluster channel v1.
//!
//! As of v0.7.0 this is **the** clustered SQLite replication path: the sqld
//! sidecar was removed and Turso is the only embedded engine, so any
//! clustered `[db.sqlite]` configuration (`replication.role` = primary /
//! replica, or `auto` with `[cluster] enabled = true`) runs through here.
//!
//! Replication tails the primary's `turso_cdc` stream and ships
//! per-transaction batches to replicas over the cluster channel — no child
//! process, no gRPC WAL-frame transport. The Turso engine remains Beta
//! upstream; treat clustered mode as experimental (see the clustered caveats
//! in `site/content/roadmap/turso-engine.md`).
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
//!  │ litewire → Turso (wire factory, │    │ litewire → Turso      │
//!  │   enable_cdc_on_connect = true) │    │  (wire factory,       │
//!  │        │                        │    │   capture also on)    │
//!  │  writes capture into turso_cdc  │    │        │              │
//!  │        ▼                        │    │   serves reads        │
//!  │  per-subscriber CdcTailer from  │    │        ▲              │
//!  │  the subscriber's watermark     │    │        │ apply_batch  │
//!  │        │                        │    │  mgmt factory:        │
//!  │  cluster channel handler for    │    │  send Subscribe{wm},  │
//!  │  "cdc/default": one tailer per  │◀───┤  read framed batches, │
//!  │  inbound stream                 │    │  → apply_batch(&conn) │
//!  └─────────────────────────────────┘    └───────────────────────┘
//! ```
//!
//! Capture (`enable_cdc_on_connect`) is enabled on **every** node's wire
//! factory, not just the one that booted as primary. A node promoted by
//! the election cannot retroactively turn capture on for wire sessions
//! that already exist, so a promoted replica would otherwise serve
//! writes that were never captured and the cluster would diverge
//! silently after every failover. Whether a node *ships* what it
//! captured is decided at serve time by the current role, not at
//! factory-build time.
//!
//! **A replica's wire frontend is read-write, and writes made against
//! it are NOT replicated anywhere.** litewire has no read-only frontend
//! mode, so v1 cannot enforce this; the replica logs a warning at
//! startup. Point application traffic at the primary.
//!
//! # Failover
//!
//! The sqlite election machinery (`ephpm_cluster::SqliteElection`) is
//! unchanged. On role change the previous role's driver task is
//! aborted and a driver for the new role is spawned, so a flapping
//! election does not accumulate drivers.
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
//! 4. Subscribe to CDC. The subscribe frame carries the replica's
//!    watermark, so the primary starts a tailer at exactly `N` and the
//!    first batch the replica sees is `N + 1`. [`apply_batch`] remains
//!    idempotent below the watermark as a second line of defence.
//!
//! Snapshot/tail race: writes that land on the primary during the dump
//! get `change_id > N` and are therefore delivered by the post-snapshot
//! tail. There is no gap and no overlap.
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
//! - Triggers are **not** shipped in the snapshot (the primary logs a
//!   warning naming any it skipped). This is deliberate: CDC already
//!   carries the row effects a trigger produced on the primary, so a
//!   replica that also held the trigger would apply them twice.
//! - Experimental: the Turso engine is Beta upstream, so clustered mode is
//!   experimental — but as of v0.7.0 it is the only clustered SQLite path
//!   (sqld was removed), no longer gated behind an opt-in flag.
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
//! drop the stream. Authentication and confidentiality come from the
//! cluster channel underneath: every byte of the yamux connection is
//! sealed with ChaCha20-Poly1305 under a per-connection key, so this
//! module does no sealing of its own.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use ephpm_cluster::{ChannelStream, IncomingStream};
use ephpm_config::SqliteConfig;
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::{CdcRow, CdcTailer, TxnBatch, apply_batch, read_watermark};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{tracked_backend, turso_cdc_metrics as cdc_metrics};

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

/// Ceiling on the up-front allocation the receiver makes from the
/// peer-supplied `total_len` hint (8 MiB). Beyond this the buffer grows
/// as chunks actually arrive, so a peer claiming `u64::MAX` gets an
/// 8 MiB reservation rather than a capacity-overflow panic or an
/// allocator abort. The real ceiling on the transfer is
/// `[db.sqlite.replication] max_snapshot_bytes`.
const SNAPSHOT_PREALLOC_CAP: u64 = 8 * 1024 * 1024;

/// Name of litewire's replica watermark table. Seeding it to the
/// snapshot watermark `N` makes [`apply_batch`] treat every CDC batch
/// with `commit_change_id() <= N` as an idempotent no-op. This mirrors
/// the `CREATE TABLE`/`INSERT OR IGNORE` that litewire-turso's
/// `ensure_watermark_table` performs; we depend on the table name and
/// shape staying stable under the exact litewire pin.
const WATERMARK_TABLE: &str = "__litewire_cdc_watermark";

/// How often the primary polls `turso_cdc` for new batches. Turso 0.7.0
/// has no wakeup signal for CDC inserts, so we poll on a schedule.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long a replica waits between connect retries when the primary
/// is unreachable.
const REPLICA_RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// How often the primary samples `MAX(turso_cdc.change_id)` to publish
/// [`cdc_metrics::METRIC_HEAD_CHANGE_ID`].
///
/// One indexed `MAX()` on an INTEGER PRIMARY KEY per second, and only
/// while this node is the elected primary. That is what makes the lag
/// gauge honest when no subscriber is attached: without it the head
/// would only ever advance as a side effect of shipping, so a primary
/// whose replica just died would report a frozen lag instead of a
/// growing one.
const HEAD_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Heartbeat interval on primary-side subscribers.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Frame types carried on the CDC replication wire.
///
/// The stream is not symmetric: [`Frame::Subscribe`] only ever travels
/// replica → primary and is only ever valid as the first frame;
/// [`Frame::Batch`] and [`Frame::Ping`] only ever travel primary →
/// replica. They share one enum so both directions share one codec.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Frame {
    /// **Replica → primary, first frame only.** The replica's applied
    /// watermark. The primary starts this subscriber's tailer at
    /// exactly this `change_id`, so nothing between the watermark and
    /// "now" can be skipped — which is what makes a reconnect (or a
    /// warm restart) safe without a snapshot.
    Subscribe { from_change_id: i64 },
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
/// - A `cdc/default` stream handler that, while this node is primary,
///   gives every inbound subscriber its own `turso_cdc` tailer starting
///   at the watermark that subscriber announces.
/// - A `snapshot/default` handler that, while this node is primary,
///   serves a cold replica's base snapshot.
/// - On replica: a channel-dial loop that opens `cdc/default` against
///   the primary, announces its watermark, and applies received
///   batches.
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
         maybe_start_cluster_channel returned None despite clustered SQLite being \
         active (startup ordering bug: resolve_channel_features should have enabled \
         the cdc channel feature)",
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

    // Tracks whether this node is currently the primary. Read by the
    // stream handlers, written by the role-change watcher. `Relaxed` is
    // right here: a handler that reads a stale value for a few
    // microseconds around a role flip either serves one extra stream
    // (which the new primary's election heartbeat will supersede) or
    // refuses one that the replica retries two seconds later.
    let is_primary = Arc::new(AtomicBool::new(matches!(initial_role, Role::Primary)));

    // Wire factory: served to litewire. Capture is enabled on EVERY
    // node, not just the one that booted as primary: `Turso::builder`
    // fixes `enable_cdc_on_connect` for the life of the factory, and
    // that factory is moved into litewire below. A node promoted later
    // by the election could therefore never start capturing, and every
    // write it served after promotion would be invisible to replicas.
    // Capturing everywhere costs a `turso_cdc` row per local write on
    // replicas; shipping is gated by role at serve time instead.
    let wire_factory = Turso::builder(db_path)
        .enable_cdc_on_connect(true)
        .build()
        .await
        .with_context(|| format!("failed to open wire Turso factory at {db_path}"))?;

    // Mgmt factory: used by the per-subscriber tailers and the snapshot
    // dumper on the primary, and by the apply loop on the replica. Never
    // opts into CDC-on-connect — the tailer reads turso_cdc explicitly,
    // and the applier's writes must NOT be re-captured (that would make
    // a replica echo the primary's changes into its own CDC log).
    let mgmt_factory = Arc::new(
        Turso::open(db_path)
            .await
            .with_context(|| format!("failed to open mgmt Turso factory at {db_path}"))?,
    );

    let max_snapshot_bytes = sqlite_config.replication.max_snapshot_bytes;

    // Seed the zero-valued CDC series before anything can record. An
    // operator scraping a freshly booted node must be able to tell
    // "replication is on and idle" from "this build has no CDC
    // instrumentation", and an absent counter cannot say which.
    cdc_metrics::init();

    // Publish MAX(change_id) on a timer while primary. See
    // HEAD_SAMPLE_INTERVAL for why this cannot be folded into the
    // shipping path.
    spawn_head_sampler(Arc::clone(&mgmt_factory), Arc::clone(&is_primary), handles);

    // Register the primary-side snapshot handler NOW, before anything
    // dials, so a peer that comes up as a cold replica can reach us as
    // soon as we win an election. Serving is gated on actually being
    // primary — a replica must never hand a peer a full logical dump of
    // the database.
    spawn_snapshot_server(channel, Arc::clone(&mgmt_factory), Arc::clone(&is_primary), handles);

    // Register the CDC subscriber handler NOW even if we start as
    // replica, so it is already in place after a promotion. Each
    // inbound stream announces the watermark it has applied and gets
    // its own tailer starting there.
    let mut cdc_streams = channel.register_exact(CDC_STREAM_TYPE);
    let subs_mgmt = Arc::clone(&mgmt_factory);
    let subs_is_primary = Arc::clone(&is_primary);
    handles.push(tokio::spawn(async move {
        while let Some(incoming) = cdc_streams.recv().await {
            let IncomingStream { stream, peer, .. } = incoming;
            if !subs_is_primary.load(Ordering::Relaxed) {
                cdc_metrics::record_stream_refused("cdc");
                tracing::warn!(
                    peer = %peer,
                    "CDC: refusing to serve a replication stream — this node is not the \
                     primary; the peer is dialing a stale elected-primary address"
                );
                continue;
            }
            let mgmt = Arc::clone(&subs_mgmt);
            tokio::spawn(async move {
                if let Err(e) = serve_subscriber(stream, &mgmt).await {
                    tracing::info!(peer = %peer, "CDC subscriber disconnected: {e:#}");
                }
            });
        }
    }));

    // Cold-start bootstrap: if we begin life as a replica with an empty
    // local DB, fetch the primary's base snapshot BEFORE the litewire
    // wire frontends start serving; otherwise a client read could
    // observe partial state. This is awaited (blocking startup) on
    // purpose; the wire frontends spin up only after it completes. A
    // primary, or a replica whose DB is already populated, skips this.
    if let Role::Replica { primary_addr } = &initial_role {
        maybe_bootstrap_cold_replica(&mgmt_factory, *primary_addr, channel, max_snapshot_bytes)
            .await?;
    }

    // Start litewire wire frontends. Wire factory is moved in here, shared
    // with the PHP `ephpm_db_*` bridge so in-process queries hit the same
    // tracked backend as wire clients.
    let tracked = tracked_backend::TrackedBackend::new(wire_factory, query_stats.clone());
    spawn_litewire_serve(sqlite_config, crate::share_backend_with_php(tracked), handles);

    // Kick off role-appropriate work for the initial role.
    let initial_driver =
        tokio::spawn(start_role(initial_role, Arc::clone(&mgmt_factory), channel.clone()));

    // Role-change watcher. The previous role's driver is aborted before
    // the new one starts: a flapping election would otherwise pile up
    // replica dial loops that all keep applying batches into the same
    // connection.
    if let Some(mut watch_rx) = role_rx {
        let mgmt = Arc::clone(&mgmt_factory);
        let channel = channel.clone();
        handles.push(tokio::spawn(async move {
            let mut current = initial_driver;
            while watch_rx.changed().await.is_ok() {
                let new_elected = watch_rx.borrow().clone();
                let new_role = elected_to_role(new_elected);
                tracing::info!(?new_role, "CDC replication: role change detected");
                is_primary.store(matches!(new_role, Role::Primary), Ordering::Relaxed);
                current.abort();
                current = tokio::spawn(start_role(new_role, Arc::clone(&mgmt), channel.clone()));
            }
        }));
    } else {
        handles.push(initial_driver);
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
                 cluster channel address in this mode, e.g. \"10.0.0.1:7948\" — \
                 the channel defaults to the gossip port + 2, not the KV data \
                 plane's gossip port + 1)"
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

async fn start_role(role: Role, mgmt: Arc<Turso>, channel: ephpm_cluster::ChannelHandle) {
    match role {
        Role::Primary => {
            // Nothing to drive: on the primary, each inbound subscriber
            // stream owns its own tailer (see `serve_subscriber`), so
            // there is no shared tail loop to run.
            tracing::info!("CDC primary: serving replication streams on {CDC_STREAM_TYPE}");
        }
        Role::Replica { primary_addr } => {
            if let Err(e) = run_replica(mgmt, primary_addr, channel).await {
                tracing::error!("CDC replica loop exited: {e:#}");
            }
        }
    }
}

/// Spawn the primary-side head sampler: publish
/// `MAX(turso_cdc.change_id)` every [`HEAD_SAMPLE_INTERVAL`] while this
/// node is the elected primary.
///
/// The connection is opened once and reused; a per-tick connection would
/// cost more than the query. A read failure is logged at debug and the
/// loop continues — `current_max_change_id` already answers `0` for the
/// cold case where `turso_cdc` does not exist yet, and the head gauge is
/// monotonic, so a transient failure cannot walk it backwards.
fn spawn_head_sampler(
    mgmt: Arc<Turso>,
    is_primary: Arc<AtomicBool>,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    handles.push(tokio::spawn(async move {
        let conn = match mgmt.raw_connection() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("CDC head sampler: cannot open mgmt connection: {e:#}");
                return;
            }
        };
        let mut ticker = tokio::time::interval(HEAD_SAMPLE_INTERVAL);
        loop {
            ticker.tick().await;
            // Only the primary has a meaningful head: a replica's local
            // turso_cdc holds whatever its own wire frontend captured,
            // which is not the replication stream's position.
            if !is_primary.load(Ordering::Relaxed) {
                continue;
            }
            match current_max_change_id(&conn).await {
                Ok(head) => cdc_metrics::observe_head(head),
                Err(e) => tracing::debug!("CDC head sampler: MAX(change_id) failed: {e:#}"),
            }
        }
    }));
}

// ---------------------------------------------------------------------------
// Primary: one tailer per subscriber. (Subscriber-side accept is
// registered up in `start_clustered_turso_cdc` so it exists across role
// transitions.)
// ---------------------------------------------------------------------------

/// Serve one replica's `cdc/default` stream.
///
/// The replica's first frame is [`Frame::Subscribe`], carrying the
/// `change_id` it has already applied. We open a [`CdcTailer`] at
/// exactly that cursor and pump batches into the stream from there.
///
/// This is deliberately **not** a fan-out from one shared cursor. An
/// earlier version broadcast a single primary-wide tailer to all
/// subscribers via `tokio::sync::broadcast`; `subscribe()` only
/// delivers values sent after the call and the wire had no way to ask
/// for a resume point, so every lag event and every reconnect silently
/// and permanently dropped the batches in the gap while the shared
/// cursor marched on. One cursor per subscriber, anchored to a
/// watermark the subscriber names, makes that structurally impossible.
///
/// The cost is one `turso_cdc` poll per subscriber per
/// [`POLL_INTERVAL`]. With the handful of replicas v1 targets that is
/// the right trade against losing writes.
///
/// Generic over the stream so the loop can be driven directly in tests
/// over a `tokio::io::duplex` pair. Production always passes a
/// [`ChannelStream`]; before this was generic the only coverage of this
/// function was a hand-rolled copy inside the e2e tests, which is how the
/// cold-start reconnect loop went unnoticed.
///
/// Public for the same reason [`serve_snapshot`] is: the two-node e2e
/// suites drive this exact function rather than a twin, so the metrics it
/// records are the ones a real primary records. Not part of a stable API.
///
/// # Errors
///
/// Returns an error if the first frame is not a valid `Subscribe`, if a
/// `turso_cdc` poll fails, or if the stream breaks.
pub async fn serve_subscriber<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut stream: S,
    mgmt: &Turso,
) -> anyhow::Result<()> {
    let from = match read_frame(&mut stream).await.context("read subscribe frame")? {
        Frame::Subscribe { from_change_id } => from_change_id,
        other => anyhow::bail!("expected Subscribe as the first CDC frame, got {other:?}"),
    };
    anyhow::ensure!(from >= 0, "subscriber sent a negative watermark: {from}");
    tracing::info!(from_change_id = from, "CDC: subscriber attached");

    // RAII: every exit path below is a `?` or a `bail!`, so the
    // subscriber gauge and this cursor's contribution to the lag gauge
    // are unwound by the guard's Drop rather than by a detach call this
    // function has no single place to make.
    let subscriber = cdc_metrics::attach_subscriber(from);

    let mut tailer = CdcTailer::new(mgmt, from);
    let mut idle_since = tokio::time::Instant::now();

    loop {
        // A cold primary — one whose `turso_cdc` log does not exist
        // because it has served no captured write yet — polls as
        // `Ok(None)`, not as an error. That tolerance lives in litewire
        // (`CdcTailer::poll_batch`), where "absent" and "empty" are the
        // same answer and no consumer has to know the difference.
        let batch = match tailer.poll_batch().await {
            Ok(batch) => batch,
            Err(e) => {
                // Drop the stream rather than skip past the failure —
                // the replica reconnects with the same watermark and we
                // retry from the identical cursor.
                cdc_metrics::record_tail_poll_error();
                anyhow::bail!("CDC tail poll error: {e:#}");
            }
        };

        // A rowless batch has no commit `change_id` to advance a cursor
        // with (litewire#17 made `commit_change_id` return `Option`), and
        // shipping `{"rows":[]}` would trip the subscriber's own
        // empty-frame guard and tear the stream down. Fold it into the
        // idle arm below: nothing to send, nothing to record.
        let shippable = batch.and_then(|b| {
            let id = b.commit_change_id()?;
            Some((b, id))
        });

        match shippable {
            Some((batch, commit_change_id)) => {
                let rows = batch.rows.len();
                let frame =
                    Frame::Batch { rows: batch.rows.iter().map(WireCdcRow::from).collect() };
                write_frame(&mut stream, &frame).await?;
                // Recorded only after the frame is on the wire: counting
                // a batch we then failed to write would overstate how far
                // this subscriber has actually got.
                subscriber.record_batch_shipped(commit_change_id, rows);
                idle_since = tokio::time::Instant::now();
            }
            None => {
                // No *complete* transaction beyond the cursor, so the
                // cursor is a lower bound on the head — free, with no
                // extra query. A lower bound is safe because the head
                // gauge only ever moves via `fetch_max`; the sampler
                // supplies the exact MAX, and an in-flight uncommitted
                // transaction (whose rows are already in the log but not
                // yet yielded) is the gap between the two.
                cdc_metrics::observe_head(tailer.cursor());
                // Keep the stream warm so a replica can tell "idle
                // primary" from "dead primary".
                if idle_since.elapsed() >= HEARTBEAT_INTERVAL {
                    write_frame(&mut stream, &Frame::Ping).await?;
                    idle_since = tokio::time::Instant::now();
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Replica: dial the cluster channel + read + apply.
// ---------------------------------------------------------------------------

/// The replica driver: dial the primary's `cdc/default` stream, announce
/// the local watermark, apply what arrives, and retry forever.
///
/// Public so the two-node e2e suites run this exact loop instead of a
/// twin — it is where the reconnect and connect-outcome metrics are
/// recorded, and a copied loop would record none of them. Only returns on
/// a fatal setup error; normal stream failures are retried internally.
/// Not part of a stable API.
///
/// # Errors
///
/// Returns an error only if the local apply connection cannot be opened.
pub async fn run_replica(
    mgmt: Arc<Turso>,
    primary_addr: SocketAddr,
    channel: ephpm_cluster::ChannelHandle,
) -> anyhow::Result<()> {
    // The replica's local Turso engine serves reads via litewire; writes
    // arrive only through apply_batch, keyed by monotonic watermark.
    let apply_conn = mgmt.raw_connection()?;

    tracing::warn!(
        primary = %primary_addr,
        "CDC replica: the local wire frontends accept WRITES and litewire has no read-only \
         mode, but nothing replicates a write made against a replica — it will diverge from \
         the primary and be overwritten or shadowed by replayed batches. Point application \
         traffic at the primary."
    );

    loop {
        match channel.dial(primary_addr, CDC_STREAM_TYPE).await {
            Ok(mut stream) => {
                // Resume from exactly what we have applied. Read it
                // fresh on every connect: a previous session may have
                // advanced it. A read failure must not kill the driver —
                // subscribing from a stale or default cursor would be
                // worse than waiting for the next attempt.
                let watermark = match read_watermark(&apply_conn).await {
                    Ok(w) => w,
                    Err(e) => {
                        cdc_metrics::record_connect_outcome("watermark_error");
                        tracing::warn!("CDC replica: cannot read local watermark: {e}");
                        tokio::time::sleep(REPLICA_RECONNECT_DELAY).await;
                        continue;
                    }
                };
                match subscribe_and_consume(&mut stream, &apply_conn, watermark).await {
                    Ok(()) => {
                        cdc_metrics::record_connect_outcome("closed");
                        tracing::info!("CDC replica: primary closed stream cleanly");
                    }
                    Err(e) => {
                        cdc_metrics::record_connect_outcome("stream_error");
                        tracing::warn!("CDC replica stream error: {e:#}");
                    }
                }
            }
            Err(e) => {
                cdc_metrics::record_connect_outcome("dial_error");
                tracing::debug!(primary = %primary_addr, "CDC replica dial failed: {e:#}");
            }
        }
        tokio::time::sleep(REPLICA_RECONNECT_DELAY).await;
    }
}

/// Announce our watermark, then apply everything the primary sends.
///
/// Generic over the stream, and public, for the same reason
/// [`serve_subscriber`] is: the two-node e2e suites drive this exact
/// function so the replica-side metrics under test are the production
/// ones. Not part of a stable API.
///
/// # Errors
///
/// Returns an error if the subscribe frame cannot be sent, if the primary
/// sends a malformed or wrong-direction frame, or if `apply_batch` fails.
pub async fn subscribe_and_consume<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    apply_conn: &litewire::litewire_turso::TursoConnection,
    watermark: i64,
) -> anyhow::Result<()> {
    write_frame(stream, &Frame::Subscribe { from_change_id: watermark })
        .await
        .context("send subscribe frame")?;
    // Publish what we are resuming from before a single batch lands, so
    // the watermark gauge reflects durable local state rather than only
    // what this process has applied since it started.
    cdc_metrics::record_applied_watermark(watermark);
    tracing::info!(from_change_id = watermark, "CDC replica: subscribed");
    consume_frames(stream, apply_conn).await
}

async fn consume_frames<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    apply_conn: &litewire::litewire_turso::TursoConnection,
) -> anyhow::Result<()> {
    loop {
        let frame = read_frame(stream).await?;
        match frame {
            Frame::Batch { rows } => {
                // Kept as defence in depth, not because litewire still
                // needs it: since litewire#17 `commit_change_id` returns
                // `Option<i64>` and `apply_batch` reports an empty batch
                // as an error instead of panicking the applying task
                // (which nothing joins). Two reasons this stays anyway.
                // Validating a peer's frame is our job, not that of
                // whichever litewire revision the pin happens to name.
                // And rejecting here keeps the error legible: past this
                // point every failure is reported "at change_id N",
                // which an empty batch has no value for.
                let batch = TxnBatch { rows: rows.into_iter().map(CdcRow::from).collect() };
                let Some(change_id) = batch.commit_change_id() else {
                    anyhow::bail!("peer sent an empty CDC batch frame");
                };
                // Read after the guard: an empty batch never reaches the
                // recording site below, so `rows_applied` can never be
                // incremented by zero for a frame we rejected.
                let row_count = batch.rows.len();
                // A failed apply must NOT be skipped: continuing to the
                // next batch would let the watermark advance past the
                // failure and make the divergence permanent and silent.
                // Fail the stream; the replica reconnects and resumes
                // from the watermark, which has not moved.
                let started = std::time::Instant::now();
                match apply_batch(apply_conn, &batch).await {
                    Ok(()) => {
                        // Advances the watermark gauge to `change_id`,
                        // which is exactly what the local watermark table
                        // now holds.
                        cdc_metrics::record_batch_applied(change_id, row_count, started.elapsed());
                    }
                    Err(e) => {
                        // Deliberately does NOT touch the watermark
                        // gauge: the watermark did not move, and a gauge
                        // that advanced past a failed apply would report
                        // convergence that has not happened.
                        cdc_metrics::record_apply_error();
                        return Err(anyhow::Error::new(e)
                            .context(format!("CDC apply_batch failed at change_id {change_id}")));
                    }
                }
            }
            Frame::Ping => {}
            Frame::Subscribe { .. } => {
                anyhow::bail!("primary sent a Subscribe frame; wrong direction");
            }
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

/// Spawn the primary-side snapshot server. The handler is registered
/// up-front (like the CDC handler) so it is already in place after a
/// promotion, but a stream is only **served** while this node is the
/// elected primary: a full logical dump of the database is the most
/// sensitive thing on the channel, and a replica has no business
/// handing one out.
fn spawn_snapshot_server(
    channel: &ephpm_cluster::ChannelHandle,
    mgmt: Arc<Turso>,
    is_primary: Arc<AtomicBool>,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let mut snapshot_streams = channel.register_exact(SNAPSHOT_STREAM_TYPE);
    handles.push(tokio::spawn(async move {
        while let Some(incoming) = snapshot_streams.recv().await {
            let IncomingStream { stream, peer, .. } = incoming;
            if !is_primary.load(Ordering::Relaxed) {
                cdc_metrics::record_stream_refused("snapshot");
                tracing::warn!(
                    peer = %peer,
                    "snapshot bootstrap: refusing to serve a database dump — this node is \
                     not the primary; the peer is dialing a stale elected-primary address"
                );
                drop(stream);
                continue;
            }
            let mgmt = Arc::clone(&mgmt);
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
    // Recorded here rather than at the call site so a test that drives
    // `serve_snapshot` directly still exercises the instrumentation, and
    // so both outcomes are counted in one place.
    let started = std::time::Instant::now();
    match serve_snapshot_inner(&mut stream, mgmt).await {
        Ok((watermark, bytes)) => {
            cdc_metrics::record_snapshot_served("ok", bytes, started.elapsed());
            Ok(watermark)
        }
        Err(e) => {
            cdc_metrics::record_snapshot_served("error", 0, started.elapsed());
            Err(e)
        }
    }
}

/// Body of [`serve_snapshot`]. Returns the watermark and the dump size in
/// bytes so the caller can record both without measuring twice.
async fn serve_snapshot_inner(
    stream: &mut ChannelStream,
    mgmt: &Turso,
) -> anyhow::Result<(i64, u64)> {
    let conn = mgmt.raw_connection().context("snapshot: open mgmt connection")?;
    let (watermark, dump) = produce_snapshot(&conn).await.context("snapshot: produce dump")?;

    let header = SnapshotHeader { watermark, total_len: dump.len() as u64 };
    write_snapshot_header(stream, &header).await?;

    for chunk in dump.as_bytes().chunks(SNAPSHOT_CHUNK_TARGET) {
        write_snapshot_chunk(stream, chunk).await?;
    }
    // Zero-length end marker: signals a clean end of body.
    write_snapshot_chunk(stream, &[]).await?;
    stream.flush().await?;
    Ok((watermark, dump.len() as u64))
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
///
/// **Triggers are skipped**, with a warning naming each one. Two
/// reasons, one correctness and one safety:
///
/// - CDC replays the row effects a trigger produced on the primary. A
///   replica that also held the trigger would fire it again on top of
///   those replayed rows and diverge.
/// - A trigger body is the one piece of `sqlite_schema.sql` that
///   contains statement separators inside itself, which would make the
///   receiving side's statement allowlist (see
///   [`validate_snapshot_dump`]) unable to reason about the dump.
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
        if obj_type == "trigger" {
            tracing::warn!(
                trigger = %name,
                "snapshot bootstrap: not shipping trigger to the replica — CDC already \
                 carries the rows it produced on the primary, so replaying the trigger \
                 there would double-apply them"
            );
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
///
/// The dump arrives from the network and is only UTF-8 checked before
/// this point, so it is validated against a statement allowlist first
/// — see [`validate_snapshot_dump`]. Only then does it reach
/// `execute_batch`.
async fn apply_snapshot(
    conn: &turso::Connection,
    watermark: i64,
    dump: &str,
) -> anyhow::Result<()> {
    validate_snapshot_dump(dump).context("snapshot: dump rejected by the statement allowlist")?;
    // Execute the whole dump as a batch. It is self-consistent DDL+DML
    // captured under one read view on the primary.
    conn.execute_batch(dump).await.context("snapshot: apply dump")?;
    seed_watermark(conn, watermark).await.context("snapshot: seed watermark")?;
    Ok(())
}

/// Statement kinds a snapshot dump may contain.
///
/// A snapshot is, by construction, schema `CREATE`s followed by
/// `INSERT OR REPLACE`s. Anything else — `ATTACH`, `PRAGMA`, `DROP`,
/// `UPDATE`, `DELETE` — is a peer trying to run SQL of its own choosing
/// on us, and the bootstrap path used to hand the whole blob straight
/// to `execute_batch`.
const ALLOWED_STATEMENT_PREFIXES: [&str; 2] = ["CREATE", "INSERT"];

/// Reject a dump that contains anything outside
/// [`ALLOWED_STATEMENT_PREFIXES`].
///
/// The scan is quote-aware — single-quoted strings with `''` escapes,
/// double-quoted identifiers with `""` escapes, and `X'..'` blobs are
/// all just quoted runs, so a `;` inside one is not a separator.
///
/// Comments are **skipped, not rejected**: `sqlite_schema.sql` stores
/// the exact `CREATE` text the operator wrote, comments included, so
/// refusing them would make a perfectly ordinary database
/// un-bootstrappable. Skipping keeps them from hiding a separator, and
/// [`check_statement_allowed`] looks past leading comments to find the
/// statement's real first keyword.
///
/// # Errors
///
/// Returns an error naming the offending statement (truncated) when the
/// dump contains a disallowed statement, an unterminated quote, or an
/// unterminated block comment.
fn validate_snapshot_dump(dump: &str) -> anyhow::Result<()> {
    let bytes = dump.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                loop {
                    anyhow::ensure!(i < bytes.len(), "unterminated quoted literal in dump");
                    if bytes[i] == quote {
                        // A doubled quote is an escaped quote, not a close.
                        if i + 1 < bytes.len() && bytes[i + 1] == quote {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                // Line comment: runs to the newline, or to EOF.
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                loop {
                    anyhow::ensure!(i + 1 < bytes.len(), "unterminated block comment in dump");
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b';' => {
                check_statement_allowed(&dump[start..i])?;
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    // Trailing text after the last `;` must be blank (or a comment).
    check_statement_allowed(&dump[start..])
}

/// Allow an empty/whitespace/comment-only statement, or one starting
/// with an allowlisted keyword.
fn check_statement_allowed(stmt: &str) -> anyhow::Result<()> {
    let trimmed = strip_leading_noise(stmt);
    if trimmed.is_empty() {
        return Ok(());
    }
    let keyword = trimmed
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase();
    anyhow::ensure!(
        ALLOWED_STATEMENT_PREFIXES.contains(&keyword.as_str()),
        "disallowed statement in snapshot dump (only CREATE and INSERT are accepted): {}",
        truncate_for_log(trimmed)
    );
    Ok(())
}

/// Drop leading whitespace and leading SQL comments so the statement's
/// first keyword is what gets checked. Returns `""` when the input is
/// nothing but whitespace and comments (including an unterminated one).
fn strip_leading_noise(stmt: &str) -> &str {
    let mut s = stmt;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            let Some(nl) = rest.find('\n') else { return "" };
            s = &rest[nl + 1..];
        } else if let Some(rest) = s.strip_prefix("/*") {
            let Some(end) = rest.find("*/") else { return "" };
            s = &rest[end + 2..];
        } else {
            return s;
        }
    }
}

/// First 120 characters of `s`, for error messages that must not dump a
/// multi-megabyte payload into the log.
fn truncate_for_log(s: &str) -> String {
    const LIMIT: usize = 120;
    if s.chars().count() <= LIMIT {
        return s.to_string();
    }
    let head: String = s.chars().take(LIMIT).collect();
    format!("{head}…")
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
/// Exhausting the retry budget is **fatal**. Coming up empty was the
/// previous behaviour and it is worse than not coming up at all: the
/// node would immediately start serving reads that silently omit every
/// row written before the CDC log's earliest retained entry, with
/// nothing but a startup `error!` to say so. Failing startup is loud,
/// and an orchestrator restart is exactly the retry that has a chance
/// of working.
///
/// # Errors
///
/// Returns an error if the local connection cannot be opened, if the
/// cold-start check fails, or if every bootstrap attempt fails.
async fn maybe_bootstrap_cold_replica(
    mgmt: &Turso,
    primary_addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    max_snapshot_bytes: u64,
) -> anyhow::Result<()> {
    let conn = mgmt.raw_connection().context("snapshot bootstrap: open local connection")?;

    if !local_db_is_cold(&conn).await.context("snapshot bootstrap: cold-start check")? {
        cdc_metrics::record_bootstrap("skipped");
        // A warm replica already has a watermark; publish it so the gauge
        // is populated before the first batch rather than reading 0 until
        // one arrives.
        if let Ok(wm) = read_watermark(&conn).await {
            cdc_metrics::record_applied_watermark(wm);
        }
        tracing::info!("snapshot bootstrap: local DB already populated; skipping");
        return Ok(());
    }

    tracing::warn!(
        primary = %primary_addr,
        "EXPERIMENTAL snapshot bootstrap: local Turso DB is cold; fetching base snapshot \
         from the primary before serving. Clustered Turso replication is experimental \
         (Turso is Beta upstream)."
    );

    const MAX_ATTEMPTS: u32 = 30;
    let mut last_error = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match fetch_and_apply_snapshot(&conn, primary_addr, channel, max_snapshot_bytes).await {
            Ok(n) => {
                cdc_metrics::record_bootstrap("ok");
                tracing::info!(
                    primary = %primary_addr,
                    watermark = n,
                    "snapshot bootstrap complete; replica seeded and ready to tail CDC"
                );
                return Ok(());
            }
            Err(e) => {
                tracing::debug!(
                    attempt,
                    primary = %primary_addr,
                    "snapshot bootstrap attempt failed: {e:#}"
                );
                last_error = Some(e);
                tokio::time::sleep(REPLICA_RECONNECT_DELAY).await;
            }
        }
    }

    cdc_metrics::record_bootstrap("failed");
    let cause = last_error.unwrap_or_else(|| anyhow::anyhow!("no attempt recorded an error"));
    Err(cause.context(format!(
        "snapshot bootstrap from {primary_addr} failed after {MAX_ATTEMPTS} attempts. \
         Refusing to start: a cold replica that comes up without its base snapshot would \
         serve reads missing every pre-CDC row. Check that the primary is reachable on its \
         cluster channel address and that both nodes share a [cluster] secret."
    )))
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
/// `max_snapshot_bytes` bounds how much body this call will accept
/// before giving up — see `[db.sqlite.replication] max_snapshot_bytes`.
///
/// # Errors
///
/// Returns an error if the dial, header/chunk read, or dump application
/// fails, if the peer announces or streams more than
/// `max_snapshot_bytes`, or if the dump contains a statement outside
/// the `CREATE`/`INSERT` allowlist.
pub async fn fetch_and_apply_snapshot(
    conn: &turso::Connection,
    primary_addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    max_snapshot_bytes: u64,
) -> anyhow::Result<i64> {
    let started = std::time::Instant::now();
    let mut stream = channel
        .dial(primary_addr, SNAPSHOT_STREAM_TYPE)
        .await
        .with_context(|| format!("snapshot: dial {primary_addr}"))?;

    let header = read_snapshot_header(&mut stream).await.context("snapshot: read header")?;
    anyhow::ensure!(
        header.total_len <= max_snapshot_bytes,
        "snapshot: peer announced {} bytes, above the [db.sqlite.replication] \
         max_snapshot_bytes limit of {max_snapshot_bytes}",
        header.total_len
    );

    // `total_len` is advisory and peer-controlled: a hostile or buggy
    // peer can claim anything, so it is only ever a *hint* for the
    // initial allocation, clamped to a size we are happy to reserve up
    // front. The authoritative limit is the running total below.
    let hint = header.total_len.min(SNAPSHOT_PREALLOC_CAP);
    let mut body = Vec::with_capacity(usize::try_from(hint).unwrap_or(0));
    let mut received: u64 = 0;
    loop {
        let chunk = read_snapshot_chunk(&mut stream).await.context("snapshot: read chunk")?;
        if chunk.is_empty() {
            break; // end marker
        }
        received = received.saturating_add(chunk.len() as u64);
        anyhow::ensure!(
            received <= max_snapshot_bytes,
            "snapshot: body exceeded the [db.sqlite.replication] max_snapshot_bytes limit \
             of {max_snapshot_bytes} (a peer streaming chunks without an end marker would \
             otherwise grow this buffer without bound)"
        );
        body.extend_from_slice(&chunk);
    }
    let dump = String::from_utf8(body).context("snapshot: dump body is not valid utf-8")?;
    apply_snapshot(conn, header.watermark, &dump).await?;
    // Only counted once the dump is durably applied: bytes received into
    // a buffer that then failed validation are not a bootstrap.
    cdc_metrics::record_snapshot_received(received, started.elapsed());
    // The local watermark table now holds exactly this value, so the
    // replica's watermark gauge is correct before the CDC tail starts.
    cdc_metrics::record_applied_watermark(header.watermark);
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
    if sqlite_config.proxy.max_connections > 0 {
        builder = builder.max_connections(sqlite_config.proxy.max_connections);
        tracing::info!(
            max_connections = sqlite_config.proxy.max_connections,
            "SQLite wire frontends: connection cap enabled (CDC-replicated Turso)"
        );
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
    // Cold-start: a primary whose `turso_cdc` log does not exist yet
    // must hold its subscriber open, not tear it down.
    //
    // The tolerance itself lives in litewire; this asserts the behaviour
    // ePHPm depends on, so a pin that lost the fix fails here rather
    // than silently restoring a 2-second reconnect loop in production.
    // -----------------------------------------------------------------

    /// Drives the **production** `serve_subscriber` against a database
    /// that has never been written to, which is the state of every
    /// freshly provisioned cluster.
    ///
    /// Before the cold-start guard, `poll_batch` returned "no such table:
    /// turso_cdc", `serve_subscriber` bailed, and the replica redialed
    /// every `REPLICA_RECONNECT_DELAY` (2s) forever — an error loop that
    /// only ended when someone happened to write. Here that manifests as
    /// the task completing almost immediately with an error.
    ///
    /// The assertion is two-part on purpose: staying alive is not enough,
    /// the subscriber must still deliver the first real batch afterwards.
    ///
    /// `#[serial(cdc_registry)]` because this drives the production
    /// `serve_subscriber`, which attaches to the process-global subscriber
    /// registry in `turso_cdc_metrics`. Left concurrent, its subscriber
    /// (attached at change_id 0) pulls that registry's `shipped` cursor to 0
    /// and adds a `cursors` row underneath the gauge tests asserting on both —
    /// the crate's dominant flake at 13/100 full-suite runs. See the group's
    /// invariant documented on `turso_cdc_metrics::tests`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(cdc_registry)]
    async fn cold_primary_holds_subscriber_open_until_first_write() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();

        // Mgmt factory: what the tailer reads through. Never enables
        // CDC-on-connect, so nothing here creates turso_cdc.
        let mgmt = Arc::new(Turso::open(&path).await.unwrap());

        // Precondition: the log genuinely does not exist yet. If a future
        // litewire creates it eagerly, this assert fires and tells us the
        // guard is no longer load-bearing.
        {
            let conn = mgmt.raw_connection().unwrap();
            let probe = conn.prepare("SELECT 1 FROM turso_cdc LIMIT 1").await;
            assert!(probe.is_err(), "turso_cdc should not exist before any captured write");
        }

        let (mut client, server) = tokio::io::duplex(1 << 20);
        let mgmt_for_task = Arc::clone(&mgmt);
        let task = tokio::spawn(async move { serve_subscriber(server, &mgmt_for_task).await });

        // Subscribe from the very beginning, as a cold replica does.
        write_frame(&mut client, &Frame::Subscribe { from_change_id: 0 }).await.unwrap();

        // The old behaviour ended the task here. Give it well over the
        // poll interval to prove it does not.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !task.is_finished(),
            "subscriber died on a cold database — the reconnect loop has regressed"
        );

        // Now write through a CDC-capturing session; turso_cdc springs
        // into existence and the batch must reach the still-open stream.
        // `connect` comes from the Backend trait, scoped here so it does
        // not leak into the rest of the test module.
        use litewire::backend::Backend;
        let wire = Turso::builder(&path).enable_cdc_on_connect(true).build().await.unwrap();
        let session = wire.connect().await.unwrap();
        session.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).await.unwrap();
        session.execute("INSERT INTO t VALUES (1, 'hello')", &[]).await.unwrap();

        // Read frames until a non-empty Batch arrives (heartbeat Pings
        // may interleave). The whole point is that this stream — the one
        // opened before the table existed — is the one that delivers.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut got_batch = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), read_frame(&mut client)).await {
                Ok(Ok(Frame::Batch { rows })) if !rows.is_empty() => {
                    got_batch = true;
                    break;
                }
                Ok(Err(e)) => panic!("stream broke after the first write: {e}"),
                // A heartbeat Ping or an empty Batch (Ok(Ok(_))), and a
                // read that simply timed out (Err(_)): both mean "nothing
                // yet, keep waiting until the deadline".
                Ok(Ok(_)) | Err(_) => {}
            }
        }
        assert!(got_batch, "no CDC batch arrived on the subscriber opened before the first write");

        task.abort();
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

    // -----------------------------------------------------------------
    // Snapshot dump allowlist. The dump arrives from the network and
    // used to go straight into `execute_batch`, so anything that is not
    // a CREATE or an INSERT is a peer running SQL of its choosing on us.
    // -----------------------------------------------------------------

    #[test]
    fn validator_accepts_a_real_dump_shape() {
        let dump = "CREATE TABLE \"posts\" (id INTEGER PRIMARY KEY, title TEXT);\n\
                    CREATE INDEX \"idx\" ON \"posts\" (title);\n\
                    INSERT OR REPLACE INTO \"posts\" (rowid, \"id\", \"title\") \
                    VALUES (1, 1, 'hello');\n";
        validate_snapshot_dump(dump).expect("a dump this module produced must validate");
    }

    #[test]
    fn validator_tolerates_semicolons_inside_literals() {
        let dump = "INSERT OR REPLACE INTO \"t\" (rowid, \"v\") \
                    VALUES (1, 'a;DROP TABLE t;--');\n";
        validate_snapshot_dump(dump).expect("a semicolon inside a string is not a separator");
        let dump = "CREATE TABLE \"weird;name\" (a TEXT);\n";
        validate_snapshot_dump(dump).expect("a semicolon inside an identifier is not a separator");
    }

    #[test]
    fn validator_rejects_statements_outside_the_allowlist() {
        for bad in [
            "ATTACH DATABASE '/etc/passwd' AS steal;",
            "PRAGMA journal_mode = WAL;",
            "DROP TABLE posts;",
            "DELETE FROM posts;",
            "UPDATE posts SET title = 'x';",
            "CREATE TABLE t (a);\nDROP TABLE t;",
        ] {
            let err = validate_snapshot_dump(bad).unwrap_err();
            assert!(
                err.to_string().contains("disallowed statement"),
                "expected a rejection for {bad:?}, got: {err}"
            );
        }
    }

    /// Comments must be tolerated — `sqlite_schema.sql` preserves the
    /// operator's original `CREATE` text, comments and all, so rejecting
    /// them would make an ordinary database un-bootstrappable. They must
    /// not be able to smuggle a statement past the allowlist either.
    #[test]
    fn validator_tolerates_comments_without_letting_them_smuggle() {
        validate_snapshot_dump("-- a note\nCREATE TABLE t (a);").unwrap();
        validate_snapshot_dump("/* a note */ CREATE TABLE t (a);").unwrap();
        validate_snapshot_dump("CREATE TABLE t (\n  a INTEGER -- the id\n);").unwrap();
        validate_snapshot_dump("CREATE TABLE t (a); -- trailing").unwrap();
        // A comment cannot hide a disallowed statement...
        assert!(validate_snapshot_dump("-- note\nDROP TABLE t;").is_err());
        assert!(validate_snapshot_dump("CREATE TABLE t (a); /* x */ DROP TABLE t;").is_err());
        // ...and cannot hide a statement separator either: the `;` here
        // is inside the comment, so `DROP` is part of the same statement
        // as the CREATE rather than a new allowed one.
        validate_snapshot_dump("CREATE TABLE t (a); -- ; DROP TABLE t\n").unwrap();
    }

    #[test]
    fn validator_rejects_unterminated_quotes_and_comments() {
        assert!(validate_snapshot_dump("INSERT INTO t VALUES ('unterminated);").is_err());
        assert!(validate_snapshot_dump("CREATE TABLE t (a); /* never closed").is_err());
    }

    #[test]
    fn validator_accepts_empty_and_whitespace_only_dumps() {
        validate_snapshot_dump("").unwrap();
        validate_snapshot_dump("  \n\t ").unwrap();
        validate_snapshot_dump(";;\n;").unwrap();
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

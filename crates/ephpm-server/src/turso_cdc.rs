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
//! Each CDC stream is named `cdc/<vhost>`. The single-database path uses
//! the one name `cdc/default`, registering an exact handler for it;
//! replicas dial the primary's channel address and open a stream of that
//! name. Per-site clustered mode (`[db.sqlite.replication] per_site`)
//! instead registers a *prefix* handler and runs one `cdc/<site>` stream
//! per virtual host — see the per-site section further down. The
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
//! Capture is switched on per session just before that session's first
//! statement that is not a plain `SELECT` (litewire#51), not when the session
//! opens: the capture pragma is a write transaction, and running it at
//! connect made every new session — readers included — wait on another
//! connection's write lock. Every write is still captured.
//!
//! **A replica's wire frontend is read-write, and writes made against
//! it are NOT replicated anywhere.** litewire has no read-only frontend
//! mode, so v1 cannot enforce this; the replica logs a warning at
//! startup. Point application traffic at the primary.
//!
//! # Failover, and why watermarks are scoped to a *log identity*
//!
//! The sqlite election machinery (`ephpm_cluster::SqliteElection`) decides
//! who is primary. On role change the previous role's driver task is
//! aborted and a driver for the new role is spawned, so a flapping
//! election does not accumulate drivers.
//!
//! `turso_cdc.change_id` is a **per-database** sequence: every node's local
//! CDC log numbers from 1, and a replica's log stays empty because
//! [`apply_batch`] bypasses capture. A watermark is therefore only
//! meaningful *against the specific log it was read from*. Treating it as
//! cluster-global was issue #315: after a failover, a survivor would
//! subscribe to the promoted node with the watermark it had accumulated
//! against the dead primary's log (say 5), the promoted node's own writes
//! landed at `change_id` 1‑2 — below that cursor — and were **silently
//! never shipped**. Not an in-flight window: unbounded, silent write loss
//! after any failover, with the replica reporting itself caught up.
//!
//! The fix gives every node's CDC log a persistent random identity
//! (`__ephpm_cdc_log_id`, generated once per database file). The primary
//! announces it as the first frame of every replication stream
//! ([`Frame::Hello`]) and in the snapshot header. The replica tracks which
//! log its litewire watermark refers to (`__ephpm_cdc_source`) plus a
//! per-log history (`__ephpm_cdc_watermarks`); on connecting to a
//! *different* log than the one it was following, it saves the old log's
//! cursor, re-seeds litewire's watermark table to whatever it has applied
//! from the new log (usually 0), and subscribes from there — so a promoted
//! node's writes can never land below the cursor. See
//! [`resolve_subscribe_watermark`].
//!
//! **The remaining divergence window is the same class as sqld async
//! replication:** a former primary that had unshipped batches at the
//! moment it died has lost those writes (they exist on it and nowhere
//! else), and writes made directly against a replica's wire frontend are
//! never replicated. Both are documented caveats of the experimental
//! clustered mode, not silent — the first is bounded by what was in
//! flight, the second warns at startup.
//!
//! The Hello handshake and the snapshot `log_id` field are **wire protocol
//! changes** (v0.7.2). Mixed-version clustered nodes are not supported;
//! a replica that never receives a Hello fails its stream loudly after a
//! timeout instead of hanging.
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
//! - turso 0.7.0 (still true of 0.8.2) exposes no online-backup, `serialize`, or local
//!   `checkpoint` API (verified against `turso = 0.7.0` and re-checked on 0.8.2
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
//!    the same table [`apply_batch`] maintains) and record the header's
//!    `log_id` as this replica's source log (issue #315). This all
//!    completes before the litewire wire frontends start serving, so a
//!    client read never observes partial snapshot state.
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
//! [`ephpm_cluster::stream_type::SNAPSHOT_PREFIX`]. The single-database
//! path uses only `snapshot/default` (matching `cdc/default`); per-site
//! clustered mode uses one `snapshot/<site>` per virtual host.
//!
//! ## Scope guards
//!
//! - No CDC-log truncation handling: the path relies on the log growing
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
//! ## Engine work stays off the async workers
//!
//! Turso's futures run each step's page I/O and the commit `fsync`
//! synchronously inside `poll`, yielding only between steps, so a tokio
//! worker polling one is held for every step as long as the disk takes.
//! Enough of them (applies, dumps, head samples across dozens of sites)
//! and no worker is left to turn the runtime's I/O and timer drivers: the
//! node stopped answering HTTP and handled `SIGTERM` up to 53 s late under
//! per-site replication load. Every engine call this module makes on a raw
//! connection therefore runs under [`off_runtime`] (litewire's
//! `offload::engine`, `block_in_place` on a multi-thread runtime), exactly
//! inside the WAL-gate permit scope that already marked it; litewire does
//! the same for `poll_batch`, `apply_batch` and its sessions. Network waits
//! stay outside.
//!
//! An offloaded scope cannot be interrupted from outside: its nested
//! `block_on` never yields to the scheduler, so a `select!` arm (the per-site
//! replica loop's role-change arm) or a task abort takes effect only once the
//! scope (say, a snapshot install) has finished.
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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use dashmap::DashMap;
use ephpm_cluster::{ChannelStream, ElectedRole, IncomingStream, NodeInfo};
use ephpm_config::SqliteConfig;
use litewire::litewire_turso::Turso;
use litewire::litewire_turso::cdc::{CdcRow, CdcTailer, TxnBatch, apply_batch, read_watermark};
use litewire::litewire_turso::offload::engine as off_runtime;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::handoff_gate::{HandoffGate, SiteServeState};
use crate::wal_gate::{self, WalGateSample, WalGateStatsSource};
use crate::{tracked_backend, turso_cdc_metrics as cdc_metrics};

/// Logical database name used by the **single-database** clustered path.
///
/// That path replicates exactly one database, so its CDC and snapshot
/// streams are `"cdc/default"` / `"snapshot/default"`. The per-site
/// clustered path ([`start_clustered_per_site_turso`]) uses the vhost's
/// canonical site key in place of `"default"`.
const DEFAULT_SITE: &str = "default";

/// CDC stream-type string for `site`: `"cdc/<site>"`.
///
/// Built from [`ephpm_cluster::stream_type::CDC_PREFIX`] so the
/// "lives under the CDC prefix" invariant the owner-side prefix handler
/// relies on holds by construction.
#[must_use]
fn cdc_stream_type(site: &str) -> String {
    format!("{}{site}", ephpm_cluster::stream_type::CDC_PREFIX)
}

/// Snapshot stream-type string for `site`: `"snapshot/<site>"`. A cold
/// replica dials this once to fetch the owner's base state before
/// subscribing to the site's CDC stream.
#[must_use]
fn snapshot_stream_type(site: &str) -> String {
    format!("{}{site}", ephpm_cluster::stream_type::SNAPSHOT_PREFIX)
}

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

/// Nominal pause between snapshot-bootstrap attempts, and the cap on the
/// mgmt-open backoff. Every *sleep* on it is jittered
/// ([`ephpm_cluster::reconnect::jittered`]); the replica stream reconnect loops
/// do not use it at all — they back off exponentially with jitter
/// ([`ephpm_cluster::ReconnectBackoff`]), because a cohort of streams retrying
/// on one fixed delay is what turned an owner stall into a self-sustaining
/// reconnect storm (stress report F1).
const REPLICA_RECONNECT_DELAY: Duration = ephpm_cluster::ReconnectBackoff::REPLICA_BASE;

/// Base backoff between per-site mgmt-factory open retries when the file's
/// write lock is held by the cdc-on wire factory. Grows linearly per attempt,
/// capped at [`REPLICA_RECONNECT_DELAY`]. See [`acquire_site_mgmt`].
const MGMT_OPEN_BACKOFF: Duration = Duration::from_millis(100);

/// Emit a single visible warning once a per-site mgmt open has retried this
/// many times without succeeding — a lock that outlives ~20 attempts is worth
/// surfacing, but per-attempt warnings would bury the log.
const MGMT_LOCK_WARN_AFTER: u32 = 20;

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

/// How long a replica waits for the primary's [`Frame::Hello`] before
/// declaring the stream broken. Generous — the Hello is the primary's
/// first write after accepting the stream — but bounded, so a peer that
/// never sends one (a pre-v0.7.2 primary, or a wedged process) fails the
/// stream loudly instead of hanging it forever.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an auto-role node waits at startup for the election to
/// resolve (an existing primary's claim to arrive, or — on a fresh
/// cluster — its own post-grace self-election) before giving up and
/// starting unresolved. Must comfortably exceed the election's startup
/// grace (~15s) plus gossip convergence; see `determine_role` /
/// `wait_for_initial_role`.
const RESOLVE_ROLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Frame types carried on the CDC replication wire.
///
/// The stream is not symmetric: [`Frame::Subscribe`] only ever travels
/// replica → primary and is only ever valid as the first frame;
/// [`Frame::Batch`] and [`Frame::Ping`] only ever travel primary →
/// replica. They share one enum so both directions share one codec.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Frame {
    /// **Primary → replica, first frame on every stream.** Identifies
    /// which CDC log the primary is about to serve (see the module docs
    /// on log identity, issue #315). The replica resolves its watermark
    /// *for that log* before subscribing, so a cursor accumulated against
    /// a dead primary's log can never be applied to a promoted node's
    /// (differently-numbered) log.
    Hello { log_id: String },
    /// **Replica → primary, sent after [`Frame::Hello`].** The replica's
    /// applied watermark **in the log the Hello named**. The primary
    /// starts this subscriber's tailer at exactly this `change_id`, so
    /// nothing between the watermark and "now" can be skipped — which is
    /// what makes a reconnect (or a warm restart) safe without a
    /// snapshot.
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
    // Shared "am I the primary?" view exposed at `/_ephpm/primary` for
    // active-passive LB routing. This is the SAME `AtomicBool` the router
    // reads: the role-change watcher below flips it, so the health endpoint
    // tracks the live election result with a single relaxed load. It is the
    // canonical primary flag for this path — the stream handlers and head
    // sampler read it too.
    primary_view: Arc<AtomicBool>,
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
    // Fail startup now if a wire listener cannot exist, rather than from the
    // detached litewire task spawned after election and bootstrap (see
    // `probe_sqlite_listeners`).
    crate::probe_sqlite_listeners(&crate::sqlite_frontend_addrs(&sqlite_config.proxy))
        .await
        .context("cannot start the embedded database's wire listeners")?;

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

    // Mgmt factory: used by the per-subscriber tailers and the snapshot
    // dumper on the primary, and by the apply loop on the replica. Never
    // opts into CDC-on-connect — the tailer reads turso_cdc explicitly,
    // and the applier's writes must NOT be re-captured (that would make
    // a replica echo the primary's changes into its own CDC log).
    //
    // Opened BEFORE the election (moved up from below) so this node's CDC
    // log identity is available to the election: the primary claim carries
    // that identity, and the fast-restart reclaim must match on it (issue
    // #344).
    //
    // WAL checkpoint gate: installed on BOTH factories on this file. The
    // gate itself is per file (litewire keys it by canonical path), so the
    // two share one lock and one set of counters; what each factory needs
    // its own opt-in for is (a) the wire factory's sessions taking permits —
    // without that the gate could never reach a reader-free instant — and
    // (b) `wal_gate_permit()` answering on the mgmt factory, which is what
    // the raw-connection apply/snapshot/head-sampler paths below hold. Two
    // maintenance tasks on one file are harmless by litewire's contract (the
    // attempt is serialized on the exclusive side).
    wal_gate::log_active();
    let mgmt_factory = Arc::new(
        wal_gate::builder_with_gate(Turso::builder(db_path))
            .build()
            .await
            .with_context(|| format!("failed to open mgmt Turso factory at {db_path}"))?,
    );
    wal_gate::DatabaseProbe::register(wal_gate::DEFAULT_SITE_LABEL, db_path, &mgmt_factory);

    // Establish this database's persistent CDC log identity (issue #315)
    // BEFORE the election runs. Done on EVERY node, not just the primary:
    // any node can be promoted later, and the identity must already exist
    // when the first subscriber dials in. Written through the mgmt
    // connection so it is never captured into the CDC log itself. Passing it
    // to the election is what lets a node whose database was wiped/replaced
    // across a restart (fresh identity) refuse to fast-reclaim its own stale
    // primary claim (issue #344).
    let local_log_id = {
        let conn =
            mgmt_factory.raw_connection().context("CDC: open mgmt connection for log identity")?;
        off_runtime(ensure_log_id(&conn)).await.context("CDC: establish local log identity")?
    };
    tracing::info!(log_id = %local_log_id, "CDC: local log identity established");

    let (initial_role, role_rx) =
        determine_role(sqlite_config, cluster, channel_advertise, local_log_id.clone()).await?;

    // In auto mode the initial role is deliberately unresolved until the
    // election has actually seen the cluster (issue #314: deciding
    // "primary" before gossip converges is how a joining node stole the
    // role and skipped its snapshot bootstrap). Wait — bounded — for the
    // election to deliver a definite role before wiring anything up, so a
    // cold node that turns out to be a replica still takes the snapshot
    // bootstrap path below.
    let (initial_role, role_rx) = match role_rx {
        Some(mut rx) => {
            let resolved = wait_for_initial_role(initial_role, &mut rx).await;
            (resolved, Some(rx))
        }
        None => (initial_role, None),
    };

    // Tracks whether this node is currently the primary. Read by the
    // stream handlers, written by the role-change watcher. `Relaxed` is
    // right here: a handler that reads a stale value for a few
    // microseconds around a role flip either serves one extra stream
    // (which the new primary's election heartbeat will supersede) or
    // refuses one that the replica retries two seconds later.
    // Reuse the router-shared view as the canonical primary flag rather than a
    // private `AtomicBool`, so `/_ephpm/primary` and the stream handlers agree.
    // Seed it with the resolved initial role; the role-change watcher below
    // keeps it current. A standalone node never reaches here, so its constant
    // `true` default is untouched.
    let is_primary = primary_view;
    is_primary.store(matches!(initial_role, Role::Primary), Ordering::Relaxed);

    // Wire factory: served to litewire. Capture is enabled on EVERY
    // node, not just the one that booted as primary: `Turso::builder`
    // fixes `enable_cdc_on_connect` for the life of the factory, and
    // that factory is moved into litewire below. A node promoted later
    // by the election could therefore never start capturing, and every
    // write it served after promotion would be invisible to replicas.
    // Capturing everywhere costs a `turso_cdc` row per local write on
    // replicas; shipping is gated by role at serve time instead.
    let wire_factory = wal_gate::builder_with_gate(Turso::builder(db_path))
        .enable_cdc_on_connect(true)
        .build()
        .await
        .with_context(|| format!("failed to open wire Turso factory at {db_path}"))?;

    // Mgmt factory and this node's CDC log identity are established above,
    // before the election, so the primary claim can carry the identity
    // (issue #344).

    let max_snapshot_bytes = sqlite_config.replication.max_snapshot_bytes;

    // Seed the zero-valued CDC series before anything can record. An
    // operator scraping a freshly booted node must be able to tell
    // "replication is on and idle" from "this build has no CDC
    // instrumentation", and an absent counter cannot say which.
    cdc_metrics::init();
    // One database, labelled `site="default"`: its subscriber gauge reads 0
    // from boot rather than appearing on the first attach.
    cdc_metrics::track_shipping(DEFAULT_SITE);

    // Publish MAX(change_id) on a timer while primary. See
    // HEAD_SAMPLE_INTERVAL for why this cannot be folded into the
    // shipping path.
    spawn_head_sampler(Arc::clone(&mgmt_factory), Arc::clone(&is_primary), handles);

    // Register the primary-side snapshot handler NOW, before anything
    // dials, so a peer that comes up as a cold replica can reach us as
    // soon as we win an election. Serving is gated on actually being
    // primary — a replica must never hand a peer a full logical dump of
    // the database.
    spawn_snapshot_server(
        channel,
        Arc::clone(&mgmt_factory),
        Arc::clone(&is_primary),
        local_log_id.clone(),
        handles,
    );

    // Register the CDC subscriber handler NOW even if we start as
    // replica, so it is already in place after a promotion. Each
    // inbound stream announces the watermark it has applied and gets
    // its own tailer starting there.
    let mut cdc_streams = channel.register_exact(cdc_stream_type(DEFAULT_SITE));
    let subs_mgmt = Arc::clone(&mgmt_factory);
    let subs_is_primary = Arc::clone(&is_primary);
    let subs_log_id = local_log_id.clone();
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
            let log_id = subs_log_id.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_subscriber(stream, &mgmt, &log_id).await {
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
    match &initial_role {
        Role::Replica { primary_addr } => {
            maybe_bootstrap_cold_replica(&mgmt_factory, *primary_addr, channel, max_snapshot_bytes)
                .await?;
        }
        Role::Awaiting => {
            // Only reachable when the election never resolved within
            // RESOLVE_ROLE_TIMEOUT (e.g. every seed is down). Starting
            // anyway matches the pre-#314 behaviour for an unresolved
            // replica, minus the bogus 127.0.0.1:0 dials — but a COLD node
            // on this path never fetched a snapshot, so if it later
            // becomes a replica it will miss any pre-CDC rows. Loud, not
            // silent:
            tracing::warn!(
                "CDC: starting with an UNRESOLVED replication role (no primary elected \
                 within the startup window). If this node's database is cold and it later \
                 becomes a replica, rows written before the primary enabled CDC will be \
                 missing here — restart this node once the cluster has a primary."
            );
        }
        Role::Primary => {}
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
    Replica {
        primary_addr: SocketAddr,
    },
    /// The election has not resolved to a definite role yet: no primary
    /// claim is visible (or the claim's address was unusable). There is
    /// nothing to drive in this state — no primary to dial, nothing to
    /// serve — so its driver is a no-op and the role watcher swaps it
    /// out when the election delivers a real role.
    Awaiting,
}

fn elected_to_role(elected: ephpm_cluster::ElectedRole) -> Role {
    match elected {
        ephpm_cluster::ElectedRole::Primary => Role::Primary,
        ephpm_cluster::ElectedRole::Replica { primary_grpc_url } => {
            // An empty URL is the election's explicit "no primary known
            // yet" state (fresh cluster pre-election, or an incumbent's
            // claim not yet gossiped over). Not an error — just nothing
            // to dial yet.
            if primary_grpc_url.is_empty() {
                return Role::Awaiting;
            }
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
                    // Treat as unresolved; the election loop re-publishes
                    // and the role watcher will pick up a usable address.
                    // (This used to fall back to dialing a bogus
                    // 127.0.0.1:0 forever.)
                    Role::Awaiting
                }
            }
        }
    }
}

/// Wait — bounded by [`RESOLVE_ROLE_TIMEOUT`] — for the election to turn
/// an [`Role::Awaiting`] initial role into a definite one.
///
/// Startup blocks on this deliberately. The cold-replica snapshot
/// bootstrap runs before the wire frontends come up, and it needs to know
/// *whether* this node is a replica and *whom* to dial; deciding either
/// before the election has seen the cluster is issue #314. On timeout the
/// node starts unresolved (the caller warns) and the role watcher takes
/// over when the election eventually resolves.
async fn wait_for_initial_role(
    initial: Role,
    rx: &mut tokio::sync::watch::Receiver<ephpm_cluster::ElectedRole>,
) -> Role {
    let mut role = initial;
    let deadline = tokio::time::Instant::now() + RESOLVE_ROLE_TIMEOUT;
    loop {
        if !matches!(role, Role::Awaiting) {
            return role;
        }
        match tokio::time::timeout_at(deadline, rx.changed()).await {
            Ok(Ok(())) => {
                role = elected_to_role(rx.borrow_and_update().clone());
            }
            // Election task gone — nothing more will arrive.
            Ok(Err(_)) => return role,
            Err(_) => {
                tracing::warn!(
                    timeout_secs = RESOLVE_ROLE_TIMEOUT.as_secs(),
                    "CDC: election did not resolve a primary within the startup window; \
                     starting with an unresolved role"
                );
                return role;
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
    local_log_id: String,
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
                local_log_id,
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
            tracing::info!(
                stream = %cdc_stream_type(DEFAULT_SITE),
                "CDC primary: serving replication streams"
            );
        }
        Role::Replica { primary_addr } => {
            if let Err(e) = run_replica(mgmt, primary_addr, channel).await {
                tracing::error!("CDC replica loop exited: {e:#}");
            }
        }
        Role::Awaiting => {
            // Nothing to drive until the election resolves; the role
            // watcher replaces this driver when it does.
            tracing::info!("CDC: no primary elected yet; awaiting election result");
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
            // Raw connections get no busy timeout, so a read that lands
            // inside the gate's exclusive window would fail outright;
            // holding the shared side for the one statement makes that
            // collision impossible (and costs a reader slot for microseconds).
            let head = {
                let _permit = mgmt.wal_gate_permit().await;
                off_runtime(current_max_change_id(&conn)).await
            };
            match head {
                Ok(head) => cdc_metrics::observe_head(DEFAULT_SITE, head),
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
/// The first frame on the wire is ours: [`Frame::Hello`], naming
/// `local_log_id` — the persistent identity of the CDC log this stream
/// will serve — so the replica can resolve its watermark for *this* log
/// before announcing it (issue #315). The replica answers with
/// [`Frame::Subscribe`], carrying the `change_id` it has already applied
/// from this log. We open a [`CdcTailer`] at exactly that cursor and pump
/// batches into the stream from there.
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
    stream: S,
    mgmt: &Turso,
    local_log_id: &str,
) -> anyhow::Result<()> {
    serve_site_subscriber(stream, mgmt, local_log_id, DEFAULT_SITE).await
}

/// [`serve_subscriber`] for one named site: the shipping metrics (subscriber
/// count, head, shipped cursor, lag) are recorded under `site`, so the
/// per-site clustered path's sites never mix their logs' positions.
async fn serve_site_subscriber<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut stream: S,
    mgmt: &Turso,
    local_log_id: &str,
    site: &str,
) -> anyhow::Result<()> {
    write_frame(&mut stream, &Frame::Hello { log_id: local_log_id.to_string() })
        .await
        .context("send hello frame")?;
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
    let subscriber = cdc_metrics::attach_subscriber(site, from);

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
                cdc_metrics::observe_head(site, tailer.cursor());
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

/// Context marker attached to watermark-resolution failures inside
/// [`subscribe_and_consume`], so [`run_replica`] can attribute them to the
/// `watermark_error` connect outcome instead of the generic
/// `stream_error` (anyhow context types are downcastable).
#[derive(Debug, Clone, Copy)]
struct WatermarkResolution;

impl std::fmt::Display for WatermarkResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("local watermark resolution failed")
    }
}

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
    // arrive only through apply_batch, keyed by monotonic watermark. The
    // factory stays in scope alongside the connection: every apply takes
    // its WAL-gate permit (see `consume_frames`).
    let apply_conn = mgmt.raw_connection()?;

    tracing::warn!(
        primary = %primary_addr,
        "CDC replica: the local wire frontends accept WRITES and litewire has no read-only \
         mode, but nothing replicates a write made against a replica — it will diverge from \
         the primary and be overwritten or shadowed by replayed batches. Point application \
         traffic at the primary."
    );

    let mut backoff = ephpm_cluster::ReconnectBackoff::replica_stream();
    loop {
        let attempt_started = tokio::time::Instant::now();
        match channel.dial(primary_addr, &cdc_stream_type(DEFAULT_SITE)).await {
            Ok(mut stream) => {
                // Watermark resolution happens INSIDE subscribe_and_consume,
                // after the primary's Hello: the cursor is only meaningful
                // against the specific log the Hello names (issue #315), so
                // it cannot be read up front any more.
                match subscribe_and_consume(&mut stream, &mgmt, &apply_conn).await {
                    Ok(()) => {
                        cdc_metrics::record_connect_outcome("closed");
                        tracing::info!("CDC replica: primary closed stream cleanly");
                    }
                    Err(e) => {
                        // Failures of the local watermark bookkeeping keep
                        // their own outcome label — they mean "this node
                        // cannot ground a cursor", which an operator debugs
                        // very differently from a broken network stream.
                        if e.downcast_ref::<WatermarkResolution>().is_some() {
                            cdc_metrics::record_connect_outcome("watermark_error");
                        } else {
                            cdc_metrics::record_connect_outcome("stream_error");
                        }
                        tracing::warn!("CDC replica stream error: {e:#}");
                    }
                }
            }
            Err(e) => {
                cdc_metrics::record_connect_outcome("dial_error");
                tracing::debug!(primary = %primary_addr, "CDC replica dial failed: {e:#}");
            }
        }
        // A subscription that stayed up for a while resets the failure run;
        // a dial or stream that keeps failing backs off (jittered) instead
        // of re-dialing on a fixed beat.
        backoff.record_attempt(attempt_started.elapsed());
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

/// Read the primary's [`Frame::Hello`], resolve our watermark **for that
/// log**, announce it, then apply everything the primary sends.
///
/// Generic over the stream, and public, for the same reason
/// [`serve_subscriber`] is: the two-node e2e suites drive this exact
/// function so the replica-side metrics under test are the production
/// ones. Not part of a stable API.
///
/// `factory` is the Turso factory `apply_conn` came from. `apply_conn` is a
/// raw connection — no busy timeout — so every statement run on it here is
/// wrapped in the factory's WAL-gate permit (`Turso::wal_gate_permit`,
/// `None` when the gate is off): a `BEGIN IMMEDIATE` colliding with a
/// truncating checkpoint would otherwise fail the stream and cost a
/// reconnect. The permit is held for one batch at a time and never across
/// an await that waits on another permit holder.
///
/// # Errors
///
/// Returns an error if the Hello does not arrive within
/// [`HELLO_TIMEOUT`] (protocol mismatch — mixed-version clusters are
/// unsupported), if the watermark cannot be resolved for the announced
/// log, if the subscribe frame cannot be sent, if the primary sends a
/// malformed or wrong-direction frame, or if `apply_batch` fails.
pub async fn subscribe_and_consume<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    factory: &Turso,
    apply_conn: &litewire::litewire_turso::TursoConnection,
) -> anyhow::Result<()> {
    subscribe_and_consume_site(stream, factory, apply_conn, DEFAULT_SITE).await
}

/// [`subscribe_and_consume`] for one named site: the applied-watermark gauge
/// is recorded under `site`.
async fn subscribe_and_consume_site<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    factory: &Turso,
    apply_conn: &litewire::litewire_turso::TursoConnection,
    site: &str,
) -> anyhow::Result<()> {
    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_frame(stream)).await.map_err(|_| {
        anyhow::anyhow!(
            "primary sent no Hello within {HELLO_TIMEOUT:?} — wire protocol mismatch? \
             mixed-version clustered nodes are not supported"
        )
    })?;
    let source_log_id = match hello.context("read hello frame")? {
        Frame::Hello { log_id } => log_id,
        other => anyhow::bail!("expected Hello as the first CDC frame, got {other:?}"),
    };
    anyhow::ensure!(!source_log_id.is_empty(), "primary announced an empty log id");

    let watermark = {
        let _permit = factory.wal_gate_permit().await;
        off_runtime(resolve_subscribe_watermark(apply_conn, &source_log_id))
            .await
            .with_context(|| format!("resolve watermark for source log {source_log_id}"))
            .context(WatermarkResolution)?
    };

    write_frame(stream, &Frame::Subscribe { from_change_id: watermark })
        .await
        .context("send subscribe frame")?;
    // Publish what we are resuming from before a single batch lands, so
    // the watermark gauge reflects durable local state rather than only
    // what this process has applied since it started.
    cdc_metrics::record_applied_watermark(site, watermark);
    tracing::info!(
        from_change_id = watermark,
        source_log = %source_log_id,
        "CDC replica: subscribed"
    );
    consume_frames(stream, factory, apply_conn, site).await
}

async fn consume_frames<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    factory: &Turso,
    apply_conn: &litewire::litewire_turso::TursoConnection,
    site: &str,
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
                // Permit scoped to the one apply: taken after the frame is
                // fully read (never held across the stream read) and
                // released before the next frame is awaited.
                let applied = {
                    let _permit = factory.wal_gate_permit().await;
                    apply_batch(apply_conn, &batch).await
                };
                match applied {
                    Ok(()) => {
                        // Advances the watermark gauge to `change_id`,
                        // which is exactly what the local watermark table
                        // now holds.
                        cdc_metrics::record_batch_applied(
                            site,
                            change_id,
                            row_count,
                            started.elapsed(),
                        );
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
            Frame::Hello { .. } => {
                anyhow::bail!("primary sent a second Hello frame; only valid as the first frame");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CDC log identity + per-log watermarks (issue #315).
//
// `turso_cdc.change_id` is a per-database sequence, so a watermark is only
// meaningful against the specific log it was read from. Three tiny local
// tables make that explicit:
//
//   __ephpm_cdc_log_id     — this database's own log identity (random,
//                            generated once, announced in Hello/snapshot).
//   __ephpm_cdc_source     — which log litewire's __litewire_cdc_watermark
//                            currently refers to.
//   __ephpm_cdc_watermarks — per-log history: the cursor we had reached in
//                            each log we ever followed, saved when we
//                            switch away and restored if we switch back
//                            (a re-promotion cycle).
//
// All three are written through the mgmt/apply connection (capture off, so
// they never enter the CDC log), are `__`-prefixed (so the snapshot dump
// and the cold-start check both exclude them), and mirror litewire's
// watermark-table shape conventions.
// ---------------------------------------------------------------------------

/// Table holding this database's persistent CDC log identity.
const LOG_ID_TABLE: &str = "__ephpm_cdc_log_id";

/// Table recording which source log [`WATERMARK_TABLE`] currently refers to.
const SOURCE_LOG_TABLE: &str = "__ephpm_cdc_source";

/// Table holding the per-source-log watermark history.
const LOG_WATERMARKS_TABLE: &str = "__ephpm_cdc_watermarks";

/// Read this database's persistent CDC log identity, generating and
/// storing a fresh random one if none exists yet.
///
/// The identity is per database *file*, not per node: wiping a node's
/// data directory yields a new identity, so peers' stale cursors for the
/// old log can never be misapplied to the rebuilt one.
///
/// Public so the cross-node integration tests can establish identities on
/// their fixture databases the exact way production does. Not part of a
/// stable API.
///
/// # Errors
///
/// Returns an error if the table cannot be created, read, or seeded.
pub async fn ensure_log_id(conn: &turso::Connection) -> anyhow::Result<String> {
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {LOG_ID_TABLE} (\
                id INTEGER PRIMARY KEY CHECK (id = 0), \
                log_id TEXT NOT NULL)"
        ),
        (),
    )
    .await
    .context("cdc: create log-id table")?;

    if let Some(existing) = read_single_text(conn, LOG_ID_TABLE, "log_id").await? {
        return Ok(existing);
    }

    let fresh = generate_log_id()?;
    // INSERT OR IGNORE: if two connections race here, the first write
    // wins and the re-read below returns the winner for both.
    conn.execute(
        &format!("INSERT OR IGNORE INTO {LOG_ID_TABLE} (id, log_id) VALUES (0, ?)"),
        (fresh,),
    )
    .await
    .context("cdc: seed log id")?;
    read_single_text(conn, LOG_ID_TABLE, "log_id")
        .await?
        .context("cdc: log id missing immediately after seeding it")
}

/// 32 hex chars from the OS CSPRNG — 128 bits, no coordination needed.
fn generate_log_id() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf)
        .map_err(|e| anyhow::anyhow!("cdc: OS randomness unavailable for log id: {e}"))?;
    use std::fmt::Write as _;
    let mut s = String::with_capacity(32);
    for b in buf {
        write!(s, "{b:02x}").expect("writing to a String is infallible");
    }
    Ok(s)
}

/// Read the single row-0 text value of a `(id=0, <col>)`-shaped table.
/// `None` when the table or row is absent.
async fn read_single_text(
    conn: &turso::Connection,
    table: &str,
    col: &str,
) -> anyhow::Result<Option<String>> {
    let mut stmt = match conn.prepare(&format!("SELECT {col} FROM {table} WHERE id = 0")).await {
        Ok(s) => s,
        Err(_) => return Ok(None), // table absent
    };
    let mut rows = stmt.query(()).await.with_context(|| format!("cdc: query {table}"))?;
    match rows.next().await.with_context(|| format!("cdc: read {table}"))? {
        Some(row) => match row.get_value(0).with_context(|| format!("cdc: {table} value"))? {
            turso::Value::Text(s) => Ok(Some(s)),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

/// Record which source log [`WATERMARK_TABLE`] refers to from now on.
async fn set_source_log(conn: &turso::Connection, log_id: &str) -> anyhow::Result<()> {
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {SOURCE_LOG_TABLE} (\
                id INTEGER PRIMARY KEY CHECK (id = 0), \
                log_id TEXT NOT NULL)"
        ),
        (),
    )
    .await
    .context("cdc: create source-log table")?;
    conn.execute(
        &format!("INSERT OR REPLACE INTO {SOURCE_LOG_TABLE} (id, log_id) VALUES (0, ?)"),
        (log_id,),
    )
    .await
    .context("cdc: write source log")?;
    Ok(())
}

/// The watermark we had reached in `log_id`, from the per-log history.
/// `0` when we have never followed that log.
async fn read_log_watermark(conn: &turso::Connection, log_id: &str) -> anyhow::Result<i64> {
    let mut stmt = match conn
        .prepare(&format!("SELECT applied_change_id FROM {LOG_WATERMARKS_TABLE} WHERE log_id = ?"))
        .await
    {
        Ok(s) => s,
        Err(_) => return Ok(0), // table absent
    };
    let mut rows = stmt.query((log_id,)).await.context("cdc: query per-log watermark")?;
    match rows.next().await.context("cdc: read per-log watermark")? {
        Some(row) => match row.get_value(0).context("cdc: per-log watermark value")? {
            turso::Value::Integer(i) => Ok(i),
            _ => Ok(0),
        },
        None => Ok(0),
    }
}

/// Upsert the per-log history entry for `log_id`.
async fn store_log_watermark(
    conn: &turso::Connection,
    log_id: &str,
    watermark: i64,
) -> anyhow::Result<()> {
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {LOG_WATERMARKS_TABLE} (\
                log_id TEXT PRIMARY KEY, \
                applied_change_id INTEGER NOT NULL)"
        ),
        (),
    )
    .await
    .context("cdc: create per-log watermark table")?;
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO {LOG_WATERMARKS_TABLE} (log_id, applied_change_id) \
             VALUES (?, ?)"
        ),
        (log_id, watermark),
    )
    .await
    .context("cdc: write per-log watermark")?;
    Ok(())
}

/// Resolve the watermark to subscribe with, given the log the primary
/// announced in its [`Frame::Hello`]. This is the issue #315 fix's core:
///
/// - **Same log as last time** → litewire's own watermark table is
///   authoritative (it advances transactionally with every applied batch).
/// - **Different log** (we are following a *new* primary after a
///   failover) → save the old log's cursor into the history, restore
///   whatever cursor we had for the new log (0 if we never followed it —
///   a promoted ex-replica's log is new to us), re-seed litewire's table
///   to that value so [`apply_batch`]'s idempotency guard is aligned with
///   the new sequence, and record the switch.
/// - **No source recorded** (a database from before this fix, or one
///   populated without a snapshot) → adopt litewire's current watermark
///   as belonging to the announced log and record that. This matches the
///   pre-fix behaviour for the one legacy transition; every switch after
///   it is tracked.
///
/// # Errors
///
/// Returns an error if any of the local bookkeeping reads/writes fail —
/// the caller must fail the stream rather than subscribe from a cursor it
/// could not ground.
pub async fn resolve_subscribe_watermark(
    conn: &turso::Connection,
    source_log_id: &str,
) -> anyhow::Result<i64> {
    match read_single_text(conn, SOURCE_LOG_TABLE, "log_id").await? {
        Some(current) if current == source_log_id => {
            read_watermark(conn).await.map_err(|e| anyhow::anyhow!("read local watermark: {e}"))
        }
        Some(previous) => {
            let old_wm = read_watermark(conn)
                .await
                .map_err(|e| anyhow::anyhow!("read local watermark: {e}"))?;
            store_log_watermark(conn, &previous, old_wm).await?;
            let new_wm = read_log_watermark(conn, source_log_id).await?;
            seed_watermark(conn, new_wm).await.context("re-seed litewire watermark")?;
            set_source_log(conn, source_log_id).await?;
            tracing::warn!(
                previous_log = %previous,
                previous_watermark = old_wm,
                new_log = %source_log_id,
                new_watermark = new_wm,
                "CDC replica: switching source log (failover) — watermark re-seeded for \
                 the new primary's log so its writes cannot land below our cursor"
            );
            Ok(new_wm)
        }
        None => {
            let wm = read_watermark(conn)
                .await
                .map_err(|e| anyhow::anyhow!("read local watermark: {e}"))?;
            store_log_watermark(conn, source_log_id, wm).await?;
            set_source_log(conn, source_log_id).await?;
            Ok(wm)
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
    /// Identity of the CDC log `watermark` belongs to — the serving
    /// primary's [`LOG_ID_TABLE`] value (issue #315). The replica records
    /// it as its source log so a later failover to a *different* log is
    /// detected and the cursor is not misapplied.
    log_id: String,
    /// **"I hold no copy of this site."** Set by a per-site node asked for a
    /// `snapshot/<site>` it has no database file for: it answers with this
    /// header and an immediate end marker instead of creating an empty
    /// database just to dump it (see [`serve_no_copy`]).
    ///
    /// Backward compatible both ways. Omitted from the wire when `false`, so a
    /// normal header is byte-identical to an older node's; an older receiver
    /// ignores the unknown field and sees an empty dump, which every receiver
    /// already treats as carrying no data. And an older *server* never sets it
    /// but answers an unknown site with an empty dump, which the ownership
    /// handoff classifies the same way ([`snapshot_declares_a_table`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    no_copy: bool,
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
    local_log_id: String,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let mut snapshot_streams = channel.register_exact(snapshot_stream_type(DEFAULT_SITE));
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
            let log_id = local_log_id.clone();
            tokio::spawn(async move {
                match serve_snapshot(stream, &mgmt, &log_id).await {
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
pub async fn serve_snapshot(
    mut stream: ChannelStream,
    mgmt: &Turso,
    local_log_id: &str,
) -> anyhow::Result<i64> {
    // Recorded here rather than at the call site so a test that drives
    // `serve_snapshot` directly still exercises the instrumentation, and
    // so both outcomes are counted in one place.
    let started = std::time::Instant::now();
    match serve_snapshot_inner(&mut stream, mgmt, local_log_id).await {
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
async fn serve_snapshot_inner<S: AsyncWriteExt + Unpin>(
    stream: &mut S,
    mgmt: &Turso,
    local_log_id: &str,
) -> anyhow::Result<(i64, u64)> {
    let conn = mgmt.raw_connection().context("snapshot: open mgmt connection")?;
    // The dump is one long read transaction on a raw connection. Holding the
    // gate's shared side for its duration means a truncate attempt during a
    // bootstrap simply times out and retries (the documented long-transaction
    // behaviour) instead of a raw `BEGIN` failing against the exclusive
    // window. Released before the dump is written to the stream.
    let (watermark, dump) = {
        let _permit = mgmt.wal_gate_permit().await;
        off_runtime(produce_snapshot(&conn)).await.context("snapshot: produce dump")?
    };

    let header = SnapshotHeader {
        watermark,
        total_len: dump.len() as u64,
        log_id: local_log_id.to_string(),
        no_copy: false,
    };
    write_snapshot_header(stream, &header).await?;

    for chunk in dump.as_bytes().chunks(SNAPSHOT_CHUNK_TARGET) {
        write_snapshot_chunk(stream, chunk).await?;
    }
    // Zero-length end marker: signals a clean end of body.
    write_snapshot_chunk(stream, &[]).await?;
    stream.flush().await?;
    Ok((watermark, dump.len() as u64))
}

/// Answer a `snapshot/<site>` request for a site this node holds no copy of:
/// a [`SnapshotHeader`] with `no_copy` set, then the end marker.
///
/// The point is what this does **not** do. Serving a real snapshot resolves
/// the site's mgmt factory, and opening a Turso factory creates the database
/// file. A cold new owner asks every alive peer in turn
/// ([`settle_site_serving`]), so answering by dumping would leave an empty
/// `<site>.db` on every node it asked — and an empty file on disk then looks
/// like a site this node holds (`scan_existing_sites` starts a replication
/// driver for it at the next boot).
async fn serve_no_copy<S: AsyncWriteExt + Unpin>(stream: &mut S) -> anyhow::Result<()> {
    let header =
        SnapshotHeader { watermark: 0, total_len: 0, log_id: String::new(), no_copy: true };
    write_snapshot_header(stream, &header).await?;
    write_snapshot_chunk(stream, &[]).await?;
    stream.flush().await?;
    Ok(())
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

/// Apply an already-vetted snapshot dump, then seed the watermark to `N` so
/// the CDC tail resumes exactly past it, and record which log `N` belongs to
/// (issue #315) so a later failover to a different primary is detected as a
/// source switch.
///
/// Only ever called by [`install_snapshot`], **inside its transaction**: the
/// dump, the watermark and the source record commit together or not at all.
/// Run outside one, every `INSERT` of the dump was its own autocommit
/// transaction (one fsync per row: 177 s to bootstrap one 9,706-change site
/// on a loaded disk), and an apply cut short — a `database is locked` from
/// the shutdown checkpoint, a crash — left a partly populated database with
/// no watermark and no source record, which a restart then mistook for
/// local-origin data.
async fn apply_snapshot(
    conn: &turso::Connection,
    header: &SnapshotHeader,
    dump: &str,
) -> anyhow::Result<()> {
    // Execute the whole dump as a batch. It is self-consistent DDL+DML
    // captured under one read view on the primary.
    conn.execute_batch(dump).await.context("snapshot: apply dump")?;
    seed_watermark(conn, header.watermark).await.context("snapshot: seed watermark")?;
    store_log_watermark(conn, &header.log_id, header.watermark)
        .await
        .context("snapshot: record per-log watermark")?;
    set_source_log(conn, &header.log_id).await.context("snapshot: record source log")?;
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
    for_each_statement(dump, &mut check_statement_allowed)
}

/// Split `dump` into statements on unquoted, uncommented `;` and hand each one
/// (comments and whitespace included, exactly as written) to `visit`.
///
/// The scan is quote-aware — single-quoted strings with `''` escapes,
/// double-quoted identifiers with `""` escapes, and `X'..'` blobs are all just
/// quoted runs, so a `;` inside one is not a separator — and comment-aware, so
/// a `;` inside `--` or `/* */` is not one either.
///
/// Shared by [`validate_snapshot_dump`] (which allowlists each statement) and
/// [`snapshot_declares_a_table`] (which asks whether the dump carries any
/// schema at all), so the two can never disagree about where a statement ends.
///
/// # Errors
///
/// Returns an error on an unterminated quote or block comment, or whatever
/// `visit` returns for a statement it rejects.
fn for_each_statement(
    dump: &str,
    visit: &mut impl FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
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
                visit(&dump[start..i])?;
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    // Trailing text after the last `;` (blank, a comment, or a statement
    // written without a terminator).
    visit(&dump[start..])
}

/// Does `dump` declare at least one user table?
///
/// A snapshot is, by construction, `CREATE` DDL followed by
/// `INSERT OR REPLACE`s, so "no `CREATE TABLE` anywhere" means the serving node
/// dumped an **empty database**. That is the degenerate snapshot
/// [`snapshot_may_replace_local_data`] refuses to apply over live tenant data.
///
/// A malformed dump (unterminated quote/comment) reports `false`: it declares
/// nothing we can vouch for, and it is about to be rejected by
/// [`validate_snapshot_dump`] anyway.
fn snapshot_declares_a_table(dump: &str) -> bool {
    let mut found = false;
    let _ = for_each_statement(dump, &mut |stmt| {
        let trimmed = strip_leading_noise(stmt);
        let mut words = trimmed.split_ascii_whitespace();
        if words.next().is_some_and(|w| w.eq_ignore_ascii_case("CREATE"))
            && words.next().is_some_and(|w| w.eq_ignore_ascii_case("TABLE"))
        {
            found = true;
        }
        Ok(())
    });
    found
}

/// Gate on replacing a populated local database with a peer's snapshot.
///
/// # The invariant
///
/// A bootstrap **discards** the local user tables before applying the peer's
/// dump. That is only ever safe when the peer is actually authoritative for the
/// site, and a dump carrying no schema at all is positive evidence that it is
/// not — an empty database is what a node that has never held the site produces.
/// `validate_snapshot_dump("")` succeeds (an empty dump is trivially inside the
/// allowlist), so without this check the sequence "drop every user table, then
/// apply nothing" completed *successfully* and a tenant's data was gone.
///
/// Applying an empty snapshot over an already-empty database is fine and stays
/// allowed — that is a brand-new site on a fresh cluster.
///
/// # Errors
///
/// Returns an error when `local_has_data` and the dump declares no table, so
/// the caller retries later (by which time the real owner has the data, or
/// ownership has moved back) rather than destroying anything.
fn snapshot_may_replace_local_data(dump: &str, local_has_data: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        !local_has_data || snapshot_declares_a_table(dump),
        "refusing to bootstrap from a degenerate snapshot: the peer sent a dump with no user \
         tables while this node holds live data for the site. Applying it would discard that \
         data and replace it with nothing. This is what a node that has never held the site \
         serves — check that ownership moved to a node that actually has this database."
    );
    Ok(())
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

    let cold = {
        let _permit = mgmt.wal_gate_permit().await;
        off_runtime(local_db_is_cold(&conn))
            .await
            .context("snapshot bootstrap: cold-start check")?
    };
    if !cold {
        cdc_metrics::record_bootstrap("skipped");
        // A warm replica already has a watermark; publish it so the gauge
        // is populated before the first batch rather than reading 0 until
        // one arrives.
        if let Ok(wm) = read_watermark(&conn).await {
            cdc_metrics::record_applied_watermark(DEFAULT_SITE, wm);
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
        match fetch_and_apply_snapshot(mgmt, &conn, primary_addr, channel, max_snapshot_bytes).await
        {
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
                // Fixed attempt budget, so the nominal pause stays fixed (the
                // budget's total duration is what the error message promises)
                // — but jittered, so cold replicas do not retry in lockstep.
                tokio::time::sleep(ephpm_cluster::reconnect::jittered(REPLICA_RECONNECT_DELAY))
                    .await;
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
/// `factory` is the Turso factory `conn` came from; the apply step holds
/// its WAL-gate permit (a raw connection has no busy timeout to ride out a
/// checkpoint's exclusive window). The fetch — a network wait — runs
/// outside the permit.
///
/// # Errors
///
/// Returns an error if the dial, header/chunk read, or dump application
/// fails, if the peer announces or streams more than
/// `max_snapshot_bytes`, or if the dump contains a statement outside
/// the `CREATE`/`INSERT` allowlist.
pub async fn fetch_and_apply_snapshot(
    factory: &Turso,
    conn: &turso::Connection,
    primary_addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    max_snapshot_bytes: u64,
) -> anyhow::Result<i64> {
    fetch_and_apply_snapshot_on(
        factory,
        conn,
        primary_addr,
        channel,
        max_snapshot_bytes,
        &snapshot_stream_type(DEFAULT_SITE),
    )
    .await
}

/// Body of [`fetch_and_apply_snapshot`], parameterized by the snapshot
/// stream type so the per-site clustered path can dial `snapshot/<site>`
/// while the single-database path dials `snapshot/default`. Everything
/// else — the size bounds, the allocation-hint clamp, the statement
/// allowlist, the watermark seed — is identical.
async fn fetch_and_apply_snapshot_on(
    factory: &Turso,
    conn: &turso::Connection,
    primary_addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    max_snapshot_bytes: u64,
    snapshot_stream: &str,
) -> anyhow::Result<i64> {
    let started = std::time::Instant::now();
    let (header, dump, received) =
        fetch_snapshot(primary_addr, channel, max_snapshot_bytes, snapshot_stream).await?;
    {
        let _permit = factory.wal_gate_permit().await;
        off_runtime(install_snapshot(conn, &header, &dump, false)).await?;
    }
    // Only counted once the dump is durably applied: bytes received into
    // a buffer that then failed validation are not a bootstrap.
    cdc_metrics::record_snapshot_received(received, started.elapsed());
    // The local watermark table now holds exactly this value, so the
    // replica's watermark gauge is correct before the CDC tail starts.
    cdc_metrics::record_applied_watermark(DEFAULT_SITE, header.watermark);
    Ok(header.watermark)
}

/// Dial `primary_addr` and receive a snapshot **without applying it**, returning
/// `(header, dump, bytes_received)`.
///
/// Split out from [`fetch_and_apply_snapshot_on`] so the per-site replica can
/// **fetch first and only then decide** whether the snapshot is fit to replace
/// what it already holds (see [`snapshot_may_replace_local_data`]). Deciding
/// after the fetch is the whole point: the previous order — discard local
/// tables, then go and get a snapshot — could not fail safe, because by the
/// time the snapshot turned out to be empty the local data was already gone.
///
/// # Errors
///
/// Returns an error if the dial or the header/chunk read fails, or if the peer
/// announces or streams more than `max_snapshot_bytes`.
async fn fetch_snapshot(
    primary_addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    max_snapshot_bytes: u64,
    snapshot_stream: &str,
) -> anyhow::Result<(SnapshotHeader, String, u64)> {
    let (mut stream, header) = open_snapshot(primary_addr, channel, snapshot_stream).await?;
    let (dump, received) = read_snapshot_body(&mut stream, &header, max_snapshot_bytes).await?;
    Ok((header, dump, received))
}

/// Dial `addr` on `snapshot_stream` and read the snapshot header — the part of
/// a fetch during which the peer produces its dump. Split from the body read so
/// a caller can bound it (the ownership handoff's per-peer probe) without
/// bounding how long a large body may take to arrive.
async fn open_snapshot(
    addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    snapshot_stream: &str,
) -> anyhow::Result<(ChannelStream, SnapshotHeader)> {
    let mut stream = channel
        .dial(addr, snapshot_stream)
        .await
        .with_context(|| format!("snapshot: dial {addr}"))?;
    let header = read_snapshot_header(&mut stream).await.context("snapshot: read header")?;
    Ok((stream, header))
}

/// Read a snapshot body after its `header`, enforcing `max_snapshot_bytes`.
/// Returns the dump and the number of body bytes received.
async fn read_snapshot_body<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    header: &SnapshotHeader,
    max_snapshot_bytes: u64,
) -> anyhow::Result<(String, u64)> {
    read_snapshot_body_within(stream, header, max_snapshot_bytes, None).await
}

/// [`read_snapshot_body`], failing when no chunk arrives within
/// `chunk_timeout` (`None` = wait indefinitely). Bounds the **gap between
/// chunks**, not the whole body, so a large dump arriving steadily is never
/// cut off while a peer that stops sending is.
async fn read_snapshot_body_within<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    header: &SnapshotHeader,
    max_snapshot_bytes: u64,
    chunk_timeout: Option<Duration>,
) -> anyhow::Result<(String, u64)> {
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
        let chunk = match chunk_timeout {
            None => read_snapshot_chunk(stream).await,
            Some(limit) => {
                tokio::time::timeout(limit, read_snapshot_chunk(stream)).await.map_err(|_| {
                    anyhow::anyhow!(
                        "the peer stopped sending: no snapshot chunk for {}s after {received} \
                         body bytes",
                        limit.as_secs()
                    )
                })?
            }
        }
        .context("snapshot: read chunk")?;
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
    Ok((dump, received))
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
// Per-site clustered replication (Phase 0).
//
// Multi-tenant clustered mode: `[server] sites_dir` + `[db.sqlite] dir` +
// clustered + `[db.sqlite.replication] per_site = true`. Each virtual host
// has its own Turso database at `<dir>/<site-key>.db`, and every alive node
// replicates every active site so any node can serve any site's reads.
//
// Ownership of a site's *writes* is decided by rendezvous hashing (HRW,
// `ephpm_cluster::hrw_owner`) over the alive nodes: `owner(site) = argmax
// node of hash(node_id ‖ site)`. A node death re-homes only that node's
// sites, each to a node already holding a warm replica.
//
// Wire serving (the per-site MySQL listener + `ephpm_db_*` bridge) is the
// SAME single-node per-site machinery, wired in `serve()` — the only
// clustered addition on the serving side is that each site's Turso wire
// factory is opened with CDC capture on (see `SiteBackends::new_clustered`),
// so a site owned here captures the writes it must ship. THIS module adds
// the replication plane: a second (mgmt) Turso factory per site for
// tailing/applying CDC, owner-side stream handlers, and a per-site driver.
//
// ## Owner side (reactive)
//
// One `register_prefix("cdc/")` and one `register_prefix("snapshot/")`
// handler route every site. For each inbound stream the handler parses the
// site out of the stream type and authorizes it (`stream_is_authorized`:
// known cluster member, plus HRW ownership — with the one deliberate
// exception that any node will serve a *snapshot* to the site's current
// owner, which is the ownership handoff below). Only then does it resolve
// that site's mgmt factory and spawn the generic `serve_subscriber` /
// `serve_snapshot`. A refused dialer is chasing a stale owner and retries.
//
// ## Replica side (per-site driver)
//
// A site enters this node's working set when either (a) its `.db` already
// exists on disk at startup, or (b) a request opens it locally (the
// `SiteBackends` open hook feeds the site key in over `site_events`). For
// each such site a driver runs a per-site `SqliteElection`
// (`sqlite:primary:<site>`, HRW ownership) that publishes THIS node's
// channel address while it owns the site and names the owner otherwise;
// when this node is a replica of the site it cold-bootstraps (if empty) then
// tails `cdc/<site>` from the owner, exactly like the single-database path.
//
// ## Ownership moves carry the data
//
// HRW ownership is recomputed from live membership, so it moves whenever
// membership changes — a node *joining* is enough. Ownership on its own moves
// no bytes, so these make a move safe:
//
// - The new owner **pulls** the site when its own copy is cold, before it
//   serves (`settle_site_serving`). It asks every alive peer in HRW rank order
//   — the previous owner first — until one answers with a dump that declares a
//   table, and until then the serving path refuses the site
//   (`crate::handoff_gate`). A peer without a copy answers `no_copy` and
//   creates nothing (`SiteMgmtRegistry::existing`, `serve_no_copy`). The site
//   starts empty only when every alive peer has answered "no copy" from a
//   membership view past the startup grace; a peer that cannot answer keeps it
//   refused and retried, because serving empty would fork the data.
// - The old owner never *discards* data to bootstrap from someone else. Its
//   database's `site_data_lineage` names it as the data's origin, so it tails
//   rather than resets; and even a justified reset refuses a snapshot that
//   carries no schema (`snapshot_may_replace_local_data`). The destructive
//   step is ordered fetch → vet → discard → apply, never discard → fetch.
//
// ## Write path
//
// Writes are **owner-served**. `start_clustered_per_site_turso` registers
// [`crate::sql_forward::spawn_owner_sql_handler`], and the `ephpm_db_*`
// bridge resolves through `sql_forward::ClusteredSiteResolver`: a node that
// is not the site's HRW owner forwards each statement over `sql/<site>` to
// the owner, which runs it locally so the write is captured into CDC and
// replicates everywhere. Reads and writes therefore both work on any node.
//
// ## What is NOT here yet (documented divergence)
//
// **`pdo_mysql` is not forwarded.** The per-site MySQL wire listener resolves
// this node's LOCAL backend, so a stock `pdo_mysql` write to a non-owner is
// applied locally and is not captured for replication. Apps on the per-site
// clustered path must use the `db-*` drop-ins, which call `ephpm_db_*`.
//
// **Multi-node ownership churn is unvalidated on a live cluster.** The handoff
// and the refusal rules above are covered by unit tests against real Turso
// databases, not by a running three-node failover.
// ---------------------------------------------------------------------------

/// Lazily-opened per-site replication management state.
///
/// A **second** Turso factory on the site's database file (distinct from
/// the wire factory the `SiteBackends` registry serves to litewire/PHP —
/// two factories on one file in one process is the same pattern the
/// single-database path uses and is verified safe upstream). Capture is
/// **off** here: the tailer reads `turso_cdc` explicitly and the applier's
/// writes must not be re-captured. Also carries the database's persistent
/// CDC log identity (issue #315).
struct SiteMgmt {
    factory: Arc<Turso>,
    log_id: String,
    /// Serializes the two things that may seed a cold copy of this site: the
    /// ownership handoff ([`settle_site_serving`], while live HRW names this
    /// node the owner) and the replica bootstrap ([`replicate_site_once`],
    /// while the election names another node). The two follow different views
    /// of ownership and can briefly overlap; each re-checks the local copy
    /// under this lock, so a cold copy is seeded once, not twice.
    seed: tokio::sync::Mutex<()>,
}

/// Per-site lock guarding that one site's opened-or-not [`SiteMgmt`].
type SiteMgmtSlot = Arc<tokio::sync::Mutex<Option<Arc<SiteMgmt>>>>;

/// Lazily-populated map of site key → its [`SiteMgmt`].
///
/// Locking is **per site**, not registry-wide: a `DashMap` of per-site async
/// mutexes each guarding that one site's `Option<SiteMgmt>`. Opening site A
/// therefore never blocks resolving site B (a registry-wide mutex held across
/// the open was itself part of the first-open cascade — a slow/contended open
/// of one site stalled every other site's driver). The per-site mutex still
/// guarantees at most one mgmt factory is opened per file.
///
/// [`ensure`](Self::ensure) **fast-fails** on a `database is locked` error
/// (the wire factory — opened cdc-on by the request that first touched the
/// site — contends on the file's write lock while `ensure_log_id` creates its
/// bookkeeping table). The per-site driver retries with backoff via
/// [`acquire_site_mgmt`]; the owner-side stream handlers drop the stream and
/// the replica re-dials, by which time the driver has usually populated the
/// cache. Retrying here instead would hold the per-site mutex across the
/// backoff and serialize the owner handler behind it.
///
/// **Unbounded.** Slots are never evicted, so unlike [`SiteBackends`] this
/// registry is not capped by `[db.sqlite] max_open_dbs` — it holds a second
/// `Turso` factory open per site for the process's life. See the known-issues
/// note on `start_clustered_per_site_turso`.
#[derive(Clone)]
struct SiteMgmtRegistry {
    dir: Arc<PathBuf>,
    slots: Arc<DashMap<String, SiteMgmtSlot>>,
}

impl WalGateStatsSource for SiteMgmtRegistry {
    /// One sample per opened mgmt factory. `try_lock` skips a site mid-open.
    /// The sampler dedups by site against the serving registry, which on an
    /// owner reports the same file (and, the gate being per file, the same
    /// counters).
    fn wal_gate_samples(&self, out: &mut Vec<WalGateSample>) {
        for entry in self.slots.iter() {
            let Ok(guard) = entry.value().try_lock() else { continue };
            let Some(mgmt) = guard.as_ref() else { continue };
            out.push(WalGateSample {
                site: entry.key().clone(),
                db_path: self.dir.join(format!("{}.db", entry.key())),
                stats: mgmt.factory.wal_gate_stats(),
            });
        }
    }
}

impl SiteMgmtRegistry {
    fn new(dir: PathBuf) -> Self {
        Self { dir: Arc::new(dir), slots: Arc::new(DashMap::new()) }
    }

    /// The per-site lock guarding that site's opened-or-not [`SiteMgmt`].
    fn slot(&self, site: &str) -> SiteMgmtSlot {
        Arc::clone(self.slots.entry(site.to_string()).or_default().value())
    }

    /// Get-or-open the mgmt factory + log identity for `site`.
    ///
    /// Fast-fails on a transient `database is locked` — see the type docs.
    async fn ensure(&self, site: &str) -> anyhow::Result<Arc<SiteMgmt>> {
        anyhow::ensure!(
            crate::router::is_valid_site_key(site),
            "refusing to open a mgmt factory for an invalid site key: {site:?}"
        );
        let slot = self.slot(site);
        let mut guard = slot.lock().await;
        if let Some(existing) = guard.as_ref() {
            return Ok(Arc::clone(existing));
        }
        self.open_into(site, &mut guard).await
    }

    /// The mgmt factory for `site` **only if this node holds the site's
    /// database** — already open here, or present on disk. `None` otherwise,
    /// and in that case nothing is created: opening a factory creates the
    /// file, which is exactly what answering a snapshot request for a site
    /// this node has never held must not do (see [`serve_no_copy`]).
    ///
    /// Fast-fails on a transient `database is locked`, like
    /// [`ensure`](Self::ensure).
    async fn existing(&self, site: &str) -> anyhow::Result<Option<Arc<SiteMgmt>>> {
        anyhow::ensure!(
            crate::router::is_valid_site_key(site),
            "refusing to open a mgmt factory for an invalid site key: {site:?}"
        );
        let slot = self.slot(site);
        let mut guard = slot.lock().await;
        if let Some(existing) = guard.as_ref() {
            return Ok(Some(Arc::clone(existing)));
        }
        // Checked under the slot lock, which every mgmt open of this site
        // takes, so no open here can create the file between this check and
        // the decision. (The serving registry may create it concurrently; then
        // this node is the site's owner and opening it is correct.)
        if !self.dir.join(format!("{site}.db")).exists() {
            return Ok(None);
        }
        self.open_into(site, &mut guard).await.map(Some)
    }

    /// Open the mgmt factory for `site` into its (locked, empty) slot.
    async fn open_into(
        &self,
        site: &str,
        guard: &mut tokio::sync::MutexGuard<'_, Option<Arc<SiteMgmt>>>,
    ) -> anyhow::Result<Arc<SiteMgmt>> {
        let path = self.dir.join(format!("{site}.db"));
        let path_str = path
            .to_str()
            .with_context(|| format!("per-site db path is not valid UTF-8: {}", path.display()))?;
        // Gated like every factory. On a node that does not own a site, the
        // mgmt factory is the **only** factory open on that file — the apply
        // loop's writes grow its WAL — so the gate has to live here too, not
        // just on the serving registry.
        let factory = Arc::new(
            wal_gate::builder_with_gate(Turso::builder(path_str))
                .build()
                .await
                .with_context(|| format!("open mgmt Turso factory for site {site}"))?,
        );
        let log_id = {
            let conn = factory
                .raw_connection()
                .with_context(|| format!("open mgmt connection for site {site}"))?;
            off_runtime(ensure_log_id(&conn))
                .await
                .with_context(|| format!("establish CDC log identity for site {site}"))?
        };
        let mgmt = Arc::new(SiteMgmt { factory, log_id, seed: tokio::sync::Mutex::new(()) });
        **guard = Some(Arc::clone(&mgmt));
        Ok(mgmt)
    }
}

/// Does this error chain describe a transient Turso/SQLite write-lock
/// contention (`database is locked` / `table is locked`)?
///
/// Turso surfaces write-lock contention as a message, not a stable typed
/// variant across the pinned version, so the classification is by message
/// (case-insensitive) — used to decide *retry* vs *give up* when opening a
/// per-site mgmt factory whose file the cdc-on wire factory is also writing.
fn is_locked_error(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_ascii_lowercase();
    s.contains("database is locked") || s.contains("table is locked")
}

/// Does this error chain describe a schema-object collision on CDC apply
/// (`... already exists`)?
///
/// This is the fingerprint of a *diverged* replica: it holds a local-origin
/// object (e.g. a `markers` table a stray non-owner write created) that the
/// owner's CDC batch then tries to `CREATE` again. It must trigger a clean
/// re-bootstrap from the owner rather than wedge the apply loop forever.
fn is_schema_collision_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").to_ascii_lowercase().contains("already exists")
}

/// Open (with retry) the per-site mgmt factory for the driver.
///
/// A `database is locked` is definitionally transient — the cdc-on wire
/// factory is mid-write on the same file — so this retries it **indefinitely**
/// with capped backoff rather than letting the driver exit and lose
/// replication for a site (Bug: a first-open lock race killed the driver
/// permanently, and a startup-scanned site got no further open event to
/// restart it). Any other error is permanent and propagates.
async fn acquire_site_mgmt(mgmt: &SiteMgmtRegistry, site: &str) -> anyhow::Result<Arc<SiteMgmt>> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match mgmt.ensure(site).await {
            Ok(m) => return Ok(m),
            Err(e) if is_locked_error(&e) => {
                // Cap backoff at REPLICA_RECONNECT_DELAY; warn once past a
                // threshold so a genuinely stuck lock is visible without
                // spamming the log every attempt.
                let backoff =
                    MGMT_OPEN_BACKOFF.saturating_mul(attempt).min(REPLICA_RECONNECT_DELAY);
                if attempt == MGMT_LOCK_WARN_AFTER {
                    tracing::warn!(
                        site = %site,
                        attempt,
                        "per-site mgmt open still contending on the database write lock after \
                         many retries; continuing to retry (replication cannot start until it \
                         opens)"
                    );
                } else {
                    tracing::debug!(site = %site, attempt, "per-site mgmt open contended on the db lock; retrying: {e:#}");
                }
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Parse and validate the site key out of a `<prefix><site>` stream type.
/// Returns `None` for an empty or non-`[a-z0-9._-]` key (a peer will not
/// get a stream served for a name that could name anything but a direct
/// child of `dir`).
fn site_from_stream(stream_type: &str, prefix: &str) -> Option<String> {
    let site = stream_type.strip_prefix(prefix)?;
    if !site.is_empty() && crate::router::is_valid_site_key(site) {
        Some(site.to_string())
    } else {
        None
    }
}

/// May `peer` be served a `cdc/<site>` or `snapshot/<site>` stream by this node?
///
/// Every inbound stream must come from a **known cluster member** — re-checked
/// here rather than trusted from connection admission, because membership can
/// change while a long-lived connection stays open. On top of that:
///
/// - **CDC** (`cdc/<site>`) is served only by the site's current HRW owner. A
///   non-owner's CDC log is not the site's authoritative log, so tailing it
///   would replicate the wrong sequence.
/// - **Snapshot** (`snapshot/<site>`) is served by the owner, *and* by any node
///   to the site's current owner. That second case is the ownership handoff
///   ([`settle_site_serving`]): when HRW re-homes a site to a node that holds
///   none of its data, the node that does hold it is by definition not the
///   owner any more, so an owner-only gate would refuse the very transfer that
///   makes the move safe. The requester must be the current owner, which is a
///   membership-derived fact no peer can assert for itself. A node holding no
///   copy answers `no_copy` rather than creating the site's database.
async fn stream_is_authorized(
    cluster: &ephpm_cluster::ClusterHandle,
    self_id: &str,
    site: &str,
    kind: StreamKind,
    peer: SocketAddr,
) -> bool {
    if !ephpm_cluster::peer_is_cluster_member(cluster, peer.ip()).await {
        return false;
    }
    let nodes = cluster.nodes().await;
    let Some(owner) = ephpm_cluster::hrw_owner(&nodes, site) else {
        return false;
    };
    if owner.id == self_id {
        return true;
    }
    matches!(kind, StreamKind::Snapshot) && peer_is_node(owner, peer)
}

/// Is `peer` the address of `node` as gossip knows it? Host-only, because the
/// cluster-channel port differs from the gossip port on the same host.
fn peer_is_node(node: &NodeInfo, peer: SocketAddr) -> bool {
    node.gossip_addr.parse::<SocketAddr>().is_ok_and(|addr| addr.ip() == peer.ip())
}

/// Enumerate the sites whose databases already exist under `dir` at startup
/// (warm working set). Non-`.db` files and invalid keys are ignored.
fn scan_existing_sites(dir: &Path) -> Vec<String> {
    let mut sites = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return sites;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(site) = name.strip_suffix(".db") else { continue };
        if crate::router::is_valid_site_key(site) {
            sites.push(site.to_string());
        }
    }
    sites
}

/// Start Phase 0 per-site clustered Turso replication.
///
/// Registers the owner-side `cdc/` and `snapshot/` prefix handlers (HRW-gated
/// per site), scans the on-disk working set, and starts a per-site
/// replication driver for each active site (plus any that later become active
/// via `site_events`). The serving plane (per-site MySQL listener + the
/// `ephpm_db_*` bridge over a capture-on `SiteBackends`) is wired separately
/// in `serve()`.
///
/// `dir` is `[db.sqlite] dir` (the per-site database directory). `site_events`
/// carries site keys that became active locally (a request opened them), fed
/// by the `SiteBackends` open hook.
///
/// # Errors
///
/// Returns an error if the cluster handle or channel is missing (a startup
/// ordering bug — `resolve_channel_features` enables the `cdc` feature for
/// this mode), or if the channel has no advertisable address.
pub async fn start_clustered_per_site_turso(
    sqlite_config: &SqliteConfig,
    dir: PathBuf,
    cluster: Option<&Arc<ephpm_cluster::ClusterHandle>>,
    channel_handle: Option<&ephpm_cluster::ChannelHandle>,
    mut site_events: tokio::sync::mpsc::UnboundedReceiver<String>,
    registry: crate::site_backends::SiteBackends,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
    primary_view: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let cluster = cluster.context(
        "per-site clustered Turso replication requires [cluster] enabled = true; \
         no cluster handle available",
    )?;
    let channel = channel_handle.context(
        "per-site clustered Turso replication requires the cluster channel to be bound; \
         maybe_start_cluster_channel returned None despite clustered SQLite being active \
         (startup ordering bug: resolve_channel_features should have enabled the cdc feature)",
    )?;
    let channel_advertise = channel.advertise_addr().context(
        "per-site clustered Turso replication cannot advertise the cluster channel address: \
         both [cluster] bind and [cluster.channel] listen use an unspecified IP (0.0.0.0 / ::). \
         Bind [cluster] to a specific IP peers can reach, or set [cluster.channel] listen \
         explicitly.",
    )?;

    let self_id = cluster.self_node().id;
    let max_snapshot_bytes = sqlite_config.replication.max_snapshot_bytes;

    // `/_ephpm/primary`: in per-site clustered mode EVERY healthy node reports
    // 200, deliberately and permanently.
    //
    // The endpoint answers "may a load balancer send writes here?". In
    // single-database clustered mode there is one writable node and the
    // election flips this flag as the role moves — active-passive routing. In
    // per-site mode there is no cluster-wide primary at all: ownership is
    // per-tenant, and a hundred sites can have a hundred different owners on
    // the same set of nodes, so no single boolean can answer the question for
    // the node as a whole. What *is* true node-wide is that every node accepts
    // writes for every site — a non-owner forwards them to the site's owner
    // over `sql/<site>` (see `crate::sql_forward`) — so the honest answer for
    // the whole node is 200, and steering traffic away from any healthy node
    // would only remove capacity.
    //
    // Set explicitly rather than left at the constructor default so this is a
    // decision in the code rather than an omission: before this, the flag was
    // simply never passed to this path and read `true` by accident.
    primary_view.store(true, Ordering::Relaxed);

    cdc_metrics::init();

    tracing::warn!(
        dir = %dir.display(),
        channel_listen = %channel.listen_addr(),
        "starting EXPERIMENTAL per-site clustered Turso replication (HRW ownership, one \
         database per virtual host). Reads and ephpm_db_* writes work on any node: a \
         non-owner forwards its statements to the site's owner over sql/<site>. Stock \
         pdo_mysql is NOT forwarded — its writes on a non-owner are local-only and are \
         discarded when that replica re-bootstraps. Turso is Beta upstream."
    );

    // Gated like the serving registry in `wire_per_site_clustered_db`.
    let mgmt = SiteMgmtRegistry::new(dir.clone());
    wal_gate::register_source(Arc::new(mgmt.clone()));

    // Owner-side handlers (reactive; registered up front so a promotion is
    // served immediately).
    spawn_per_site_stream_handler(
        channel,
        Arc::clone(cluster),
        self_id.clone(),
        mgmt.clone(),
        StreamKind::Cdc,
        handles,
    );
    spawn_per_site_stream_handler(
        channel,
        Arc::clone(cluster),
        self_id.clone(),
        mgmt.clone(),
        StreamKind::Snapshot,
        handles,
    );

    // Owner-serves write-forwarding: a non-owner node dials `sql/<site>` to
    // forward its `ephpm_db_*` statements here; this node (when it is the
    // site's HRW owner) runs them against the site's LOCAL backend — capturing
    // writes into CDC, which then replicate to every replica. Registered up
    // front so any site this node owns is served, opened on demand.
    // From here on a site is served locally only once its driver has settled
    // it — the local copy holds data, or a peer's copy was pulled into it, or
    // no peer has one (`crate::handoff_gate`, `settle_site_serving`).
    let gate = Arc::clone(registry.handoff_gate());
    gate.arm();

    // Every node must compute site ownership with the same hash scheme. While
    // an alive peer confirmably does not (a pre-v0.13.0 node during a
    // forbidden rolling upgrade), this node serves no site as its owner: the
    // older node cannot know the new scheme exists, so only this side can step
    // back, and stepping back leaves one writer per site (see
    // `HandoffGate::set_scheme_block`).
    {
        let gate = Arc::clone(&gate);
        handles.push(ephpm_cluster::spawn_hrw_scheme_watch(
            Arc::clone(cluster),
            move |confirmed| {
                gate.set_scheme_block(scheme_block_reason(confirmed));
            },
        ));
    }

    crate::sql_forward::spawn_owner_sql_handler(
        channel,
        Arc::clone(cluster),
        self_id.clone(),
        registry,
        handles,
    );

    // Replica-side per-site drivers. `drivers` guards against double-starting
    // a driver for one site (startup scan vs. the live open-event stream).
    let drivers: Arc<DashMap<String, ()>> = Arc::new(DashMap::new());

    let ctx = SiteDriverCtx {
        cluster: Arc::clone(cluster),
        channel: channel.clone(),
        channel_advertise,
        mgmt: mgmt.clone(),
        max_snapshot_bytes,
        gate,
        plane_started: tokio::time::Instant::now(),
    };
    let start_driver = {
        let drivers = Arc::clone(&drivers);
        move |site: String| ensure_site_driver(&drivers, site, &ctx)
    };

    // Warm working set already on disk.
    for site in scan_existing_sites(&dir) {
        start_driver(site);
    }

    // Sites that become active locally later.
    handles.push(tokio::spawn(async move {
        while let Some(site) = site_events.recv().await {
            start_driver(site);
        }
    }));

    Ok(())
}

/// Which per-site stream a handler serves.
#[derive(Clone, Copy)]
enum StreamKind {
    Cdc,
    Snapshot,
}

impl StreamKind {
    fn prefix(self) -> &'static str {
        match self {
            StreamKind::Cdc => ephpm_cluster::stream_type::CDC_PREFIX,
            StreamKind::Snapshot => ephpm_cluster::stream_type::SNAPSHOT_PREFIX,
        }
    }

    fn label(self) -> &'static str {
        match self {
            StreamKind::Cdc => "cdc",
            StreamKind::Snapshot => "snapshot",
        }
    }
}

/// Spawn one owner-side per-site handler for `kind`, routing every site
/// under its prefix. Each inbound stream is gated on HRW ownership of the
/// parsed site before this node's (owner) mgmt factory is resolved and the
/// generic serve function is driven.
fn spawn_per_site_stream_handler(
    channel: &ephpm_cluster::ChannelHandle,
    cluster: Arc<ephpm_cluster::ClusterHandle>,
    self_id: String,
    mgmt: SiteMgmtRegistry,
    kind: StreamKind,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let mut streams = channel.register_prefix(kind.prefix());
    handles.push(tokio::spawn(async move {
        while let Some(incoming) = streams.recv().await {
            let IncomingStream { stream, peer, stream_type } = incoming;
            let Some(site) = site_from_stream(&stream_type, kind.prefix()) else {
                tracing::warn!(
                    peer = %peer,
                    stream = %stream_type,
                    "per-site {}: unparseable or invalid site key; dropping stream",
                    kind.label()
                );
                continue;
            };
            let cluster = Arc::clone(&cluster);
            let mgmt = mgmt.clone();
            let self_id = self_id.clone();
            tokio::spawn(async move {
                if !stream_is_authorized(&cluster, &self_id, &site, kind, peer).await {
                    cdc_metrics::record_stream_refused(kind.label());
                    tracing::warn!(
                        peer = %peer,
                        site = %site,
                        "per-site {}: refusing (not this site's owner, or the peer is not a \
                         known cluster member); a peer chasing a stale owner will retry",
                        kind.label()
                    );
                    return;
                }
                // A CDC stream is only ever served by the site's owner, which
                // serves the site and may open (create) its database. A
                // snapshot can be asked of ANY node — a cold new owner probes
                // every alive peer — so it must not create the file of a site
                // this node never held; it answers "no copy" instead.
                let opened = match kind {
                    StreamKind::Cdc => mgmt.ensure(&site).await.map(Some),
                    StreamKind::Snapshot => mgmt.existing(&site).await,
                };
                let site_mgmt = match opened {
                    Ok(Some(m)) => m,
                    Ok(None) => {
                        let mut stream = stream;
                        match serve_no_copy(&mut stream).await {
                            Ok(()) => tracing::debug!(peer = %peer, site = %site, "per-site snapshot: holding no copy of the site; answered no-copy"),
                            Err(e) => tracing::info!(peer = %peer, site = %site, "per-site snapshot: no-copy answer failed: {e:#}"),
                        }
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(site = %site, "per-site {}: cannot open mgmt factory: {e:#}", kind.label());
                        return;
                    }
                };
                match kind {
                    StreamKind::Cdc => {
                        if let Err(e) =
                            serve_site_subscriber(stream, &site_mgmt.factory, &site_mgmt.log_id, &site).await
                        {
                            tracing::info!(peer = %peer, site = %site, "per-site CDC subscriber disconnected: {e:#}");
                        }
                    }
                    StreamKind::Snapshot => match serve_snapshot(stream, &site_mgmt.factory, &site_mgmt.log_id).await {
                        Ok(n) => tracing::info!(peer = %peer, site = %site, watermark = n, "served per-site snapshot bootstrap"),
                        Err(e) => tracing::warn!(peer = %peer, site = %site, "per-site snapshot bootstrap failed: {e:#}"),
                    },
                }
            });
        }
    }));
}

/// Cancelled once at process shutdown by [`stop_replication_drivers`].
fn drivers_shutdown() -> &'static tokio_util::sync::CancellationToken {
    static TOKEN: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
        std::sync::OnceLock::new();
    TOKEN.get_or_init(tokio_util::sync::CancellationToken::new)
}

/// Stop every per-site replication driver and refuse to start new ones.
///
/// Process shutdown only (it is permanent): the drivers are detached, so
/// nothing else would stop them, and a driver applying a replicated batch, a
/// snapshot or an ownership handoff writes to its site's database — which
/// would refill a log the final checkpoint (`db_shutdown`) just truncated.
///
/// Cooperative, never a mid-operation cancel: a driver leaves at its next
/// safe point (the top of its loop, a reconnect backoff, a wait for an
/// owner), so no engine call is dropped half-way — an abandoned statement
/// future can leave a read lock behind that blocks the final checkpoint. The
/// cluster channel is shut down first, which ends any in-flight replica
/// stream after its current batch.
pub fn stop_replication_drivers() {
    drivers_shutdown().cancel();
}

/// Start a per-site replication driver for `site` unless one is already
/// running. Detached (the working set is dynamic, so these do not live in
/// the startup `handles` vec); the driver removes itself from `drivers` when
/// it exits so a later open-event can restart it. Every driver ends when
/// [`stop_replication_drivers`] runs.
fn ensure_site_driver(drivers: &Arc<DashMap<String, ()>>, site: String, ctx: &SiteDriverCtx) {
    if !crate::router::is_valid_site_key(&site) || drivers_shutdown().is_cancelled() {
        return;
    }
    // Atomic check-and-mark: hold the shard entry across the insert so the
    // startup scan and the live event stream cannot both start a driver.
    match drivers.entry(site.clone()) {
        dashmap::mapref::entry::Entry::Occupied(_) => return,
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(());
        }
    }

    let ctx = ctx.clone();
    let drivers = Arc::clone(drivers);
    tokio::spawn(async move {
        if let Err(e) = run_site_replication(site.clone(), ctx).await {
            tracing::warn!(site = %site, "per-site replication driver exited: {e:#}");
        }
        // The site has left this node's replication set: its per-site CDC
        // gauges must disappear rather than linger at their last value.
        cdc_metrics::forget_site(&site);
        // Allow a future open-event to restart the driver for this site.
        drivers.remove(&site);
    });
}

/// The per-site replication driver: run a per-site election for `site` and,
/// whenever this node is a *replica* of it, cold-bootstrap (if empty) then
/// tail `cdc/<site>` from the current owner. When this node *owns* the site
/// the owner-side handlers serve replicas and this driver only keeps the
/// election's address claim published (via the election task) — there is
/// nothing to drive until the role changes.
///
/// # Errors
///
/// Returns an error only on a *permanent* failure to open the site's mgmt
/// factory (a transient `database is locked` is retried indefinitely). Stream
/// failures are retried internally.
async fn run_site_replication(site: String, ctx: SiteDriverCtx) -> anyhow::Result<()> {
    let SiteDriverCtx {
        cluster,
        channel,
        channel_advertise,
        mgmt,
        max_snapshot_bytes,
        gate,
        plane_started,
    } = ctx;
    // Open the mgmt factory with retry: a first-open write-lock race with the
    // cdc-on wire factory must not kill the driver permanently (Bug 1).
    let site_mgmt = acquire_site_mgmt(&mgmt, &site)
        .await
        .with_context(|| format!("open mgmt for site {site}"))?;

    // Settle the serving gate alongside the election: it follows live HRW
    // ownership (what the serving path follows), not the election's role,
    // which lags it by up to the startup grace. Ends once the site is settled
    // (or at shutdown); the driver does not wait for it, but does not outlive
    // it either — a restarted driver starts its own. Aborting it is safe: its
    // engine work runs in offloaded scopes, which an abort on the server's
    // multi-thread runtime cannot cut short (it lands at the next await).
    let _settle = AbortOnDrop(tokio::spawn(settle_site_serving(
        site.clone(),
        Arc::clone(&cluster),
        channel.clone(),
        Arc::clone(&site_mgmt),
        gate,
        max_snapshot_bytes,
        plane_started,
    )));

    // Per-site election: publishes THIS node's channel address as owner while
    // it is the site's HRW owner (so replicas can dial it), and names the
    // owner otherwise.
    let election = ephpm_cluster::SqliteElection::new_per_site(
        Arc::clone(&cluster),
        channel_advertise.to_string(),
        site_mgmt.log_id.clone(),
        site.clone(),
    );
    let mut role_rx = election.watch_role();
    let _ = election.determine_initial_role().await;
    tokio::spawn(election.run());

    tracing::info!(site = %site, "per-site replication driver started");

    // Set after a tail fails because the replica holds divergent local state:
    // the next attempt discards local tables and re-bootstraps clean (Bug 2 —
    // "replica is authoritative-from-owner"). Cleared on any clean attempt or
    // role change.
    let mut force_reset = false;
    // Reconnect pacing for this site's replica stream; see the failure arm.
    let mut backoff = ephpm_cluster::ReconnectBackoff::replica_stream();

    loop {
        // Process shutdown: leave at this safe point (see
        // `stop_replication_drivers`).
        if drivers_shutdown().is_cancelled() {
            return Ok(());
        }
        let role = role_rx.borrow().clone();
        match role {
            ElectedRole::Primary => {
                // We own this site; owner-side handlers serve replicas.
                // Nothing to drive until the role changes. (Becoming the owner
                // of a site whose data lives on another node is handled by
                // `settle_site_serving`, which follows live ownership rather
                // than this role and keeps the site unserved until it has
                // pulled the data.)
                force_reset = false;
                // Metrics: this node now ships the site and is nobody's
                // replica of it.
                cdc_metrics::forget_applied(&site);
                cdc_metrics::track_shipping(&site);
                let keep_going = tokio::select! {
                    still = sample_owned_head_until_role_changes(&site, &site_mgmt.factory, &mut role_rx) => still,
                    () = drivers_shutdown().cancelled() => false,
                };
                if !keep_going {
                    return Ok(());
                }
            }
            ElectedRole::Replica { primary_grpc_url } if !primary_grpc_url.is_empty() => {
                // Another node ships this site now; drop our owner-side
                // series so a stale lag/head cannot outlive the ownership.
                cdc_metrics::forget_shipping(&site);
                let owner = match parse_primary_addr(&primary_grpc_url) {
                    Ok(addr) => addr,
                    Err(e) => {
                        tracing::warn!(site = %site, "per-site replica: owner address unusable: {e:#}");
                        if role_rx.changed().await.is_err() {
                            return Ok(());
                        }
                        continue;
                    }
                };
                // One dial+tail attempt, interruptible by a role change (a
                // failover re-homes this site to a different owner).
                let attempt_started = tokio::time::Instant::now();
                let failed = tokio::select! {
                    biased;
                    changed = role_rx.changed() => {
                        // A failover re-homes the site; drop any reset intent
                        // and start the next owner's retries from scratch.
                        force_reset = false;
                        backoff.reset();
                        if changed.is_err() {
                            return Ok(());
                        }
                        false
                    }
                    res = replicate_site_once(&site, owner, &channel, &site_mgmt, max_snapshot_bytes, force_reset) => {
                        match res {
                            Ok(()) => {
                                force_reset = false;
                                backoff.reset();
                                false
                            }
                            Err(e) if is_schema_collision_error(&e) => {
                                // Divergence: the replica holds a local-origin
                                // object the owner's CDC batch collides with.
                                // Force a clean re-bootstrap next attempt rather
                                // than wedge the apply loop forever.
                                tracing::warn!(
                                    site = %site,
                                    owner = %owner,
                                    "per-site replica holds divergent local state; discarding it \
                                     and re-bootstrapping from the owner: {e:#}"
                                );
                                force_reset = true;
                                true
                            }
                            Err(e) => {
                                tracing::warn!(site = %site, owner = %owner, "per-site replica stream error: {e:#}");
                                true
                            }
                        }
                    }
                };
                if failed {
                    // Jittered exponential backoff (stress report F1): every
                    // site's driver that failed in the same instant must not
                    // re-dial in the same instant, and an owner that keeps
                    // refusing gets progressively fewer dials. The wait is
                    // still cut short by a role change, so a failover is
                    // followed as promptly as before.
                    backoff.record_attempt(attempt_started.elapsed());
                    let delay = backoff.next_delay();
                    tokio::select! {
                        biased;
                        changed = role_rx.changed() => {
                            force_reset = false;
                            backoff.reset();
                            if changed.is_err() {
                                return Ok(());
                            }
                        }
                        () = tokio::time::sleep(delay) => {}
                        () = drivers_shutdown().cancelled() => return Ok(()),
                    }
                }
            }
            ElectedRole::Replica { .. } => {
                // Owner not resolved yet — wait for the election to name one.
                // Either way this node no longer ships the site.
                cdc_metrics::forget_shipping(&site);
                tokio::select! {
                    changed = role_rx.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                    }
                    () = drivers_shutdown().cancelled() => return Ok(()),
                }
            }
        }
    }
}

/// While this node owns `site`, sample `MAX(turso_cdc.change_id)` of the
/// site's log every [`HEAD_SAMPLE_INTERVAL`] into
/// `ephpm_cdc_primary_head_change_id{site}`, until the role changes.
///
/// The per-site counterpart of [`spawn_head_sampler`], and needed for the
/// same reason: without it the site's head only advances as a side effect
/// of shipping, so an owner whose replica just died would report a frozen
/// lag instead of a growing one. One indexed `MAX()` per owned site per
/// tick; the connection is opened on the first tick and held for the
/// ownership stint (a raw connection, so each read takes the WAL-gate
/// permit, exactly like the whole-database sampler).
///
/// Returns `false` when the election's role channel closed (the driver
/// should exit), `true` on a role change.
async fn sample_owned_head_until_role_changes(
    site: &str,
    factory: &Turso,
    role_rx: &mut tokio::sync::watch::Receiver<ElectedRole>,
) -> bool {
    let mut conn = None;
    let mut ticker = tokio::time::interval(HEAD_SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            changed = role_rx.changed() => return changed.is_ok(),
            _ = ticker.tick() => {
                if conn.is_none() {
                    match factory.raw_connection() {
                        Ok(c) => conn = Some(c),
                        Err(e) => {
                            tracing::debug!(site = %site, "per-site CDC head sampler: cannot open a connection: {e:#}");
                            continue;
                        }
                    }
                }
                let Some(c) = conn.as_ref() else { continue };
                let head = {
                    let _permit = factory.wal_gate_permit().await;
                    off_runtime(current_max_change_id(c)).await
                };
                match head {
                    Ok(head) => cdc_metrics::observe_head(site, head),
                    Err(e) => tracing::debug!(site = %site, "per-site CDC head sampler: MAX(change_id) failed: {e:#}"),
                }
            }
        }
    }
}

/// The node-wide serving refusal for one scheme-watch check: `Some(peers)`
/// while any alive peer is confirmed to run a different ownership-hash scheme,
/// `None` once all agree.
fn scheme_block_reason(confirmed: &[(String, Option<u32>)]) -> Option<String> {
    (!confirmed.is_empty()).then(|| ephpm_cluster::describe_hrw_scheme_mismatches(confirmed))
}

/// Aborts the task it holds when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Everything a per-site driver needs, cloned into each one by
/// [`ensure_site_driver`].
#[derive(Clone)]
struct SiteDriverCtx {
    cluster: Arc<ephpm_cluster::ClusterHandle>,
    channel: ephpm_cluster::ChannelHandle,
    channel_advertise: SocketAddr,
    mgmt: SiteMgmtRegistry,
    max_snapshot_bytes: u64,
    /// The serving registry's handoff gate, armed by
    /// [`start_clustered_per_site_turso`]; settled per site by
    /// [`settle_site_serving`].
    gate: Arc<HandoffGate>,
    /// When the replication plane started. "No alive peer holds a copy" is
    /// only trusted once [`HANDOFF_SETTLE_GRACE`] has passed since then.
    plane_started: tokio::time::Instant,
}

/// How long after the replication plane starts before "no alive peer holds a
/// copy of this site" is taken to mean the site is genuinely new.
///
/// Right after boot the membership view holds only the peers gossip has found
/// so far — possibly none — and a peer it has not found yet may hold the only
/// copy. Concluding "new site" then would serve an empty database over data
/// that exists. The same reasoning, and the same duration, as the election's
/// startup grace (heartbeat + claim TTL).
const HANDOFF_SETTLE_GRACE: Duration = Duration::from_secs(15);

/// Bound on one handoff probe's dial and snapshot header — the part during
/// which the peer produces its dump. A peer that does not answer within it is
/// treated as unreachable (it may hold the data), not as holding no copy. The
/// body that follows is not bounded by this.
const HANDOFF_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the gap between two chunks of a handoff probe's snapshot body.
/// The body itself may take as long as it takes to arrive; a peer that stops
/// sending mid-body (alive to gossip, its channel connection kept open by
/// keepalive) used to hold the probe — and so the site, `Pending` — forever,
/// with no `WARN`. It now counts as unreachable for this round: the next
/// peer is asked, and if none has the data the site is reported blocked and
/// retried, like any unreachable peer.
const HANDOFF_BODY_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How often an unsettled site that this node does not own re-checks whether
/// its copy has warmed up (a replica bootstrap) or ownership has moved here.
/// One `sqlite_schema` count per tick, only while the site is unsettled.
const HANDOFF_OWNERSHIP_RECHECK: Duration = Duration::from_secs(2);

/// Log a still-blocked handoff at `WARN` again after this many retries with an
/// unchanged reason (every change of reason is logged at once).
const HANDOFF_BLOCKED_WARN_EVERY: u32 = 10;

/// The alive peers that could hold a copy of `site`, in the order a cold new
/// owner should ask them: HRW rank over the alive members **excluding self**.
///
/// The first entry is the node that owned the site before this one *under the
/// current ownership hash* (HRW is a pure function of `(site, alive set)`), so
/// in the ordinary case — a node joining, or a node dying and its sites moving
/// to the next-ranked replica — the first probe finds the data. It is not
/// enough on its own: after a change of ownership hash (v0.13.0's scheme 2,
/// with a full-cluster restart) the node the new hash ranks second need not
/// hold the site at all, while the node the *old* hash made its owner does.
/// Asking every alive peer in rank order finds that copy wherever it is.
fn handoff_candidates(nodes: &[NodeInfo], self_id: &str, site: &str) -> Vec<NodeInfo> {
    let mut remaining: Vec<NodeInfo> = nodes
        .iter()
        .filter(|n| n.id != self_id && n.state == ephpm_cluster::NodeState::Alive)
        .cloned()
        .collect();
    let mut ranked = Vec::with_capacity(remaining.len());
    // Repeatedly take the HRW winner of what is left: the canonical scoring and
    // tie-break, with no second implementation of either.
    while let Some(winner) = ephpm_cluster::hrw_owner(&remaining, site).map(|n| n.id.clone()) {
        let Some(idx) = remaining.iter().position(|n| n.id == winner) else { break };
        ranked.push(remaining.swap_remove(idx));
    }
    ranked
}

/// One peer's answer to a handoff probe.
enum PeerProbe {
    /// The peer sent a snapshot that declares at least one table.
    Data { header: SnapshotHeader, dump: String, received: u64 },
    /// The peer holds no copy: an explicit `no_copy` answer, or (from an older
    /// node, or a node whose copy is still cold) a dump with no tables.
    NoCopy,
    /// No usable answer — the peer may hold the data. Carries the reason.
    Unreachable(String),
}

/// Ask the peer at `addr` for its copy of `site`.
async fn probe_peer(
    addr: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    max_snapshot_bytes: u64,
    site: &str,
) -> PeerProbe {
    let stream_type = snapshot_stream_type(site);
    let opened =
        tokio::time::timeout(HANDOFF_PROBE_TIMEOUT, open_snapshot(addr, channel, &stream_type))
            .await;
    let (mut stream, header) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => return PeerProbe::Unreachable(format!("{e:#}")),
        Err(_) => {
            return PeerProbe::Unreachable(format!(
                "no snapshot header within {}s",
                HANDOFF_PROBE_TIMEOUT.as_secs()
            ));
        }
    };
    if header.no_copy {
        return PeerProbe::NoCopy;
    }
    probe_body(&mut stream, header, max_snapshot_bytes, site, addr, HANDOFF_BODY_STALL_TIMEOUT)
        .await
}

/// The body half of [`probe_peer`]: read the dump with a per-chunk stall
/// bound and classify it. A stall is logged at `WARN` here — the peer is
/// alive to gossip, so nothing else would say why the site is waiting.
async fn probe_body<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    header: SnapshotHeader,
    max_snapshot_bytes: u64,
    site: &str,
    addr: SocketAddr,
    stall: Duration,
) -> PeerProbe {
    match read_snapshot_body_within(stream, &header, max_snapshot_bytes, Some(stall)).await {
        Ok((dump, received)) if snapshot_declares_a_table(&dump) => {
            PeerProbe::Data { header, dump, received }
        }
        Ok(_) => PeerProbe::NoCopy,
        Err(e) => {
            tracing::warn!(
                site = %site,
                peer = %addr,
                "ownership handoff: reading the site's snapshot from a peer failed; asking the \
                 next peer (the site stays unserved until one answers): {e:#}"
            );
            PeerProbe::Unreachable(format!("{e:#}"))
        }
    }
}

/// The outcome of asking the candidates in turn.
struct PeerSearch {
    /// The first peer (in candidate order) that answered with data, and what
    /// it sent.
    found: Option<(NodeInfo, SnapshotHeader, String, u64)>,
    /// `(peer id, reason)` for every peer asked that gave no usable answer.
    unreachable: Vec<(String, String)>,
}

/// Ask `candidates` in order until one answers with data.
///
/// Generic over the probe so the search order and its stopping rule are
/// unit-testable without a cluster.
async fn search_peers_for_copy<F, Fut>(candidates: &[NodeInfo], mut probe: F) -> PeerSearch
where
    F: FnMut(&NodeInfo) -> Fut,
    Fut: std::future::Future<Output = PeerProbe>,
{
    let mut unreachable = Vec::new();
    for peer in candidates {
        match probe(peer).await {
            PeerProbe::Data { header, dump, received } => {
                return PeerSearch {
                    found: Some((peer.clone(), header, dump, received)),
                    unreachable,
                };
            }
            PeerProbe::NoCopy => {}
            PeerProbe::Unreachable(reason) => unreachable.push((peer.id.clone(), reason)),
        }
    }
    PeerSearch { found: None, unreachable }
}

/// What a cold owner does when no peer answered with data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandoffVerdict {
    /// Every alive peer answered that it holds no copy, and membership has had
    /// time to converge: the site is genuinely new. Serve it empty.
    NewSite,
    /// Every peer known so far holds no copy, but it is too soon after startup
    /// to trust that the membership view is complete. Wait and ask again.
    AwaitMembership,
    /// At least one peer could not answer, and it may hold the data. Refuse to
    /// serve, and retry.
    Blocked,
}

/// Decide a cold owner's next step once no peer has answered with data.
///
/// The asymmetry is deliberate. Serving an empty database while a copy may
/// exist does not fail anywhere: the tenant's first write lands on the empty
/// copy, makes it "warm", and from then on the site has two histories and no
/// later step reconciles them. Refusing fails loudly and costs only the
/// unreachable peer's downtime. So only a complete set of negative answers —
/// from a membership view old enough to trust — licenses serving empty.
fn handoff_verdict(unreachable: usize, past_grace: bool) -> HandoffVerdict {
    if unreachable > 0 {
        HandoffVerdict::Blocked
    } else if past_grace {
        HandoffVerdict::NewSite
    } else {
        HandoffVerdict::AwaitMembership
    }
}

/// One settle attempt's result.
enum SettleRound {
    /// The site may be served from this node's copy.
    Settled,
    /// This node's copy is cold but it does not own the site; nothing to
    /// settle until one of those changes.
    NotOwner,
    /// Too soon after startup to conclude "new site"; ask again after the wait.
    AwaitMembership(Duration),
    /// Cannot settle now; the reason is what a refused request is told.
    Blocked(String),
}

/// Settle `site`'s serving-gate state, then return. Runs alongside the site's
/// replication driver (see [`crate::handoff_gate`] for why the gate exists).
///
/// # The rule
///
/// A site may be served from this node's database once:
///
/// - the local copy holds data (a warm restart, a replica that bootstrapped, a
///   node that created the site), or
/// - this node owns the site by live HRW, its copy is cold, and a peer's copy
///   has been pulled into it — the peers are asked in HRW rank order
///   ([`handoff_candidates`]), every alive one if need be, and the first that
///   answers with a dump declaring a table wins; or
/// - every alive peer answered that it holds no copy, from a membership view
///   at least [`HANDOFF_SETTLE_GRACE`] old: the site is genuinely new and
///   starts empty.
///
/// If some peer could not answer, the site stays unserved
/// ([`SiteServeState::Blocked`] — requests refused with the reason, `WARN`
/// logged, `ephpm_cdc_bootstrap_total{outcome="handoff_blocked"}` counted)
/// and the round is retried with backoff. See [`handoff_verdict`] for why
/// refusing beats serving empty.
///
/// # Safety of the pull
///
/// The copy is installed with the same fetch → vet → apply order as every
/// other bootstrap, onto a copy re-checked cold **inside** the install scope
/// and under the site's seed lock ([`SiteMgmt::seed`]), so it can never land
/// on data: nothing else writes a cold, unsettled copy (the serving path is
/// gated, and the replica bootstrap takes the same lock). Members gossip does
/// not report alive are not asked and not waited for — HRW already treats
/// them as gone.
async fn settle_site_serving(
    site: String,
    cluster: Arc<ephpm_cluster::ClusterHandle>,
    channel: ephpm_cluster::ChannelHandle,
    site_mgmt: Arc<SiteMgmt>,
    gate: Arc<HandoffGate>,
    max_snapshot_bytes: u64,
    plane_started: tokio::time::Instant,
) {
    let self_id = cluster.self_node().id;
    let mut conn: Option<turso::Connection> = None;
    let mut backoff = ephpm_cluster::ReconnectBackoff::replica_stream();
    let mut blocked_rounds: u32 = 0;
    let mut last_reason: Option<String> = None;
    loop {
        if drivers_shutdown().is_cancelled() {
            return;
        }
        let round_started = tokio::time::Instant::now();
        let round = match conn.as_ref() {
            Some(c) => {
                settle_round(
                    &site,
                    &self_id,
                    &cluster,
                    &channel,
                    &site_mgmt,
                    c,
                    max_snapshot_bytes,
                    plane_started.elapsed() >= HANDOFF_SETTLE_GRACE,
                )
                .await
            }
            None => match site_mgmt.factory.raw_connection() {
                Ok(c) => {
                    conn = Some(c);
                    continue;
                }
                Err(e) => SettleRound::Blocked(format!("cannot open the local database: {e:#}")),
            },
        };
        let delay = match round {
            SettleRound::Settled => {
                if blocked_rounds > 0 {
                    tracing::info!(
                        site = %site,
                        retries = blocked_rounds,
                        "ownership handoff unblocked; serving the site"
                    );
                }
                gate.set(&site, SiteServeState::Ready);
                return;
            }
            SettleRound::NotOwner => {
                gate.set(&site, SiteServeState::Pending);
                backoff.reset();
                blocked_rounds = 0;
                last_reason = None;
                HANDOFF_OWNERSHIP_RECHECK
            }
            SettleRound::AwaitMembership(wait) => {
                gate.set(&site, SiteServeState::Pending);
                wait
            }
            SettleRound::Blocked(reason) => {
                blocked_rounds = blocked_rounds.saturating_add(1);
                cdc_metrics::record_bootstrap("handoff_blocked");
                if last_reason.as_deref() != Some(reason.as_str())
                    || blocked_rounds.is_multiple_of(HANDOFF_BLOCKED_WARN_EVERY)
                {
                    tracing::warn!(
                        site = %site,
                        retries = blocked_rounds,
                        "ownership handoff blocked: this node owns the site but holds no copy of \
                         its data, and a peer that may hold it did not answer; refusing to serve \
                         the site (an empty copy would fork its data) and retrying: {reason}"
                    );
                } else {
                    tracing::debug!(site = %site, retries = blocked_rounds, "ownership handoff still blocked: {reason}");
                }
                gate.set(&site, SiteServeState::Blocked(reason.clone()));
                last_reason = Some(reason);
                backoff.record_attempt(round_started.elapsed());
                backoff.next_delay()
            }
        };
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = drivers_shutdown().cancelled() => return,
        }
    }
}

/// One attempt of [`settle_site_serving`].
#[allow(clippy::too_many_arguments)]
async fn settle_round(
    site: &str,
    self_id: &str,
    cluster: &ephpm_cluster::ClusterHandle,
    channel: &ephpm_cluster::ChannelHandle,
    site_mgmt: &SiteMgmt,
    conn: &turso::Connection,
    max_snapshot_bytes: u64,
    past_grace: bool,
) -> SettleRound {
    let factory = &*site_mgmt.factory;
    let cold = {
        let _permit = factory.wal_gate_permit().await;
        off_runtime(local_db_is_cold(conn)).await
    };
    match cold {
        Ok(false) => return SettleRound::Settled,
        Ok(true) => {}
        Err(e) => return SettleRound::Blocked(format!("cold-start check failed: {e:#}")),
    }

    let nodes = cluster.nodes().await;
    if ephpm_cluster::hrw_owner(&nodes, site).is_none_or(|owner| owner.id != self_id) {
        return SettleRound::NotOwner;
    }

    let candidates = handoff_candidates(&nodes, self_id, site);
    let search = search_peers_for_copy(&candidates, |peer| {
        let addr = crate::sql_forward::member_channel_addr(peer);
        async move {
            match addr {
                Some(addr) => probe_peer(addr, channel, max_snapshot_bytes, site).await,
                None => PeerProbe::Unreachable("its gossip address is unusable".to_string()),
            }
        }
    })
    .await;

    if let Some((peer, header, dump, received)) = search.found {
        let started = std::time::Instant::now();
        let installed = {
            let _seed = site_mgmt.seed.lock().await;
            let _permit = factory.wal_gate_permit().await;
            off_runtime(async {
                // Re-checked inside the scope: a replica bootstrap may have
                // seeded the copy while the snapshot was in flight.
                if !local_db_is_cold(conn).await? {
                    return anyhow::Ok(false);
                }
                install_snapshot(conn, &header, &dump, false).await?;
                Ok(true)
            })
            .await
        };
        return match installed {
            Ok(true) => {
                cdc_metrics::record_snapshot_received(received, started.elapsed());
                cdc_metrics::record_bootstrap("handoff_ok");
                tracing::info!(
                    site = %site,
                    source = %peer.id,
                    watermark = header.watermark,
                    source_rank = candidates.iter().position(|c| c.id == peer.id).map_or(0, |i| i + 1),
                    "ownership handoff complete: pulled the site's data from a peer before serving it"
                );
                SettleRound::Settled
            }
            Ok(false) => SettleRound::Settled,
            Err(e) => {
                SettleRound::Blocked(format!("installing the copy from {} failed: {e:#}", peer.id))
            }
        };
    }

    match handoff_verdict(search.unreachable.len(), past_grace) {
        HandoffVerdict::NewSite => {
            cdc_metrics::record_bootstrap("handoff_new");
            tracing::info!(
                site = %site,
                peers_asked = candidates.len(),
                "ownership handoff: no alive peer holds a copy of this site; it is new and starts \
                 from an empty database"
            );
            SettleRound::Settled
        }
        HandoffVerdict::AwaitMembership => SettleRound::AwaitMembership(HANDOFF_OWNERSHIP_RECHECK),
        HandoffVerdict::Blocked => SettleRound::Blocked(
            search
                .unreachable
                .iter()
                .map(|(id, reason)| format!("peer {id}: {reason}"))
                .collect::<Vec<_>>()
                .join("; "),
        ),
    }
}

/// The **data lineage** of a per-site database: the identity of the CDC log its
/// contents descend from, or `None` when it holds no data at all.
///
/// This is the invariant the per-site replica path turns on — *who is
/// authoritative for the bytes already on this disk*:
///
/// - A database seeded by a bootstrap has the serving owner's log id recorded
///   in [`SOURCE_LOG_TABLE`]; its lineage is that owner's log.
/// - A database with user tables and **no** source record is **local-origin**:
///   this node created the site and its writes are the only writes it has ever
///   had. Its lineage is therefore its *own* log id, which this function
///   records so the origin is never re-derived (and never mistaken for
///   something else) again.
/// - A cold database has no lineage: there is nothing to be authoritative for.
///
/// # Why the "no source record" case is not divergence
///
/// It used to be read as one: `replicate_site_once` treated "tables but no
/// owner source" as a diverged replica and dropped every user table before
/// bootstrapping. But that is precisely the state of the node that **created**
/// the site — it has never been anyone's replica, so nothing ever wrote a
/// source record for it. HRW re-homing (a node merely *joining* is enough) then
/// sent that node into the replica arm and destroyed the tenant's only copy of
/// its data. Lineage names the origin explicitly instead of inferring
/// divergence from an absence.
///
/// Divergence is still handled — but only when it is *proven*, by a CDC apply
/// colliding with local state (`force_reset`), never by inference.
///
/// # Errors
///
/// Returns an error if the bookkeeping read or the lineage stamp fails.
async fn site_data_lineage(
    conn: &turso::Connection,
    own_log_id: &str,
) -> anyhow::Result<Option<String>> {
    if let Some(source) = read_single_text(conn, SOURCE_LOG_TABLE, "log_id")
        .await
        .context("per-site replica: read owner-source record")?
    {
        return Ok(Some(source));
    }
    if local_db_is_cold(conn).await.context("per-site replica: cold-start check")? {
        return Ok(None);
    }
    // Non-cold with no recorded source: this node is the origin of the data.
    set_source_log(conn, own_log_id)
        .await
        .context("per-site replica: record this node as the site's data origin")?;
    tracing::info!(
        log_id = %own_log_id,
        "per-site database has local-origin data and no recorded source log; stamping this \
         node's own CDC log as its lineage (it created the site — it is not a diverged replica)"
    );
    Ok(Some(own_log_id.to_string()))
}

/// What a replica attempt must do before it can tail the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplicaAction {
    /// Fetch the owner's snapshot first. `discard_local` says whether local
    /// user tables must be dropped to make room for it — only ever true when
    /// this node holds data AND divergence has been proven.
    Bootstrap { discard_local: bool },
    /// The local database has a lineage and no proven divergence: tail the
    /// owner's CDC on top of it, destroying nothing.
    TailOnly,
}

/// Decide a replica attempt from `(proven divergence, data lineage)`.
///
/// Pure so the re-homing scenario is unit-testable without a cluster: see
/// `originating_node_is_not_treated_as_a_diverged_replica`.
fn plan_replica_attempt(force_reset: bool, lineage: Option<&str>) -> ReplicaAction {
    match lineage {
        // No data of our own — a fresh replica. Bootstrap, nothing to discard.
        None => ReplicaAction::Bootstrap { discard_local: false },
        // We hold data. Only a *proven* divergence (a CDC batch that collided
        // with local state) justifies discarding it.
        Some(_) if force_reset => ReplicaAction::Bootstrap { discard_local: true },
        Some(_) => ReplicaAction::TailOnly,
    }
}

/// One replica attempt for `site`: (re-)bootstrap from the owner when needed,
/// then tail `cdc/<site>` and apply until the stream ends.
///
/// A replica is **authoritative-from-owner** *for data it received from an
/// owner*. It bootstraps from the owner's snapshot — which seeds the watermark
/// to the owner's head, so the subsequent tail resumes *past* the schema DDL
/// and never replays a colliding `CREATE TABLE` — when either:
///
/// - the local database has no [`site_data_lineage`] (a fresh/cold replica), or
/// - `force_reset`: a prior tail collided with divergent local state, which is
///   the one *proof* of divergence this path accepts.
///
/// A re-bootstrap of a database that holds data discards its user tables, so it
/// is ordered **fetch, then check, then discard, then apply** — never
/// discard-then-fetch. A snapshot that carries no schema cannot replace live
/// data (see [`snapshot_may_replace_local_data`]); the attempt fails and is
/// retried instead.
async fn replicate_site_once(
    site: &str,
    owner: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    site_mgmt: &SiteMgmt,
    max_snapshot_bytes: u64,
    force_reset: bool,
) -> anyhow::Result<()> {
    let factory = &*site_mgmt.factory;
    let conn = factory.raw_connection().context("per-site replica: open apply connection")?;

    {
        // Decide and seed under the seed lock, so an ownership handoff running
        // concurrently cannot seed the same cold copy in between (see
        // `SiteMgmt::seed`). Released before the tail, which runs for as long
        // as the stream stays up.
        let _seed = site_mgmt.seed.lock().await;
        let lineage = {
            let _permit = factory.wal_gate_permit().await;
            off_runtime(site_data_lineage(&conn, &site_mgmt.log_id)).await?
        };
        if let ReplicaAction::Bootstrap { discard_local } =
            plan_replica_attempt(force_reset, lineage.as_deref())
        {
            // Publish this replica as holding none of the owner's log until
            // the snapshot installs. The owner cannot see a replica that has
            // not subscribed yet — its lag gauge covers attached subscribers
            // only — so without this a replica stuck bootstrapping had no
            // applied series at all and the cross-node lag query skipped it.
            cdc_metrics::record_applied_watermark(site, 0);
            let bconn =
                factory.raw_connection().context("per-site replica: open snapshot connection")?;
            bootstrap_site_from(
                factory,
                &bconn,
                site,
                owner,
                channel,
                max_snapshot_bytes,
                discard_local,
                &snapshot_stream_type(site),
            )
            .await
            .with_context(|| format!("per-site snapshot bootstrap for {site}"))?;
        }
    }

    let mut stream = channel
        .dial(owner, &cdc_stream_type(site))
        .await
        .with_context(|| format!("per-site replica: dial {owner} for {site}"))?;
    subscribe_and_consume_site(&mut stream, factory, &conn, site).await
}

/// Fetch a snapshot for `site` from `source` and apply it locally, discarding
/// the local user tables first when `discard_local`.
///
/// Fetch-then-swap: the dump is on hand and vetted *before* anything local is
/// touched, so a peer that turns out to hold nothing costs a retry rather than
/// a tenant's data.
#[allow(clippy::too_many_arguments)]
async fn bootstrap_site_from(
    factory: &Turso,
    conn: &turso::Connection,
    site: &str,
    source: SocketAddr,
    channel: &ephpm_cluster::ChannelHandle,
    max_snapshot_bytes: u64,
    discard_local: bool,
    snapshot_stream: &str,
) -> anyhow::Result<i64> {
    let started = std::time::Instant::now();
    let (header, dump, received) =
        fetch_snapshot(source, channel, max_snapshot_bytes, snapshot_stream).await?;
    // A `no_copy` answer carries no log identity to record as this copy's
    // lineage; there is nothing to seed from yet, so retry later.
    anyhow::ensure!(
        !header.no_copy,
        "{source} holds no copy of site {site} yet (it has not opened the site's database); \
         retrying"
    );
    // Fetched outside the permit (a network wait); installed inside it (raw
    // connection, no busy timeout — see `subscribe_and_consume`).
    {
        let _permit = factory.wal_gate_permit().await;
        off_runtime(install_snapshot(conn, &header, &dump, discard_local))
            .await
            .with_context(|| format!("per-site replica: snapshot for {site} from {source}"))?;
    }
    cdc_metrics::record_snapshot_received(received, started.elapsed());
    cdc_metrics::record_applied_watermark(site, header.watermark);
    Ok(header.watermark)
}

/// The local half of a per-site bootstrap: vet the fetched dump, then — and
/// only then — discard local user tables (when `discard_local`) and apply it.
///
/// Separated from the transport so the destructive step is a pure function of
/// data already in hand, and so the "an empty snapshot must not clobber live
/// data" invariant is directly testable against a real database.
///
/// Used by both clustered modes (whole-database bootstrap passes
/// `discard_local = false`).
///
/// # Atomicity
///
/// The discard, the dump, the watermark seed and the source-log record run
/// in **one** `BEGIN IMMEDIATE … COMMIT`. Either the replica ends up with the
/// whole snapshot and the bookkeeping that names it, or with exactly what it
/// had before. That is what makes an interrupted install safe to retry:
///
/// - an error part-way (a `database is locked` from the shutdown checkpoint,
///   a constraint failure, a malformed statement) rolls back here;
/// - a process that dies mid-install never commits, so the uncommitted WAL
///   frames are discarded when the file is next opened and the next start
///   sees the pre-install state — cold again, or its own data with its
///   lineage intact — and bootstraps again.
///
/// Before v0.13.1 the steps ran as separate autocommit statements: a failure
/// after the discard left the replica stripped, and a failure mid-dump left
/// tables with partial rows and no source record, which the next start read
/// as local-origin data ([`site_data_lineage`]) — 22 of 40 sites in the
/// v0.13.0 RC stress run never reconverged. One transaction is also one
/// commit, so the dump no longer pays an fsync per row.
///
/// The caller holds the factory's WAL-gate permit (raw connection, no busy
/// timeout) and runs this through `off_runtime`; nothing here awaits the
/// network.
///
/// # Errors
///
/// Returns an error, **before touching anything local**, if the dump cannot
/// replace the local data ([`snapshot_may_replace_local_data`]), announces no
/// log identity, or contains a statement outside the allowlist
/// ([`validate_snapshot_dump`]); or, after a rollback, if the discard, the
/// apply or the commit fails.
async fn install_snapshot(
    conn: &turso::Connection,
    header: &SnapshotHeader,
    dump: &str,
    discard_local: bool,
) -> anyhow::Result<()> {
    snapshot_may_replace_local_data(dump, discard_local)?;
    anyhow::ensure!(!header.log_id.is_empty(), "snapshot: peer announced an empty log id");
    // The dump arrives from the network and is only UTF-8 checked before this
    // point, so it is vetted against the statement allowlist before it can
    // reach `execute_batch`.
    validate_snapshot_dump(dump).context("snapshot: dump rejected by the statement allowlist")?;

    // A connection reused across attempts may still be inside the
    // transaction of an earlier attempt whose rollback failed. `BEGIN` would
    // then fail on every retry; end that transaction first.
    if !conn.is_autocommit().context("snapshot: read transaction state")? {
        conn.execute("ROLLBACK", ())
            .await
            .context("snapshot: roll back a transaction left open by an earlier attempt")?;
    }
    conn.execute("BEGIN IMMEDIATE", ()).await.context("snapshot: begin install transaction")?;

    let staged = async {
        if discard_local {
            reset_local_user_tables(conn)
                .await
                .context("per-site replica: discard divergent local tables")?;
        }
        apply_snapshot(conn, header, dump).await
    }
    .await;

    let outcome = match staged {
        Ok(()) => conn.execute("COMMIT", ()).await.map(drop).context("snapshot: commit install"),
        Err(e) => Err(e),
    };
    if let Err(e) = outcome {
        // On failure the connection stays inside the transaction. The next
        // attempt on it rolls back first (above); a dropped connection never
        // commits it.
        if !conn.is_autocommit().unwrap_or(false)
            && let Err(rb) = conn.execute("ROLLBACK", ()).await
        {
            tracing::warn!(
                error = %rb,
                "snapshot: rollback of a failed install did not complete; the partial install \
                 is uncommitted and will be discarded"
            );
        }
        return Err(e);
    }
    Ok(())
}

/// User (non-bookkeeping) table names in a local database — the same predicate
/// [`local_db_is_cold`] counts: excludes `sqlite_*`, `__`-prefixed bookkeeping
/// (log id / watermark / source), and `turso_cdc`.
async fn list_user_tables(conn: &turso::Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name NOT LIKE '\\_\\_%' ESCAPE '\\' \
             AND name != 'turso_cdc'",
        )
        .await
        .context("list user tables: prepare")?;
    let mut rows = stmt.query(()).await.context("list user tables: query")?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().await.context("list user tables: next")? {
        if let turso::Value::Text(name) = row.get_value(0).context("list user tables: value")? {
            names.push(name);
        }
    }
    Ok(names)
}

/// Drop every user table in a diverged replica so the owner's snapshot can be
/// applied clean. Bookkeeping tables (`__ephpm_cdc_*`, the litewire watermark,
/// `turso_cdc`) are left alone — the snapshot re-seeds the watermark/source
/// records, and `turso_cdc` is engine-managed capture. Runs on the mgmt
/// connection (capture off), so the drops are never captured.
async fn reset_local_user_tables(conn: &turso::Connection) -> anyhow::Result<()> {
    let tables = list_user_tables(conn).await?;
    for table in &tables {
        conn.execute(&format!("DROP TABLE IF EXISTS {}", quote_ident(table)), ())
            .await
            .with_context(|| format!("reset diverged replica: drop table {table}"))?;
    }
    if !tables.is_empty() {
        tracing::warn!(
            dropped = tables.len(),
            "per-site replica: discarded local-origin tables before re-bootstrapping from the \
             owner (replica is authoritative-from-owner)"
        );
    }
    Ok(())
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
    /// This drives the production `serve_subscriber`, which attaches to the
    /// process-global registry in `turso_cdc_metrics` under `site="default"`.
    /// It used to need `#[serial(cdc_registry)]` against the gauge tests
    /// there (13/100 full-suite flake), because the registry was one
    /// unkeyed slot; it is keyed by site now and those tests use their own
    /// site keys, so nothing here can move a value they assert on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
        let task =
            tokio::spawn(
                async move { serve_subscriber(server, &mgmt_for_task, "test-log-id").await },
            );

        // The primary speaks first: its Hello names the log it serves.
        match read_frame(&mut client).await.unwrap() {
            Frame::Hello { log_id } => assert_eq!(log_id, "test-log-id"),
            other => panic!("expected Hello as the first frame, got {other:?}"),
        }
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

    /// A tenant on a capturing session (every clustered write session) cannot
    /// switch the database to MVCC, and its writes keep reaching `turso_cdc`.
    ///
    /// ePHPm's tenant SQL screen now refuses `journal_mode` assignments on the
    /// bridge and per-site paths, but the shared clustered `pdo_mysql` listener
    /// is not screened, so this engine-level refusal is what holds there.
    /// Turso 0.7.2
    /// accepted WAL -> MVCC on a capturing connection, after which upstream
    /// documents capture stopping silently: writes committed locally and
    /// never replicated. Turso 0.8 refuses the change (ephpm#535). This pins
    /// it at the layer tenants actually reach, a litewire `Backend` session
    /// with `enable_cdc_on_connect`.
    #[tokio::test]
    async fn tenant_journal_mode_switch_cannot_stop_capture() {
        use litewire::backend::Backend;

        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();
        let wire = Turso::builder(&path).enable_cdc_on_connect(true).build().await.unwrap();
        let session = wire.connect().await.unwrap();
        session.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).await.unwrap();

        let err = session
            .query("PRAGMA journal_mode = mvcc", &[])
            .await
            .expect_err("a journal_mode switch on a capturing session must be refused");
        assert!(err.to_string().contains("CDC capture is active"), "unexpected error: {err}");

        session.execute("INSERT INTO t VALUES (1, 'after')", &[]).await.unwrap();
        let mgmt = Turso::open(&path).await.unwrap();
        let mut tailer = CdcTailer::new(&mgmt, 0);
        let mut captured_insert = false;
        while let Some(batch) = tailer.poll_batch().await.unwrap() {
            captured_insert |= batch
                .rows
                .iter()
                .any(|r| r.table_name.as_deref() == Some("t") && r.change_type == 1);
        }
        assert!(captured_insert, "the INSERT after the refused PRAGMA must reach turso_cdc");
    }

    // -----------------------------------------------------------------
    // Snapshot codec: header + chunks + end marker roundtrip, and the
    // oversized-chunk rejection. Mirrors the CDC frame_codec_* tests.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn snapshot_codec_header_chunks_end_marker_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(1 << 20);

        let header = SnapshotHeader {
            watermark: 42,
            total_len: 11,
            log_id: "snap-log-id".to_string(),
            no_copy: false,
        };
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
        assert_eq!(got_header.log_id, "snap-log-id");

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
        let snapshot = snapshot_stream_type(DEFAULT_SITE);
        assert_eq!(snapshot, "snapshot/default");
        assert!(
            snapshot.starts_with(ephpm_cluster::stream_type::SNAPSHOT_PREFIX),
            "snapshot stream type {snapshot:?} must live under the {:?} prefix",
            ephpm_cluster::stream_type::SNAPSHOT_PREFIX
        );
        // Per-site stream names live under the same prefix so one owner-side
        // `register_prefix("snapshot/")` handler routes every site.
        assert!(
            snapshot_stream_type("blog.example.com")
                .starts_with(ephpm_cluster::stream_type::SNAPSHOT_PREFIX)
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
        // The log-identity bookkeeping must never ship in a snapshot: a
        // replica that inherited the primary's log id would corrupt the
        // per-log watermark scoping the moment it was promoted.
        assert!(is_internal_object(LOG_ID_TABLE));
        assert!(is_internal_object(SOURCE_LOG_TABLE));
        assert!(is_internal_object(LOG_WATERMARKS_TABLE));
        // turso 0.7.0's autoincrement backing table for turso_cdc: its
        // sqlite_schema.sql is a real CREATE the engine refuses to
        // replay, so it must be filtered.
        assert!(is_internal_object(
            "__turso_internal_seq___turso_internal_autoincrement_turso_cdc"
        ));
        assert!(!is_internal_object("posts"));
        assert!(!is_internal_object("users"));
    }

    // -----------------------------------------------------------------
    // CDC log identity + per-log watermark scoping (issue #315).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn hello_frame_roundtrips() {
        let (mut client, mut server) = tokio::io::duplex(65536);
        write_frame(&mut client, &Frame::Hello { log_id: "abc123".into() }).await.unwrap();
        match read_frame(&mut server).await.unwrap() {
            Frame::Hello { log_id } => assert_eq!(log_id, "abc123"),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    /// A database's log identity is generated once and survives reopening
    /// the file; two databases never share one.
    #[tokio::test]
    async fn log_identity_is_persistent_and_distinct() {
        let db_a = tempfile::NamedTempFile::new().unwrap();
        let db_b = tempfile::NamedTempFile::new().unwrap();
        let path_a = db_a.path().to_str().unwrap().to_string();
        let path_b = db_b.path().to_str().unwrap().to_string();

        let id_a = {
            let t = Turso::open(&path_a).await.unwrap();
            let conn = t.raw_connection().unwrap();
            let first = ensure_log_id(&conn).await.unwrap();
            // Idempotent on the same connection...
            assert_eq!(first, ensure_log_id(&conn).await.unwrap());
            first
        };
        // ...and stable across a full reopen (this is what makes it a
        // *log* identity rather than a process identity).
        {
            let t = Turso::open(&path_a).await.unwrap();
            let conn = t.raw_connection().unwrap();
            assert_eq!(id_a, ensure_log_id(&conn).await.unwrap());
        }
        let t_b = Turso::open(&path_b).await.unwrap();
        let id_b = ensure_log_id(&t_b.raw_connection().unwrap()).await.unwrap();
        assert_ne!(id_a, id_b, "two databases must never share a log identity");
        assert_eq!(id_a.len(), 32, "128-bit hex identity");
    }

    /// The issue #315 core, as a state-machine walk: a cursor accumulated
    /// against one log must never be presented to (or guard applies from)
    /// a different log, and switching back to a previously-followed log
    /// restores its saved cursor — the primary → replica → re-promotion
    /// cycle.
    #[tokio::test]
    async fn watermark_resolution_is_scoped_to_the_source_log() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();
        let t = Turso::open(&path).await.unwrap();
        let conn = t.raw_connection().unwrap();

        // Legacy/unknown source: adopt the current litewire watermark (0
        // here) as belonging to the announced log.
        assert_eq!(resolve_subscribe_watermark(&conn, "log-A").await.unwrap(), 0);

        // Apply progress against log-A (what apply_batch would do).
        seed_watermark(&conn, 5).await.unwrap();
        // Reconnect to the same log: litewire's table is authoritative.
        assert_eq!(resolve_subscribe_watermark(&conn, "log-A").await.unwrap(), 5);

        // FAILOVER: the new primary announces a different log. Before the
        // fix the replica would have subscribed at 5 and apply_batch would
        // have skipped everything <= 5 — silent write loss. Now the cursor
        // for the new log starts at 0 and litewire's guard is re-seeded to
        // match.
        assert_eq!(resolve_subscribe_watermark(&conn, "log-B").await.unwrap(), 0);
        assert_eq!(
            read_watermark(&conn).await.unwrap(),
            0,
            "litewire's idempotency guard must follow the source switch, or every \
             batch from the new log would be skipped as 'already applied'"
        );

        // Apply progress against log-B, then switch back to log-A (the
        // old primary restarted and was re-elected): its saved cursor is
        // restored, so nothing already applied is re-requested.
        seed_watermark(&conn, 3).await.unwrap();
        assert_eq!(resolve_subscribe_watermark(&conn, "log-A").await.unwrap(), 5);
        assert_eq!(read_watermark(&conn).await.unwrap(), 5);
        // And log-B's progress was saved for a potential switch back.
        assert_eq!(resolve_subscribe_watermark(&conn, "log-B").await.unwrap(), 3);
    }

    /// The `cdc/` prefix constant this module uses matches the well-known
    /// prefix registered on the cluster channel side.
    #[test]
    fn site_from_stream_parses_and_validates() {
        // Valid site under each prefix.
        assert_eq!(
            site_from_stream("cdc/blog.example.com", ephpm_cluster::stream_type::CDC_PREFIX)
                .as_deref(),
            Some("blog.example.com")
        );
        assert_eq!(
            site_from_stream(
                "snapshot/shop.example.com",
                ephpm_cluster::stream_type::SNAPSHOT_PREFIX
            )
            .as_deref(),
            Some("shop.example.com")
        );
        // Wrong prefix, empty site, and traversal-shaped keys are all rejected
        // so a stream can never name anything but a valid site.
        assert!(site_from_stream("cdc/", ephpm_cluster::stream_type::CDC_PREFIX).is_none());
        assert!(
            site_from_stream("snapshot/x", ephpm_cluster::stream_type::CDC_PREFIX).is_none(),
            "a snapshot stream must not match the cdc prefix"
        );
        assert!(
            site_from_stream("cdc/../etc/passwd", ephpm_cluster::stream_type::CDC_PREFIX).is_none()
        );
        assert!(site_from_stream("cdc/a/b", ephpm_cluster::stream_type::CDC_PREFIX).is_none());
    }

    #[test]
    fn scan_existing_sites_finds_db_files_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::write(dir.join("blog.example.com.db"), b"").unwrap();
        std::fs::write(dir.join("shop.example.com.db"), b"").unwrap();
        // Sidecars and non-db files are ignored.
        std::fs::write(dir.join("blog.example.com.db-wal"), b"").unwrap();
        std::fs::write(dir.join("notes.txt"), b"").unwrap();
        // A file whose stem is not a valid site key is ignored.
        std::fs::write(dir.join("bad key.db"), b"").unwrap();

        let mut sites = scan_existing_sites(dir);
        sites.sort();
        assert_eq!(sites, vec!["blog.example.com".to_string(), "shop.example.com".to_string()]);

        // A missing directory yields an empty set, not an error.
        assert_eq!(scan_existing_sites(&dir.join("does-not-exist")), Vec::<String>::new());
    }

    #[test]
    fn error_classifiers_match_the_live_messages() {
        // The exact messages from the live 3-node cluster.
        let locked = anyhow::anyhow!(
            "establish CDC log identity for site site-a: cdc: create log-id table: \
             database is locked"
        );
        let collide = anyhow::anyhow!(
            "CDC apply_batch failed at change_id 2: SQLite error: Parse error: \
             table markers already exists"
        );
        assert!(is_locked_error(&locked));
        assert!(is_schema_collision_error(&collide));
        // Neither classifier fires on the other's message or an unrelated one.
        assert!(!is_schema_collision_error(&locked));
        assert!(!is_locked_error(&collide));
        let unrelated = anyhow::anyhow!("connection refused");
        assert!(!is_locked_error(&unrelated));
        assert!(!is_schema_collision_error(&unrelated));
    }

    /// The Bug 2 fix core: discarding a diverged replica's local state drops
    /// its user tables while leaving the CDC bookkeeping intact, so the owner's
    /// snapshot (plain `CREATE TABLE`) then applies clean.
    #[tokio::test]
    async fn reset_discards_user_tables_but_keeps_bookkeeping() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();
        let t = Turso::open(&path).await.unwrap();
        let conn = t.raw_connection().unwrap();

        // A local-origin user table (a stray non-owner write) + bookkeeping.
        conn.execute("CREATE TABLE markers (id INTEGER PRIMARY KEY, v TEXT)", ()).await.unwrap();
        conn.execute("INSERT INTO markers (v) VALUES ('local')", ()).await.unwrap();
        ensure_log_id(&conn).await.unwrap();
        seed_watermark(&conn, 7).await.unwrap();

        assert_eq!(
            list_user_tables(&conn).await.unwrap(),
            vec!["markers".to_string()],
            "only the user table is listed, not the __-prefixed bookkeeping"
        );

        reset_local_user_tables(&conn).await.unwrap();

        assert!(list_user_tables(&conn).await.unwrap().is_empty(), "user tables dropped");
        // Bookkeeping survives, so the fresh bootstrap's re-seed is coherent.
        assert!(read_single_text(&conn, LOG_ID_TABLE, "log_id").await.unwrap().is_some());
        assert_eq!(read_watermark(&conn).await.unwrap(), 7);

        // And the owner's schema DDL now applies clean where it used to collide.
        conn.execute("CREATE TABLE markers (id INTEGER PRIMARY KEY, v TEXT)", ()).await.unwrap();
    }

    // -----------------------------------------------------------------
    // Per-site ownership moves must not destroy data (PR #416 blocker 1).
    // -----------------------------------------------------------------

    /// Count rows in `table`, for asserting that data survived.
    async fn row_count(conn: &turso::Connection, table: &str) -> i64 {
        let mut stmt =
            conn.prepare(&format!("SELECT COUNT(*) FROM {}", quote_ident(table))).await.unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        match rows.next().await.unwrap().unwrap().get_value(0).unwrap() {
            turso::Value::Integer(i) => i,
            v => panic!("unexpected count value: {v:?}"),
        }
    }

    /// **The data-loss regression.** A node that CREATED a site has user tables
    /// and no owner-source record — it has never been anyone's replica, so
    /// nothing ever wrote one. HRW then re-homes the site (a node merely
    /// *joining* is enough) and that node enters the replica arm.
    ///
    /// The old rule read "tables but no source record" as divergence and
    /// dropped every user table before bootstrapping from the new owner, which
    /// may hold nothing at all. This asserts the origin is now recognised as
    /// the site's data lineage instead: the attempt tails, discards nothing,
    /// and the tenant's rows are still there.
    #[tokio::test]
    async fn originating_node_is_not_treated_as_a_diverged_replica() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();
        let t = Turso::open(&path).await.unwrap();
        let conn = t.raw_connection().unwrap();

        // Exactly the state of the node that created the site and has been
        // serving it as owner: real tenant data, its own CDC log identity, and
        // no `__ephpm_cdc_source` (it has never bootstrapped from anyone).
        conn.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)", ()).await.unwrap();
        conn.execute("INSERT INTO posts (title) VALUES ('hello')", ()).await.unwrap();
        conn.execute("INSERT INTO posts (title) VALUES ('world')", ()).await.unwrap();
        let own_log = ensure_log_id(&conn).await.unwrap();
        assert!(
            read_single_text(&conn, SOURCE_LOG_TABLE, "log_id").await.unwrap().is_none(),
            "precondition: an originating node has no owner-source record"
        );

        // Ownership moves away; this node evaluates a replica attempt.
        let lineage = site_data_lineage(&conn, &own_log).await.unwrap();
        assert_eq!(
            lineage.as_deref(),
            Some(own_log.as_str()),
            "the origin's lineage is its own CDC log, not 'unknown'"
        );
        assert_eq!(
            plan_replica_attempt(false, lineage.as_deref()),
            ReplicaAction::TailOnly,
            "re-homing a site must not send its originating node into a destructive \
             re-bootstrap — that is how a joining node destroyed a tenant's only copy"
        );

        // The lineage is now recorded, so it is never re-derived...
        assert_eq!(
            read_single_text(&conn, SOURCE_LOG_TABLE, "log_id").await.unwrap().as_deref(),
            Some(own_log.as_str())
        );
        // ...and the data is untouched.
        assert_eq!(row_count(&conn, "posts").await, 2);

        // A cold database still bootstraps (with nothing to discard), and a
        // PROVEN divergence still re-bootstraps destructively.
        assert_eq!(
            plan_replica_attempt(false, None),
            ReplicaAction::Bootstrap { discard_local: false }
        );
        assert_eq!(
            plan_replica_attempt(true, lineage.as_deref()),
            ReplicaAction::Bootstrap { discard_local: true }
        );
    }

    /// **The second half of the same invariant.** Even when a re-bootstrap is
    /// justified, an empty snapshot must not be able to complete it: the
    /// discard-then-apply sequence used to succeed with an empty dump, leaving
    /// the tenant with nothing. Vetting happens before anything local is
    /// touched, so the rows are still there after the refusal.
    #[tokio::test]
    async fn empty_snapshot_cannot_clobber_a_populated_local_database() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();
        let t = Turso::open(&path).await.unwrap();
        let conn = t.raw_connection().unwrap();

        conn.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)", ()).await.unwrap();
        conn.execute("INSERT INTO posts (title) VALUES ('irreplaceable')", ()).await.unwrap();

        // What a node that has never held the site serves: a valid, empty dump.
        let header = SnapshotHeader {
            watermark: 0,
            total_len: 0,
            log_id: "peer-log".into(),
            no_copy: false,
        };
        assert!(
            validate_snapshot_dump("").is_ok(),
            "precondition: an empty dump passes the statement allowlist — which is exactly \
             why the emptiness itself has to be checked"
        );

        let err = install_snapshot(&conn, &header, "", true).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("degenerate snapshot"), "unexpected error: {msg}");

        // Nothing was dropped and nothing was lost.
        assert_eq!(list_user_tables(&conn).await.unwrap(), vec!["posts".to_string()]);
        assert_eq!(row_count(&conn, "posts").await, 1);

        // A snapshot that actually carries schema IS allowed to replace it.
        install_snapshot(
            &conn,
            &header,
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
             INSERT OR REPLACE INTO posts (rowid, id, title) VALUES (1, 1, 'from-owner');\n",
            true,
        )
        .await
        .unwrap();
        assert_eq!(row_count(&conn, "posts").await, 1);

        // And an empty dump onto an EMPTY database is fine — a brand-new site
        // on a fresh cluster must still bootstrap.
        reset_local_user_tables(&conn).await.unwrap();
        install_snapshot(&conn, &header, "", false).await.unwrap();
    }

    /// A replica holding the site's data with a recorded lineage, as a
    /// re-bootstrap (`discard_local = true`) finds it.
    async fn populated_replica(conn: &turso::Connection) {
        conn.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)", ()).await.unwrap();
        conn.execute("INSERT INTO posts (title) VALUES ('kept-1')", ()).await.unwrap();
        conn.execute("INSERT INTO posts (title) VALUES ('kept-2')", ()).await.unwrap();
        seed_watermark(conn, 7).await.unwrap();
        set_source_log(conn, "old-owner-log").await.unwrap();
    }

    /// The pre-install state [`populated_replica`] wrote is intact: no table
    /// dropped, no partial table created, bookkeeping unchanged.
    async fn assert_pre_install_state(conn: &turso::Connection) {
        assert_eq!(list_user_tables(conn).await.unwrap(), vec!["posts".to_string()]);
        assert_eq!(row_count(conn, "posts").await, 2);
        assert_eq!(read_watermark(conn).await.unwrap(), 7);
        assert_eq!(
            read_single_text(conn, SOURCE_LOG_TABLE, "log_id").await.unwrap().as_deref(),
            Some("old-owner-log")
        );
    }

    /// R2 from the v0.13.0 RC stress run: an install that fails part-way must
    /// leave the replica exactly as it was, not stripped of its tables and not
    /// holding a partial copy. The dump passes the allowlist but its third
    /// statement fails at execution, after the discard and after the first
    /// table and row were written, which is where the shutdown checkpoint's
    /// `database is locked` used to land. A retry with a good dump then
    /// installs cleanly on the same connection.
    #[tokio::test]
    async fn failed_install_rolls_back_to_the_pre_install_state() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();
        let t = Turso::open(&path).await.unwrap();
        let conn = t.raw_connection().unwrap();
        populated_replica(&conn).await;

        let header = SnapshotHeader {
            watermark: 40,
            total_len: 0,
            log_id: "new-owner-log".into(),
            no_copy: false,
        };
        let failing = "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
                       INSERT OR REPLACE INTO posts (rowid, id, title) VALUES (1, 1, 'owner');\n\
                       INSERT OR REPLACE INTO missing (rowid, a) VALUES (1, 1);\n";
        let err = install_snapshot(&conn, &header, failing, true).await.unwrap_err();
        assert!(format!("{err:#}").contains("apply dump"), "unexpected error: {err:#}");
        assert!(conn.is_autocommit().unwrap(), "the failed install's transaction was ended");
        assert_pre_install_state(&conn).await;

        install_snapshot(
            &conn,
            &header,
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
             INSERT OR REPLACE INTO posts (rowid, id, title) VALUES (1, 1, 'owner');\n",
            true,
        )
        .await
        .unwrap();
        assert_eq!(row_count(&conn, "posts").await, 1);
        assert_eq!(read_watermark(&conn).await.unwrap(), 40);
        assert_eq!(
            read_single_text(&conn, SOURCE_LOG_TABLE, "log_id").await.unwrap().as_deref(),
            Some("new-owner-log")
        );
    }

    /// A process that dies mid-install never commits. Simulated by running the
    /// install's statements inside an open transaction and dropping the
    /// connection and the database without a `COMMIT`: another connection can
    /// still write (the dropped one does not keep the write lock), and after a
    /// reopen the file holds the pre-install state, so the next start's
    /// install succeeds.
    #[tokio::test]
    async fn install_interrupted_by_exit_leaves_pre_install_state_and_retries_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("site.db").to_str().unwrap().to_string();
        let header = SnapshotHeader {
            watermark: 40,
            total_len: 0,
            log_id: "new-owner-log".into(),
            no_copy: false,
        };
        let dump = snapshot_dump_with_rows(500);
        {
            let t = Turso::open(&path).await.unwrap();
            let conn = t.raw_connection().unwrap();
            populated_replica(&conn).await;

            // The install's own steps, cut off before COMMIT.
            conn.execute("BEGIN IMMEDIATE", ()).await.unwrap();
            reset_local_user_tables(&conn).await.unwrap();
            apply_snapshot(&conn, &header, &dump).await.unwrap();
            drop(conn);

            let other = t.raw_connection().unwrap();
            other.execute("CREATE TABLE IF NOT EXISTS probe (a)", ()).await.unwrap();
            other.execute("DROP TABLE probe", ()).await.unwrap();
        }

        let t = Turso::open(&path).await.unwrap();
        let conn = t.raw_connection().unwrap();
        assert_pre_install_state(&conn).await;

        install_snapshot(&conn, &header, &dump, true).await.unwrap();
        assert_eq!(row_count(&conn, "posts").await, 500);
        assert_eq!(read_watermark(&conn).await.unwrap(), 40);
    }

    /// A dump of `rows` rows of the `posts` table, shaped like
    /// `dump_user_tables` output.
    fn snapshot_dump_with_rows(rows: usize) -> String {
        use std::fmt::Write as _;
        let mut dump = String::from("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n");
        for i in 1..=rows {
            writeln!(
                dump,
                "INSERT OR REPLACE INTO posts (rowid, id, title) VALUES ({i}, {i}, 'row {i} {}');",
                "x".repeat(200)
            )
            .unwrap();
        }
        dump
    }

    /// R3: the dump commits once. A cold install of 20,000 rows (about 4.5 MB)
    /// completes in one transaction; the old per-row autocommit paid 20,000
    /// commits for the same dump.
    #[tokio::test]
    async fn large_snapshot_installs_in_one_transaction() {
        let db = tempfile::NamedTempFile::new().unwrap();
        let path = db.path().to_str().unwrap().to_string();
        let t = Turso::open(&path).await.unwrap();
        let conn = t.raw_connection().unwrap();
        let header = SnapshotHeader {
            watermark: 20_000,
            total_len: 0,
            log_id: "owner-log".into(),
            no_copy: false,
        };
        install_snapshot(&conn, &header, &snapshot_dump_with_rows(20_000), false).await.unwrap();
        assert_eq!(row_count(&conn, "posts").await, 20_000);
        assert_eq!(read_watermark(&conn).await.unwrap(), 20_000);
        assert!(conn.is_autocommit().unwrap());
    }

    #[test]
    fn degenerate_snapshot_detection() {
        assert!(!snapshot_declares_a_table(""));
        assert!(!snapshot_declares_a_table("   \n\t "));
        assert!(!snapshot_declares_a_table("-- just a comment\n"));
        // Indexes and inserts alone are not a schema.
        assert!(!snapshot_declares_a_table("CREATE INDEX i ON t (a);"));
        assert!(!snapshot_declares_a_table("INSERT OR REPLACE INTO t (rowid) VALUES (1);"));
        // A real dump declares tables, comments and odd casing included.
        assert!(snapshot_declares_a_table("CREATE TABLE t (a);"));
        assert!(snapshot_declares_a_table("create   table \"t\" (a);"));
        assert!(snapshot_declares_a_table(
            "-- note\nCREATE TABLE t (a);\nINSERT INTO t VALUES (1);"
        ));
        // A `CREATE TABLE` mentioned inside a string literal is not a
        // declaration — the scan is quote-aware.
        assert!(!snapshot_declares_a_table("INSERT INTO t VALUES ('CREATE TABLE x (a)');"));
    }

    #[test]
    fn snapshot_replacement_gate_only_bites_when_local_data_exists() {
        // Populated local + empty dump: refuse.
        assert!(snapshot_may_replace_local_data("", true).is_err());
        // Populated local + real dump: allow.
        assert!(snapshot_may_replace_local_data("CREATE TABLE t (a);", true).is_ok());
        // Cold local: anything goes, including nothing.
        assert!(snapshot_may_replace_local_data("", false).is_ok());
    }

    fn alive(id: &str, ip: &str) -> ephpm_cluster::NodeInfo {
        ephpm_cluster::NodeInfo::new(id, format!("{ip}:7946"), ephpm_cluster::NodeState::Alive)
    }

    /// The handoff asks the previous owner first — HRW over the alive set
    /// *without this node*, derivable identically on every node with no extra
    /// state — and then every other alive peer in rank order, never itself and
    /// never a member gossip does not report alive.
    #[test]
    fn handoff_candidates_rank_every_alive_peer_previous_owner_first() {
        let before = [alive("ephpm-0", "10.0.0.1"), alive("ephpm-1", "10.0.0.2")];
        let mut after = vec![
            alive("ephpm-0", "10.0.0.1"),
            alive("ephpm-1", "10.0.0.2"),
            alive("ephpm-2", "10.0.0.3"),
            alive("ephpm-3", "10.0.0.4"),
        ];
        after.push(ephpm_cluster::NodeInfo::new(
            "ephpm-dead",
            "10.0.0.9:7946",
            ephpm_cluster::NodeState::Dead,
        ));

        // A site the joiner takes over — the case the handoff exists for.
        let moved = (0..500)
            .map(|i| format!("site-{i:03}"))
            .find(|s| ephpm_cluster::hrw_owner(&after, s).unwrap().id == "ephpm-2")
            .expect("some site must land on ephpm-2");

        let ranked = handoff_candidates(&after, "ephpm-2", &moved);
        let ids: Vec<&str> = ranked.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "every alive peer, minus self and the dead member: {ids:?}");
        assert!(!ids.contains(&"ephpm-2") && !ids.contains(&"ephpm-dead"), "{ids:?}");

        // Rank 1 is exactly HRW over the alive set excluding the new owner...
        let others: Vec<_> = after.iter().filter(|n| n.id != "ephpm-2").cloned().collect();
        assert_eq!(ids[0], ephpm_cluster::hrw_owner(&others, &moved).unwrap().id);
        // ...and each later rank is HRW over what is left.
        let rest: Vec<_> = others.iter().filter(|n| n.id != ids[0]).cloned().collect();
        assert_eq!(ids[1], ephpm_cluster::hrw_owner(&rest, &moved).unwrap().id);

        // In the ordinary join case the old owner is among them (a join moves
        // a site only to the joiner, so the old owner is HRW of the old set).
        let old = ephpm_cluster::hrw_owner(&before, &moved).unwrap().id.clone();
        assert!(ids.contains(&old.as_str()));

        // A probed peer's channel address is derived from gossip membership,
        // never from an attacker-shapeable claim.
        assert_eq!(
            crate::sql_forward::member_channel_addr(&ranked[0]),
            Some(
                format!("{}:7948", ranked[0].gossip_addr.split(':').next().unwrap())
                    .parse()
                    .unwrap()
            )
        );

        // A sole member has nobody to ask.
        assert!(handoff_candidates(&before[..1], "ephpm-0", &moved).is_empty());
    }

    /// **Mixed-scheme cluster fails closed.** A confirmed peer on another
    /// ownership-hash scheme (a pre-v0.13.0 node advertises none) refuses
    /// owner-serving of every site, settled ones included; once every peer
    /// agrees, serving resumes without a restart.
    #[tokio::test]
    async fn a_mixed_scheme_cluster_refuses_owner_serving_until_all_agree() {
        let gate = HandoffGate::default();
        gate.arm();
        gate.set("shop.test", SiteServeState::Ready);
        let mut debounce = ephpm_cluster::HrwSchemeDebounce::default();
        let legacy = vec![("ephpm-old".to_string(), None)];

        // First sighting (a node whose gossip is still propagating): no flap.
        gate.set_scheme_block(scheme_block_reason(&debounce.observe(&legacy)));
        gate.wait_serveable("shop.test", Duration::ZERO).await.expect("not yet confirmed");

        // Confirmed on the second check: refused, naming the peer.
        gate.set_scheme_block(scheme_block_reason(&debounce.observe(&legacy)));
        let err = gate.wait_serveable("shop.test", Duration::from_secs(5)).await.unwrap_err();
        assert!(err.contains("ephpm-old") && err.contains("pre-v0.13.0"), "{err}");

        // The old node is upgraded (or leaves): serving resumes.
        gate.set_scheme_block(scheme_block_reason(&debounce.observe(&[])));
        gate.wait_serveable("shop.test", Duration::ZERO).await.expect("all peers agree again");
    }

    fn data_probe(log_id: &str) -> PeerProbe {
        PeerProbe::Data {
            header: SnapshotHeader {
                watermark: 7,
                total_len: 0,
                log_id: log_id.to_string(),
                no_copy: false,
            },
            dump: "CREATE TABLE t (a);".to_string(),
            received: 19,
        }
    }

    /// **The v0.13.0 hash-change gap.** After an ownership-hash change the
    /// node the new hash ranks second need not hold the site; the copy sits on
    /// whichever node the old hash made its owner. The search must get past a
    /// rank-2 "no copy" (and an unreachable peer) and take rank 3's data —
    /// and stop there.
    #[tokio::test]
    async fn the_search_takes_the_first_peer_with_data_not_the_runner_up() {
        let peers = [
            alive("rank-2", "10.0.0.2"),
            alive("rank-3", "10.0.0.3"),
            alive("rank-4", "10.0.0.4"),
            alive("rank-5", "10.0.0.5"),
        ];
        let mut asked = Vec::new();
        let search = search_peers_for_copy(&peers, |peer| {
            asked.push(peer.id.clone());
            let answer = match peer.id.as_str() {
                "rank-2" => PeerProbe::NoCopy,
                "rank-3" => PeerProbe::Unreachable("connection refused".into()),
                "rank-4" => data_probe("log-of-rank-4"),
                _ => panic!("the search must stop at the first peer with data"),
            };
            async move { answer }
        })
        .await;

        assert_eq!(asked, ["rank-2", "rank-3", "rank-4"]);
        let (peer, header, dump, _) = search.found.expect("rank 4 holds the data");
        assert_eq!(peer.id, "rank-4");
        assert_eq!(header.log_id, "log-of-rank-4");
        assert!(snapshot_declares_a_table(&dump));
        assert_eq!(search.unreachable, [("rank-3".to_string(), "connection refused".to_string())]);
    }

    /// Nobody holds a copy: the search asks everyone and finds nothing; the
    /// verdict then depends only on whether anyone failed to answer.
    #[tokio::test]
    async fn the_search_asks_every_peer_when_none_has_data() {
        let peers = [alive("a", "10.0.0.2"), alive("b", "10.0.0.3")];
        let mut asked = 0;
        let search = search_peers_for_copy(&peers, |_| {
            asked += 1;
            async { PeerProbe::NoCopy }
        })
        .await;
        assert_eq!(asked, 2);
        assert!(search.found.is_none() && search.unreachable.is_empty());
    }

    /// A peer that sends the snapshot header and part of the body, then goes
    /// quiet with the connection held open, used to pin the probe — and the
    /// site, `Pending` — forever. The body read now gives up after the stall
    /// bound and reports the peer unreachable, so the search moves on to the
    /// next peer (and the round is reported blocked if none has the data).
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stalls_mid_body_is_unreachable_not_pending_forever() {
        let (mut peer, mut local) = tokio::io::duplex(1 << 16);
        let header = SnapshotHeader {
            watermark: 7,
            total_len: 1000,
            log_id: "stalling-log".to_string(),
            no_copy: false,
        };
        write_snapshot_header(&mut peer, &header).await.unwrap();
        write_snapshot_chunk(&mut peer, b"CREATE TABLE t(x);").await.unwrap();
        peer.flush().await.unwrap();
        // ...and nothing more: `peer` stays open, like a stalled channel.

        let got = read_snapshot_header(&mut local).await.unwrap();
        let addr: SocketAddr = "10.0.0.9:7000".parse().unwrap();
        let stall = Duration::from_secs(30);
        let started = tokio::time::Instant::now();
        let probe = probe_body(&mut local, got, 1 << 20, "site-a", addr, stall).await;
        let PeerProbe::Unreachable(reason) = probe else {
            panic!("a stalled body must make the peer unreachable");
        };
        assert!(reason.contains("stopped sending"), "{reason}");
        assert!(started.elapsed() >= stall, "it waited the stall bound, not less");
        drop(peer);
    }

    /// Serving empty is licensed only by a complete set of "no copy" answers
    /// from a membership view old enough to trust; one silent peer blocks.
    #[test]
    fn handoff_verdict_never_serves_empty_past_a_silent_peer() {
        assert_eq!(handoff_verdict(0, true), HandoffVerdict::NewSite);
        assert_eq!(handoff_verdict(0, false), HandoffVerdict::AwaitMembership);
        assert_eq!(handoff_verdict(1, true), HandoffVerdict::Blocked);
        assert_eq!(handoff_verdict(3, false), HandoffVerdict::Blocked);
    }

    /// `no_copy` is wire-compatible in both directions: a normal header omits
    /// it (byte-identical to an older node's), an older node's header parses
    /// with it `false`, and an older receiver ignores it.
    #[test]
    fn the_no_copy_flag_is_compatible_with_older_peers() {
        #[derive(Serialize, Deserialize)]
        struct OldSnapshotHeader {
            watermark: i64,
            total_len: u64,
            log_id: String,
        }
        let normal =
            SnapshotHeader { watermark: 3, total_len: 9, log_id: "l".into(), no_copy: false };
        let json = serde_json::to_string(&normal).unwrap();
        assert!(!json.contains("no_copy"), "{json}");

        let old = serde_json::to_vec(&OldSnapshotHeader {
            watermark: 3,
            total_len: 9,
            log_id: "l".into(),
        })
        .unwrap();
        let parsed: SnapshotHeader = serde_json::from_slice(&old).unwrap();
        assert!(!parsed.no_copy);

        let refusal =
            SnapshotHeader { watermark: 0, total_len: 0, log_id: String::new(), no_copy: true };
        let parsed_by_old: OldSnapshotHeader =
            serde_json::from_slice(&serde_json::to_vec(&refusal).unwrap()).unwrap();
        assert_eq!(parsed_by_old.total_len, 0, "an older receiver sees an empty dump");
    }

    /// **A peer asked for a site it never held must not create its database.**
    /// Opening a Turso factory creates the file, so the snapshot handler asks
    /// the registry for an *existing* copy only and answers "no copy" — which
    /// reads back as a `no_copy` header followed straight by the end marker.
    #[tokio::test]
    async fn answering_a_snapshot_for_an_unknown_site_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SiteMgmtRegistry::new(dir.path().to_path_buf());
        let site = "never-held.test";
        let path = dir.path().join(format!("{site}.db"));

        assert!(registry.existing(site).await.unwrap().is_none());
        assert!(!path.exists(), "asking about an unknown site must not create its database");

        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        serve_no_copy(&mut server).await.unwrap();
        let header = read_snapshot_header(&mut client).await.unwrap();
        assert!(header.no_copy);
        let (dump, received) = read_snapshot_body(&mut client, &header, 1 << 20).await.unwrap();
        assert!(dump.is_empty() && received == 0);
        assert!(!path.exists());

        // A site this node does hold is served as before.
        {
            let t = Turso::open(path.to_str().unwrap()).await.unwrap();
            let conn = t.raw_connection().unwrap();
            conn.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY)", ()).await.unwrap();
        }
        let held = registry.existing(site).await.unwrap().expect("an on-disk site is held");
        let (mut client, mut server) = tokio::io::duplex(1 << 20);
        serve_snapshot_inner(&mut server, &held.factory, &held.log_id).await.unwrap();
        let header = read_snapshot_header(&mut client).await.unwrap();
        assert!(!header.no_copy);
        assert_eq!(header.log_id, held.log_id);
        let (dump, _) = read_snapshot_body(&mut client, &header, 1 << 20).await.unwrap();
        assert!(snapshot_declares_a_table(&dump), "{dump}");
    }

    #[test]
    fn peer_is_node_compares_hosts_not_ports() {
        let owner = ephpm_cluster::NodeInfo::new(
            "ephpm-1",
            "10.0.0.2:7946",
            ephpm_cluster::NodeState::Alive,
        );
        // Same host, cluster-channel port — this is what a real dial looks like.
        assert!(peer_is_node(&owner, "10.0.0.2:7948".parse().unwrap()));
        assert!(peer_is_node(&owner, "10.0.0.2:51234".parse().unwrap()));
        // A different host is not that node.
        assert!(!peer_is_node(&owner, "10.0.0.3:7948".parse().unwrap()));
        // An unparseable gossip address never matches.
        let broken =
            ephpm_cluster::NodeInfo::new("ephpm-9", "not-an-addr", ephpm_cluster::NodeState::Alive);
        assert!(!peer_is_node(&broken, "10.0.0.2:7948".parse().unwrap()));
    }

    #[test]
    fn cdc_stream_type_matches_registry_prefix() {
        let cdc = cdc_stream_type(DEFAULT_SITE);
        assert_eq!(cdc, "cdc/default");
        assert!(
            cdc.starts_with(ephpm_cluster::stream_type::CDC_PREFIX),
            "CDC stream type {cdc:?} must live under the {:?} prefix so the \
             cluster channel dispatch table stays coherent",
            ephpm_cluster::stream_type::CDC_PREFIX
        );
        // A per-site stream name routes through the same prefix handler.
        assert!(
            cdc_stream_type("shop.example.com").starts_with(ephpm_cluster::stream_type::CDC_PREFIX)
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
            other @ (Role::Primary | Role::Awaiting) => {
                panic!("expected Role::Replica, got {other:?}")
            }
        }
    }

    /// The election's explicit "no primary known yet" state (empty URL)
    /// and an unparseable address both map to `Role::Awaiting` — there is
    /// nothing to dial, and the old bogus `127.0.0.1:0` fallback dialed
    /// a dead loopback port forever.
    #[test]
    fn elected_to_role_maps_unknown_primary_to_awaiting() {
        let empty = ephpm_cluster::ElectedRole::Replica { primary_grpc_url: String::new() };
        assert!(matches!(elected_to_role(empty), Role::Awaiting));
        let garbage =
            ephpm_cluster::ElectedRole::Replica { primary_grpc_url: "not-a-host-port".into() };
        assert!(matches!(elected_to_role(garbage), Role::Awaiting));
    }
}

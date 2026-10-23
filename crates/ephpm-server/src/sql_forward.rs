//! Owner-serves SQL write-forwarding for per-site clustered replication.
//!
//! In per-site clustered mode a virtual host's writes are only captured (and
//! therefore only replicated via CDC) on the node that **owns** that site by
//! rendezvous hashing ([`ephpm_cluster::hrw_owner`]). A request for the site
//! that lands on any other node must not write to that node's local replica —
//! that write would be local-origin, unreplicated, and eventually discarded
//! when the replica re-bootstraps from the owner (see `turso_cdc`). This module
//! closes that gap: a non-owner **forwards** every `ephpm_db_*` statement to
//! the owner over the cluster channel, and the owner runs it against the site's
//! local database (capturing writes into CDC, which then replicate everywhere).
//!
//! This is the "owner-serves" first increment: a non-owner forwards **both**
//! reads and writes for a site it does not own, so read-your-writes is
//! automatic — one node serves the site's reads and writes, and every replica
//! converges through CDC. Serving reads from a local replica is a later phase.
//!
//! # Two halves
//!
//! - **Client side** ([`ClusteredSiteResolver`]) — the [`SiteBackendResolver`]
//!   the PHP `ephpm_db_*` bridge resolves each request's backend through. When
//!   this node is the site's HRW owner (or no node is alive to own it), it
//!   returns the **local** per-site backend, exactly as single-node per-site
//!   does. Otherwise it returns a [`RemoteProxyBackend`] whose connections
//!   forward each statement to the owner over `dial(owner, "sql/<site>")`
//!   **and** announces the site to this node's replication plane, so a node
//!   that only ever forwards still replicates the site — see "Replication
//!   working set" below.
//! - **Owner side** ([`spawn_owner_sql_handler`]) — a `register_prefix("sql/")`
//!   handler that parses the site, HRW-gates ("am I this site's owner?"), and
//!   runs each forwarded statement against that site's local screened/tracked
//!   backend, streaming the rowset / OK / error back.
//!
//! # Replication working set vs. the open-database LRU
//!
//! Announcing a forwarded site does **not** open that site's database in the
//! [`SiteBackends`] registry, and that separation is deliberate.
//!
//! The two are independent by construction: `SiteBackends` holds the *serving*
//! handle (LRU-bounded by `[db.sqlite] max_open_dbs`, evicted when idle),
//! while a per-site replication driver holds its own mgmt factory out of
//! `SiteMgmtRegistry` (see [`crate::turso_cdc`]). Opening the site locally
//! just to fire the announcement would consume an LRU slot for a database this
//! node is not serving from — pure cost, since the driver opens its own handle
//! regardless — and on a node forwarding many sites it would evict the
//! databases the node actually *is* serving.
//!
//! The semantics that follow, and which callers may rely on:
//!
//! * **Eviction never stops replication.** A driver's mgmt factory is not the
//!   registry's handle, so closing the registry's handle (or never opening one)
//!   leaves the `cdc/<site>` subscription untouched. There is no path by which
//!   LRU pressure silently de-replicates a tenant.
//! * **The replication working set is not bounded by `max_open_dbs`.** It is
//!   bounded by the sites this node has served or found on disk — the same
//!   bound the startup scan already established, since a replicated site has a
//!   file on disk from its first snapshot onward. `max_open_dbs` bounds
//!   *serving* handles only; budget file descriptors for both.
//! * **Announcement is idempotent and cheap.** `ensure_site_driver` dedups on a
//!   `DashMap` entry, so re-announcing a site with a running driver is one
//!   sharded lookup. The resolver therefore announces on every forwarded
//!   resolve rather than caching "already announced" — which also means a
//!   driver that exited can be restarted by ordinary traffic.
//!
//! # Transport & security
//!
//! Forwarding rides the **cluster channel**, which is already mutually
//! authenticated (shared `[cluster] secret`) and per-connection encrypted
//! (ChaCha20-Poly1305). It does **not** touch the MySQL wire or per-site
//! `pdo_mysql` credentials, so no cluster-wide `DB_PASSWORD` derivation is
//! needed for this path.
//!
//! The channel's identity is coarse — "holds the cluster secret" plus a
//! gossip-membership check on the peer address; there is no PKI and no
//! per-node-id proof. So the owner side treats it as *authentication* only and
//! adds its own *authorization*: the peer must still be a known member at
//! stream-accept time, **and** this node must be the named site's current HRW
//! owner. See [`serve_forwarded_sql`]. The trust boundary this leaves is
//! per-cluster-node, not per-tenant: any node holding the cluster secret can
//! reach any tenant database on the node that owns it. That is the same trust
//! level the CDC and snapshot streams already assume; a per-tenant credential
//! on this path is future work.
//!
//! # Transaction affinity
//!
//! Each [`RemoteProxyConn`] holds **one** channel stream for its lifetime, and
//! the owner-side handler holds **one** backend connection per stream, so a
//! forwarded `BEGIN` / … / `COMMIT` all land on the same owner connection — the
//! same per-session isolation litewire gives a wire client. The PHP bridge
//! opens one proxy connection per (thread, site) session, so a request's
//! statements share one owner connection.
//!
//! # Failure
//!
//! If the owner is unreachable or mid-election, a forwarded statement fails
//! with a normal [`BackendError`] that the bridge surfaces as an ordinary
//! `ephpm_db_*` exception — it never wedges. A transport failure also drops the
//! proxy's stream so the next statement re-dials.
//!
//! An owner that is reachable but cannot serve the statement **answers** with
//! the reason rather than closing the stream: a site database or session that
//! will not open (a busy database still reads "database is locked", which the
//! client maps to 1205) is that statement's error and is retried on the next
//! one, and a refused stream (not the owner any more, unknown peer) answers the
//! first request with a connection-shaped refusal flagged `closed`. Only a real
//! transport failure reaches the client as `owner connection closed ...`.
//!
//! # Every wait is bounded
//!
//! A forward runs on a PHP thread (`block_on`), so a wait with no bound is a
//! wedged PHP thread — and before these bounds a stalled owner did exactly
//! that: the thread sat in `read_msg` until the HTTP request timeout abandoned
//! it, and the pool spawned a replacement while the stuck one never came back
//! (stress report F6). Now:
//!
//! * the dial is bounded by the channel's connect and handshake timeouts;
//! * the request write by [`FORWARD_SEND_TIMEOUT`];
//! * the reply by [`FORWARD_REPLY_TIMEOUT`] of **silence** — the owner sends a
//!   [`SqlResponse::Progress`] heartbeat every [`PROGRESS_INTERVAL`] while a
//!   statement runs, so a slow statement is not cut off, only a stalled or
//!   partitioned owner is;
//! * on the owner, an idle stream is closed after [`OWNER_STREAM_IDLE_TIMEOUT`],
//!   releasing its pinned database and session (and any transaction a vanished
//!   client left open — stress report F7); clients retire idle streams first
//!   ([`CLIENT_STREAM_MAX_IDLE`]) so they never reuse one the owner closed.
//!
//! Both ends' sockets also run TCP keepalive (`ephpm-cluster`), so a peer
//! whose host died or was partitioned away is noticed in about 30 s even when
//! nothing is in flight. A timeout is reported with connection-shaped wording,
//! so the bridge drops the session and the next statement re-resolves the
//! site's owner.
//!
//! # Both tenant routes forward
//!
//! [`ClusteredSiteResolver`] serves **both** ways a tenant reaches its
//! database:
//!
//! * the `ephpm_db_*` bridge, via [`SiteBackendResolver`] (synchronous,
//!   `block_on` on a PHP worker thread);
//! * stock `pdo_mysql` on the multi-tenant MySQL wire listener, via
//!   [`crate::site_wire_auth::SiteWireRoute`] (asynchronous, on a tokio
//!   worker — which is why that seam exists at all: `block_on` would panic
//!   there).
//!
//! The wire route was the last hole in this design. It used to resolve the
//! node's **local** backend unconditionally, so a stock-`pdo_mysql` write that
//! landed on a non-owner committed to a replica whose writes are never
//! captured into CDC: unreplicated, invisible to the owner, and discarded the
//! next time that replica re-bootstrapped. It diverged silently.
//!
//! Two consequences of routing wire traffic through here, both intended:
//!
//! * **A non-owner no longer opens the site's database to serve wire traffic.**
//!   It announces the site (below) instead of consuming an LRU slot for a
//!   database it is not serving from — the same treatment the bridge route
//!   already got. Replication is unaffected: announcement, not opening, is what
//!   puts a node in a site's replication set.
//! * **A forwarded statement's query stats and SQL screen run on the owner**,
//!   against the owner's screened/tracked backend, not locally. litewire's own
//!   tenant-session screen still runs locally at the frontend, so the
//!   `ATTACH`/`VACUUM`/path-`PRAGMA` rejections a tenant sees are unchanged.
//!
//! Connection-lifetime semantics (resolve-at-connect, and what an ownership
//! move does to an open connection) are documented on
//! [`crate::site_wire_auth`].
//!
//! # Named locks (`GET_LOCK()` & co.)
//!
//! MySQL's named locks are session state, so in this mode they must live
//! where the site's sessions live: **on the owner**. A non-owner forwards each
//! lock operation as a [`SqlRequest::Lock`] over the same pinned stream its
//! statements use ([`RemoteProxyConn`] implements
//! [`BackendConn::user_lock`] by forwarding), and the owner runs it on the
//! stream's own backend connection — a connection the site registry scoped to
//! the site's lock namespace ([`crate::site_backends`]). Two connections for
//! one site therefore exclude each other wherever they landed: the owner's
//! local `pdo_mysql` and bridge sessions and every forwarded stream share one
//! lock table, keyed by site, on the owner.
//!
//! **Release is the stream's lifetime.** The owner-side connection — and with
//! it the `LockSession` holding the locks — is dropped when the forwarding
//! stream ends, on every path: the client's connection closed (its
//! `RemoteProxyConn` drops the stream, which closes the channel connection),
//! the client's node died (TCP keepalive / reset ends the stream), the
//! [`OWNER_STREAM_IDLE_TIMEOUT`], or a transport error mid-reply. The client
//! never has to send a release for a lock it can no longer release.
//!
//! **Ownership moves.** Ownership follows live membership, so it can move
//! while locks are held. The locks do not move with it: the old owner's
//! sessions keep theirs until they end, and the new owner starts with an empty
//! table for the site. What keeps this from granting two holders is that the
//! old owner **refuses the pinned stream** once it is no longer the owner — it
//! re-checks HRW ownership before serving a request ([`OWNER_RECHECK_INTERVAL`]
//! at most, and always before a lock operation), and on loss drops the
//! stream's session (releasing its locks), answers with a connection-shaped
//! refusal flagged `closed`, and ends the stream. The holder's next statement
//! therefore fails loudly and it must reconnect, at which point it resolves the
//! new owner, where the lock is free and must be taken again. The window this
//! leaves is a holder that issues **no** statement across the move: it keeps
//! believing it holds a lock the new owner can grant to someone else, until
//! its next statement fails. Side effects it performs during that window
//! outside the database are not protected — no lock survives its server, and
//! the same is true of MySQL across a failover. Ownership moves only on
//! membership change (a node joining or dying), not in steady state.
//!
//! An owner that dies takes every lock for the sites it owned with it, exactly
//! as a MySQL server restart would; clients get a connection error on their
//! next statement and re-resolve.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use ephpm_cluster::{
    ChannelHandle, ChannelStream, ClusterHandle, IncomingStream, NodeInfo, hrw_owner,
};
use ephpm_php::db_bridge::SiteBackendResolver;
use litewire::backend::user_lock::{LockTimeout, UserLockOp, UserLockReply};
use litewire::backend::{
    Backend, BackendConn, BackendError, Column, ExecuteResult, ResultSet, SharedBackend, Value,
};
use litewire::litewire_turso::offload::engine as off_runtime;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::site_backends::{SiteBackends, SiteOpenHook};

/// Maximum length of one length-prefixed forwarding message (64 MiB). A single
/// forwarded result set must fit — larger than the CDC frame cap because a
/// `SELECT` can legitimately return far more than one transaction's worth of
/// rows.
const MAX_MSG_LEN: u32 = 64 * 1024 * 1024;

/// How long a forwarding client waits for **any** message from the owner —
/// the reply, or a [`SqlResponse::Progress`] heartbeat — before giving up on
/// the stream.
///
/// This is an *inactivity* bound, not a statement-duration bound: an owner
/// that is still executing sends a heartbeat every [`PROGRESS_INTERVAL`], so a
/// legitimately slow statement runs as long as it needs. What it bounds is a
/// stalled owner (`SIGSTOP`, swapped out, partitioned with the connection
/// still nominally open): before it, a forward blocked in `read_msg` until TCP
/// gave up — for an idle-then-silent connection, never — and the PHP thread
/// blocked with it until the HTTP request timeout abandoned the thread
/// (stress report F6). Expiry is reported with connection-shaped wording, so
/// the bridge drops the session and the next statement re-resolves the site's
/// owner (which gossip may have moved by then).
const FORWARD_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long writing one request into the stream may take. A healthy owner
/// drains its stream continuously; a full send window means it stopped
/// reading.
const FORWARD_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// How often an owner that is still executing a forwarded statement tells the
/// client it is alive. Several heartbeats fit in [`FORWARD_REPLY_TIMEOUT`],
/// so one delayed frame never trips the client's timeout.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// How long an owner keeps an **idle** forwarding stream open (no request
/// from the client) before closing it and releasing what it pins: the site's
/// backend (which defeats the `max_open_dbs` LRU) and the owner session,
/// including any transaction a vanished client left open.
///
/// Generous on purpose — a forwarding session normally parks on an idle PHP
/// thread between requests. Clients retire their own streams well before this
/// ([`CLIENT_STREAM_MAX_IDLE`]), so an owner closing one never surprises a
/// current client. TCP keepalive on the channel (see `ephpm-cluster`) is what
/// detects a dead forwarder *quickly*; this is the backstop for a peer whose
/// kernel still answers keepalive but whose session will never speak again.
const OWNER_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A client re-dials instead of reusing a stream that has been idle this long
/// (outside a transaction), so it never writes into a stream the owner's
/// [`OWNER_STREAM_IDLE_TIMEOUT`] may already have closed. Half the owner's
/// bound, leaving ample margin for the two sides' clocks starting at
/// slightly different moments.
const CLIENT_STREAM_MAX_IDLE: Duration = Duration::from_secs(4 * 60);

/// Counter: forwarded-SQL streams given up on by a timeout, by `stage`:
/// `send` / `reply` (client side — the owner stopped reading or answering)
/// and `owner_idle` (owner side — an idle stream closed by
/// [`OWNER_STREAM_IDLE_TIMEOUT`]).
const METRIC_FORWARD_TIMEOUTS: &str = "ephpm_sql_forward_timeouts_total";

/// How long an owner may serve a pinned stream between two re-checks of its
/// HRW ownership of the site. Ownership is gated at stream accept, but it can
/// move while the stream is open (membership change); re-checking per
/// request bounds how long a demoted owner keeps serving — and keeps granting
/// named locks — to this interval. A lock operation is always re-checked (see
/// the module docs, "Named locks").
const OWNER_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Wire protocol (length-prefixed JSON, u32 BE length).
// ---------------------------------------------------------------------------

/// Wire twin of litewire's [`Value`], Serde-derived so it crosses the channel
/// without leaking derive requirements onto the litewire type. `Blob` rides as
/// a JSON byte array — correct if verbose; forwarding is a control-plane path,
/// not a bulk one.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireValue {
    Null,
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<&Value> for WireValue {
    fn from(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Integer(i) => Self::Integer(*i),
            Value::Float(f) => Self::Float(*f),
            Value::Text(s) => Self::Text(s.clone()),
            Value::Blob(b) => Self::Blob(b.clone()),
        }
    }
}

impl From<WireValue> for Value {
    fn from(v: WireValue) -> Self {
        match v {
            WireValue::Null => Self::Null,
            WireValue::Integer(i) => Self::Integer(i),
            WireValue::Float(f) => Self::Float(f),
            WireValue::Text(s) => Self::Text(s),
            WireValue::Blob(b) => Self::Blob(b),
        }
    }
}

/// Wire twin of litewire's [`Column`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireColumn {
    name: String,
    decltype: Option<String>,
}

impl From<&Column> for WireColumn {
    fn from(c: &Column) -> Self {
        Self { name: c.name.clone(), decltype: c.decltype.clone() }
    }
}

impl From<WireColumn> for Column {
    fn from(c: WireColumn) -> Self {
        Self { name: c.name, decltype: c.decltype }
    }
}

/// Wire twin of litewire's [`UserLockOp`]. `timeout` carries MySQL's own
/// convention (`< 0` forever, `0` one attempt, `> 0` seconds) so the message
/// is self-describing and version-stable.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireLockOp {
    Get { name: String, timeout: i64 },
    Release { name: String },
    ReleaseAll,
    IsFree { name: String },
    IsUsed { name: String },
}

impl From<&UserLockOp> for WireLockOp {
    fn from(op: &UserLockOp) -> Self {
        match op {
            UserLockOp::Get { name, timeout } => Self::Get {
                name: name.clone(),
                timeout: match timeout {
                    LockTimeout::Forever => -1,
                    LockTimeout::Immediate => 0,
                    LockTimeout::Wait(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                },
            },
            UserLockOp::Release { name } => Self::Release { name: name.clone() },
            UserLockOp::ReleaseAll => Self::ReleaseAll,
            UserLockOp::IsFree { name } => Self::IsFree { name: name.clone() },
            UserLockOp::IsUsed { name } => Self::IsUsed { name: name.clone() },
        }
    }
}

impl From<WireLockOp> for UserLockOp {
    fn from(op: WireLockOp) -> Self {
        match op {
            WireLockOp::Get { name, timeout } => {
                Self::Get { name, timeout: LockTimeout::from_seconds(timeout) }
            }
            WireLockOp::Release { name } => Self::Release { name },
            WireLockOp::ReleaseAll => Self::ReleaseAll,
            WireLockOp::IsFree { name } => Self::IsFree { name },
            WireLockOp::IsUsed { name } => Self::IsUsed { name },
        }
    }
}

/// A forwarded statement. `Query` expects a rowset back, `Execute` an OK — the
/// same split the litewire [`BackendConn`] trait draws, so the owner runs the
/// matching method and transaction/`RETURNING` semantics are preserved.
/// `Lock` is a named-lock operation (module docs, "Named locks"), run on the
/// stream's owner-side connection and answered with [`SqlResponse::Lock`].
///
/// `progress` asks the owner to send [`SqlResponse::Progress`] heartbeats
/// while the statement runs. Mixed-version clusters: an owner that predates
/// the field ignores it (serde skips unknown fields) and sends no heartbeats,
/// so on such an owner a statement that runs longer than
/// [`FORWARD_REPLY_TIMEOUT`] fails; and an old client never sets it, so a new
/// owner never sends an old client a frame it cannot parse. An owner that
/// predates `Lock` cannot parse it at all and ends the stream, which the
/// client reports as a connection error — a `GET_LOCK` fails rather than
/// pretends.
#[derive(Debug, Serialize, Deserialize)]
enum SqlRequest {
    Query {
        sql: String,
        params: Vec<WireValue>,
        #[serde(default)]
        progress: bool,
    },
    Execute {
        sql: String,
        params: Vec<WireValue>,
        #[serde(default)]
        progress: bool,
    },
    Lock {
        op: WireLockOp,
        #[serde(default)]
        progress: bool,
    },
}

impl SqlRequest {
    /// The statement's SQL; empty for a lock operation (which touches no
    /// transaction state).
    fn sql(&self) -> &str {
        match self {
            Self::Query { sql, .. } | Self::Execute { sql, .. } => sql,
            Self::Lock { .. } => "",
        }
    }

    fn wants_progress(&self) -> bool {
        match self {
            Self::Query { progress, .. }
            | Self::Execute { progress, .. }
            | Self::Lock { progress, .. } => *progress,
        }
    }

    fn is_lock(&self) -> bool {
        matches!(self, Self::Lock { .. })
    }
}

/// The owner's reply to a [`SqlRequest`].
#[derive(Debug, Serialize, Deserialize)]
enum SqlResponse {
    Rows {
        columns: Vec<WireColumn>,
        rows: Vec<Vec<WireValue>>,
    },
    Ok {
        affected_rows: u64,
        last_insert_rowid: Option<i64>,
    },
    /// `sqlite` distinguishes a real SQL error ([`BackendError::Sqlite`], which
    /// the client re-maps to a MySQL error code so e.g. a duplicate key stays
    /// 1062) from an infrastructure error ([`BackendError::Other`]).
    ///
    /// `closed` is set when the owner ends the stream right after this reply
    /// (it refused the stream), so the client drops it and re-dials for the
    /// next statement instead of writing into a closed stream. Defaulted so a
    /// reply from an owner that predates the field still parses, and ignored
    /// by a client that predates it.
    Error {
        sqlite: bool,
        message: String,
        #[serde(default)]
        closed: bool,
    },
    /// "Still executing" — sent every [`PROGRESS_INTERVAL`] while a statement
    /// whose request asked for it (`progress: true`) runs. Not a reply: the
    /// client keeps reading. Only ever sent to a client that asked.
    Progress,
    /// The answer to a [`SqlRequest::Lock`]: the integer MySQL would return,
    /// or `None` for `NULL`.
    Lock {
        value: Option<i64>,
    },
}

async fn write_msg<W: AsyncWriteExt + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> anyhow::Result<()> {
    let json = serde_json::to_vec(msg).context("sql-forward: serialize message")?;
    let len = u32::try_from(json.len()).context("sql-forward: message too large for u32 prefix")?;
    anyhow::ensure!(len <= MAX_MSG_LEN, "sql-forward: message too large: {len} > {MAX_MSG_LEN}");
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&json).await?;
    w.flush().await?;
    Ok(())
}

async fn read_msg<R: AsyncReadExt + Unpin, T: DeserializeOwned>(r: &mut R) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(len <= MAX_MSG_LEN, "sql-forward: message too large: {len} > {MAX_MSG_LEN}");
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("sql-forward: parse message")
}

// ---------------------------------------------------------------------------
// Client side: the remote-proxy backend and its connection.
// ---------------------------------------------------------------------------

/// A litewire [`Backend`] whose connections forward every statement to a site's
/// HRW owner over the cluster channel (`sql/<site>`). Handed to the PHP bridge
/// (through the thread-local session) by [`ClusteredSiteResolver`] when this
/// node does not own the site.
pub struct RemoteProxyBackend {
    channel: ChannelHandle,
    owner_addr: SocketAddr,
    /// `"sql/<site>"`, precomputed.
    stream_type: String,
}

impl RemoteProxyBackend {
    /// Build a proxy that forwards `site`'s statements to `owner_addr`.
    #[must_use]
    pub fn new(channel: ChannelHandle, owner_addr: SocketAddr, site: &str) -> Self {
        Self {
            channel,
            owner_addr,
            stream_type: format!("{}{site}", ephpm_cluster::stream_type::SQL_PREFIX),
        }
    }
}

#[async_trait::async_trait]
impl Backend for RemoteProxyBackend {
    async fn connect(&self) -> Result<Box<dyn BackendConn>, BackendError> {
        // Dial lazily on first statement (see RemoteProxyConn), so opening a
        // session — which the bridge does on a thread's first query — does not
        // itself pay a round trip, and a transient owner blip fails the
        // statement rather than the session open.
        Ok(Box::new(RemoteProxyConn {
            channel: self.channel.clone(),
            owner_addr: self.owner_addr,
            stream_type: self.stream_type.clone(),
            stream: Mutex::new(None),
        }))
    }
}

/// One forwarding session: owns a single channel stream to the owner for its
/// lifetime (transaction affinity), dialed lazily and re-dialed after a
/// transport failure.
struct RemoteProxyConn {
    channel: ChannelHandle,
    owner_addr: SocketAddr,
    stream_type: String,
    stream: Mutex<Option<OpenStream>>,
}

/// A forwarding session's open stream plus what the client tracks about it.
struct OpenStream {
    stream: ChannelStream,
    /// When the last reply arrived (or the stream was dialed).
    last_used: Instant,
    /// Whether the last statements left an explicit transaction open on the
    /// owner, by [`transaction_effect`]. Only consulted to decide whether an
    /// idle stream may be quietly replaced.
    in_transaction: bool,
}

/// Why one request/reply exchange on a forwarding stream failed.
#[derive(Debug)]
enum ExchangeError {
    /// Writing the request failed.
    Send(anyhow::Error),
    /// Writing the request did not finish within [`FORWARD_SEND_TIMEOUT`].
    SendTimeout,
    /// Reading the reply failed (EOF, reset, garbage).
    Recv(anyhow::Error),
    /// Nothing — no reply, no heartbeat — arrived within
    /// [`FORWARD_REPLY_TIMEOUT`].
    ReplyTimeout,
}

/// Send `req` and wait for its reply, skipping [`SqlResponse::Progress`]
/// heartbeats. Every wait is bounded (see [`FORWARD_REPLY_TIMEOUT`]); a
/// heartbeat restarts the reply wait, so only a *silent* owner times out.
async fn exchange<S>(stream: &mut S, req: &SqlRequest) -> Result<SqlResponse, ExchangeError>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    match tokio::time::timeout(FORWARD_SEND_TIMEOUT, write_msg(stream, req)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(ExchangeError::Send(e)),
        Err(_) => return Err(ExchangeError::SendTimeout),
    }
    loop {
        match tokio::time::timeout(FORWARD_REPLY_TIMEOUT, read_msg::<_, SqlResponse>(stream)).await
        {
            Ok(Ok(SqlResponse::Progress)) => {}
            Ok(Ok(resp)) => return Ok(resp),
            Ok(Err(e)) => return Err(ExchangeError::Recv(e)),
            Err(_) => return Err(ExchangeError::ReplyTimeout),
        }
    }
}

/// What a successfully executed statement does to the session's explicit
/// transaction state: `Some(true)` opens one, `Some(false)` ends it, `None`
/// leaves it as it was.
///
/// Deliberately coarse (leading keyword only). It is consulted for exactly one
/// decision — may an idle stream be replaced by a fresh one? — and errs towards
/// "in a transaction", whose only cost is keeping a stream the owner may since
/// have closed, which then fails loudly exactly as it would have without this
/// check. `ROLLBACK TO <savepoint>` and `RELEASE` therefore report `None`.
fn transaction_effect(sql: &str) -> Option<bool> {
    let mut words = sql.split_ascii_whitespace();
    let first = words.next()?.trim_end_matches(';').to_ascii_uppercase();
    match first.as_str() {
        "BEGIN" | "START" | "SAVEPOINT" => Some(true),
        "COMMIT" | "END" => Some(false),
        "ROLLBACK" => match words.next() {
            Some(next) if next.trim_end_matches(';').eq_ignore_ascii_case("TO") => None,
            _ => Some(false),
        },
        _ => None,
    }
}

/// Whether an open stream should be replaced by a fresh dial before it is
/// used: it has sat idle long enough that the owner may have closed it, and no
/// transaction would be lost by switching.
fn should_replace_idle_stream(idle: Duration, in_transaction: bool) -> bool {
    idle >= CLIENT_STREAM_MAX_IDLE && !in_transaction
}

impl RemoteProxyConn {
    /// Send `req` to the owner and read its reply, dialing the stream if it is
    /// not yet open. A transport failure drops the stream (so the next call
    /// re-dials) and is reported with connection-shaped wording, which is what
    /// lets the bridge recycle the session on a genuinely dead owner.
    ///
    /// Bounded end to end: the dial by the channel's connect and handshake
    /// timeouts, the exchange by [`FORWARD_SEND_TIMEOUT`] and
    /// [`FORWARD_REPLY_TIMEOUT`]. The caller is a PHP thread (`block_on`), so
    /// an unbounded wait here is a wedged thread.
    async fn forward(&self, req: &SqlRequest) -> Result<SqlResponse, BackendError> {
        let mut guard = self.stream.lock().await;
        if guard.as_ref().is_some_and(|open| {
            should_replace_idle_stream(open.last_used.elapsed(), open.in_transaction)
        }) {
            tracing::debug!(
                owner = %self.owner_addr,
                "forwarded-SQL: retiring an idle forwarding stream before reuse"
            );
            *guard = None;
        }
        if guard.is_none() {
            // "connection refused" is load-bearing: it is one of the
            // bridge's connection-shaped markers (`is_connection_error`).
            // `{e:#}` keeps the whole anyhow chain so the OS-level cause
            // (EADDRNOTAVAIL from port exhaustion, ECONNREFUSED, a
            // handshake timeout) is visible instead of just the outermost
            // "connect to <addr>" context.
            let dialed =
                self.channel.dial(self.owner_addr, &self.stream_type).await.map_err(|e| {
                    BackendError::Other(format!(
                        "connection refused forwarding to site owner {}: {e:#}",
                        self.owner_addr
                    ))
                })?;
            *guard = Some(OpenStream {
                stream: dialed,
                last_used: Instant::now(),
                in_transaction: false,
            });
        }
        let open = guard.as_mut().expect("stream was just ensured");
        match exchange(&mut open.stream, req).await {
            Ok(resp) => {
                open.last_used = Instant::now();
                match &resp {
                    // The owner answered and then ended the stream (it refused
                    // it): forget it so the next statement re-dials rather
                    // than failing on a closed stream.
                    SqlResponse::Error { closed: true, .. } => *guard = None,
                    SqlResponse::Error { .. }
                    | SqlResponse::Progress
                    | SqlResponse::Lock { .. } => {}
                    SqlResponse::Rows { .. } | SqlResponse::Ok { .. } => {
                        if let Some(open_now) = transaction_effect(req.sql()) {
                            open.in_transaction = open_now;
                        }
                    }
                }
                Ok(resp)
            }
            Err(e) => {
                *guard = None;
                Err(self.exchange_error(e))
            }
        }
    }

    /// Turn a failed exchange into the statement's error. Every arm is
    /// connection-shaped (`connection closed` / `error sending request` are
    /// bridge markers), so the bridge recycles the session and re-resolves the
    /// site's owner on its next statement.
    fn exchange_error(&self, e: ExchangeError) -> BackendError {
        let owner = self.owner_addr;
        BackendError::Other(match e {
            ExchangeError::Send(e) => format!("error sending request to site owner {owner}: {e}"),
            ExchangeError::SendTimeout => {
                metrics::counter!(METRIC_FORWARD_TIMEOUTS, "stage" => "send").increment(1);
                tracing::warn!(
                    owner = %owner,
                    timeout_secs = FORWARD_SEND_TIMEOUT.as_secs(),
                    "forwarded-SQL: the site owner stopped reading its forwarding stream; \
                     dropping it"
                );
                format!(
                    "owner connection closed: site owner {owner} did not accept the request \
                     within {}s (stalled or unreachable)",
                    FORWARD_SEND_TIMEOUT.as_secs()
                )
            }
            ExchangeError::Recv(e) => {
                format!("owner connection closed awaiting a reply from {owner}: {e}")
            }
            ExchangeError::ReplyTimeout => {
                metrics::counter!(METRIC_FORWARD_TIMEOUTS, "stage" => "reply").increment(1);
                tracing::warn!(
                    owner = %owner,
                    timeout_secs = FORWARD_REPLY_TIMEOUT.as_secs(),
                    "forwarded-SQL: no reply or heartbeat from the site owner; dropping the \
                     forwarding stream"
                );
                format!(
                    "owner connection closed: no reply from site owner {owner} within {}s \
                     (the owner is stalled or partitioned; any open transaction there is lost)",
                    FORWARD_REPLY_TIMEOUT.as_secs()
                )
            }
        })
    }
}

/// Reconstruct a [`BackendError`] from an [`SqlResponse::Error`], preserving the
/// SQL-vs-infrastructure distinction so the client's error map still assigns
/// the right MySQL code.
fn error_from_response(sqlite: bool, message: String) -> BackendError {
    if sqlite { BackendError::Sqlite(message) } else { BackendError::Other(message) }
}

#[async_trait::async_trait]
impl BackendConn for RemoteProxyConn {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
        let req = SqlRequest::Query {
            sql: sql.to_string(),
            params: params.iter().map(WireValue::from).collect(),
            progress: true,
        };
        match self.forward(&req).await? {
            SqlResponse::Rows { columns, rows } => Ok(ResultSet {
                columns: columns.into_iter().map(Column::from).collect(),
                rows: rows.into_iter().map(|r| r.into_iter().map(Value::from).collect()).collect(),
            }),
            SqlResponse::Error { sqlite, message, .. } => Err(error_from_response(sqlite, message)),
            SqlResponse::Ok { .. } | SqlResponse::Lock { .. } => Err(BackendError::Other(
                "site owner returned a non-rowset reply to a forwarded query".to_string(),
            )),
            // exchange() consumes heartbeats; this arm exists for exhaustiveness.
            SqlResponse::Progress => Err(BackendError::Other(
                "site owner sent a stray heartbeat in place of a reply".to_string(),
            )),
        }
    }

    /// Forward a named-lock operation to the owner, where it runs on this
    /// stream's own connection — so the lock is held by, and released with,
    /// the owner-side session this stream pins (module docs, "Named locks").
    /// A transport failure is an error, never a `0`/`NULL`: the caller must
    /// not mistake "could not ask" for "not granted".
    async fn user_lock(&self, op: &UserLockOp) -> Result<UserLockReply, BackendError> {
        let req = SqlRequest::Lock { op: WireLockOp::from(op), progress: true };
        match self.forward(&req).await? {
            SqlResponse::Lock { value } => Ok(value),
            SqlResponse::Error { sqlite, message, .. } => Err(error_from_response(sqlite, message)),
            SqlResponse::Rows { .. } | SqlResponse::Ok { .. } => Err(BackendError::Other(
                "site owner returned a statement reply to a forwarded lock operation".to_string(),
            )),
            SqlResponse::Progress => Err(BackendError::Other(
                "site owner sent a stray heartbeat in place of a reply".to_string(),
            )),
        }
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        let req = SqlRequest::Execute {
            sql: sql.to_string(),
            params: params.iter().map(WireValue::from).collect(),
            progress: true,
        };
        match self.forward(&req).await? {
            SqlResponse::Ok { affected_rows, last_insert_rowid } => {
                Ok(ExecuteResult { affected_rows, last_insert_rowid })
            }
            SqlResponse::Error { sqlite, message, .. } => Err(error_from_response(sqlite, message)),
            SqlResponse::Rows { .. } | SqlResponse::Lock { .. } => Err(BackendError::Other(
                "site owner returned a non-OK reply to a forwarded execute".to_string(),
            )),
            // exchange() consumes heartbeats; this arm exists for exhaustiveness.
            SqlResponse::Progress => Err(BackendError::Other(
                "site owner sent a stray heartbeat in place of a reply".to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Client side: the resolver that decides local vs. remote.
// ---------------------------------------------------------------------------

/// The per-site backend resolver for per-site **clustered** mode.
///
/// Wraps the node's local per-site registry and, per request's site, decides
/// whether to serve locally (this node is the HRW owner) or forward to the
/// owner. Registered with the `ephpm_db_*` bridge in place of the plain
/// [`SiteBackends`] resolver.
pub struct ClusteredSiteResolver {
    local: SiteBackends,
    cluster: Arc<ClusterHandle>,
    channel: ChannelHandle,
    self_id: String,
    handle: tokio::runtime::Handle,
    /// Announces a site to this node's replication plane. The **same** hook
    /// [`SiteBackends`] fires when it opens a site's database; the forwarding
    /// path must fire it itself, because it never opens the site locally. See
    /// the module docs, "Replication working set vs. the open-database LRU".
    note_active: SiteOpenHook,
}

impl ClusteredSiteResolver {
    /// Build a resolver over the local registry and cluster context.
    ///
    /// `note_active` must be the same site-activation hook handed to
    /// [`SiteBackends::new_clustered`], so a site announces itself exactly once
    /// per activation regardless of which branch served it.
    #[must_use]
    pub fn new(
        local: SiteBackends,
        cluster: Arc<ClusterHandle>,
        channel: ChannelHandle,
        self_id: String,
        handle: tokio::runtime::Handle,
        note_active: SiteOpenHook,
    ) -> Self {
        Self { local, cluster, channel, self_id, handle, note_active }
    }

    /// Resolve one request's backend: the local database when this node owns
    /// the site, a forwarding proxy to the owner otherwise.
    ///
    /// # Why the forwarding branch announces the site
    ///
    /// A node joins the replication working set for a site when that site is
    /// announced to the per-site driver (`ensure_site_driver` in
    /// [`crate::turso_cdc`]), which is what subscribes it to `cdc/<site>` and
    /// materializes a local replica. The only thing that used to announce a
    /// site was [`SiteBackends`] opening its database — so a node that served
    /// a site **exclusively** by forwarding never opened it, never announced
    /// it, and never replicated it. The tenant's data then lived on exactly one
    /// node while every health check stayed green, and HRW failover moved
    /// ownership to a node holding nothing.
    ///
    /// Stock `pdo_mysql` traffic used to hide the gap by incidentally opening
    /// the database locally (the wire listener resolved locally, which was its
    /// own divergence bug); the recommended deployment — the `db-*` drop-ins,
    /// which call `ephpm_db_*` only — hit it squarely. Now that **both** routes
    /// resolve through here, neither opens a non-owned site locally, and this
    /// announcement is the only thing that puts the node in the site's
    /// replication set.
    ///
    /// The announcement happens **before** the fallible owner-address lookup:
    /// a node that cannot reach the site's owner right now is precisely a node
    /// that should already be replicating it.
    ///
    /// # Errors
    ///
    /// Returns the registry's error on the local branch — or, before that, the
    /// ownership-handoff gate's refusal when this node owns the site but has
    /// not yet pulled its data from a peer (`crate::handoff_gate`) — or a
    /// message naming the site whose owner could not be addressed on the
    /// forwarding branch.
    ///
    /// # Panics
    ///
    /// Panics if [`plan_serve`] chose to forward while no node is alive to own
    /// the site. That is unreachable by construction: `should_serve_locally`
    /// returns `true` (serve locally) precisely when `hrw_owner` is `None`, so
    /// reaching the forwarding branch already implies an owner exists.
    pub async fn resolve_backend(&self, site: &str) -> Result<SharedBackend, String> {
        let nodes = self.cluster.nodes().await;
        if plan_serve(&nodes, &self.self_id, site, &self.note_active) == ServePlan::Local {
            // Owning a site is not the same as holding its data: a node that
            // has just become the owner of a site it has no copy of must pull
            // the copy before it serves (`crate::handoff_gate`).
            self.local.await_serveable(site).await?;
            return self.local.get_or_open(site).await.map_err(|e| format!("{e:#}"));
        }
        let owner = hrw_owner(&nodes, site).expect("non-local implies an alive HRW owner");
        let addr = self.owner_channel_addr(owner, site).await.ok_or_else(|| {
            format!(
                "cannot resolve the cluster-channel address of site {site:?}'s owner {} — \
                 the owner has not advertised it and its gossip address is unusable",
                owner.id
            )
        })?;
        Ok(Arc::new(RemoteProxyBackend::new(self.channel.clone(), addr, site)) as SharedBackend)
    }

    /// The channel address to forward `site` to: the owner's election-advertised
    /// address when published (exact, honours a custom `[cluster.channel]
    /// listen`), else derived from the owner's gossip address (`ip` :
    /// `gossip_port + 2`, the default channel-port rule) so a brand-new site no
    /// node has opened yet is still reachable cold.
    async fn owner_channel_addr(&self, owner: &NodeInfo, site: &str) -> Option<SocketAddr> {
        if let Some((claim_node, claim_addr)) =
            ephpm_cluster::per_site_primary(&self.cluster, site).await
            && claim_node == owner.id
            && let Ok(addr) = claim_addr.parse::<SocketAddr>()
        {
            return Some(addr);
        }
        derived_channel_addr(&owner.gossip_addr)
    }
}

impl SiteBackendResolver for ClusteredSiteResolver {
    fn resolve(&self, site_key: &str) -> Result<SharedBackend, String> {
        // block_on is legal: `resolve` is only ever called from the bridge on a
        // PHP execution thread (per-request pool / worker pool), never an
        // async task — the same invariant that licenses the plain registry's
        // `resolve`.
        //
        // The wire path must NOT come through here: it resolves from inside a
        // tokio worker (litewire's authenticator is async), where `block_on`
        // panics. It calls `resolve_backend` directly — see `SiteWireRoute`.
        self.handle.clone().block_on(self.resolve_backend(site_key))
    }
}

/// The same local-or-forward decision, for the multi-tenant MySQL wire
/// listener.
///
/// Routing stock `pdo_mysql` through the *same* resolver as the `ephpm_db_*`
/// bridge is the whole point: before this, a wire connection on a non-owner
/// resolved that node's **local** database, so the write committed to a replica
/// whose writes are never captured into CDC — unreplicated, invisible to the
/// owner, and discarded the next time that replica re-bootstraps. Silent
/// divergence, with every health check green.
///
/// This is `async` (unlike [`SiteBackendResolver::resolve`]) because litewire's
/// authenticator runs on a tokio worker, where the bridge's `block_on` would
/// panic rather than block.
#[litewire::async_trait]
impl crate::site_wire_auth::SiteWireRoute for ClusteredSiteResolver {
    async fn backend_for(&self, site_key: &str) -> anyhow::Result<SharedBackend> {
        self.resolve_backend(site_key).await.map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// How a resolve will be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServePlan {
    /// Open (or reuse) the site's local database.
    Local,
    /// Forward the site's statements to its HRW owner.
    Forward,
}

/// Decide how to serve `site`, announcing it to the replication plane on the
/// forwarding branch.
///
/// This is the whole of [`ClusteredSiteResolver::resolve_async`]'s branch
/// decision, split out as a free function so the announcement is unit-testable
/// without standing up a live multi-node cluster. That matters more than usual
/// here: the failure it guards against is silent — a forwarded-only site reads
/// and writes correctly on every node, and the missing replica only becomes
/// visible when the owner dies and takes the tenant's only copy with it.
///
/// The [`ServePlan::Local`] branch deliberately does **not** announce: the
/// registry's own open hook ([`SiteBackends::new_clustered`]) fires there, and
/// announcing twice would be redundant (harmless — `ensure_site_driver` dedups
/// — but it would leave two places claiming the same responsibility).
fn plan_serve(
    nodes: &[NodeInfo],
    self_id: &str,
    site: &str,
    note_active: &SiteOpenHook,
) -> ServePlan {
    if should_serve_locally(nodes, self_id, site) {
        return ServePlan::Local;
    }
    note_active(site);
    ServePlan::Forward
}

/// Whether this node serves `site` from its local database rather than
/// forwarding: it is the site's HRW owner, or — degrading safely — no node is
/// alive to own it.
fn should_serve_locally(nodes: &[NodeInfo], self_id: &str, site: &str) -> bool {
    match hrw_owner(nodes, site) {
        Some(owner) => owner.id == self_id,
        None => true,
    }
}

/// Derive a node's cluster-channel address from its gossip address: same IP,
/// `gossip_port + 2` (the default `resolve_listen_addr` rule). `None` if the
/// gossip address is unparseable or the port would overflow.
fn derived_channel_addr(gossip_addr: &str) -> Option<SocketAddr> {
    let gossip: SocketAddr = gossip_addr.parse().ok()?;
    let port = gossip.port().checked_add(2)?;
    Some(SocketAddr::new(gossip.ip(), port))
}

/// The cluster-channel address of a known member, derived from the address
/// gossip holds for it.
///
/// Derived rather than claim-advertised on purpose: a gossip *membership*
/// address is published by the membership layer, whereas an election claim is a
/// plain KV value any writer can shape. Callers that need to dial a member for
/// which no validated claim exists (e.g. the ownership handoff in `turso_cdc`)
/// use this and never an attacker-shapeable string.
#[must_use]
pub(crate) fn member_channel_addr(node: &NodeInfo) -> Option<SocketAddr> {
    derived_channel_addr(&node.gossip_addr)
}

/// Is `peer` an address gossip currently knows as a cluster member?
///
/// # The identity the channel gives us
///
/// The cluster channel authenticates a peer as "holds the shared `[cluster]`
/// secret" (mutual challenge/response) and encrypts the connection per session,
/// and it admits inbound connections only from IPs gossip knows. What it does
/// **not** give is a per-node-id identity: there is no PKI, so the strongest
/// statement available at a stream handler is "an authenticated member, by
/// address". This re-checks that at stream-accept time, because admission ran
/// once when the TCP connection was opened and membership can change while a
/// long-lived connection stays up.
///
/// Authentication is not authorization, so [`serve_forwarded_sql`] pairs this
/// with the HRW ownership gate: a member may only reach a tenant database on a
/// node that is currently that site's owner.
async fn peer_is_member(cluster: &ClusterHandle, peer: SocketAddr) -> bool {
    ephpm_cluster::peer_is_cluster_member(cluster, peer.ip()).await
}

// ---------------------------------------------------------------------------
// Owner side: serve forwarded statements against the local per-site backend.
// ---------------------------------------------------------------------------

/// Parse and validate the site key out of a `sql/<site>` stream type.
fn site_from_sql_stream(full: &str) -> Option<String> {
    let site = full.strip_prefix(ephpm_cluster::stream_type::SQL_PREFIX)?;
    if !site.is_empty() && crate::router::is_valid_site_key(site) {
        Some(site.to_string())
    } else {
        None
    }
}

/// Spawn the owner-side `sql/` prefix handler.
///
/// One handler routes every site: each inbound stream is parsed, HRW-gated on
/// ownership, and — if owned here — driven against that site's local backend
/// out of `registry`. Registered up front (like the CDC handlers) so a node
/// serves forwarded statements for any site it owns, opening the site's
/// database on demand.
pub fn spawn_owner_sql_handler(
    channel: &ChannelHandle,
    cluster: Arc<ClusterHandle>,
    self_id: String,
    registry: SiteBackends,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let mut streams = channel.register_prefix(ephpm_cluster::stream_type::SQL_PREFIX);
    handles.push(tokio::spawn(async move {
        while let Some(incoming) = streams.recv().await {
            let IncomingStream { stream, peer, stream_type } = incoming;
            let Some(site) = site_from_sql_stream(&stream_type) else {
                tracing::warn!(
                    peer = %peer,
                    stream = %stream_type,
                    "forwarded-SQL: unparseable or invalid site key; dropping stream"
                );
                continue;
            };
            let cluster = Arc::clone(&cluster);
            let registry = registry.clone();
            let self_id = self_id.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    serve_forwarded_sql(stream, &cluster, &self_id, &registry, &site, peer).await
                {
                    tracing::info!(peer = %peer, site = %site, "forwarded-SQL stream ended: {e:#}");
                }
            });
        }
    }));
}

/// Serve one forwarding stream: authorize the peer, HRW-gate, open the site's
/// local session once, then execute each forwarded statement against it and
/// stream the reply back.
///
/// # Authorization
///
/// Two checks, both required, because this stream grants read **and write**
/// access to one tenant's database:
///
/// 1. **The peer is a cluster member** ([`peer_is_member`]) — re-verified here
///    rather than trusted from connection admission, since membership can
///    change under a long-lived connection. This is the strongest identity the
///    channel offers (secret-holder + known address); there is no per-node PKI.
/// 2. **This node currently owns the site** by HRW. Membership alone is not
///    authorization: without this, a member could reach any tenant database on
///    any node just by naming it. Ownership is also what makes the write
///    correct — only the owner's writes are captured into the CDC log that
///    replicates.
///
/// # Failure is answered, never hung up on
///
/// Every way this side can fail is sent back as an [`SqlResponse::Error`]
/// carrying the real reason. Dropping the stream instead is what made a
/// non-owner's client report `owner connection closed ... early eof` — a
/// transport error that hid the owner's actual problem (in the stress run that
/// surfaced it: a site whose session could not open while another connection
/// held its write lock). A refused stream gets its refusal as the answer to the
/// client's first request, marked `closed` so the client re-dials; see
/// [`refuse_stream`].
async fn serve_forwarded_sql(
    mut stream: ChannelStream,
    cluster: &ClusterHandle,
    self_id: &str,
    registry: &SiteBackends,
    site: &str,
    peer: SocketAddr,
) -> anyhow::Result<()> {
    if !peer_is_member(cluster, peer).await {
        tracing::warn!(
            peer = %peer,
            site = %site,
            "forwarded-SQL: refusing a stream from an address gossip does not know as a \
             cluster member"
        );
        refuse_stream(
            &mut stream,
            format!(
                "connection refused by the node serving site {site}: this peer is not a known \
                 cluster member"
            ),
        )
        .await;
        return Ok(());
    }
    let nodes = cluster.nodes().await;
    if hrw_owner(&nodes, site).is_none_or(|n| n.id != *self_id) {
        tracing::warn!(
            peer = %peer,
            site = %site,
            "forwarded-SQL: not this site's HRW owner; refusing (peer is chasing a stale owner \
             and will re-resolve)"
        );
        // "connection refused" is one of the bridge's connection-shaped
        // markers, so the bridge recycles its session and re-resolves the
        // site's current owner — the same recovery the old hang-up triggered,
        // with the reason attached.
        refuse_stream(
            &mut stream,
            format!(
                "connection refused by node {self_id}: it is no longer the owner of site {site} \
                 (ownership moved); reconnect to reach the current owner"
            ),
        )
        .await;
        return Ok(());
    }

    tracing::debug!(peer = %peer, site = %site, "forwarded-SQL: serving owner session");
    let ownership = OwnershipCheck { cluster, self_id, site };
    serve_owner_statements(&mut stream, registry, site, Some(&ownership)).await
}

/// What [`serve_owner_statements`] needs to re-check, while a stream is open,
/// that this node still owns the site it is serving.
struct OwnershipCheck<'a> {
    cluster: &'a ClusterHandle,
    self_id: &'a str,
    site: &'a str,
}

impl OwnershipCheck<'_> {
    /// Is this node the site's HRW owner right now?
    async fn still_owner(&self) -> bool {
        let nodes = self.cluster.nodes().await;
        hrw_owner(&nodes, self.site).is_some_and(|n| n.id == *self.self_id)
    }
}

/// Whether a request on an open stream must re-verify ownership first: always
/// for a lock operation, otherwise once per [`OWNER_RECHECK_INTERVAL`].
fn ownership_recheck_due(req: &SqlRequest, last_checked: Instant) -> bool {
    req.is_lock() || last_checked.elapsed() >= OWNER_RECHECK_INTERVAL
}

/// How long a refused stream waits for the client's first request before
/// giving up on delivering the refusal. A client always sends one immediately
/// after dialing; this only bounds a peer that never does.
const REFUSAL_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Answer the client's first request on a stream this node will not serve
/// with `reason` (an infrastructure error, flagged `closed`), then let the
/// stream end. Best effort: a client that has already gone away gets nothing.
async fn refuse_stream<S>(stream: &mut S, reason: String)
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let first: Result<Result<SqlRequest, _>, _> =
        tokio::time::timeout(REFUSAL_READ_TIMEOUT, read_msg(stream)).await;
    if let Ok(Ok(_)) = first {
        let _ =
            write_msg(stream, &SqlResponse::Error { sqlite: false, message: reason, closed: true })
                .await;
    }
}

/// Serve forwarded statements on an authorized stream until the client closes
/// it.
///
/// The site's database and the owner session are opened **lazily**, on the
/// first request, and re-attempted on each later request until they open. A
/// failure to open is that request's answer — the owner's reason as an
/// [`SqlResponse::Error`], with a busy database still saying "database is
/// locked" so the client maps it to 1205 — and the stream stays up, so the
/// client's next statement simply tries again. Nothing is lost by retrying: a
/// session that never opened holds no transaction.
///
/// Holds the site's backend `Arc` for the stream's lifetime so the registry's
/// refcount-aware LRU cannot evict the database out from under the live owner
/// connection; the connection is dropped before that pin.
///
/// # Bounded lifetime
///
/// A stream that stays idle (no request) for [`OWNER_STREAM_IDLE_TIMEOUT`] is
/// closed, releasing that pin and the session — and with the session any
/// transaction a vanished client left open, which would otherwise hold the
/// site's write lock and fail every other writer with `database is locked`
/// (stress report F7). A live client never notices: it retires its own idle
/// streams after the shorter [`CLIENT_STREAM_MAX_IDLE`].
///
/// While a statement runs for a client that asked for it, a
/// [`SqlResponse::Progress`] heartbeat goes out every [`PROGRESS_INTERVAL`],
/// which is what lets the client bound its wait on a *silent* owner without
/// bounding how long a statement may legitimately take.
///
/// # Ownership is re-checked while the stream is open
///
/// The accept-time HRW gate in [`serve_forwarded_sql`] admits the stream; this
/// loop re-verifies it (`ownership`, when given) at most every
/// [`OWNER_RECHECK_INTERVAL`] and before **every** lock operation. A node that
/// has lost the site refuses the request with a connection-shaped error
/// flagged `closed`, drops the session — releasing the named locks it held and
/// any open transaction — and ends the stream, so the client re-resolves the
/// site's current owner. This is what makes "the pinned old owner refuses"
/// true at statement granularity, which the named-lock semantics rely on (see
/// the module docs).
async fn serve_owner_statements<S>(
    stream: &mut S,
    registry: &SiteBackends,
    site: &str,
    ownership: Option<&OwnershipCheck<'_>>,
) -> anyhow::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut backend: Option<SharedBackend> = None;
    let mut conn: Option<Box<dyn BackendConn>> = None;
    let mut ownership_checked = Instant::now();
    loop {
        let req: SqlRequest =
            match tokio::time::timeout(OWNER_STREAM_IDLE_TIMEOUT, read_msg(stream)).await {
                Ok(Ok(r)) => r,
                // Clean close by the client (EOF) or a torn stream — either way
                // this stream is done; the client re-dials for its next session.
                Ok(Err(_)) => break,
                Err(_) => {
                    metrics::counter!(METRIC_FORWARD_TIMEOUTS, "stage" => "owner_idle")
                        .increment(1);
                    tracing::debug!(
                        site = %site,
                        idle_secs = OWNER_STREAM_IDLE_TIMEOUT.as_secs(),
                        session_open = conn.is_some(),
                        "forwarded-SQL: closing an idle forwarding stream (dropping its session \
                         ends any transaction it held)"
                    );
                    break;
                }
            };
        if let Some(check) = ownership
            && ownership_recheck_due(&req, ownership_checked)
        {
            if !check.still_owner().await {
                tracing::warn!(
                    site = %site,
                    session_open = conn.is_some(),
                    "forwarded-SQL: ownership of the site moved away while a stream was pinned \
                     here; refusing the request and dropping the stream's session (its named \
                     locks and any open transaction are released)"
                );
                // Session before the pin (see the end of the loop).
                drop(conn.take());
                drop(backend.take());
                let _ = write_msg(
                    stream,
                    &SqlResponse::Error {
                        sqlite: false,
                        message: format!(
                            "connection refused by node {}: it is no longer the owner of site \
                             {site} (ownership moved); reconnect to reach the current owner",
                            check.self_id
                        ),
                        closed: true,
                    },
                )
                .await;
                return Ok(());
            }
            ownership_checked = Instant::now();
        }
        let progress = req.wants_progress();
        let resp = match open_owner_session(registry, site, &mut backend, &mut conn).await {
            Ok(session) if progress => execute_with_progress(stream, session, req).await?,
            Ok(session) => execute_forwarded(session, req).await,
            Err(e) => {
                tracing::info!(site = %site, "forwarded-SQL: owner session not open: {e}");
                error_response(&e)
            }
        };
        write_msg(stream, &resp).await.context("forwarded-SQL: write reply")?;
    }
    // Connection before the pin (see above).
    drop(conn);
    drop(backend);
    Ok(())
}

/// This stream's owner session, opening the site's database and a session on
/// it if they are not open yet. Errors keep the backend's own words (so a busy
/// database still classifies as 1205 on the client) and say which step failed.
async fn open_owner_session<'c>(
    registry: &SiteBackends,
    site: &str,
    backend: &mut Option<SharedBackend>,
    conn: &'c mut Option<Box<dyn BackendConn>>,
) -> Result<&'c dyn BackendConn, BackendError> {
    if conn.is_none() {
        if backend.is_none() {
            // An owner still pulling the site's data from a peer refuses
            // rather than serve (and accept writes onto) an empty copy; see
            // `crate::handoff_gate`.
            registry.await_serveable(site).await.map_err(BackendError::Other)?;
            let opened = registry.get_or_open(site).await.map_err(|e| {
                BackendError::Other(format!(
                    "could not open the database for site {site} on its owner: {e:#}"
                ))
            })?;
            *backend = Some(opened);
        }
        let pinned = backend.as_ref().expect("backend was just ensured");
        let session = pinned.connect().await.map_err(|e| {
            let context =
                format!("could not open a session on site {site}'s database on its owner");
            match e {
                BackendError::Sqlite(m) => BackendError::Sqlite(format!("{m} ({context})")),
                BackendError::Other(m) => BackendError::Other(format!("{context}: {m}")),
            }
        })?;
        *conn = Some(session);
    }
    Ok(conn.as_deref().expect("session was just ensured"))
}

/// Run one forwarded statement against the owner's local connection and shape
/// the reply. Never returns `Err`: a backend error becomes an
/// [`SqlResponse::Error`] carried back to the client.
async fn execute_forwarded(conn: &dyn BackendConn, req: SqlRequest) -> SqlResponse {
    match req {
        SqlRequest::Query { sql, params, .. } => {
            let params: Vec<Value> = params.into_iter().map(Value::from).collect();
            match conn.query(&sql, &params).await {
                Ok(rs) => SqlResponse::Rows {
                    columns: rs.columns.iter().map(WireColumn::from).collect(),
                    rows: rs.rows.iter().map(|r| r.iter().map(WireValue::from).collect()).collect(),
                },
                Err(e) => error_response(&e),
            }
        }
        SqlRequest::Execute { sql, params, .. } => {
            let params: Vec<Value> = params.into_iter().map(Value::from).collect();
            match conn.execute(&sql, &params).await {
                Ok(ok) => SqlResponse::Ok {
                    affected_rows: ok.affected_rows,
                    last_insert_rowid: ok.last_insert_rowid,
                },
                Err(e) => error_response(&e),
            }
        }
        // On the stream's own connection, which the site registry scoped to
        // the site's lock namespace: the lock is held by this session and
        // released when the stream ends.
        SqlRequest::Lock { op, .. } => match conn.user_lock(&UserLockOp::from(op)).await {
            Ok(value) => SqlResponse::Lock { value },
            Err(e) => error_response(&e),
        },
    }
}

/// [`execute_forwarded`], with a [`SqlResponse::Progress`] heartbeat written to
/// the client every [`PROGRESS_INTERVAL`] until the statement finishes.
///
/// # Errors
///
/// Only when a heartbeat cannot be written: the client is gone, so the
/// statement is abandoned (its future dropped) and the stream ends, which
/// drops the session like any other disconnect.
async fn execute_with_progress<S>(
    stream: &mut S,
    conn: &dyn BackendConn,
    req: SqlRequest,
) -> anyhow::Result<SqlResponse>
where
    S: AsyncWriteExt + Unpin,
{
    // The whole race runs off the async workers (the statement is engine
    // work). Offloading only the statement would not do: litewire offloads
    // each statement itself, and from a worker that runs the statement to
    // completion inside a `block_on` the heartbeat branch below never gets
    // to poll — so a slow statement would go silent and the client would
    // time out. Inside one outer `offload::engine` litewire's own calls run
    // in place, and the statement still yields at the engine's `Pending`
    // points, which is where the heartbeat gets its turn.
    off_runtime(async move {
        let exec = execute_forwarded(conn, req);
        tokio::pin!(exec);
        let mut heartbeat =
            tokio::time::interval_at(Instant::now() + PROGRESS_INTERVAL, PROGRESS_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                resp = &mut exec => return anyhow::Ok(resp),
                _ = heartbeat.tick() => {
                    write_msg(stream, &SqlResponse::Progress)
                        .await
                        .context("forwarded-SQL: write heartbeat (client gone)")?;
                }
            }
        }
    })
    .await
}

/// Serialize a [`BackendError`] into an [`SqlResponse::Error`], sending the
/// *inner* message (not the `Display`, which prefixes "SQLite error:") so the
/// client can rebuild the exact variant and re-map it.
fn error_response(e: &BackendError) -> SqlResponse {
    match e {
        BackendError::Sqlite(msg) => {
            SqlResponse::Error { sqlite: true, message: msg.clone(), closed: false }
        }
        BackendError::Other(msg) => {
            SqlResponse::Error { sqlite: false, message: msg.clone(), closed: false }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use ephpm_cluster::NodeState;

    use super::*;

    fn node(id: &str, state: NodeState) -> NodeInfo {
        NodeInfo::new(id, "10.0.0.1:7946", state)
    }

    #[test]
    fn serve_locally_iff_owner_or_no_alive_owner() {
        let nodes = [
            node("ephpm-0", NodeState::Alive),
            node("ephpm-1", NodeState::Alive),
            node("ephpm-2", NodeState::Alive),
        ];
        // For some site the owner is a specific node; that node serves locally,
        // the others forward.
        let owner = hrw_owner(&nodes, "tenant-a").unwrap().id.clone();
        assert!(should_serve_locally(&nodes, &owner, "tenant-a"));
        for n in &nodes {
            if n.id != owner {
                assert!(
                    !should_serve_locally(&nodes, &n.id, "tenant-a"),
                    "a non-owner must forward, not serve locally"
                );
            }
        }
        // No alive node → degrade to local rather than fail closed.
        let dead = [node("ephpm-0", NodeState::Dead)];
        assert!(should_serve_locally(&dead, "ephpm-0", "tenant-a"));
        assert!(should_serve_locally(&[], "ephpm-0", "tenant-a"));
    }

    /// A recording site-activation hook plus the sites it was told about.
    fn recording_hook() -> (SiteOpenHook, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let hook: SiteOpenHook = Arc::new(move |site: &str| {
            sink.lock().unwrap().push(site.to_string());
        });
        (hook, seen)
    }

    /// **Regression test for the bridge-only replication gap.**
    ///
    /// A node that serves a site only by forwarding must still announce it to
    /// the replication plane, or it never subscribes to `cdc/<site>` and never
    /// materializes a replica — leaving the tenant's data on exactly one node
    /// while every health check passes. Before the fix this branch returned a
    /// `RemoteProxyBackend` and announced nothing.
    ///
    /// This used to be invisible to any test that drove `pdo_mysql`, because
    /// that path opened the database locally and announced as a side effect —
    /// it only bit the recommended `ephpm_db_*`-only deployment. Both routes
    /// now resolve through `plan_serve`, so this branch is the sole announcer
    /// for a non-owned site and the assertion covers both.
    #[test]
    fn forwarding_a_site_announces_it_to_the_replication_plane() {
        let nodes = [
            node("ephpm-0", NodeState::Alive),
            node("ephpm-1", NodeState::Alive),
            node("ephpm-2", NodeState::Alive),
        ];
        let owner = hrw_owner(&nodes, "tenant-a").unwrap().id.clone();

        for n in &nodes {
            if n.id == owner {
                continue;
            }
            let (hook, seen) = recording_hook();
            let plan = plan_serve(&nodes, &n.id, "tenant-a", &hook);
            assert_eq!(plan, ServePlan::Forward, "a non-owner must forward");
            assert_eq!(
                seen.lock().unwrap().as_slice(),
                ["tenant-a".to_string()],
                "a forwarded site must be announced so this node starts replicating it"
            );
        }
    }

    /// The owner branch does not announce: the registry's open hook fires there
    /// (`SiteBackends::new_clustered`), and one announcement per activation
    /// keeps a single place responsible.
    #[test]
    fn serving_a_site_locally_leaves_the_announcement_to_the_registry() {
        let nodes = [
            node("ephpm-0", NodeState::Alive),
            node("ephpm-1", NodeState::Alive),
            node("ephpm-2", NodeState::Alive),
        ];
        let owner = hrw_owner(&nodes, "tenant-a").unwrap().id.clone();

        let (hook, seen) = recording_hook();
        assert_eq!(plan_serve(&nodes, &owner, "tenant-a", &hook), ServePlan::Local);
        assert!(
            seen.lock().unwrap().is_empty(),
            "the local branch must not announce; SiteBackends::get_or_open does"
        );

        // Degraded case: no alive node to own the site. Serving locally is the
        // safe degradation, and it likewise announces via the registry.
        let (hook, seen) = recording_hook();
        assert_eq!(plan_serve(&[], "ephpm-0", "tenant-a", &hook), ServePlan::Local);
        assert!(seen.lock().unwrap().is_empty());
    }

    /// Announcement is per-resolve, not once-per-process: `ensure_site_driver`
    /// dedups, and re-announcing is what lets ordinary traffic restart a driver
    /// that exited. A resolver that cached "already announced" would leave a
    /// site permanently unreplicated after one driver failure.
    #[test]
    fn every_forwarded_resolve_announces() {
        let nodes = [
            node("ephpm-0", NodeState::Alive),
            node("ephpm-1", NodeState::Alive),
            node("ephpm-2", NodeState::Alive),
        ];
        let owner = hrw_owner(&nodes, "tenant-a").unwrap().id.clone();
        let non_owner = nodes.iter().find(|n| n.id != owner).unwrap().id.clone();

        let (hook, seen) = recording_hook();
        for _ in 0..3 {
            plan_serve(&nodes, &non_owner, "tenant-a", &hook);
        }
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    /// The owner side needs BOTH gates. HRW ownership alone would let any
    /// cluster member reach any tenant database on any node just by naming it;
    /// membership alone would let a member reach a database on a node that is
    /// not the site's owner (whose writes are not captured for replication and
    /// would be discarded). This pins the ownership half — the membership half
    /// is `peer_is_member`, which needs a live gossip mesh.
    #[test]
    fn only_the_current_owner_may_serve_a_forwarded_stream() {
        let nodes = [
            node("ephpm-0", NodeState::Alive),
            node("ephpm-1", NodeState::Alive),
            node("ephpm-2", NodeState::Alive),
        ];
        let owner = hrw_owner(&nodes, "tenant-a").unwrap().id.clone();
        // The gate `serve_forwarded_sql` applies, stated directly.
        let owns = |me: &str| hrw_owner(&nodes, "tenant-a").is_some_and(|n| n.id == *me);
        assert!(owns(&owner));
        for n in &nodes {
            if n.id != owner {
                assert!(!owns(&n.id), "a non-owner must refuse a forwarded-SQL stream");
            }
        }
        // No alive node at all: refuse rather than serve (fail closed) — the
        // resolver's "degrade to local" only applies on the client side.
        assert!(hrw_owner(&[], "tenant-a").is_none());
    }

    #[test]
    fn member_channel_addr_is_derived_from_gossip_membership() {
        let n = NodeInfo::new("ephpm-1", "10.0.0.2:7946", NodeState::Alive);
        assert_eq!(member_channel_addr(&n), Some("10.0.0.2:7948".parse().unwrap()));
        let broken = NodeInfo::new("ephpm-2", "nonsense", NodeState::Alive);
        assert_eq!(member_channel_addr(&broken), None);
    }

    #[test]
    fn derived_channel_addr_is_gossip_port_plus_two() {
        assert_eq!(
            derived_channel_addr("192.168.165.48:7946"),
            Some("192.168.165.48:7948".parse().unwrap())
        );
        assert_eq!(derived_channel_addr("[::1]:7946"), Some("[::1]:7948".parse().unwrap()));
        assert_eq!(derived_channel_addr("not-an-addr"), None);
    }

    #[test]
    fn site_from_sql_stream_parses_and_validates() {
        assert_eq!(
            site_from_sql_stream("sql/blog.example.com").as_deref(),
            Some("blog.example.com")
        );
        assert!(site_from_sql_stream("sql/").is_none());
        assert!(site_from_sql_stream("cdc/x").is_none(), "wrong prefix must not match");
        assert!(site_from_sql_stream("sql/../etc/passwd").is_none());
        assert!(site_from_sql_stream("sql/a/b").is_none());
    }

    #[tokio::test]
    async fn request_response_codec_roundtrips() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let req = SqlRequest::Execute {
            sql: "INSERT INTO t (v) VALUES (?)".into(),
            params: vec![
                WireValue::Text("x".into()),
                WireValue::Blob(vec![0, 1, 255]),
                WireValue::Null,
            ],
            progress: true,
        };
        write_msg(&mut a, &req).await.unwrap();
        let got: SqlRequest = read_msg(&mut b).await.unwrap();
        match got {
            SqlRequest::Execute { sql, params, progress } => {
                assert!(progress);
                assert_eq!(sql, "INSERT INTO t (v) VALUES (?)");
                assert_eq!(params.len(), 3);
                assert!(matches!(params[1], WireValue::Blob(ref bytes) if bytes == &[0, 1, 255]));
            }
            SqlRequest::Query { .. } | SqlRequest::Lock { .. } => panic!("expected Execute"),
        }

        let resp = SqlResponse::Rows {
            columns: vec![WireColumn { name: "id".into(), decltype: Some("INTEGER".into()) }],
            rows: vec![vec![WireValue::Integer(42)]],
        };
        write_msg(&mut a, &resp).await.unwrap();
        let got: SqlResponse = read_msg(&mut b).await.unwrap();
        match got {
            SqlResponse::Rows { columns, rows } => {
                assert_eq!(columns[0].name, "id");
                assert!(matches!(rows[0][0], WireValue::Integer(42)));
            }
            _ => panic!("expected Rows"),
        }
    }

    #[test]
    fn error_response_preserves_the_sql_vs_infra_distinction() {
        // A SQL error round-trips as Sqlite (so the client re-maps 1062 etc.);
        // the inner message is sent, not the "SQLite error:"-prefixed Display.
        let sql_err = BackendError::Sqlite("UNIQUE constraint failed: t.id".into());
        match error_response(&sql_err) {
            SqlResponse::Error { sqlite, message, closed } => {
                assert!(!closed, "a statement error never ends the stream");
                assert!(sqlite);
                assert_eq!(message, "UNIQUE constraint failed: t.id");
            }
            _ => panic!("expected Error"),
        }
        match error_response(&BackendError::Other("boom".into())) {
            SqlResponse::Error { sqlite, message, closed } => {
                assert!(!closed, "a statement error never ends the stream");
                assert!(!sqlite);
                assert_eq!(message, "boom");
            }
            _ => panic!("expected Error"),
        }
    }

    /// A stream the owner will not serve is answered, not hung up on: the
    /// client's first request gets the refusal as an infrastructure error
    /// flagged `closed`, so the client reports the reason (and re-dials)
    /// instead of `owner connection closed ... early eof`.
    #[tokio::test]
    async fn a_refused_stream_answers_the_first_request_with_the_reason() {
        let (mut client, mut owner) = tokio::io::duplex(64 * 1024);
        let refusal = tokio::spawn(async move {
            refuse_stream(&mut owner, "connection refused by node n2: not the owner".into()).await;
        });
        write_msg(&mut client, &query("SELECT 1")).await.unwrap();
        match read_msg::<_, SqlResponse>(&mut client).await.unwrap() {
            SqlResponse::Error { sqlite, message, closed } => {
                assert!(!sqlite, "a refusal is infrastructure, not SQL");
                assert!(closed, "the client must drop the stream it was refused on");
                assert!(message.contains("not the owner"), "{message}");
                assert!(
                    message.contains("connection refused"),
                    "must stay connection-shaped so the bridge recycles and re-resolves: {message}"
                );
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
        refusal.await.unwrap();
    }

    fn query(sql: &str) -> SqlRequest {
        SqlRequest::Query { sql: sql.into(), params: vec![], progress: true }
    }

    /// A lock operation round-trips with MySQL's timeout convention intact,
    /// and its reply distinguishes `NULL` from an integer.
    #[tokio::test]
    async fn lock_request_and_reply_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        for (op, secs) in [
            (UserLockOp::Get { name: "job".into(), timeout: LockTimeout::Forever }, -1),
            (UserLockOp::Get { name: "job".into(), timeout: LockTimeout::Immediate }, 0),
            (
                UserLockOp::Get {
                    name: "job".into(),
                    timeout: LockTimeout::Wait(Duration::from_secs(7)),
                },
                7,
            ),
        ] {
            let req = SqlRequest::Lock { op: WireLockOp::from(&op), progress: true };
            assert!(req.is_lock());
            assert_eq!(req.sql(), "", "a lock op has no SQL and no transaction effect");
            assert_eq!(transaction_effect(req.sql()), None);
            write_msg(&mut a, &req).await.unwrap();
            let got: SqlRequest = read_msg(&mut b).await.unwrap();
            let SqlRequest::Lock { op: wire, progress } = got else { panic!("expected Lock") };
            assert!(progress);
            assert!(matches!(&wire, WireLockOp::Get { timeout, .. } if *timeout == secs));
            assert_eq!(UserLockOp::from(wire), op);
        }
        for value in [Some(1), Some(0), None] {
            write_msg(&mut a, &SqlResponse::Lock { value }).await.unwrap();
            let got: SqlResponse = read_msg(&mut b).await.unwrap();
            assert!(matches!(got, SqlResponse::Lock { value: v } if v == value));
        }
    }

    /// A lock operation always re-checks ownership; statements do so once per
    /// interval.
    #[test]
    fn lock_operations_always_recheck_ownership() {
        let lock = SqlRequest::Lock { op: WireLockOp::ReleaseAll, progress: false };
        assert!(ownership_recheck_due(&lock, Instant::now()));
        assert!(!ownership_recheck_due(&query("SELECT 1"), Instant::now()));
        assert!(ownership_recheck_due(&query("SELECT 1"), Instant::now() - OWNER_RECHECK_INTERVAL));
    }

    fn execute(sql: &str) -> SqlRequest {
        SqlRequest::Execute { sql: sql.into(), params: vec![], progress: true }
    }

    /// The bounds only work as a set: several heartbeats must fit in the
    /// client's silence budget, and clients must retire idle streams well
    /// before owners close them.
    #[test]
    fn timeout_constants_are_consistent() {
        assert!(PROGRESS_INTERVAL * 3 <= FORWARD_REPLY_TIMEOUT);
        assert!(CLIENT_STREAM_MAX_IDLE * 2 <= OWNER_STREAM_IDLE_TIMEOUT);
    }

    /// **F6 regression.** An owner that accepts the request and then goes
    /// silent (stopped process, partition with the connection still up) used
    /// to block the forwarding PHP thread until TCP gave up — for an idle
    /// connection, never. The exchange now gives up after
    /// `FORWARD_REPLY_TIMEOUT` of silence. In-memory duplex, so the paused
    /// clock never races real I/O.
    #[tokio::test(start_paused = true)]
    async fn a_silent_owner_times_out_instead_of_wedging_the_caller() {
        let (mut client, mut owner) = tokio::io::duplex(64 * 1024);
        let silent_owner = tokio::spawn(async move {
            let _req: SqlRequest = read_msg(&mut owner).await.unwrap();
            // Hold the stream open and never answer.
            tokio::time::sleep(Duration::from_secs(3600)).await;
            drop(owner);
        });
        let started = Instant::now();
        let err = exchange(&mut client, &execute("INSERT INTO t VALUES (1)")).await.unwrap_err();
        assert!(matches!(err, ExchangeError::ReplyTimeout), "{err:?}");
        let waited = started.elapsed();
        assert!(
            waited >= FORWARD_REPLY_TIMEOUT && waited < FORWARD_REPLY_TIMEOUT * 2,
            "gave up after {waited:?}"
        );
        silent_owner.abort();
    }

    /// The timeout bounds *silence*, not statement duration: an owner that
    /// keeps heartbeating may take far longer than `FORWARD_REPLY_TIMEOUT`.
    #[tokio::test(start_paused = true)]
    async fn heartbeats_keep_a_slow_statement_alive() {
        let (mut client, mut owner) = tokio::io::duplex(64 * 1024);
        let slow_owner = tokio::spawn(async move {
            let _req: SqlRequest = read_msg(&mut owner).await.unwrap();
            for _ in 0..24 {
                tokio::time::sleep(PROGRESS_INTERVAL).await;
                write_msg(&mut owner, &SqlResponse::Progress).await.unwrap();
            }
            write_msg(&mut owner, &SqlResponse::Ok { affected_rows: 1, last_insert_rowid: None })
                .await
                .unwrap();
            owner
        });
        let started = Instant::now();
        let resp = exchange(&mut client, &execute("UPDATE t SET x = 1")).await.unwrap();
        assert!(matches!(resp, SqlResponse::Ok { affected_rows: 1, .. }), "{resp:?}");
        assert!(started.elapsed() >= Duration::from_secs(120), "the statement ran 2 minutes");
        drop(slow_owner.await.unwrap());
    }

    /// A send into a stream the owner stopped draining is bounded too.
    #[tokio::test(start_paused = true)]
    async fn a_full_send_window_times_out() {
        // A 16-byte pipe nobody reads: the request cannot be written.
        let (mut client, _owner) = tokio::io::duplex(16);
        let err = exchange(&mut client, &execute("INSERT INTO t VALUES (1)")).await.unwrap_err();
        assert!(matches!(err, ExchangeError::SendTimeout), "{err:?}");
    }

    /// A stub session whose statements take `delay` (on the tokio clock).
    struct SlowConn {
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl BackendConn for SlowConn {
        async fn query(&self, _sql: &str, _params: &[Value]) -> Result<ResultSet, BackendError> {
            tokio::time::sleep(self.delay).await;
            Ok(ResultSet { columns: vec![], rows: vec![] })
        }

        async fn execute(
            &self,
            _sql: &str,
            _params: &[Value],
        ) -> Result<ExecuteResult, BackendError> {
            tokio::time::sleep(self.delay).await;
            Ok(ExecuteResult { affected_rows: 0, last_insert_rowid: None })
        }
    }

    /// The owner half of the heartbeat: a statement that runs 12 s produces
    /// two heartbeats (at 5 s and 10 s), then the reply is returned for the
    /// caller to write.
    #[tokio::test(start_paused = true)]
    async fn the_owner_heartbeats_while_a_statement_runs() {
        let (mut owner, mut client) = tokio::io::duplex(64 * 1024);
        let conn = SlowConn { delay: Duration::from_secs(12) };
        let resp = execute_with_progress(&mut owner, &conn, query("SELECT 1")).await.unwrap();
        assert!(matches!(resp, SqlResponse::Rows { .. }), "{resp:?}");
        drop(owner);
        let mut heartbeats = 0;
        while let Ok(msg) = read_msg::<_, SqlResponse>(&mut client).await {
            assert!(matches!(msg, SqlResponse::Progress), "only heartbeats precede the reply");
            heartbeats += 1;
        }
        assert_eq!(heartbeats, 2);
    }

    /// A stub session shaped like litewire's Turso session: the statement runs
    /// under `offload::engine` (so on a worker it would `block_on` to
    /// completion) and yields while it runs.
    struct OffloadedSlowConn {
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl BackendConn for OffloadedSlowConn {
        async fn query(&self, _sql: &str, _params: &[Value]) -> Result<ResultSet, BackendError> {
            off_runtime(tokio::time::sleep(self.delay)).await;
            Ok(ResultSet { columns: vec![], rows: vec![] })
        }

        async fn execute(
            &self,
            _sql: &str,
            _params: &[Value],
        ) -> Result<ExecuteResult, BackendError> {
            off_runtime(tokio::time::sleep(self.delay)).await;
            Ok(ExecuteResult { affected_rows: 0, last_insert_rowid: None })
        }
    }

    /// The heartbeat still flows on a multi-thread runtime, where the session
    /// offloads its statement: the owner's whole race is offloaded, so the
    /// statement's own offload runs in place and yields to the heartbeat
    /// instead of running to completion in a nested `block_on`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_owner_heartbeats_while_an_offloaded_statement_runs() {
        let (mut owner, mut client) = tokio::io::duplex(64 * 1024);
        let served = tokio::spawn(async move {
            let conn = OffloadedSlowConn { delay: PROGRESS_INTERVAL + Duration::from_millis(700) };
            let resp = execute_with_progress(&mut owner, &conn, query("SELECT 1")).await.unwrap();
            assert!(matches!(resp, SqlResponse::Rows { .. }), "{resp:?}");
        });
        served.await.unwrap();
        let mut heartbeats = 0;
        while let Ok(msg) = read_msg::<_, SqlResponse>(&mut client).await {
            assert!(matches!(msg, SqlResponse::Progress), "only heartbeats precede the reply");
            heartbeats += 1;
        }
        assert_eq!(heartbeats, 1, "the heartbeat at {PROGRESS_INTERVAL:?} was not sent");
    }

    /// A client that vanished mid-statement ends the owner's work instead of
    /// leaving it to run against nobody.
    #[tokio::test(start_paused = true)]
    async fn the_owner_abandons_a_statement_whose_client_is_gone() {
        let (mut owner, client) = tokio::io::duplex(64 * 1024);
        drop(client);
        let conn = SlowConn { delay: Duration::from_secs(3600) };
        let err = execute_with_progress(&mut owner, &conn, query("SELECT 1")).await.unwrap_err();
        assert!(format!("{err:#}").contains("client gone"), "{err:#}");
    }

    /// Mixed versions, both directions. A request from a client that predates
    /// `progress` parses as "no heartbeats" (so a new owner never sends an old
    /// client a frame it cannot read); a new request still parses on an owner
    /// that predates the field.
    #[test]
    fn the_progress_flag_is_compatible_with_older_peers() {
        let old_client: SqlRequest =
            serde_json::from_str(r#"{"Execute":{"sql":"SELECT 1","params":[]}}"#).unwrap();
        assert!(!old_client.wants_progress());

        #[derive(Deserialize)]
        #[allow(dead_code, reason = "shape-only twin of the pre-heartbeat request")]
        enum OldSqlRequest {
            Query { sql: String, params: Vec<WireValue> },
            Execute { sql: String, params: Vec<WireValue> },
        }
        let new_json = serde_json::to_string(&execute("SELECT 1")).unwrap();
        let parsed: OldSqlRequest = serde_json::from_str(&new_json).unwrap();
        assert!(matches!(parsed, OldSqlRequest::Execute { .. }));
    }

    #[test]
    fn transaction_effect_reads_the_leading_keyword() {
        for sql in ["BEGIN", "begin immediate", "  BEGIN TRANSACTION;", "START TRANSACTION"] {
            assert_eq!(transaction_effect(sql), Some(true), "{sql}");
        }
        assert_eq!(transaction_effect("SAVEPOINT a"), Some(true));
        for sql in ["COMMIT", "commit;", "END", "ROLLBACK", "rollback;"] {
            assert_eq!(transaction_effect(sql), Some(false), "{sql}");
        }
        for sql in ["ROLLBACK TO a", "rollback to savepoint a", "RELEASE a", "SELECT 1", ""] {
            assert_eq!(transaction_effect(sql), None, "{sql}");
        }
    }

    /// A client replaces a stream only when it is idle long enough that the
    /// owner may have closed it AND no transaction would be lost by it.
    #[test]
    fn an_idle_stream_is_replaced_only_outside_a_transaction() {
        assert!(!should_replace_idle_stream(Duration::from_secs(10), false));
        assert!(should_replace_idle_stream(CLIENT_STREAM_MAX_IDLE, false));
        assert!(!should_replace_idle_stream(CLIENT_STREAM_MAX_IDLE * 10, true));
    }

    fn registry(dir: &std::path::Path) -> SiteBackends {
        SiteBackends::new(
            dir.to_path_buf(),
            8,
            ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig::default()),
            tokio::runtime::Handle::current(),
        )
        .unwrap()
    }

    /// **F7 regression.** An owner-side stream whose client never speaks
    /// again is closed after `OWNER_STREAM_IDLE_TIMEOUT` instead of living (and
    /// pinning the site's database) forever. No statement is sent, so no real
    /// I/O runs under the paused clock.
    #[tokio::test(start_paused = true)]
    async fn the_owner_closes_an_idle_forwarding_stream() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry(dir.path());
        let (mut owner, mut client) = tokio::io::duplex(64 * 1024);
        let started = Instant::now();
        let served = tokio::spawn(async move {
            serve_owner_statements(&mut owner, &registry, "s1", None).await.unwrap();
            owner
        });
        let owner = served.await.unwrap();
        let waited = started.elapsed();
        assert!(waited >= OWNER_STREAM_IDLE_TIMEOUT, "closed after only {waited:?}");
        drop(owner);
        assert!(read_msg::<_, SqlResponse>(&mut client).await.is_err(), "stream ended");
    }

    /// **F7, the expensive half.** A forwarder that vanished inside a
    /// transaction used to leave it open on the owner — holding the site's
    /// write lock, so every other writer failed `database is locked` — until
    /// the stream died, which for a silent peer could be never. After the idle
    /// close the lock is released and the uncommitted row is gone.
    ///
    /// The statements run on the real clock (they do real file I/O); the clock
    /// is only paused for the idle wait, when nothing but a duplex is pending.
    #[tokio::test]
    async fn an_idle_close_releases_a_transaction_the_client_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry(dir.path());
        let (mut owner, mut client) = tokio::io::duplex(64 * 1024);
        let owner_registry = registry.clone();
        let served = tokio::spawn(async move {
            serve_owner_statements(&mut owner, &owner_registry, "s1", None).await.unwrap();
        });

        for sql in ["CREATE TABLE t (x INTEGER)", "BEGIN", "INSERT INTO t VALUES (1)"] {
            let resp = exchange(&mut client, &execute(sql)).await.unwrap();
            assert!(matches!(resp, SqlResponse::Ok { .. }), "{sql}: {resp:?}");
        }

        // The client goes silent with its transaction open.
        tokio::time::pause();
        tokio::time::advance(OWNER_STREAM_IDLE_TIMEOUT + Duration::from_secs(1)).await;
        tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .expect("the owner closes the idle stream")
            .unwrap();
        tokio::time::resume();

        // Another writer can now take the lock, and the abandoned row was
        // rolled back.
        let backend = registry.get_or_open("s1").await.unwrap();
        let other = backend.connect().await.unwrap();
        other.execute("INSERT INTO t VALUES (2)", &[]).await.expect("write lock was released");
        let rs = other.query("SELECT x FROM t", &[]).await.unwrap();
        assert_eq!(rs.rows.len(), 1, "only the second writer's row survives");
        assert!(matches!(rs.rows[0][0], Value::Integer(2)));
    }

    /// Mixed-version clusters: an error reply from an owner that predates the
    /// `closed` flag still parses, as "stream stays open".
    #[test]
    fn an_error_reply_without_the_closed_flag_still_parses() {
        let resp: SqlResponse =
            serde_json::from_str(r#"{"Error":{"sqlite":true,"message":"database is locked"}}"#)
                .unwrap();
        assert!(matches!(resp, SqlResponse::Error { sqlite: true, closed: false, .. }));
    }
}

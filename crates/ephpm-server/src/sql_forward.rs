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
//! # Known gap (this increment)
//!
//! Forwarding is wired into the `ephpm_db_*` bridge only. A `pdo_mysql`
//! connection to a non-owner node still resolves that node's **local** backend
//! (the per-site MySQL wire listener is unchanged), so a stock-`pdo_mysql`
//! write to a non-owner is not forwarded. Apps using the `db-*` drop-ins (which
//! call `ephpm_db_*`) get forwarding; that is the supported per-site clustered
//! path.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context as _;
use ephpm_cluster::{
    ChannelHandle, ChannelStream, ClusterHandle, IncomingStream, NodeInfo, hrw_owner,
};
use ephpm_php::db_bridge::SiteBackendResolver;
use litewire::backend::{
    Backend, BackendConn, BackendError, Column, ExecuteResult, ResultSet, SharedBackend, Value,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::site_backends::{SiteBackends, SiteOpenHook};

/// Maximum length of one length-prefixed forwarding message (64 MiB). A single
/// forwarded result set must fit — larger than the CDC frame cap because a
/// `SELECT` can legitimately return far more than one transaction's worth of
/// rows.
const MAX_MSG_LEN: u32 = 64 * 1024 * 1024;

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

/// A forwarded statement. `Query` expects a rowset back, `Execute` an OK — the
/// same split the litewire [`BackendConn`] trait draws, so the owner runs the
/// matching method and transaction/`RETURNING` semantics are preserved.
#[derive(Debug, Serialize, Deserialize)]
enum SqlRequest {
    Query { sql: String, params: Vec<WireValue> },
    Execute { sql: String, params: Vec<WireValue> },
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
    Error {
        sqlite: bool,
        message: String,
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
    stream: Mutex<Option<ChannelStream>>,
}

impl RemoteProxyConn {
    /// Send `req` to the owner and read its reply, dialing the stream if it is
    /// not yet open. A transport failure drops the stream (so the next call
    /// re-dials) and is reported with connection-shaped wording, which is what
    /// lets the bridge recycle the session on a genuinely dead owner.
    async fn forward(&self, req: &SqlRequest) -> Result<SqlResponse, BackendError> {
        let mut guard = self.stream.lock().await;
        if guard.is_none() {
            let dialed =
                self.channel.dial(self.owner_addr, &self.stream_type).await.map_err(|e| {
                    BackendError::Other(format!(
                        "connection refused forwarding to site owner {}: {e}",
                        self.owner_addr
                    ))
                })?;
            *guard = Some(dialed);
        }
        let stream = guard.as_mut().expect("stream was just ensured");
        if let Err(e) = write_msg(stream, req).await {
            *guard = None;
            return Err(BackendError::Other(format!(
                "error sending request to site owner {}: {e}",
                self.owner_addr
            )));
        }
        match read_msg::<_, SqlResponse>(stream).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                *guard = None;
                Err(BackendError::Other(format!(
                    "owner connection closed awaiting a reply from {}: {e}",
                    self.owner_addr
                )))
            }
        }
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
        };
        match self.forward(&req).await? {
            SqlResponse::Rows { columns, rows } => Ok(ResultSet {
                columns: columns.into_iter().map(Column::from).collect(),
                rows: rows.into_iter().map(|r| r.into_iter().map(Value::from).collect()).collect(),
            }),
            SqlResponse::Error { sqlite, message } => Err(error_from_response(sqlite, message)),
            SqlResponse::Ok { .. } => Err(BackendError::Other(
                "site owner returned an OK to a forwarded query".to_string(),
            )),
        }
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult, BackendError> {
        let req = SqlRequest::Execute {
            sql: sql.to_string(),
            params: params.iter().map(WireValue::from).collect(),
        };
        match self.forward(&req).await? {
            SqlResponse::Ok { affected_rows, last_insert_rowid } => {
                Ok(ExecuteResult { affected_rows, last_insert_rowid })
            }
            SqlResponse::Error { sqlite, message } => Err(error_from_response(sqlite, message)),
            SqlResponse::Rows { .. } => Err(BackendError::Other(
                "site owner returned rows to a forwarded execute".to_string(),
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
    /// Stock `pdo_mysql` traffic hid the gap by incidentally opening the
    /// database locally; the recommended deployment (the `db-*` drop-ins, which
    /// call `ephpm_db_*` only) hits it squarely.
    ///
    /// The announcement happens **before** the fallible owner-address lookup:
    /// a node that cannot reach the site's owner right now is precisely a node
    /// that should already be replicating it.
    async fn resolve_async(&self, site: &str) -> Result<SharedBackend, String> {
        let nodes = self.cluster.nodes().await;
        if plan_serve(&nodes, &self.self_id, site, &self.note_active) == ServePlan::Local {
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
        // PHP worker / spawn_blocking thread, never an async task — the same
        // invariant that licenses the plain registry's `resolve`.
        self.handle.clone().block_on(self.resolve_async(site_key))
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
/// Holds the site's backend `Arc` for the stream's lifetime so the registry's
/// refcount-aware LRU cannot evict the database out from under the live owner
/// connection.
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
        return Ok(());
    }

    // Pin the site open for the stream's lifetime; `conn` (declared after) drops
    // before this pin so the connection closes before the database can be
    // evicted/re-opened.
    let backend = registry
        .get_or_open(site)
        .await
        .with_context(|| format!("forwarded-SQL: open local backend for {site}"))?;
    let conn = backend
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("forwarded-SQL: open owner session for {site}: {e}"))?;

    tracing::debug!(peer = %peer, site = %site, "forwarded-SQL: serving owner session");

    loop {
        let req: SqlRequest = match read_msg(&mut stream).await {
            Ok(r) => r,
            // Clean close by the client (EOF) or a torn stream — either way this
            // stream is done; the client re-dials for its next session.
            Err(_) => break,
        };
        let resp = execute_forwarded(conn.as_ref(), req).await;
        write_msg(&mut stream, &resp).await.context("forwarded-SQL: write reply")?;
    }
    Ok(())
}

/// Run one forwarded statement against the owner's local connection and shape
/// the reply. Never returns `Err`: a backend error becomes an
/// [`SqlResponse::Error`] carried back to the client.
async fn execute_forwarded(conn: &dyn BackendConn, req: SqlRequest) -> SqlResponse {
    match req {
        SqlRequest::Query { sql, params } => {
            let params: Vec<Value> = params.into_iter().map(Value::from).collect();
            match conn.query(&sql, &params).await {
                Ok(rs) => SqlResponse::Rows {
                    columns: rs.columns.iter().map(WireColumn::from).collect(),
                    rows: rs.rows.iter().map(|r| r.iter().map(WireValue::from).collect()).collect(),
                },
                Err(e) => error_response(&e),
            }
        }
        SqlRequest::Execute { sql, params } => {
            let params: Vec<Value> = params.into_iter().map(Value::from).collect();
            match conn.execute(&sql, &params).await {
                Ok(ok) => SqlResponse::Ok {
                    affected_rows: ok.affected_rows,
                    last_insert_rowid: ok.last_insert_rowid,
                },
                Err(e) => error_response(&e),
            }
        }
    }
}

/// Serialize a [`BackendError`] into an [`SqlResponse::Error`], sending the
/// *inner* message (not the `Display`, which prefixes "SQLite error:") so the
/// client can rebuild the exact variant and re-map it.
fn error_response(e: &BackendError) -> SqlResponse {
    match e {
        BackendError::Sqlite(msg) => SqlResponse::Error { sqlite: true, message: msg.clone() },
        BackendError::Other(msg) => SqlResponse::Error { sqlite: false, message: msg.clone() },
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
        NodeInfo { id: id.into(), gossip_addr: "10.0.0.1:7946".into(), state }
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
    /// Note this is invisible to any test that drives `pdo_mysql`: that path
    /// opens the database locally and announces as a side effect. It only bites
    /// the recommended `ephpm_db_*`-only deployment.
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
        let n = NodeInfo {
            id: "ephpm-1".into(),
            gossip_addr: "10.0.0.2:7946".into(),
            state: NodeState::Alive,
        };
        assert_eq!(member_channel_addr(&n), Some("10.0.0.2:7948".parse().unwrap()));
        let broken = NodeInfo {
            id: "ephpm-2".into(),
            gossip_addr: "nonsense".into(),
            state: NodeState::Alive,
        };
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
        };
        write_msg(&mut a, &req).await.unwrap();
        let got: SqlRequest = read_msg(&mut b).await.unwrap();
        match got {
            SqlRequest::Execute { sql, params } => {
                assert_eq!(sql, "INSERT INTO t (v) VALUES (?)");
                assert_eq!(params.len(), 3);
                assert!(matches!(params[1], WireValue::Blob(ref bytes) if bytes == &[0, 1, 255]));
            }
            SqlRequest::Query { .. } => panic!("expected Execute"),
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
            SqlResponse::Error { sqlite, message } => {
                assert!(sqlite);
                assert_eq!(message, "UNIQUE constraint failed: t.id");
            }
            _ => panic!("expected Error"),
        }
        match error_response(&BackendError::Other("boom".into())) {
            SqlResponse::Error { sqlite, message } => {
                assert!(!sqlite);
                assert_eq!(message, "boom");
            }
            _ => panic!("expected Error"),
        }
    }
}

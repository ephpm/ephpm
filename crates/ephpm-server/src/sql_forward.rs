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
//!   forward each statement to the owner over `dial(owner, "sql/<site>")`.
//! - **Owner side** ([`spawn_owner_sql_handler`]) — a `register_prefix("sql/")`
//!   handler that parses the site, HRW-gates ("am I this site's owner?"), and
//!   runs each forwarded statement against that site's local screened/tracked
//!   backend, streaming the rowset / OK / error back.
//!
//! # Transport & security
//!
//! Forwarding rides the **cluster channel**, which is already mutually
//! authenticated (shared `[cluster] secret`) and per-connection encrypted
//! (ChaCha20-Poly1305). It does **not** touch the MySQL wire or per-site
//! `pdo_mysql` credentials, so no cluster-wide `DB_PASSWORD` derivation is
//! needed for this path.
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

use crate::site_backends::SiteBackends;

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
}

impl ClusteredSiteResolver {
    /// Build a resolver over the local registry and cluster context.
    #[must_use]
    pub fn new(
        local: SiteBackends,
        cluster: Arc<ClusterHandle>,
        channel: ChannelHandle,
        self_id: String,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self { local, cluster, channel, self_id, handle }
    }

    async fn resolve_async(&self, site: &str) -> Result<SharedBackend, String> {
        let nodes = self.cluster.nodes().await;
        if should_serve_locally(&nodes, &self.self_id, site) {
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

/// Serve one forwarding stream: HRW-gate, open the site's local session once,
/// then execute each forwarded statement against it and stream the reply back.
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

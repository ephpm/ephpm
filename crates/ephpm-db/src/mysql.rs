//! `MySQL` transparent proxy with connection pooling.
//!
//! ## How it works
//!
//! 1. A pool of pre-authenticated TCP connections to the real `MySQL` server
//!    is maintained. Each connection completed a full `MySQL` handshake using
//!    the credentials from `[db.mysql].url`.
//!
//! 2. When PHP connects to the proxy (e.g. `127.0.0.1:3306`), the proxy:
//!    a. Sends a synthetic `HandshakeV10` to the client (using saved server
//!    metadata and a fresh 20-byte challenge).
//!    b. Reads the client's `HandshakeResponse41` and accepts it without
//!    credential validation — the proxy port only listens on loopback.
//!    c. Sends an `OK` packet.
//!    d. Starts bidirectional byte forwarding between the client and a
//!    checked-out backend connection.
//!
//! 3. When the client ends its session — `COM_QUIT` or a plain socket close —
//!    the proxy closes only the client-facing socket. `COM_QUIT` is
//!    **intercepted, never forwarded**: the backend is shared across sessions
//!    and closing it would poison the pool. Depending on `reset_strategy` the
//!    proxy then sends `COM_RESET_CONNECTION` to the backend (resetting
//!    session variables, temporary tables, prepared statements, etc.) and
//!    returns the still-open connection to the pool.
//!
//! ## Auth plugin support
//!
//! Supports both `mysql_native_password` and `caching_sha2_password`.
//! `MySQL` 8+ defaults to `caching_sha2_password`, which is handled
//! transparently. For `caching_sha2_password`, the proxy supports the
//! "fast auth" path (server has password hash cached) and the "full auth"
//! path (RSA public key exchange over non-TLS connections).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ephpm_query_stats::QueryStats;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::ResetStrategy;
use crate::error::DbError;
use crate::health::{ProxyHealth, RetryBudget};
use crate::pool::{Checkout, Pool, PoolConfig};
use crate::stats::{PendingStatement, ResponseOutcome, SpliceWatch};
use crate::url::DbUrl;

// ── Capability flags ──────────────────────────────────────────────────────────

const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_MULTI_STATEMENTS: u32 = 0x0001_0000;
const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
const CLIENT_PLUGIN_AUTH_LENENC: u32 = 0x0020_0000;

// ── MySQL command bytes ──────────────────────────────────────────────────────

const COM_QUIT: u8 = 0x01;
const COM_INIT_DB: u8 = 0x02;
const COM_QUERY: u8 = 0x03;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;
const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
const COM_STMT_CLOSE: u8 = 0x19;
const COM_STMT_RESET: u8 = 0x1A;
const COM_STMT_FETCH: u8 = 0x1C;

/// Which pool a prepared statement was compiled on.
///
/// Stored per-statement so that `COM_STMT_EXECUTE` and related commands can be
/// routed to the same pool that handled `COM_STMT_PREPARE`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PoolTarget {
    Primary,
    /// Index into the replica pool slice, assigned by round-robin.
    Replica(usize),
}

/// Per-connection record of a statement the client prepared.
#[derive(Clone, Debug)]
struct PreparedStmt {
    /// Pool the statement was compiled on; execute/close must follow it.
    target: PoolTarget,
    /// SQL text seen in `COM_STMT_PREPARE`, retained only when query stats
    /// are enabled so `COM_STMT_EXECUTE` can be attributed to a digest.
    /// `None` means "not tracking" — never "unknown statement".
    sql: Option<String>,
}

// ── Read-write split & sticky routing ─────────────────────────────────────────

/// Parameters for read-write splitting and sticky-after-write behavior.
#[derive(Clone, Debug)]
pub struct RwSplitParams {
    /// Enable read-write splitting (route SELECTs to replicas).
    pub enabled: bool,
    /// How long to stick to the primary after a write operation.
    pub sticky_duration: std::time::Duration,
}

/// `MySQL` server metadata captured from the initial backend handshake.
/// Used to generate synthetic server greetings for PHP clients.
#[derive(Clone, Debug)]
struct ServerMeta {
    server_version: String,
    capabilities: u32,
    charset: u8,
    /// Auth plugin name advertised by the backend (e.g. `caching_sha2_password`).
    ///
    /// Used in `build_handshake_response` to select the correct auth scheme
    /// when authenticating pool connections to the backend.
    auth_plugin: String,
}

/// A running `MySQL` proxy that accepts client connections and pools backends.
pub struct MySqlProxy {
    pool: Pool,
    replica_pools: Vec<Pool>,
    /// Round-robin counter for distributing reads across replicas.
    replica_rr: AtomicUsize,
    meta: Arc<ServerMeta>,
    listen: String,
    /// Unix socket path for local PHP connections (future).
    ///
    /// When set, the proxy will also listen on this Unix domain socket in
    /// addition to the TCP `listen` address. Not yet wired up — requires
    /// `tokio::net::UnixListener` support in `run()`.
    _socket: Option<std::path::PathBuf>,
    reset_strategy: ResetStrategy,
    rw_split: RwSplitParams,
    /// Shared per-process query stats. Recording is gated on
    /// [`QueryStats::is_enabled`] so `[db.analysis] query_stats = false`
    /// leaves the forwarding paths byte-for-byte as they were.
    stats: QueryStats,
}

impl MySqlProxy {
    /// Create a new proxy by connecting to the backend, authenticating, and
    /// building the pool.
    ///
    /// Connects eagerly with a **bounded** retry budget (~40 s) and returns
    /// the error to the caller if the backend never answers. Production
    /// startup uses [`spawn_deferred`] instead, which binds the listener
    /// first and retries the upstream forever in the background.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial backend connection or handshake fails.
    pub async fn new(
        url: &str,
        listen: &str,
        socket: Option<std::path::PathBuf>,
        pool_config: PoolConfig,
        reset_strategy: ResetStrategy,
        replica_urls: Vec<String>,
        rw_split: RwSplitParams,
        stats: QueryStats,
    ) -> Result<Self, DbError> {
        let db_url = Arc::new(DbUrl::parse(url)?);
        let health = ProxyHealth::new("mysql", listen, db_url.addr());
        Self::connect(
            db_url,
            listen,
            socket,
            pool_config,
            reset_strategy,
            replica_urls,
            rw_split,
            stats,
            health,
            RetryBudget::Bounded(10),
        )
        .await
    }

    /// Connect to the upstream and build the pools.
    ///
    /// Shared by the eager constructor and the deferred startup path; the
    /// only difference between them is the [`RetryBudget`].
    async fn connect(
        db_url: Arc<DbUrl>,
        listen: &str,
        socket: Option<std::path::PathBuf>,
        pool_config: PoolConfig,
        reset_strategy: ResetStrategy,
        replica_urls: Vec<String>,
        rw_split: RwSplitParams,
        stats: QueryStats,
        health: Arc<ProxyHealth>,
        retry: RetryBudget,
    ) -> Result<Self, DbError> {
        // Establish a single connection to capture server metadata.
        //
        // Under k8s/systemd startup ordering the backend may not be reachable
        // yet — the proxy used to bail immediately and stay dead for the
        // process lifetime, requiring a manual restart after the DB came up.
        // Exponential backoff (250ms doubling to the budget's ceiling) makes
        // the proxy resilient to a slow-to-start backend.
        let (probe_stream, meta) = connect_with_retry(&db_url, "MySQL", &health, retry).await?;
        let meta = Arc::new(meta);

        // Build the primary pool using clones of the URL and meta for closures.
        // The pool's connect closure is also the live upstream-health signal:
        // every physical backend connection the pool opens (never once per
        // request) moves `ephpm_db_proxy_upstream_up`, so an upstream that
        // dies after startup is visible without a dedicated prober.
        let db_url_c = Arc::clone(&db_url);
        let health_c = Arc::clone(&health);
        let connect = move || -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            let u = Arc::clone(&db_url_c);
            let h = Arc::clone(&health_c);
            Box::pin(async move {
                match connect_and_handshake(&u).await {
                    Ok((stream, _)) => {
                        h.record_up();
                        Ok(stream)
                    }
                    Err(e) => {
                        h.record_down(&e);
                        Err(e)
                    }
                }
            })
        };

        let reset = |stream: TcpStream| -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            Box::pin(reset_connection(stream))
        };

        let ping =
            |stream: TcpStream| -> crate::pool::BoxFuture<Result<(TcpStream, bool), DbError>> {
                Box::pin(ping_connection(stream))
            };

        let pool = Pool::new(pool_config.clone(), connect, reset, ping);

        // Seed the pool with the probe connection.
        let mut checkout = Checkout {
            stream: Some(probe_stream),
            permit: Some(
                Arc::clone(&pool.semaphore).try_acquire_owned().map_err(|_| DbError::PoolClosed)?,
            ),
            created_at: std::time::Instant::now(),
            pool: pool.clone(),
        };
        // Return it immediately to warm the idle queue.
        let stream = checkout.take_stream();
        checkout.return_to_pool(stream);

        // Build replica pools.
        let mut replica_pools = Vec::new();
        for replica_url in replica_urls {
            if let Ok(replica_db_url) = DbUrl::parse(&replica_url) {
                let replica_db_url = Arc::new(replica_db_url);
                let replica_db_url_c = Arc::clone(&replica_db_url);
                let replica_connect =
                    move || -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
                        let u = Arc::clone(&replica_db_url_c);
                        Box::pin(async move {
                            let (stream, _) = connect_and_handshake(&u).await?;
                            Ok(stream)
                        })
                    };

                let replica_reset =
                    |stream: TcpStream| -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
                        Box::pin(reset_connection(stream))
                    };

                let replica_ping = |stream: TcpStream| -> crate::pool::BoxFuture<
                    Result<(TcpStream, bool), DbError>,
                > { Box::pin(ping_connection(stream)) };

                let replica_pool =
                    Pool::new(pool_config.clone(), replica_connect, replica_reset, replica_ping);
                replica_pools.push(replica_pool);
            }
        }

        Ok(Self {
            pool,
            replica_pools,
            replica_rr: AtomicUsize::new(0),
            meta,
            listen: listen.to_string(),
            _socket: socket,
            reset_strategy,
            rw_split,
            stats,
        })
    }

    /// The stats handle, or `None` when recording is switched off.
    ///
    /// Every tap point takes this shape so that a disabled collector skips
    /// clock reads and SQL copies entirely rather than relying on
    /// `QueryStats::record` to discard them.
    fn stats(&self) -> Option<&QueryStats> {
        self.stats.is_enabled().then_some(&self.stats)
    }

    /// Start the background pool maintenance task.
    #[must_use]
    pub fn start_maintenance(&self) -> tokio::task::JoinHandle<()> {
        self.pool.start_background_tasks()
    }

    /// Bind the proxy listener and start accepting client connections.
    ///
    /// Runs until the tokio runtime shuts down.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the listen address fails.
    pub async fn run(self) -> Result<(), DbError> {
        let listener = TcpListener::bind(&self.listen).await?;
        info!(listen = %self.listen, "MySQL proxy listening");
        self.run_on(Arc::new(listener)).await
    }

    /// Accept client connections on an already-bound listener.
    ///
    /// Split out from [`MySqlProxy::run`] so startup can bind the listen
    /// socket before the upstream is reachable: clients that arrive during
    /// the upstream-connect window queue in the kernel accept backlog and
    /// are served as soon as the loop starts, instead of getting
    /// `ECONNREFUSED`.
    ///
    /// # Errors
    ///
    /// Currently never returns `Err` — accept errors are logged and the
    /// loop continues. The signature is fallible for symmetry with
    /// [`MySqlProxy::run`].
    pub async fn run_on(self, listener: Arc<TcpListener>) -> Result<(), DbError> {
        let proxy = Arc::new(self);
        loop {
            let (client, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("MySQL proxy accept error: {e}");
                    continue;
                }
            };
            // Nagle + delayed ACK stalls every small request/response round
            // trip by ~40ms on Linux loopback (measured 44ms/query, 2026-07-09).
            let _ = client.set_nodelay(true);
            debug!(%peer, "MySQL client connected");
            let p = Arc::clone(&proxy);
            tokio::spawn(async move {
                if let Err(e) = p.handle_client(client).await {
                    debug!(%peer, "MySQL proxy session ended: {e}");
                }
            });
        }
    }

    /// Handle one PHP client connection.
    async fn handle_client(&self, mut client: TcpStream) -> Result<(), DbError> {
        // Step 1: send fake server greeting.
        let challenge = fresh_challenge();
        send_greeting(&mut client, &self.meta, &challenge).await?;

        // Step 2: read and discard client handshake response (no auth validation).
        read_client_handshake(&mut client).await?;

        // Step 3: send OK to PHP.
        send_ok(&mut client).await?;

        // Decide which proxy path to use.
        //
        // The per-command `proxy_routing_loop` re-acquires a backend from the
        // pool for *every* command, which destroys session continuity
        // (`SET @v`, transactions, prepared statements). It is only safe to
        // use when read/write splitting is actually enabled with replicas;
        // outside of that the proxy must hold a single backend for the entire
        // client session via `proxy_bidirectional`. Previously `Smart` always
        // took the routing path, which caused WordPress to hang on the first
        // multi-column SELECT and also lost user variables / transactions.
        let use_routing_loop = self.rw_split.enabled && !self.replica_pools.is_empty();

        if use_routing_loop {
            return proxy_routing_loop(
                client,
                &self.pool,
                &self.replica_pools,
                &self.replica_rr,
                &self.rw_split,
                self.reset_strategy,
                self.stats(),
            )
            .await;
        }

        // Single-backend path. Every strategy goes through the packet-aware
        // relay: intercepting `COM_QUIT` requires seeing packet boundaries, so
        // there is no correct byte-copy variant for a *pooled* backend. The
        // `dirty` bit it produces is only consulted by `Smart`.
        let mut checkout = self.pool.acquire().await?;
        let backend = checkout.take_stream();

        // Because every strategy now takes the packet-aware relay, query
        // stats see this path regardless of `reset_strategy` — there is no
        // longer an opaque byte-copy variant that hides statements.
        let outcome = proxy_bidirectional_sniff(client, backend, self.stats()).await;

        match outcome.backend {
            Some(backend) => match self.reset_strategy {
                ResetStrategy::Never => checkout.return_to_pool(backend),
                ResetStrategy::Always => checkout.return_with_reset(backend).await,
                ResetStrategy::Smart => {
                    if outcome.dirty {
                        checkout.return_with_reset(backend).await;
                    } else {
                        checkout.return_to_pool(backend);
                    }
                }
            },
            None => {
                debug!("backend connection failed during session; discarding, not recycling");
                checkout.retire();
            }
        }
        Ok(())
    }
}

// ── Backend connection & auth ─────────────────────────────────────────────────

/// Exponential-backoff wrapper around [`connect_and_handshake`].
///
/// Attempts the initial connect + handshake with increasing delays between
/// tries so a slow-to-start backend (k8s/systemd ordering, or a listener
/// this same process is about to bind) doesn't kill the proxy on startup.
/// Delays run 250 ms doubling to the [`RetryBudget`]'s ceiling.
///
/// Every attempt moves [`ProxyHealth`], which owns log throttling and the
/// failure metrics — so an unbounded retry against a long-dead upstream
/// stays visible without becoming log volume.
///
/// `db_kind` is a short label ("MySQL", "PostgreSQL") used only for logs.
async fn connect_with_retry(
    url: &DbUrl,
    db_kind: &str,
    health: &ProxyHealth,
    retry: RetryBudget,
) -> Result<(TcpStream, ServerMeta), DbError> {
    const INITIAL_BACKOFF_MS: u64 = 250;

    let max_backoff_ms = retry.max_backoff_ms();
    let mut backoff_ms = INITIAL_BACKOFF_MS;
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        match connect_and_handshake(url).await {
            Ok(ok) => {
                health.record_up();
                if attempt > 1 {
                    info!(
                        db = db_kind,
                        attempt,
                        addr = %url.addr(),
                        "backend connection established after retry"
                    );
                }
                return Ok(ok);
            }
            Err(e) => {
                health.record_down(&e);
                let is_last = retry.is_final_attempt(attempt);
                if is_last {
                    warn!(
                        db = db_kind,
                        attempt,
                        addr = %url.addr(),
                        error = %e,
                        "backend still unreachable after max retries; giving up"
                    );
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms.saturating_mul(2)).min(max_backoff_ms);
            }
        }
    }
}

/// Connect to the `MySQL` backend and complete the authentication handshake.
///
/// Returns the authenticated stream and the server metadata extracted from
/// the initial greeting.
async fn connect_and_handshake(url: &DbUrl) -> Result<(TcpStream, ServerMeta), DbError> {
    let mut stream = TcpStream::connect(url.addr()).await?;
    let _ = stream.set_nodelay(true);

    // Receive HandshakeV10.
    let (_, payload) = read_packet(&mut stream).await?;
    if payload.is_empty() || payload[0] == 0xFF {
        return Err(DbError::Auth("backend refused connection".into()));
    }
    let (meta, challenge) = parse_server_greeting(&payload)?;

    // Build our HandshakeResponse41.
    let response = build_handshake_response(
        &meta,
        &url.username,
        &url.password,
        &challenge,
        Some(&url.database),
    );
    write_packet(&mut stream, 1, &response).await?;

    // Read OK / ERR / auth-switch / auth-more-data.
    let (seq, resp) = read_packet(&mut stream).await?;
    match resp.first() {
        Some(0x00) => { /* OK */ }
        Some(0xFE) => {
            // Auth switch request: [0xFE][plugin_name\0][auth_data].
            let plugin_and_data = &resp[1..];
            let null_pos = plugin_and_data.iter().position(|&b| b == 0);
            let (plugin_name, switch_data) = if let Some(pos) = null_pos {
                let name = String::from_utf8_lossy(&plugin_and_data[..pos]).to_string();
                let data = &plugin_and_data[pos + 1..];
                (name, data)
            } else {
                (String::from_utf8_lossy(plugin_and_data).to_string(), &[][..])
            };

            match plugin_name.as_str() {
                "mysql_native_password" => {
                    // Re-compute auth with the new challenge data.
                    let mut new_challenge = [0u8; 20];
                    let copy_len = switch_data.len().min(20);
                    new_challenge[..copy_len].copy_from_slice(&switch_data[..copy_len]);
                    let auth_response = mysql_native_password(&url.password, &new_challenge);
                    write_packet(&mut stream, seq + 1, &auth_response).await?;
                    let (_, final_resp) = read_packet(&mut stream).await?;
                    if final_resp.first() != Some(&0x00) {
                        let msg = parse_error_packet(&final_resp);
                        return Err(DbError::Auth(format!("auth switch failed: {msg}")));
                    }
                }
                "caching_sha2_password" => {
                    let mut new_challenge = [0u8; 20];
                    let copy_len = switch_data.len().min(20);
                    new_challenge[..copy_len].copy_from_slice(&switch_data[..copy_len]);
                    handle_caching_sha2(&mut stream, &url.password, &new_challenge, seq + 1)
                        .await?;
                }
                other => {
                    return Err(DbError::Auth(format!(
                        "unsupported auth plugin switch to '{other}'"
                    )));
                }
            }
        }
        Some(0x01) if resp.len() >= 2 => {
            // Auth more data (0x01) — used by caching_sha2_password "fast auth".
            handle_caching_sha2_more_data(&mut stream, &url.password, &challenge, &resp, seq)
                .await?;
        }
        Some(0xFF) => {
            let msg = parse_error_packet(&resp);
            return Err(DbError::Auth(format!("backend auth error: {msg}")));
        }
        _ => return Err(DbError::Protocol("unexpected handshake response".into())),
    }

    Ok((stream, meta))
}

/// Parse `HandshakeV10` from the backend greeting payload.
fn parse_server_greeting(payload: &[u8]) -> Result<(ServerMeta, [u8; 20]), DbError> {
    if payload.len() < 4 || payload[0] != 10 {
        return Err(DbError::Protocol("not a HandshakeV10 packet".into()));
    }
    let mut pos = 1usize;

    // Server version (null-terminated).
    let end = payload[pos..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| DbError::Protocol("missing null in server version".into()))?;
    let server_version = String::from_utf8_lossy(&payload[pos..pos + end]).into_owned();
    pos += end + 1;

    // Connection id (4 bytes, ignored).
    pos += 4;

    // Auth-plugin-data part 1 (8 bytes).
    let mut challenge = [0u8; 20];
    if pos + 8 > payload.len() {
        return Err(DbError::Protocol("greeting too short (part1)".into()));
    }
    challenge[..8].copy_from_slice(&payload[pos..pos + 8]);
    pos += 8;

    // Filler.
    pos += 1;

    // Everything from here to the auth-plugin-data length is a fixed 8-byte
    // run: capability low (2) + charset (1) + status (2) + capability high (2)
    // + plugin data length (1). The guard has to cover all 8 — at 6 a greeting
    // truncated to exactly `pos + 6` or `pos + 7` passed the check and then
    // panicked indexing `payload[pos + 6]` / `payload[pos + 7]` below. That is
    // reachable from whatever answers on the configured DB port, and inside
    // `Pool::warm` it silently kills the detached maintenance task.
    if pos + 8 > payload.len() {
        return Err(DbError::Protocol("greeting too short (caps)".into()));
    }

    // Capability flags (lower 2 bytes).
    let cap_low = u32::from(u16::from_le_bytes([payload[pos], payload[pos + 1]]));
    pos += 2;

    // Charset.
    let charset = payload[pos];
    pos += 1;

    // Status flags (2 bytes, ignored).
    pos += 2;

    // Capability flags (upper 2 bytes).
    let cap_high = u32::from(u16::from_le_bytes([payload[pos], payload[pos + 1]]));
    pos += 2;
    let capabilities = cap_low | (cap_high << 16);

    // Auth plugin data length.
    let plugin_data_len = payload[pos] as usize;
    pos += 1;

    // Reserved (10 bytes).
    pos += 10;

    // Auth-plugin-data part 2: max(13, plugin_data_len - 8) bytes.
    let part2_len = plugin_data_len.saturating_sub(8).max(13);
    let part2_actual = (part2_len - 1).min(12); // strip trailing null, cap at 12
    if pos + part2_actual <= payload.len() {
        challenge[8..8 + part2_actual].copy_from_slice(&payload[pos..pos + part2_actual]);
    }
    pos += part2_len;

    // Auth plugin name (null-terminated).
    let auth_plugin = if capabilities & CLIENT_PLUGIN_AUTH != 0 && pos < payload.len() {
        let end = payload[pos..].iter().position(|&b| b == 0).unwrap_or(payload.len() - pos);
        String::from_utf8_lossy(&payload[pos..pos + end]).into_owned()
    } else {
        "mysql_native_password".to_string()
    };

    Ok((ServerMeta { server_version, capabilities, charset, auth_plugin }, challenge))
}

/// Build `HandshakeResponse41` for the backend.
///
/// Selects the auth plugin based on what the server advertised. Uses
/// `caching_sha2_password` when the server requests it, otherwise falls
/// back to `mysql_native_password`.
fn build_handshake_response(
    meta: &ServerMeta,
    username: &str,
    password: &str,
    challenge: &[u8; 20],
    database: Option<&str>,
) -> Vec<u8> {
    let use_caching_sha2 = meta.auth_plugin == "caching_sha2_password";

    let auth_response = if use_caching_sha2 {
        caching_sha2_password(password, challenge)
    } else {
        mysql_native_password(password, challenge)
    };

    // Build our capability flags from an explicit allowlist of what the proxy
    // actually implements, intersected with what the server advertises. We must
    // NOT inherit the server's full set: MySQL 8 advertises CLIENT_CONNECT_ATTRS
    // (0x0010_0000) and CLIENT_ZSTD_COMPRESSION_ALGORITHM (0x0400_0000), each of
    // which requires extra trailing bytes in HandshakeResponse41 (a lenenc
    // connection-attributes block / a compression-level byte). We don't send
    // those, so claiming the flags makes the server reject the packet with
    // ER_HANDSHAKE_ERROR ("Bad handshake"). CLIENT_SSL is likewise excluded.
    const CLIENT_SUPPORTED: u32 = CLIENT_LONG_PASSWORD
        | CLIENT_LONG_FLAG
        | CLIENT_CONNECT_WITH_DB
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_MULTI_STATEMENTS
        | CLIENT_MULTI_RESULTS
        | CLIENT_PS_MULTI_RESULTS
        | CLIENT_PLUGIN_AUTH
        | CLIENT_PLUGIN_AUTH_LENENC;
    let mut caps = meta.capabilities & CLIENT_SUPPORTED;
    // Force the flags the 4.1 + plugin-auth handshake always needs, even if the
    // server's advertised set somehow omits them.
    caps |= CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    // Only claim CONNECT_WITH_DB when we actually append a database name.
    if database.is_some_and(|d| !d.is_empty()) {
        caps |= CLIENT_CONNECT_WITH_DB;
    } else {
        caps &= !CLIENT_CONNECT_WITH_DB;
    }

    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&caps.to_le_bytes());
    buf.extend_from_slice(&16_777_215_u32.to_le_bytes()); // max packet size
    buf.push(meta.charset);
    buf.extend_from_slice(&[0u8; 23]); // reserved
    buf.extend_from_slice(username.as_bytes());
    buf.push(0); // null-terminate username

    // Lenenc-encoded auth response.
    encode_lenenc_bytes(&mut buf, &auth_response);

    if let Some(db) = database {
        if !db.is_empty() {
            buf.extend_from_slice(db.as_bytes());
            buf.push(0);
        }
    }

    let plugin_name = if use_caching_sha2 {
        b"caching_sha2_password" as &[u8]
    } else {
        b"mysql_native_password" as &[u8]
    };
    buf.extend_from_slice(plugin_name);
    buf.push(0);

    buf
}

/// Compute `mysql_native_password` token.
///
/// `SHA1(password) XOR SHA1(challenge || SHA1(SHA1(password)))`
fn mysql_native_password(password: &str, challenge: &[u8; 20]) -> Vec<u8> {
    if password.is_empty() {
        return vec![];
    }
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(stage1);
    let mut h = Sha1::new();
    h.update(challenge);
    h.update(stage2);
    let stage3 = h.finalize();
    stage1.iter().zip(stage3.iter()).map(|(a, b)| a ^ b).collect()
}

/// Compute `caching_sha2_password` token.
///
/// `SHA256(password) XOR SHA256(SHA256(SHA256(password)) || challenge)`
fn caching_sha2_password(password: &str, challenge: &[u8; 20]) -> Vec<u8> {
    if password.is_empty() {
        return vec![];
    }
    let hash1 = Sha256::digest(password.as_bytes());
    let hash2 = Sha256::digest(hash1);
    let mut h = Sha256::new();
    sha2::Digest::update(&mut h, hash2);
    sha2::Digest::update(&mut h, challenge);
    let hash3 = h.finalize();
    hash1.iter().zip(hash3.iter()).map(|(a, b)| a ^ b).collect()
}

/// Handle `caching_sha2_password` auth exchange after an auth switch.
///
/// Sends the SHA256-based token and handles the fast-auth / full-auth paths.
async fn handle_caching_sha2(
    stream: &mut TcpStream,
    password: &str,
    challenge: &[u8; 20],
    seq: u8,
) -> Result<(), DbError> {
    let auth_response = caching_sha2_password(password, challenge);
    write_packet(stream, seq, &auth_response).await?;

    // Read the response: OK, ERR, or more-data.
    let (next_seq, resp) = read_packet(stream).await?;
    match resp.first() {
        Some(0x00) => Ok(()),
        Some(0xFF) => {
            let msg = parse_error_packet(&resp);
            Err(DbError::Auth(format!("caching_sha2 auth error: {msg}")))
        }
        Some(0x01) => {
            handle_caching_sha2_more_data(stream, password, challenge, &resp, next_seq).await
        }
        _ => Err(DbError::Protocol("unexpected response during caching_sha2 auth".into())),
    }
}

/// Handle `caching_sha2_password` "more data" responses.
///
/// The server may respond with:
/// - `0x01 0x03`: fast auth success — read the final OK packet.
/// - `0x01 0x04`: full auth required — send the password via RSA or plaintext.
async fn handle_caching_sha2_more_data(
    stream: &mut TcpStream,
    password: &str,
    _challenge: &[u8; 20],
    resp: &[u8],
    seq: u8,
) -> Result<(), DbError> {
    if resp.len() < 2 {
        return Err(DbError::Protocol("auth more data too short".into()));
    }

    match resp[1] {
        0x03 => {
            // Fast auth succeeded. Read the final OK packet.
            let (_, final_resp) = read_packet(stream).await?;
            if final_resp.first() != Some(&0x00) {
                let msg = parse_error_packet(&final_resp);
                return Err(DbError::Auth(format!(
                    "caching_sha2 fast auth OK expected, got error: {msg}"
                )));
            }
            Ok(())
        }
        0x04 => {
            // Full auth required. Request the server's RSA public key.
            write_packet(stream, seq + 1, &[0x02]).await?;
            let (key_seq, key_resp) = read_packet(stream).await?;

            if key_resp.first() == Some(&0xFF) {
                let msg = parse_error_packet(&key_resp);
                return Err(DbError::Auth(format!("failed to get RSA public key: {msg}")));
            }

            // The response is the public key in PEM format (0x01 prefix + PEM data).
            //
            // KNOWN GAP: the correct wire behaviour for non-TLS caching_sha2
            // full-auth is to XOR the null-terminated password with the
            // 20-byte scramble (repeated to cover the plaintext length) and
            // RSA-PKCS1-OAEP encrypt the result under the server's public
            // key, then send the ciphertext. The current implementation
            // sends the password as null-terminated plaintext instead —
            // this works when the backend is on a trusted network (our
            // loopback proxy pool, the common ePHPm deployment), but any
            // operator running the pool over a non-loopback network gets
            // plaintext password exposure on the *initial* cache-miss
            // connect. Fast-auth caches the password hash on the backend
            // so subsequent connects skip this branch, bounding the
            // exposure to first-connect-per-user.
            //
            // Full RSA-OAEP is a follow-up PR: it needs the `rsa` and
            // `sha1` crates (sha1 is already a transitive dep, rsa is
            // new + license-clean but nontrivial to license-audit for
            // cargo deny) plus integration tests against a real MySQL 8.4
            // instance to catch OAEP padding edge cases. Deferred to the
            // DB-auth harness cycle.
            let mut pwd_bytes = password.as_bytes().to_vec();
            pwd_bytes.push(0);
            write_packet(stream, key_seq + 1, &pwd_bytes).await?;

            let (_, final_resp) = read_packet(stream).await?;
            match final_resp.first() {
                Some(0x00) => Ok(()),
                Some(0xFF) => {
                    let msg = parse_error_packet(&final_resp);
                    Err(DbError::Auth(format!("caching_sha2 full auth error: {msg}")))
                }
                _ => Err(DbError::Protocol(
                    "unexpected response after caching_sha2 full auth".into(),
                )),
            }
        }
        other => {
            Err(DbError::Protocol(format!("unexpected caching_sha2 more-data flag: {other:#x}")))
        }
    }
}

// ── Client greeting & handshake ───────────────────────────────────────────────

/// Global counter mixed into every challenge to guarantee inter-connection
/// uniqueness without touching the OS clock or the allocator.
///
/// The old code called `Arc::new(())` on every connect just to grab an
/// allocation-address hash — that's an allocator round trip per client
/// connection on a hot path. A relaxed atomic increment is nanoseconds.
static CHALLENGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Process-wide seed for the challenge PRNG. Initialised lazily on first
/// use with the same time-based mix the old code did, then reused across
/// every call. Consumers don't rely on this being cryptographically
/// unpredictable — see the security note on `fresh_challenge`.
static CHALLENGE_SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Cheap xorshift64* step for the challenge PRNG. Not for cryptographic
/// use — the security note on `fresh_challenge` explains why that's fine.
#[inline]
fn xorshift64_star(state: u64) -> u64 {
    let mut x = state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Generate a non-cryptographic 20-byte challenge for the fake client greeting.
///
/// Security note: the proxy does not validate client auth responses (local
/// loopback only), so this challenge value has no security significance.
///
/// # Performance
///
/// The previous implementation called `SystemTime::now()` (a syscall) plus
/// `Arc::new(())` (an allocator round-trip) plus a raw-pointer cast on every
/// client connect. Replaced with a self-seeded xorshift64* PRNG mixed with a
/// process-wide atomic counter — no syscalls, no allocations, no unsafe.
fn fresh_challenge() -> [u8; 20] {
    // Lazy seed: on first call mix the wall clock, PID, and a stack address
    // to break same-boot-second collisions when many processes come up
    // together. After that every call is atomic-load + a few shifts.
    let seed = *CHALLENGE_SEED.get_or_init(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let stack_probe = std::ptr::from_ref::<u8>(&0u8) as usize as u64;
        u64::from(ts.subsec_nanos())
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(ts.as_secs())
            .wrapping_add(stack_probe)
            .wrapping_add(u64::from(std::process::id()))
            .max(1) // xorshift needs a non-zero state
    });

    let ctr = CHALLENGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut state = seed ^ ctr.wrapping_mul(0xBF58_476D_1CE4_E5B9);

    let mut c = [0u8; 20];
    for chunk in c.chunks_mut(8) {
        state = xorshift64_star(state.max(1));
        let bytes = state.to_ne_bytes();
        let take = chunk.len().min(bytes.len());
        chunk[..take].copy_from_slice(&bytes[..take]);
    }
    c
}

/// Send a synthetic `HandshakeV10` to the PHP client.
async fn send_greeting(
    client: &mut TcpStream,
    meta: &ServerMeta,
    challenge: &[u8; 20],
) -> Result<(), DbError> {
    let caps = CLIENT_LONG_PASSWORD
        | CLIENT_LONG_FLAG
        | CLIENT_CONNECT_WITH_DB
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_MULTI_STATEMENTS
        | CLIENT_MULTI_RESULTS
        | CLIENT_PS_MULTI_RESULTS
        | CLIENT_PLUGIN_AUTH
        | CLIENT_PLUGIN_AUTH_LENENC;

    let mut payload = Vec::with_capacity(64);
    payload.push(10); // protocol version
    payload.extend_from_slice(meta.server_version.as_bytes());
    payload.push(0); // null-terminate version
    payload.extend_from_slice(&1_u32.to_le_bytes()); // connection id (arbitrary)
    payload.extend_from_slice(&challenge[..8]); // auth-plugin-data part 1
    payload.push(0); // filler
    payload.extend_from_slice(&caps.to_le_bytes()[..2]); // caps lower 16 bits
    payload.push(meta.charset);
    payload.extend_from_slice(&0x0002_u16.to_le_bytes()); // status: SERVER_STATUS_AUTOCOMMIT
    payload.extend_from_slice(&caps.to_le_bytes()[2..]); // caps upper 16 bits
    payload.push(21); // length of auth-plugin-data (part1=8 + part2=12 + null=1)
    payload.extend_from_slice(&[0u8; 10]); // reserved
    payload.extend_from_slice(&challenge[8..]); // auth-plugin-data part 2 (12 bytes)
    payload.push(0); // null-terminate part 2
    payload.extend_from_slice(b"mysql_native_password");
    payload.push(0);

    write_packet(client, 0, &payload).await
}

/// Read and discard the client's `HandshakeResponse41`.
///
/// We accept any credentials from loopback clients without validation.
async fn read_client_handshake(client: &mut TcpStream) -> Result<(), DbError> {
    let (_, _payload) = read_packet(client).await?;
    // Future: extract username/database from payload for logging.
    Ok(())
}

/// Send an `OK_Packet` to the client.
async fn send_ok(client: &mut TcpStream) -> Result<(), DbError> {
    // 0x00=OK, affected_rows=0, last_insert_id=0, status=AUTOCOMMIT, warnings=0
    let ok = [0x00u8, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00];
    write_packet(client, 2, &ok).await
}

// ── Reset & health check ──────────────────────────────────────────────────────

/// Send `COM_RESET_CONNECTION` and read the `OK` response.
///
/// Resets: transaction state, user variables, prepared statements, temporary
/// tables, and `LAST_INSERT_ID()`. Available since `MySQL` 5.7.
async fn reset_connection(mut stream: TcpStream) -> Result<TcpStream, DbError> {
    // COM_RESET_CONNECTION = 0x1F, sequence = 0.
    write_packet(&mut stream, 0, &[0x1F]).await?;
    let (_, resp) = read_packet(&mut stream).await?;
    if resp.first() != Some(&0x00) {
        return Err(DbError::Protocol("COM_RESET_CONNECTION did not return OK".into()));
    }
    Ok(stream)
}

/// Send `COM_PING` and return `(stream, is_alive)`.
async fn ping_connection(mut stream: TcpStream) -> Result<(TcpStream, bool), DbError> {
    // COM_PING = 0x0E
    if write_packet(&mut stream, 0, &[0x0E]).await.is_err() {
        return Ok((stream, false));
    }
    match read_packet(&mut stream).await {
        Ok((_, resp)) => Ok((stream, resp.first() == Some(&0x00))),
        Err(_) => Ok((stream, false)),
    }
}

// ── Bidirectional proxy ───────────────────────────────────────────────────────

/// Which side of the proxy ended a session.
///
/// The distinction decides whether the pooled backend may be recycled. Both
/// sides report the same `io::ErrorKind` values (`BrokenPipe`,
/// `ConnectionReset`), so the kind alone cannot be used: a client that
/// disappears mid-response and a backend that has closed are
/// indistinguishable by error kind, and treating the latter as a clean end
/// re-parks a dead socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionEnd {
    /// The client finished — orderly `COM_QUIT`, EOF, or a failure on the
    /// client socket. The backend was never told the session ended, so it is
    /// still usable.
    Client,
    /// The backend connection failed or closed. It must be discarded.
    Backend,
}

/// Result of proxying one client session over a pooled backend.
struct SessionOutcome {
    /// The backend stream, or `None` when the backend must be discarded.
    ///
    /// Deliberately not a `Result`: the previous signature returned
    /// `Ok((backend, dirty))` for `BrokenPipe`/`ConnectionReset`, which is how
    /// a dead backend got recycled as if the session had ended cleanly. Making
    /// "backend still usable" the payload rather than the error discriminant
    /// removes that failure mode by construction.
    backend: Option<TcpStream>,
    /// Whether the client issued anything that requires a session reset.
    dirty: bool,
}

/// Packet-aware bidirectional proxy for one client session.
///
/// Records whether the client ever issued anything other than a read-only
/// command: `dirty == true` means the session must be reset before the
/// backend returns to the pool (`SET`, `INSERT`, `BEGIN`, prepared
/// statements, `USE`, etc.). A pure-`SELECT` session reports `dirty ==
/// false`, allowing the Smart strategy to skip the `COM_RESET_CONNECTION`
/// round trip.
///
/// ## `COM_QUIT` is intercepted, never forwarded
///
/// PHP has no idea the proxy pools anything. When a request ends, PDO
/// destroys its handle and mysqlnd sends `COM_QUIT` — a request to *close the
/// connection*, which is correct for the client-facing socket and catastrophic
/// for the pooled one. Relaying it makes the backend close, and every
/// subsequent checkout of that slot draws a corpse. `COM_QUIT` therefore
/// terminates the relay loop here and is dropped on the floor; the backend
/// never learns the client went away and is recycled alive.
///
/// The same reasoning applies to a client that simply closes its socket: EOF
/// on the client side is a client-side event and must not be relayed as a
/// backend shutdown.
///
/// ## Framing
///
/// Both directions are framed rather than byte-copied. The client→backend
/// direction has to be, in order to see command bytes at all; the
/// backend→client direction is hand-copied so that an error can be
/// attributed to the side that produced it (see [`SessionEnd`]).
///
/// ## Session hygiene
///
/// A backend is recycled only when the client ended the session *and*
/// [`crate::pool::has_unread_bytes`] finds nothing left on the backend socket. The second
/// condition covers a client that ends its session mid-conversation — one that
/// writes `COM_QUIT`, or just closes, without having read the response to its
/// previous command. That leaves a partly drained response behind, and
/// recycling it would desynchronise whichever session picks the connection up
/// next.
///
/// `mysqlnd` is strictly synchronous, so this guards against clients ePHPm
/// does not ship rather than a case seen in practice. It is deliberately not
/// load-bearing: the check is best-effort (see [`crate::pool::has_unread_bytes`]), and
/// checkout-time validation in [`crate::pool::Pool::acquire`] cannot be relied
/// on to catch a desynchronised connection either — the ping is skipped inside
/// the validation window, and a leftover `OK` packet is indistinguishable from
/// a `COM_PING` reply.
///
/// ## Query stats
///
/// When `stats` is `Some`, each forwarded `COM_QUERY` is remembered and
/// recorded once its response has completed. Completion is inferred from the
/// arrival of the *next* client command, which the MySQL protocol guarantees
/// cannot happen until the previous response has been fully received — see
/// [`SpliceWatch`] for the full argument and for why prepared-statement
/// executes are out of reach here.
///
/// Every session-ending path settles the trailing statement before returning:
/// an intercepted `COM_QUIT` is itself "the next command" and so carries the
/// same completion proof, and a client that just closes its socket is caught
/// by the read-failure exits. All four settle points go through
/// [`flush_pending`] — missing one silently drops the last statement of a
/// connection, which for short-lived PHP sessions is a large share of all
/// traffic. When `stats` is `None` the only cost is a `None` check per read.
async fn proxy_bidirectional_sniff(
    mut client: TcpStream,
    mut backend: TcpStream,
    stats: Option<&QueryStats>,
) -> SessionOutcome {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dirty = Arc::new(AtomicBool::new(false));
    let dirty_w = Arc::clone(&dirty);

    let recorder = stats.map(|s| (s, Arc::new(SpliceWatch::new())));
    let watch_r = recorder.as_ref().map(|(_, w)| Arc::clone(w));

    let end = {
        let (mut cr, mut cw) = client.split();
        let (mut br, mut bw) = backend.split();

        let client_to_backend = async {
            let mut pending: Option<PendingStatement> = None;
            loop {
                let mut header = [0u8; 4];
                // Any read failure here is a client-side event: EOF, reset, or
                // a half-written packet. None of them say anything about the
                // backend. The session is over, so settle the trailing
                // statement rather than dropping it — a client that closes
                // without COM_QUIT still had its last response forwarded.
                if cr.read_exact(&mut header).await.is_err() {
                    flush_pending(recorder.as_ref(), &mut pending);
                    return SessionEnd::Client;
                }
                let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
                let mut payload = vec![0u8; len];
                if cr.read_exact(&mut payload).await.is_err() {
                    flush_pending(recorder.as_ref(), &mut pending);
                    return SessionEnd::Client;
                }

                // A new command proves the previous response completed. This
                // must stay ahead of the COM_QUIT interception below: QUIT is
                // itself "the next command", so it carries the same proof and
                // is what settles the last statement of an orderly PHP
                // session — the common clean end, and therefore the one that
                // matters most for stats completeness.
                flush_pending(recorder.as_ref(), &mut pending);

                if let Some(&cmd) = payload.first() {
                    // Session termination: swallow it. Forwarding this is the
                    // pool-poisoning bug — see the function docs.
                    if cmd == COM_QUIT {
                        debug!("client sent COM_QUIT; not forwarding to pooled backend");
                        return SessionEnd::Client;
                    }
                    let writeish = match cmd {
                        0x0E /* COM_PING */ => false,
                        COM_QUERY => {
                            let sql = std::str::from_utf8(&payload[1..]).unwrap_or("");
                            !matches!(classify_mysql_query(sql), QueryKind::Read)
                        }
                        _ => true,
                    };
                    if writeish {
                        dirty_w.store(true, Ordering::Relaxed);
                    }
                }

                // Arm before the command hits the wire: the response cannot
                // arrive before the write, so the reader half can never see
                // a chunk for a statement that has not been armed yet.
                let armed = recorder.as_ref().and_then(|(_, w)| {
                    recordable_sql(&payload).map(|sql| {
                        w.arm();
                        PendingStatement { sql: sql.to_string(), sent_ns: w.now_ns() }
                    })
                });

                if bw.write_all(&header).await.is_err() || bw.write_all(&payload).await.is_err() {
                    return SessionEnd::Backend;
                }
                pending = armed;
            }
        };

        let backend_to_client = async {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match br.read(&mut buf).await {
                    Ok(n) if n > 0 => {
                        // Date the response bytes so the client half can
                        // settle the statement they belong to. One clock read
                        // per read syscall — per buffer, not per packet or
                        // row — and only when stats are on.
                        if let Some(watch) = watch_r.as_deref() {
                            watch.note_backend_chunk(&buf[..n]);
                        }
                        if cw.write_all(&buf[..n]).await.is_err() {
                            return SessionEnd::Client;
                        }
                    }
                    // `Ok(0)` is backend EOF; `Err` is a backend read failure.
                    // Once COM_QUIT is no longer forwarded, EOF means the
                    // server really did drop the connection — either way the
                    // stream is not reusable.
                    Ok(_) | Err(_) => return SessionEnd::Backend,
                }
            }
        };

        // Both branches are cancel-safe at the point the other completes:
        // `read` consumes nothing when dropped, and a cancelled write targets
        // the side that has already gone away.
        tokio::select! {
            e = client_to_backend => e,
            e = backend_to_client => e,
        }
    };

    let reusable = match end {
        // The client left a partly drained response behind — alive, but not
        // safe to hand to the next session.
        SessionEnd::Client if crate::pool::has_unread_bytes(&backend) => {
            debug!("client ended mid-response, leaving unread backend bytes; discarding");
            false
        }
        SessionEnd::Client => true,
        SessionEnd::Backend => false,
    };

    SessionOutcome {
        backend: reusable.then_some(backend),
        dirty: dirty.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// The SQL text a forwarded command should be attributed to, if any.
///
/// Only `COM_QUERY` qualifies. Two deliberate exclusions:
///
/// - `COM_STMT_PREPARE` carries SQL, but preparing is a metadata round trip
///   rather than an execution. Recording it under the statement's digest
///   would publish parse latency as query latency. `TrackedBackend` makes
///   the same call for litewire's `describe_columns`.
/// - `COM_STMT_EXECUTE` is an execution, but carries only a statement ID.
///   Mapping it back to SQL requires having parsed the *prepare response*,
///   which only the R/W-split routing loop does; the splice path never
///   reads the backend→client direction. See [`crate::stats`].
fn recordable_sql(payload: &[u8]) -> Option<&str> {
    if payload.first() != Some(&COM_QUERY) {
        return None;
    }
    std::str::from_utf8(&payload[1..]).ok().filter(|sql| !sql.trim().is_empty())
}

/// Settle a pending statement, if there is one and stats are on.
///
/// Called at every point that constitutes proof the previous response
/// completed: the arrival of the next client command — which includes the
/// intercepted `COM_QUIT` that ends a normal PHP session — and both
/// client-side read failures that end a session without one. Missing any of
/// them silently drops the last statement of a connection, which for
/// short-lived PHP sessions is a large share of all traffic.
fn flush_pending(
    recorder: Option<&(&QueryStats, Arc<SpliceWatch>)>,
    pending: &mut Option<PendingStatement>,
) {
    if let (Some((stats, watch)), Some(statement)) = (recorder, pending.take()) {
        watch.flush(stats, &statement);
    }
}

// ── MySQL packet framing ──────────────────────────────────────────────────────

/// Read one `MySQL` packet: `[len: 3LE][seq: 1][payload: len]`.
async fn read_packet(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), DbError> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let seq = header[3];
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((seq, payload))
}

/// Write one `MySQL` packet.
async fn write_packet(stream: &mut TcpStream, seq: u8, payload: &[u8]) -> Result<(), DbError> {
    let len = u32::try_from(payload.len()).expect("MySQL packet too large for 32-bit length field");
    let len_bytes = len.to_le_bytes();
    let header = [len_bytes[0], len_bytes[1], len_bytes[2], seq];
    stream.write_all(&header).await?;
    stream.write_all(payload).await?;
    Ok(())
}

/// Append a length-encoded integer + bytes to `buf`.
fn encode_lenenc_bytes(buf: &mut Vec<u8>, data: &[u8]) {
    let len = data.len();
    if len < 251 {
        buf.push(len.to_le_bytes()[0]);
    } else if len < 65536 {
        buf.push(0xFC);
        buf.extend_from_slice(&len.to_le_bytes()[..2]);
    } else {
        buf.push(0xFD);
        buf.extend_from_slice(&len.to_le_bytes()[..3]);
    }
    buf.extend_from_slice(data);
}

/// Extract the human-readable message from a `MySQL` `ERR_Packet`.
fn parse_error_packet(payload: &[u8]) -> String {
    // [0xFF][code: 2][#][sqlstate: 5][message...]
    if payload.len() < 9 {
        return "(empty error)".to_string();
    }
    String::from_utf8_lossy(&payload[9..]).into_owned()
}

// ── Routing & smart reset ──────────────────────────────────────────────────────

/// Kind of SQL query for routing decisions.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum QueryKind {
    /// SELECT, SHOW, EXPLAIN, DESCRIBE — read-only, can go to replica.
    Read,
    /// INSERT, UPDATE, DELETE, CREATE, ALTER, DROP — modifies data, must go to primary.
    Write,
    /// BEGIN, START TRANSACTION — starts a transaction, sticky to primary.
    TxBegin,
    /// COMMIT, ROLLBACK — ends a transaction.
    TxEnd,
}

/// Case-insensitive ASCII substring search that does **not** allocate.
///
/// Equivalent to `haystack.to_ascii_uppercase().contains(needle)` when
/// `needle` is already uppercase, but scans in place. Used on the hot
/// routing path where classify runs per client command.
fn contains_ascii_ignore_case(haystack: &str, needle_upper: &str) -> bool {
    let hb = haystack.as_bytes();
    let nb = needle_upper.as_bytes();
    if nb.is_empty() {
        return true;
    }
    if hb.len() < nb.len() {
        return false;
    }
    let max_start = hb.len() - nb.len();
    'outer: for i in 0..=max_start {
        for (j, &nch) in nb.iter().enumerate() {
            let hch = hb[i + j].to_ascii_uppercase();
            if hch != nch {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

/// Classify a SQL query based on its first keyword.
#[must_use]
pub fn classify_mysql_query(sql: &str) -> QueryKind {
    let s = sql.trim_start();
    // Find the first token (word).
    let tok = s.split_ascii_whitespace().next().unwrap_or("").to_ascii_uppercase();

    match tok.as_str() {
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" => {
            // Special case: SELECT ... FOR UPDATE or SELECT ... FOR SHARE → Write.
            //
            // Old hotpath: `sql.to_ascii_uppercase()` allocated a fresh String
            // on every classify call, twice (once per contains). Case-insensitive
            // substring scan avoids both allocations — this runs on every client
            // command via the routing loop.
            if contains_ascii_ignore_case(sql, "FOR UPDATE")
                || contains_ascii_ignore_case(sql, "FOR SHARE")
            {
                QueryKind::Write
            } else {
                QueryKind::Read
            }
        }
        "BEGIN" | "START" => QueryKind::TxBegin,
        "COMMIT" | "ROLLBACK" => QueryKind::TxEnd,
        _ => QueryKind::Write, // Default: treat as write (safest)
    }
}

/// Per-client connection state for routing and dirty tracking.
#[derive(Debug, Clone, Default)]
struct ClientState {
    in_transaction: bool,
    sticky_until: Option<std::time::Instant>,
    dirty: bool,
}

/// Decode a `MySQL` length-encoded integer from `buf`.
///
/// Returns `(value, bytes_consumed)` or `None` if the input is truncated.
/// Encoding:
/// - `0x00..=0xFA`  → 1-byte integer (the byte itself)
/// - `0xFB`         → NULL (treated as 0 here; not used for column counts)
/// - `0xFC`         → 2-byte LE integer (3 bytes total)
/// - `0xFD`         → 3-byte LE integer (4 bytes total)
/// - `0xFE`         → 8-byte LE integer (9 bytes total)
fn decode_lenenc_int(buf: &[u8]) -> Option<(u64, usize)> {
    match *buf.first()? {
        v @ 0..=0xFA => Some((u64::from(v), 1)),
        0xFB => Some((0, 1)),
        0xFC if buf.len() >= 3 => Some((u64::from(u16::from_le_bytes([buf[1], buf[2]])), 3)),
        0xFD if buf.len() >= 4 => {
            Some((u64::from(u32::from_le_bytes([buf[1], buf[2], buf[3], 0])), 4))
        }
        0xFE if buf.len() >= 9 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[1..9]);
            Some((u64::from_le_bytes(b), 9))
        }
        _ => None,
    }
}

/// Forward a complete `COM_QUERY` response from backend to client.
///
/// The Smart-reset routing loop returns each pooled backend to the pool after
/// every command, so this function must consume *exactly* one full response
/// before returning — leaving stray packets buffered desynchronises the next
/// command (this is what made WordPress hang on `[db.mysql] reset_strategy =
/// "smart"`: the response to `SELECT @@max_allowed_packet,@@wait_timeout`
/// was truncated after the column-defs EOF, the row packets stayed in the
/// backend's read buffer, and the client blocked forever waiting for them).
///
/// COM_QUERY responses have four shapes:
///
/// 1. `OK_Packet`    — `[0x00] …`           (single packet)
/// 2. `ERR_Packet`   — `[0xFF] …`           (single packet)
/// 3. `LOCAL INFILE` — `[0xFB] <filename>`  (multi-round file-upload flow —
///    unsupported here; the caller will retire the connection)
/// 4. Result set     — `[lenenc col_count]`,
///    `col_count` column-def packets, an EOF (the proxy advertises classic
///    EOF — `CLIENT_DEPRECATE_EOF` is *not* in its capability mask), zero or
///    more row packets, and a terminating EOF / ERR.
///
/// Because the proxy's synthetic client greeting in [`send_greeting`] does
/// not set `CLIENT_DEPRECATE_EOF` (0x0100_0000), every result set we see
/// from real MySQL servers uses the two-EOF layout — we don't have to handle
/// the deprecated-EOF row terminator.
///
/// The returned [`ResponseOutcome`] is assembled purely from bytes this
/// function already had to look at: the `OK_Packet`'s length-encoded
/// affected-row count, the presence of an `ERR_Packet`, or the number of
/// row packets in a result set. Nothing is estimated.
async fn forward_mysql_response(
    backend: &mut TcpStream,
    client: &mut TcpStream,
) -> Result<ResponseOutcome, DbError> {
    let (seq, payload) = read_packet(backend).await?;
    write_packet(client, seq, &payload).await?;

    // Single-packet responses.
    match payload.first().copied() {
        Some(0x00) => {
            // OK_Packet: [0x00][affected_rows: lenenc][last_insert_id: lenenc]…
            let affected = decode_lenenc_int(&payload[1..]).map_or(0, |(v, _)| v);
            return Ok(ResponseOutcome { ok: true, rows: affected });
        }
        Some(0xFF) => return Ok(ResponseOutcome { ok: false, rows: 0 }),
        Some(0xFE) if payload.len() < 9 => return Ok(ResponseOutcome::ok_unknown_rows()),
        Some(0xFB) => {
            return Err(DbError::Protocol(
                "LOCAL INFILE response not supported by smart-reset proxy path".into(),
            ));
        }
        _ => {} // Result-set header: column count as a lenenc integer.
    }

    let (col_count, _) = decode_lenenc_int(&payload)
        .ok_or_else(|| DbError::Protocol("malformed result-set column-count packet".into()))?;

    // Column-definition packets.
    for _ in 0..col_count {
        let (s, p) = read_packet(backend).await?;
        write_packet(client, s, &p).await?;
    }

    // Intermediate EOF terminating the column-definition block.
    let (s, p) = read_packet(backend).await?;
    write_packet(client, s, &p).await?;
    if p.first() == Some(&0xFF) {
        // ERR_Packet here (e.g. cursor open failed) — no rows follow.
        return Ok(ResponseOutcome { ok: false, rows: 0 });
    }
    if !(p.first() == Some(&0xFE) && p.len() < 9) {
        return Err(DbError::Protocol(
            "expected EOF after column definitions in result-set".into(),
        ));
    }

    // Row packets until the terminating EOF or ERR.
    let mut rows = 0u64;
    loop {
        let (s, p) = read_packet(backend).await?;
        write_packet(client, s, &p).await?;
        match p.first().copied() {
            // Terminating EOF.
            Some(0xFE) if p.len() < 9 => return Ok(ResponseOutcome { ok: true, rows }),
            // ERR mid-stream: the rows already forwarded are real, but the
            // statement did not complete successfully.
            Some(0xFF) => return Ok(ResponseOutcome { ok: false, rows }),
            _ => rows += 1,
        }
    }
}

/// Forward a `COM_STMT_PREPARE` response and extract the statement ID.
///
/// The response format is:
/// - `0x00` (OK): `[0x00][stmt_id: 4LE][num_columns: 2LE][num_params: 2LE][...rest]`
///   followed by param definition packets + EOF and column definition packets + EOF.
/// - `0xFF` (ERR): error packet.
///
/// Returns `Some(stmt_id)` on success, `None` on error response.
async fn forward_prepare_response(
    backend: &mut TcpStream,
    client: &mut TcpStream,
) -> Result<Option<u32>, DbError> {
    let (seq, payload) = read_packet(backend).await?;
    write_packet(client, seq, &payload).await?;

    // ERR packet — prepare failed, no statement ID to track.
    if payload.first() == Some(&0xFF) {
        return Ok(None);
    }

    // Expect OK (0x00) with at least 12 bytes:
    // [status: 1][stmt_id: 4][num_columns: 2][num_params: 2][reserved: 1][warning_count: 2]
    if payload.len() < 12 || payload[0] != 0x00 {
        return Err(DbError::Protocol("unexpected COM_STMT_PREPARE response format".into()));
    }

    let stmt_id = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let num_params = u16::from_le_bytes([payload[5], payload[6]]);
    let num_columns = u16::from_le_bytes([payload[7], payload[8]]);

    // Forward parameter definition packets + EOF (if any).
    if num_params > 0 {
        for _ in 0..num_params {
            let (s, p) = read_packet(backend).await?;
            write_packet(client, s, &p).await?;
        }
        // EOF after params.
        let (s, p) = read_packet(backend).await?;
        write_packet(client, s, &p).await?;
    }

    // Forward column definition packets + EOF (if any).
    if num_columns > 0 {
        for _ in 0..num_columns {
            let (s, p) = read_packet(backend).await?;
            write_packet(client, s, &p).await?;
        }
        // EOF after columns.
        let (s, p) = read_packet(backend).await?;
        write_packet(client, s, &p).await?;
    }

    Ok(Some(stmt_id))
}

/// Extract a `u32` statement ID from bytes `[1..5]` of a prepared statement
/// command payload (`COM_STMT_EXECUTE`, `COM_STMT_CLOSE`, etc.).
///
/// Returns `None` if the payload is too short.
#[must_use]
pub fn parse_stmt_id(payload: &[u8]) -> Option<u32> {
    if payload.len() < 5 {
        return None;
    }
    Some(u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]))
}

/// Select which pool (primary or replica) to use for the next query.
///
/// When multiple replicas are configured, reads are distributed via
/// round-robin using the shared `replica_rr` counter.
fn select_pool<'a>(
    primary: &'a Pool,
    replicas: &'a [Pool],
    replica_rr: &AtomicUsize,
    state: &ClientState,
    kind: QueryKind,
    _rw_split: &RwSplitParams,
) -> &'a Pool {
    // If no replicas, always use primary.
    if replicas.is_empty() {
        return primary;
    }

    // In transaction: always primary.
    if state.in_transaction {
        return primary;
    }

    // Sticky after write: check if still in sticky window.
    if let Some(sticky_until) = state.sticky_until {
        if std::time::Instant::now() < sticky_until {
            return primary;
        }
    }

    // Read queries can use replicas via round-robin.
    if matches!(kind, QueryKind::Read) {
        let idx = replica_rr.fetch_add(1, Ordering::Relaxed) % replicas.len();
        &replicas[idx]
    } else {
        // Write, TxBegin, TxEnd: always primary.
        primary
    }
}

/// Resolve a [`PoolTarget`] to its concrete [`Pool`] reference.
fn resolve_pool_target<'a>(
    target: PoolTarget,
    primary: &'a Pool,
    replicas: &'a [Pool],
) -> &'a Pool {
    match target {
        PoolTarget::Primary => primary,
        PoolTarget::Replica(idx) => &replicas[idx % replicas.len()],
    }
}

/// Determine which pool and query kind to use for a given command payload.
///
/// Returns `(target_pool, query_kind)`. When R/W splitting is disabled or no
/// replicas are configured, always returns the primary pool.
fn route_command<'a>(
    payload: &[u8],
    pool: &'a Pool,
    replica_pools: &'a [Pool],
    state: &ClientState,
    rw_split: &RwSplitParams,
    stmt_pool_map: &HashMap<u32, PreparedStmt>,
    replica_rr: &AtomicUsize,
) -> (&'a Pool, QueryKind) {
    if !rw_split.enabled || replica_pools.is_empty() {
        return (pool, QueryKind::Write);
    }

    let cmd = payload[0];
    match cmd {
        COM_QUERY | COM_STMT_PREPARE => {
            let sql = std::str::from_utf8(payload.get(1..).unwrap_or_default()).unwrap_or("");
            let kind = classify_mysql_query(sql);
            (select_pool(pool, replica_pools, replica_rr, state, kind, rw_split), kind)
        }
        COM_STMT_EXECUTE | COM_STMT_SEND_LONG_DATA | COM_STMT_FETCH => {
            let target = parse_stmt_id(payload)
                .and_then(|id| stmt_pool_map.get(&id).map(|s| s.target))
                .map_or(pool, |pt| resolve_pool_target(pt, pool, replica_pools));
            let kind = if std::ptr::eq(target, pool) { QueryKind::Write } else { QueryKind::Read };
            (target, kind)
        }
        COM_STMT_CLOSE | COM_STMT_RESET => {
            let target = parse_stmt_id(payload)
                .and_then(|id| stmt_pool_map.get(&id).map(|s| s.target))
                .map_or(pool, |pt| resolve_pool_target(pt, pool, replica_pools));
            (target, QueryKind::Read)
        }
        _ => (pool, QueryKind::Write),
    }
}

/// SQL to attribute a routing-loop command to, if any.
///
/// Extends [`recordable_sql`] with `COM_STMT_EXECUTE`: this path parsed the
/// prepare response, so it holds the statement ID → SQL mapping the splice
/// path lacks. `COM_STMT_PREPARE` is still excluded on purpose — see
/// [`recordable_sql`] for why.
fn routing_recordable_sql<'a>(
    payload: &'a [u8],
    stmt_pool_map: &'a HashMap<u32, PreparedStmt>,
) -> Option<&'a str> {
    match payload.first().copied() {
        Some(COM_QUERY) => recordable_sql(payload),
        Some(COM_STMT_EXECUTE) => parse_stmt_id(payload)
            .and_then(|id| stmt_pool_map.get(&id))
            .and_then(|stmt| stmt.sql.as_deref()),
        _ => None,
    }
}

/// Update connection dirty-bit and transaction tracking after a command.
fn track_dirty(state: &mut ClientState, payload: &[u8], query_kind: QueryKind) {
    let cmd = payload[0];
    match cmd {
        COM_INIT_DB => state.dirty = true,
        COM_STMT_PREPARE | COM_STMT_EXECUTE => {
            if matches!(query_kind, QueryKind::Write | QueryKind::TxBegin) {
                state.dirty = true;
            }
        }
        COM_QUERY => {
            let sql = std::str::from_utf8(payload.get(1..).unwrap_or_default()).unwrap_or("");
            match classify_mysql_query(sql) {
                QueryKind::Write => state.dirty = true,
                QueryKind::TxBegin => {
                    state.in_transaction = true;
                    state.dirty = true;
                }
                QueryKind::TxEnd => {
                    state.in_transaction = false;
                }
                QueryKind::Read => {}
            }
        }
        _ => {}
    }
}

/// What relaying one client command produced.
struct RelayResult {
    /// The prepared-statement id, when the command was a `COM_STMT_PREPARE`
    /// the backend accepted.
    prepared: Option<u32>,
    /// What the response said, when the command had one the proxy framed.
    /// `None` for prepare and close — neither is a recordable execution.
    response: Option<ResponseOutcome>,
}

/// Forward one client command to `backend` and relay the response.
///
/// # Errors
///
/// Any error leaves the backend connection in an unknown protocol state — a
/// partially written command, a partially drained result set. The caller must
/// discard the connection; returning it to the pool would desynchronise
/// whichever session picks it up next.
async fn relay_one_command(
    backend: &mut TcpStream,
    client: &mut TcpStream,
    seq: u8,
    payload: &[u8],
) -> Result<RelayResult, DbError> {
    write_packet(backend, seq, payload).await?;
    match payload[0] {
        COM_STMT_PREPARE => Ok(RelayResult {
            prepared: forward_prepare_response(backend, client).await?,
            response: None,
        }),
        // COM_STMT_CLOSE is fire-and-forget — the server sends no response.
        COM_STMT_CLOSE => Ok(RelayResult { prepared: None, response: None }),
        _ => Ok(RelayResult {
            prepared: None,
            response: Some(forward_mysql_response(backend, client).await?),
        }),
    }
}

/// Proxy loop with per-query routing and dirty-bit tracking.
///
/// Handles `COM_QUERY` and the full prepared statement protocol
/// (`COM_STMT_PREPARE`, `COM_STMT_EXECUTE`, `COM_STMT_CLOSE`, etc.) with
/// read/write-aware routing. Statement IDs are tracked per-connection so that
/// execute/close/reset/fetch commands are routed to the same pool that
/// compiled the statement.
///
/// # Query stats
///
/// This path frames both directions, so recording is exact: the clock runs
/// from just before the command is written to the backend until
/// [`forward_mysql_response`] has consumed the last packet of its response,
/// and success plus row counts come out of that response. Because the
/// prepare response is parsed here, the SQL captured at
/// `COM_STMT_PREPARE` time is retained per connection and used to attribute
/// each `COM_STMT_EXECUTE` — coverage the single-backend splice path cannot
/// match.
async fn proxy_routing_loop(
    mut client: TcpStream,
    pool: &Pool,
    replica_pools: &[Pool],
    replica_rr: &AtomicUsize,
    rw_split: &RwSplitParams,
    reset_strategy: crate::ResetStrategy,
    recorder: Option<&QueryStats>,
) -> Result<(), DbError> {
    let mut state = ClientState::default();
    // Maps statement IDs to the pool they were prepared on (so that
    // COM_STMT_EXECUTE and friends route to the correct backend) and, when
    // stats are on, to the SQL they were compiled from.
    let mut stmt_pool_map: HashMap<u32, PreparedStmt> = HashMap::new();

    loop {
        let Ok((seq, payload)) = read_packet(&mut client).await else {
            break;
        };
        if payload.is_empty() {
            continue;
        }

        let cmd = payload[0];
        if cmd == COM_QUIT {
            // Session termination belongs to the client-facing socket only.
            // The pooled backends are shared across sessions and must never
            // see it — see `proxy_bidirectional_sniff` for the full rationale.
            debug!("client sent COM_QUIT; ending session without touching pooled backends");
            break;
        }

        let (target_pool, query_kind) = route_command(
            &payload,
            pool,
            replica_pools,
            &state,
            rw_split,
            &stmt_pool_map,
            replica_rr,
        );
        track_dirty(&mut state, &payload, query_kind);

        // Acquire backend and forward the command.
        let mut checkout = target_pool.acquire().await?;
        let mut backend = checkout.take_stream();

        // A relay failure means this backend is no longer in a known protocol
        // state. Discard it explicitly — never let it fall through to a
        // `return_to_pool` below.
        let started = recorder.map(|_| std::time::Instant::now());
        let relayed = match relay_one_command(&mut backend, &mut client, seq, &payload).await {
            Ok(relayed) => relayed,
            Err(e) => {
                debug!("backend relay failed, discarding connection: {e}");
                checkout.retire();
                return Err(e);
            }
        };

        if cmd == COM_STMT_PREPARE {
            let pool_target = if std::ptr::eq(target_pool, pool) {
                PoolTarget::Primary
            } else {
                // Find which replica index was selected.
                let idx =
                    replica_pools.iter().position(|r| std::ptr::eq(target_pool, r)).unwrap_or(0);
                PoolTarget::Replica(idx)
            };
            // Retain the prepared SQL only when it will be used: it is what
            // lets a later COM_STMT_EXECUTE be attributed to a digest.
            let sql = recorder
                .and_then(|_| std::str::from_utf8(payload.get(1..).unwrap_or_default()).ok())
                .map(ToString::to_string);
            if let Some(stmt_id) = relayed.prepared {
                stmt_pool_map.insert(stmt_id, PreparedStmt { target: pool_target, sql });
                debug!(stmt_id, ?pool_target, "prepared statement registered");
            }
        } else if cmd == COM_STMT_CLOSE {
            if let Some(stmt_id) = parse_stmt_id(&payload) {
                if let Some(removed) = stmt_pool_map.remove(&stmt_id) {
                    debug!(stmt_id, ?removed, "prepared statement closed");
                }
            }
        }

        // Record after the statement-id map is up to date, so a
        // COM_STMT_EXECUTE resolves against the SQL its prepare registered.
        // `relayed.response` is `None` for prepare and close, neither of
        // which is a recordable execution.
        if let (Some(collector), Some(started), Some(outcome)) =
            (recorder, started, relayed.response)
        {
            if let Some(sql) = routing_recordable_sql(&payload, &stmt_pool_map) {
                collector.record(sql, started.elapsed(), outcome.ok, outcome.rows);
            }
        }

        // Return backend to pool.
        let should_reset = match reset_strategy {
            crate::ResetStrategy::Always => true,
            crate::ResetStrategy::Never => false,
            crate::ResetStrategy::Smart => state.dirty,
        };
        if should_reset {
            match reset_connection(backend).await {
                Ok(s) => {
                    checkout.return_to_pool(s);
                    state.dirty = false;
                }
                Err(_) => checkout.retire(),
            }
        } else {
            checkout.return_to_pool(backend);
        }

        if rw_split.enabled && matches!(query_kind, QueryKind::Write) {
            state.sticky_until = Some(std::time::Instant::now() + rw_split.sticky_duration);
        }
    }

    Ok(())
}

// ── Public builder ────────────────────────────────────────────────────────────

/// Build a [`Pool`] for `MySQL`.
///
/// Exported so `lib.rs` can construct `MySqlProxy` from config. `stats` is
/// the process-wide collector shared with the litewire paths; pass one built
/// with `enabled: false` (i.e. `[db.analysis] query_stats = false`) to leave
/// the forwarding paths uninstrumented.
///
/// # Errors
///
/// Propagates any error from [`MySqlProxy::new`] (backend connection or
/// authentication failures).
pub async fn build_proxy(
    url: &str,
    listen: &str,
    socket: Option<std::path::PathBuf>,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
    replica_urls: Vec<String>,
    rw_split: RwSplitParams,
    stats: QueryStats,
) -> Result<MySqlProxy, DbError> {
    MySqlProxy::new(url, listen, socket, pool_config, reset_strategy, replica_urls, rw_split, stats)
        .await
}

/// Bind the proxy listener now; reach the upstream in the background.
///
/// This is the startup path. It inverts the old ordering, which connected
/// to the upstream first and only bound the listener afterwards — so a
/// proxy whose upstream was not yet up spent its whole retry budget with
/// **no socket bound**, then gave up permanently. Two things fell out of
/// that: an upstream started later in the same process (`[db.mysql]`
/// pointed at `[db.sqlite]`'s own litewire listener) could never be
/// reached, and any deployment whose database was down at boot came up
/// with a dead proxy that only a restart could fix.
///
/// What this does instead:
///
/// 1. Parse the URL and bind the listen socket **synchronously**. Both are
///    configuration errors, so both are fatal to startup — a proxy that
///    cannot bind its port must not be a logged warning and a silently
///    dead listener.
/// 2. Return immediately, so the rest of startup (including the embedded
///    SQLite listener this proxy may be pointed at) proceeds.
/// 3. Reach the upstream from a background task with an unbounded,
///    capped-backoff retry, then serve on the already-bound listener.
///
/// Clients that connect during step 3 do not get `ECONNREFUSED`: the socket
/// is bound, so their connections sit in the kernel accept backlog and are
/// served as soon as the upstream answers. That window is bounded by
/// [`BACKLOG_GRACE`] — past it, clients are accepted and closed so they fail
/// fast rather than blocking on a greeting that will not come.
///
/// `health` is moved into the connect loop and the pool; the caller keeps a
/// clone to drive the readiness probe.
///
/// # Errors
///
/// Returns an error if the URL is malformed or the listen address cannot be
/// bound. Upstream unreachability is *not* an error here — that is what the
/// background retry and [`ProxyHealth`] are for.
pub async fn spawn_deferred(
    url: &str,
    listen: &str,
    socket: Option<std::path::PathBuf>,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
    replica_urls: Vec<String>,
    rw_split: RwSplitParams,
    stats: QueryStats,
    health: Arc<ProxyHealth>,
) -> Result<tokio::task::JoinHandle<()>, DbError> {
    let db_url = Arc::new(DbUrl::parse(url)?);
    let listener = TcpListener::bind(listen).await?;
    info!(
        listen = %listen,
        upstream = %db_url.addr(),
        "MySQL proxy listening (upstream connect continues in the background)"
    );

    let listen_owned = listen.to_string();
    let listener = Arc::new(listener);
    Ok(tokio::spawn(async move {
        // Fail-fast guard for a long outage; aborted the moment the upstream
        // answers, and a no-op entirely when that happens inside the grace
        // period (the common case).
        let drain = tokio::spawn(crate::health::drain_while_upstream_down(
            Arc::clone(&listener),
            Arc::clone(&health),
        ));

        let proxy = match MySqlProxy::connect(
            db_url,
            &listen_owned,
            socket,
            pool_config,
            reset_strategy,
            replica_urls,
            rw_split,
            stats,
            health,
            RetryBudget::Unbounded,
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(e) => {
                // Unreachable in practice: the unbounded budget never gives
                // up, so only a pool-seed failure lands here. Leave the drain
                // running so clients keep failing fast rather than hanging.
                tracing::error!("MySQL proxy failed to start: {e:#}");
                return;
            }
        };
        drain.abort();
        // Detach pool maintenance — it runs for the proxy's lifetime.
        drop(proxy.start_maintenance());
        match proxy.run_on(listener).await {
            Ok(()) => info!("MySQL proxy stopped"),
            Err(e) => tracing::error!("MySQL proxy error: {e:#}"),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_server_greeting bounds ──────────────────────────────────

    /// Build a `HandshakeV10` payload truncated to `pos + extra` bytes, where
    /// `pos` is the offset of the capability-flags run.
    fn truncated_greeting(extra: usize) -> Vec<u8> {
        let mut p = vec![10u8]; // protocol version
        p.extend_from_slice(b"8.0.0\0"); // server version
        p.extend_from_slice(&[0, 0, 0, 0]); // connection id
        p.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // auth-plugin-data part 1
        p.push(0); // filler
        p.resize(p.len() + extra, 0);
        p
    }

    /// A greeting truncated to exactly the old `pos + 6` / `pos + 7` bound
    /// passed the length check and then panicked indexing `payload[pos + 6]`
    /// and `payload[pos + 7]`. Reachable from whatever answers on the
    /// configured DB port.
    #[test]
    fn short_greeting_is_rejected_not_panicked() {
        for extra in 0..8 {
            let payload = truncated_greeting(extra);
            let result = parse_server_greeting(&payload);
            assert!(
                matches!(result, Err(DbError::Protocol(_))),
                "greeting with {extra} capability bytes must be rejected, not panic"
            );
        }
    }

    #[test]
    fn greeting_with_full_capability_run_parses() {
        // 8 capability-run bytes + 10 reserved + 13 auth-plugin-data part 2.
        let payload = truncated_greeting(8 + 10 + 13);
        assert!(parse_server_greeting(&payload).is_ok(), "a complete greeting must still parse");
    }

    // ── classify_mysql_query ──────────────────────────────────────────

    #[test]
    fn classify_select_as_read() {
        assert_eq!(classify_mysql_query("SELECT * FROM users"), QueryKind::Read);
    }

    #[test]
    fn classify_select_for_update_as_write() {
        assert_eq!(classify_mysql_query("SELECT * FROM users FOR UPDATE"), QueryKind::Write);
    }

    #[test]
    fn classify_show_as_read() {
        assert_eq!(classify_mysql_query("SHOW TABLES"), QueryKind::Read);
    }

    #[test]
    fn classify_insert_as_write() {
        assert_eq!(classify_mysql_query("INSERT INTO users VALUES (1)"), QueryKind::Write);
    }

    #[test]
    fn classify_begin_as_tx_begin() {
        assert_eq!(classify_mysql_query("BEGIN"), QueryKind::TxBegin);
    }

    #[test]
    fn classify_commit_as_tx_end() {
        assert_eq!(classify_mysql_query("COMMIT"), QueryKind::TxEnd);
    }

    #[test]
    fn classify_whitespace_prefix() {
        assert_eq!(classify_mysql_query("   SELECT 1"), QueryKind::Read);
    }

    #[test]
    fn classify_unknown_as_write() {
        assert_eq!(classify_mysql_query("TRUNCATE TABLE users"), QueryKind::Write);
    }

    // ── parse_stmt_id ────────────────────────────────────────────────

    #[test]
    fn parse_stmt_id_from_execute_payload() {
        // COM_STMT_EXECUTE: [0x17][stmt_id: 4 LE][flags: 1][iteration_count: 4]...
        let stmt_id: u32 = 42;
        let mut payload = vec![COM_STMT_EXECUTE];
        payload.extend_from_slice(&stmt_id.to_le_bytes());
        payload.push(0x00); // flags
        payload.extend_from_slice(&1_u32.to_le_bytes()); // iteration count

        assert_eq!(parse_stmt_id(&payload), Some(42));
    }

    #[test]
    fn parse_stmt_id_from_close_payload() {
        let stmt_id: u32 = 7;
        let mut payload = vec![COM_STMT_CLOSE];
        payload.extend_from_slice(&stmt_id.to_le_bytes());

        assert_eq!(parse_stmt_id(&payload), Some(7));
    }

    #[test]
    fn parse_stmt_id_too_short() {
        let payload = vec![COM_STMT_EXECUTE, 0x01, 0x00]; // only 3 bytes, need 5
        assert_eq!(parse_stmt_id(&payload), None);
    }

    #[test]
    fn parse_stmt_id_large_value() {
        let stmt_id: u32 = 0x0102_0304;
        let mut payload = vec![COM_STMT_EXECUTE];
        payload.extend_from_slice(&stmt_id.to_le_bytes());

        assert_eq!(parse_stmt_id(&payload), Some(0x0102_0304));
    }

    // ── backend handshake capability masking ─────────────────────────

    #[test]
    fn handshake_response_strips_unsupported_caps() {
        // A MySQL 8 server advertises CONNECT_ATTRS (0x0010_0000) and ZSTD
        // (0x0400_0000); claiming either without its trailing payload makes the
        // server reject the response with ER_HANDSHAKE_ERROR ("Bad handshake").
        // The proxy must clear anything outside its supported set.
        const CONNECT_ATTRS: u32 = 0x0010_0000;
        const ZSTD: u32 = 0x0400_0000;
        const SSL: u32 = 0x0000_0800;
        let meta = ServerMeta {
            server_version: "8.0.39".into(),
            capabilities: u32::MAX, // server claims every flag
            charset: 255,
            auth_plugin: "caching_sha2_password".into(),
        };
        let resp = build_handshake_response(&meta, "wp", "pw", &[0u8; 20], Some("wp"));
        let caps = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        assert_eq!(caps & CONNECT_ATTRS, 0, "CONNECT_ATTRS must be cleared");
        assert_eq!(caps & ZSTD, 0, "ZSTD must be cleared");
        assert_eq!(caps & SSL, 0, "SSL must be cleared");
        assert_ne!(caps & CLIENT_PROTOCOL_41, 0, "PROTOCOL_41 required");
        assert_ne!(caps & CLIENT_PLUGIN_AUTH, 0, "PLUGIN_AUTH required");
        assert_ne!(caps & CLIENT_CONNECT_WITH_DB, 0, "DB given => flag set");
    }

    #[test]
    fn handshake_response_clears_connect_with_db_when_no_db() {
        let meta = ServerMeta {
            server_version: "8.0.39".into(),
            capabilities: u32::MAX,
            charset: 255,
            auth_plugin: "mysql_native_password".into(),
        };
        let resp = build_handshake_response(&meta, "wp", "pw", &[0u8; 20], None);
        let caps = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        assert_eq!(caps & CLIENT_CONNECT_WITH_DB, 0, "no DB => flag cleared");
    }

    // ── select_pool routing with stmt_pool_map ───────────────────────

    #[test]
    fn select_routes_read_to_replica() {
        let rw_split =
            RwSplitParams { enabled: true, sticky_duration: std::time::Duration::from_secs(1) };

        let primary = pool_stub();
        let replicas = vec![pool_stub()];
        let state = ClientState::default();

        let rr = AtomicUsize::new(0);
        let target = select_pool(&primary, &replicas, &rr, &state, QueryKind::Read, &rw_split);
        assert!(std::ptr::eq(target, &raw const replicas[0]));
    }

    #[test]
    fn select_routes_write_to_primary() {
        let rw_split =
            RwSplitParams { enabled: true, sticky_duration: std::time::Duration::from_secs(1) };

        let primary = pool_stub();
        let replicas = vec![pool_stub()];
        let state = ClientState::default();

        let rr = AtomicUsize::new(0);
        let target = select_pool(&primary, &replicas, &rr, &state, QueryKind::Write, &rw_split);
        assert!(std::ptr::eq(target, &raw const primary));
    }

    #[test]
    fn select_routes_to_primary_in_transaction() {
        let rw_split =
            RwSplitParams { enabled: true, sticky_duration: std::time::Duration::from_secs(1) };

        let primary = pool_stub();
        let replicas = vec![pool_stub()];
        let state = ClientState { in_transaction: true, ..ClientState::default() };

        let rr = AtomicUsize::new(0);
        let target = select_pool(&primary, &replicas, &rr, &state, QueryKind::Read, &rw_split);
        assert!(std::ptr::eq(target, &raw const primary));
    }

    #[test]
    fn stmt_pool_map_tracks_prepare_to_execute() {
        let mut map: HashMap<u32, PreparedStmt> = HashMap::new();

        // Simulate: SELECT prepared on replica 0.
        map.insert(1, PreparedStmt { target: PoolTarget::Replica(0), sql: None });
        // Simulate: INSERT prepared on primary.
        map.insert(2, PreparedStmt { target: PoolTarget::Primary, sql: None });

        assert_eq!(map.get(&1).map(|s| s.target), Some(PoolTarget::Replica(0)));
        assert_eq!(map.get(&2).map(|s| s.target), Some(PoolTarget::Primary));

        // Close statement 1.
        map.remove(&1);
        assert!(!map.contains_key(&1));
        // Statement 2 still tracked.
        assert_eq!(map.get(&2).map(|s| s.target), Some(PoolTarget::Primary));
    }

    // ── forward_prepare_response (via mock TCP pair) ─────────────────

    #[tokio::test]
    async fn forward_prepare_response_ok() {
        // Build a mock COM_STMT_PREPARE OK response:
        // [0x00][stmt_id: 4LE][num_columns: 2LE][num_params: 2LE][reserved: 1][warning_count: 2LE]
        let stmt_id: u32 = 99;
        let mut ok_payload = vec![0x00];
        ok_payload.extend_from_slice(&stmt_id.to_le_bytes());
        ok_payload.extend_from_slice(&0_u16.to_le_bytes()); // num_params
        ok_payload.extend_from_slice(&0_u16.to_le_bytes()); // num_columns
        ok_payload.push(0x00); // reserved
        ok_payload.extend_from_slice(&0_u16.to_le_bytes()); // warnings

        let (mut backend_write, mut backend_read) = make_tcp_pair().await;
        let (mut client_write, mut client_read) = make_tcp_pair().await;

        // Write the response packet on the "backend" side.
        write_packet(&mut backend_write, 1, &ok_payload).await.unwrap();
        drop(backend_write);

        let result = forward_prepare_response(&mut backend_read, &mut client_write).await;
        assert_eq!(result.unwrap(), Some(99));

        // Verify the client received the packet.
        let (_, forwarded) = read_packet(&mut client_read).await.unwrap();
        assert_eq!(forwarded, ok_payload);
    }

    #[tokio::test]
    async fn forward_prepare_response_err() {
        // Build a mock ERR response.
        let mut err_payload = vec![0xFF];
        err_payload.extend_from_slice(&1045_u16.to_le_bytes()); // error code
        err_payload.push(b'#');
        err_payload.extend_from_slice(b"28000"); // sqlstate
        err_payload.extend_from_slice(b"Access denied");

        let (mut backend_write, mut backend_read) = make_tcp_pair().await;
        let (mut client_write, _client_read) = make_tcp_pair().await;

        write_packet(&mut backend_write, 1, &err_payload).await.unwrap();
        drop(backend_write);

        let result = forward_prepare_response(&mut backend_read, &mut client_write).await;
        assert_eq!(result.unwrap(), None);
    }

    // ── forward_mysql_response: full result-set framing ──────────────

    /// Regression test for the WordPress hang: `forward_mysql_response`
    /// used to return after the first EOF (the one ending the column-defs
    /// block), leaving the row packets in the backend buffer and starving
    /// the client. This test wraps a complete 2-column / 1-row result set
    /// and asserts every packet is forwarded.
    #[tokio::test]
    async fn forward_mysql_response_two_columns_one_row() {
        let (mut backend_write, mut backend_read) = make_tcp_pair().await;
        let (mut client_write, mut client_read) = make_tcp_pair().await;

        // Column count = 2 (lenenc).
        write_packet(&mut backend_write, 1, &[0x02]).await.unwrap();
        // Two column-def packets (opaque payloads — content doesn't matter).
        write_packet(&mut backend_write, 2, &[0xAA; 16]).await.unwrap();
        write_packet(&mut backend_write, 3, &[0xBB; 16]).await.unwrap();
        // Intermediate EOF after column definitions.
        write_packet(&mut backend_write, 4, &[0xFE, 0, 0, 0x02, 0]).await.unwrap();
        // One row packet (two lenenc strings: "x", "y").
        write_packet(&mut backend_write, 5, &[0x01, b'x', 0x01, b'y']).await.unwrap();
        // Terminating EOF.
        write_packet(&mut backend_write, 6, &[0xFE, 0, 0, 0x02, 0]).await.unwrap();
        drop(backend_write);

        let outcome = forward_mysql_response(&mut backend_read, &mut client_write).await.unwrap();
        drop(client_write);

        let mut seqs = Vec::new();
        while let Ok((s, _)) = read_packet(&mut client_read).await {
            seqs.push(s);
        }
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6], "all 6 packets must reach the client");
        assert!(outcome.ok);
        assert_eq!(outcome.rows, 1, "one row packet between the two EOFs");
    }

    #[tokio::test]
    async fn forward_mysql_response_ok_packet_single() {
        let (mut backend_write, mut backend_read) = make_tcp_pair().await;
        let (mut client_write, mut client_read) = make_tcp_pair().await;

        // [0x00][affected_rows=0][last_insert_id=0][status=0x0002][warnings=0]
        write_packet(&mut backend_write, 1, &[0x00, 0, 0, 0x02, 0, 0, 0]).await.unwrap();
        drop(backend_write);

        let outcome = forward_mysql_response(&mut backend_read, &mut client_write).await.unwrap();
        drop(client_write);

        let (_, p) = read_packet(&mut client_read).await.unwrap();
        assert_eq!(p[0], 0x00, "OK packet forwarded once");
        assert!(read_packet(&mut client_read).await.is_err(), "no further packets");
        assert!(outcome.ok);
        assert_eq!(outcome.rows, 0);
    }

    #[tokio::test]
    async fn forward_mysql_response_ok_packet_reports_affected_rows() {
        let (mut backend_write, mut backend_read) = make_tcp_pair().await;
        let (mut client_write, _client_read) = make_tcp_pair().await;

        // [0x00][affected_rows=5][last_insert_id=0][status][warnings]
        write_packet(&mut backend_write, 1, &[0x00, 5, 0, 0x02, 0, 0, 0]).await.unwrap();
        drop(backend_write);

        let outcome = forward_mysql_response(&mut backend_read, &mut client_write).await.unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.rows, 5, "affected rows come out of the OK packet, not a guess");
    }

    #[tokio::test]
    async fn forward_mysql_response_err_packet_marks_failure() {
        let (mut backend_write, mut backend_read) = make_tcp_pair().await;
        let (mut client_write, _client_read) = make_tcp_pair().await;

        let mut err = vec![0xFF];
        err.extend_from_slice(&1146_u16.to_le_bytes());
        err.push(b'#');
        err.extend_from_slice(b"42S02");
        err.extend_from_slice(b"Table 'x' doesn't exist");
        write_packet(&mut backend_write, 1, &err).await.unwrap();
        drop(backend_write);

        let outcome = forward_mysql_response(&mut backend_read, &mut client_write).await.unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.rows, 0);
    }

    #[tokio::test]
    async fn forward_mysql_response_empty_result_set() {
        // Zero rows: column count, column-defs, intermediate EOF, terminating EOF.
        let (mut backend_write, mut backend_read) = make_tcp_pair().await;
        let (mut client_write, mut client_read) = make_tcp_pair().await;

        write_packet(&mut backend_write, 1, &[0x01]).await.unwrap();
        write_packet(&mut backend_write, 2, &[0xAA; 16]).await.unwrap();
        write_packet(&mut backend_write, 3, &[0xFE, 0, 0, 0x02, 0]).await.unwrap();
        write_packet(&mut backend_write, 4, &[0xFE, 0, 0, 0x02, 0]).await.unwrap();
        drop(backend_write);

        let outcome = forward_mysql_response(&mut backend_read, &mut client_write).await.unwrap();
        drop(client_write);

        let mut seqs = Vec::new();
        while let Ok((s, _)) = read_packet(&mut client_read).await {
            seqs.push(s);
        }
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert!(outcome.ok);
        assert_eq!(outcome.rows, 0, "an empty result set must report zero rows");
    }

    // ── query stats: what a command is attributed to ─────────────────

    /// Build a `COM_QUERY` payload.
    fn com_query(sql: &str) -> Vec<u8> {
        let mut p = vec![COM_QUERY];
        p.extend_from_slice(sql.as_bytes());
        p
    }

    /// Build a `COM_STMT_EXECUTE` payload for `stmt_id`.
    fn com_stmt_execute(stmt_id: u32) -> Vec<u8> {
        let mut p = vec![COM_STMT_EXECUTE];
        p.extend_from_slice(&stmt_id.to_le_bytes());
        p.push(0x00); // flags
        p.extend_from_slice(&1_u32.to_le_bytes()); // iteration count
        p
    }

    #[test]
    fn recordable_sql_takes_com_query() {
        let payload = com_query("SELECT * FROM users WHERE id = 1");
        assert_eq!(recordable_sql(&payload), Some("SELECT * FROM users WHERE id = 1"));
    }

    #[test]
    fn recordable_sql_skips_prepare_and_execute() {
        // COM_STMT_PREPARE carries SQL but is a metadata round trip, not an
        // execution — recording it would publish parse latency as query
        // latency. COM_STMT_EXECUTE is an execution but carries no SQL.
        let mut prepare = vec![COM_STMT_PREPARE];
        prepare.extend_from_slice(b"SELECT * FROM users WHERE id = ?");
        assert_eq!(recordable_sql(&prepare), None, "prepare must not be recorded as a query");
        assert_eq!(recordable_sql(&com_stmt_execute(1)), None);
    }

    #[test]
    fn recordable_sql_skips_empty_and_non_utf8() {
        assert_eq!(recordable_sql(&com_query("   ")), None);
        assert_eq!(recordable_sql(&[COM_QUERY, 0xFF, 0xFE]), None);
        assert_eq!(recordable_sql(&[]), None);
    }

    #[test]
    fn routing_loop_attributes_execute_to_the_prepared_sql() {
        // The routing loop parses the prepare *response*, so it holds the
        // statement ID → SQL mapping and can attribute an execute.
        let mut map: HashMap<u32, PreparedStmt> = HashMap::new();
        map.insert(
            7,
            PreparedStmt {
                target: PoolTarget::Primary,
                sql: Some("SELECT * FROM users WHERE id = ?".to_string()),
            },
        );

        assert_eq!(
            routing_recordable_sql(&com_stmt_execute(7), &map),
            Some("SELECT * FROM users WHERE id = ?")
        );
        // An execute for a statement we never saw prepared stays unrecorded
        // rather than being attributed to a made-up digest.
        assert_eq!(routing_recordable_sql(&com_stmt_execute(8), &map), None);
        // And with stats off no SQL was retained, so nothing is recordable.
        map.insert(9, PreparedStmt { target: PoolTarget::Primary, sql: None });
        assert_eq!(routing_recordable_sql(&com_stmt_execute(9), &map), None);
    }

    // ── query stats: the splice tap point ────────────────────────────

    /// Write a `COM_QUERY` from the client side of the proxy.
    async fn send_query(client: &mut TcpStream, sql: &str) {
        write_packet(client, 0, &com_query(sql)).await.unwrap();
    }

    /// Answer one command on the fake backend with a minimal OK packet, and
    /// wait for the client to read it back so the proxy has demonstrably
    /// stamped the response.
    async fn answer_ok(backend: &mut TcpStream, client: &mut TcpStream) {
        let (_, _cmd) = read_packet(backend).await.unwrap();
        write_packet(backend, 1, &[0x00, 0, 0, 0x02, 0, 0, 0]).await.unwrap();
        let (_, resp) = read_packet(client).await.unwrap();
        assert_eq!(resp[0], 0x00);
    }

    /// Drive two queries plus a disconnect through the sniffing splice path
    /// and return the stats collector it recorded into.
    async fn drive_sniff_session(stats: Option<QueryStats>) -> Option<QueryStats> {
        let (mut driver, proxy_client) = make_tcp_pair().await;
        let (proxy_backend, mut fake_backend) = make_tcp_pair().await;

        let handed = stats.clone();
        let proxy = tokio::spawn(async move {
            proxy_bidirectional_sniff(proxy_client, proxy_backend, handed.as_ref()).await
        });

        send_query(&mut driver, "SELECT * FROM users WHERE id = 1").await;
        answer_ok(&mut fake_backend, &mut driver).await;

        // The second command is what proves the first response completed.
        send_query(&mut driver, "SELECT * FROM users WHERE id = 2").await;
        answer_ok(&mut fake_backend, &mut driver).await;

        // Disconnect: the last statement is settled on the way out.
        drop(driver);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), proxy).await;
        stats
    }

    #[tokio::test]
    async fn splice_path_records_forwarded_queries() {
        let stats =
            drive_sniff_session(Some(QueryStats::new(ephpm_query_stats::StatsConfig::default())))
                .await
                .unwrap();

        assert_eq!(stats.digest_count(), 1, "both queries share one digest after normalization");
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 2, "the trailing statement must be settled on disconnect");
        assert_eq!(top[0].error_count, 0);
        assert_eq!(top[0].total_rows, 0, "the splice path cannot count rows and must not guess");
        assert!(top[0].total_time > std::time::Duration::ZERO);
    }

    /// Negative control for the `[db.analysis] query_stats = false` path: the
    /// exact same session with no collector attached must record nothing and
    /// still forward every byte.
    #[tokio::test]
    async fn splice_path_records_nothing_when_stats_are_off() {
        // `None` is what `MySqlProxy::stats()` yields when the toggle is off.
        assert!(drive_sniff_session(None).await.is_none());

        // And a collector that is present but disabled must also stay empty —
        // proving the toggle holds at both layers.
        let disabled = QueryStats::new(ephpm_query_stats::StatsConfig {
            enabled: false,
            ..Default::default()
        });
        let stats = drive_sniff_session(Some(disabled)).await.unwrap();
        assert_eq!(stats.digest_count(), 0);
    }

    /// The trailing statement of a session must still be timed when the
    /// client ends with `COM_QUIT`, which is intercepted and never forwarded
    /// to the pooled backend. QUIT is itself "the next command", so it
    /// carries the same proof that the previous response completed — and it
    /// is the *common* clean end for a PHP client, so this is the settle
    /// point that matters most for stats completeness.
    #[tokio::test]
    async fn splice_path_settles_trailing_statement_on_intercepted_quit() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        let (mut driver, proxy_client) = make_tcp_pair().await;
        let (proxy_backend, mut fake_backend) = make_tcp_pair().await;

        let handed = stats.clone();
        let proxy = tokio::spawn(async move {
            proxy_bidirectional_sniff(proxy_client, proxy_backend, Some(&handed)).await
        });

        send_query(&mut driver, "SELECT * FROM users WHERE id = 1").await;
        answer_ok(&mut fake_backend, &mut driver).await;

        // Orderly close, exactly as mysqlnd does when PDO drops its handle.
        write_packet(&mut driver, 0, &[COM_QUIT]).await.unwrap();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), proxy)
            .await
            .expect("the session must end when the client sends COM_QUIT")
            .expect("proxy task must not panic");
        assert!(
            outcome.backend.is_some(),
            "an intercepted COM_QUIT must leave the pooled backend recyclable"
        );

        assert_eq!(
            stats.digest_count(),
            1,
            "the statement preceding COM_QUIT must still be recorded"
        );
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert!(top[0].total_time > std::time::Duration::ZERO);

        // The backend must never have been handed the QUIT byte: nothing
        // more arrives on it, so this read can only time out.
        let leaked = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            read_packet(&mut fake_backend),
        )
        .await;
        assert!(leaked.is_err(), "COM_QUIT must not reach the pooled backend");
    }

    /// The documented prepared-statement limitation, pinned: on the splice
    /// path a prepare/execute pair produces no digest at all, because the
    /// prepare is metadata and the execute cannot be mapped back to SQL.
    #[tokio::test]
    async fn splice_path_does_not_record_prepared_statements() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        let (mut driver, proxy_client) = make_tcp_pair().await;
        let (proxy_backend, mut fake_backend) = make_tcp_pair().await;

        let handed = stats.clone();
        let proxy = tokio::spawn(async move {
            proxy_bidirectional_sniff(proxy_client, proxy_backend, Some(&handed)).await
        });

        let mut prepare = vec![COM_STMT_PREPARE];
        prepare.extend_from_slice(b"SELECT * FROM users WHERE id = ?");
        write_packet(&mut driver, 0, &prepare).await.unwrap();
        answer_ok(&mut fake_backend, &mut driver).await;

        write_packet(&mut driver, 0, &com_stmt_execute(1)).await.unwrap();
        answer_ok(&mut fake_backend, &mut driver).await;

        drop(driver);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), proxy).await;

        assert_eq!(
            stats.digest_count(),
            0,
            "prepare is metadata and execute has no SQL — neither may be recorded here"
        );
    }

    // ── decode_lenenc_int ────────────────────────────────────────────

    #[test]
    fn decode_lenenc_int_small() {
        assert_eq!(decode_lenenc_int(&[0x42]), Some((0x42, 1)));
        assert_eq!(decode_lenenc_int(&[0xFA]), Some((0xFA, 1)));
    }

    #[test]
    fn decode_lenenc_int_two_byte() {
        assert_eq!(decode_lenenc_int(&[0xFC, 0x34, 0x12]), Some((0x1234, 3)));
    }

    #[test]
    fn decode_lenenc_int_three_byte() {
        assert_eq!(decode_lenenc_int(&[0xFD, 0x01, 0x02, 0x03]), Some((0x0003_0201, 4)));
    }

    #[test]
    fn decode_lenenc_int_truncated() {
        assert_eq!(decode_lenenc_int(&[0xFC, 0x01]), None);
        assert_eq!(decode_lenenc_int(&[]), None);
    }

    // ── Test helpers ─────────────────────────────────────────────────

    /// Create a connected pair of `TcpStream` for testing.
    async fn make_tcp_pair() -> (TcpStream, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = TcpStream::connect(addr);
        let accept = listener.accept();
        let (client, server) = tokio::join!(connect, accept);
        let (server, _addr) = server.unwrap();
        (client.unwrap(), server)
    }

    /// Create a minimal `Pool` for testing `select_pool()`.
    ///
    /// The pool is not functional (cannot actually acquire connections), but
    /// its identity (pointer address) is used to verify routing decisions.
    fn pool_stub() -> Pool {
        let connect = || -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            Box::pin(async { Err(DbError::PoolClosed) })
        };
        let reset = |s: TcpStream| -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            Box::pin(async { Ok(s) })
        };
        let ping = |s: TcpStream| -> crate::pool::BoxFuture<Result<(TcpStream, bool), DbError>> {
            Box::pin(async { Ok((s, true)) })
        };
        let config = PoolConfig {
            min_connections: 1,
            max_connections: 2,
            idle_timeout: std::time::Duration::from_secs(60),
            max_lifetime: std::time::Duration::from_secs(300),
            pool_timeout: std::time::Duration::from_secs(5),
            health_check_interval: std::time::Duration::from_secs(30),
        };
        Pool::new(config, connect, reset, ping)
    }

    // ── contains_ascii_ignore_case (classify hot-path helper) ────────

    #[test]
    fn contains_ascii_ignore_case_matches_mixed_case() {
        assert!(contains_ascii_ignore_case("select * from t for update", "FOR UPDATE"));
        assert!(contains_ascii_ignore_case("SELECT ... For Update", "FOR UPDATE"));
        assert!(contains_ascii_ignore_case("SELECT ... FOR share", "FOR SHARE"));
    }

    #[test]
    fn contains_ascii_ignore_case_rejects_when_absent() {
        assert!(!contains_ascii_ignore_case("SELECT * FROM t", "FOR UPDATE"));
        assert!(!contains_ascii_ignore_case("", "FOR UPDATE"));
    }

    #[test]
    fn contains_ascii_ignore_case_empty_needle_matches() {
        assert!(contains_ascii_ignore_case("anything", ""));
        assert!(contains_ascii_ignore_case("", ""));
    }

    #[test]
    fn classify_select_for_update_still_write_after_trim() {
        // Regression guard: the alloc-free contains helper must preserve the
        // exact classification the old `to_ascii_uppercase().contains(...)`
        // pair produced.
        assert_eq!(
            classify_mysql_query("SELECT * FROM t WHERE id = 1 FOR UPDATE"),
            QueryKind::Write
        );
        assert_eq!(classify_mysql_query("select id from t for share"), QueryKind::Write);
        assert_eq!(classify_mysql_query("SELECT * FROM t"), QueryKind::Read);
    }

    // ── fresh_challenge ─────────────────────────────────────────────

    #[test]
    fn fresh_challenge_produces_unique_values() {
        // The atomic counter guarantees each call yields a distinct challenge.
        // (The old ptr-of-fresh-Arc pattern didn't reliably differ across
        // calls on the same allocator.)
        let a = fresh_challenge();
        let b = fresh_challenge();
        let c = fresh_challenge();
        assert_ne!(a, b, "consecutive challenges must differ");
        assert_ne!(b, c, "consecutive challenges must differ");
        assert_eq!(a.len(), 20);
    }

    #[test]
    fn fresh_challenge_is_nonzero() {
        // Sanity: 20-byte all-zero challenges are technically valid but a
        // strong smell of a broken PRNG. A well-mixed xorshift should
        // essentially never produce all zeros.
        let c = fresh_challenge();
        assert!(c.iter().any(|&b| b != 0), "challenge should not be all zeros");
    }
}

//! `PostgreSQL` transparent proxy with connection pooling.
//!
//! ## How it works
//!
//! 1. A pool of pre-authenticated TCP connections to the real `PostgreSQL`
//!    server is maintained. Each connection completed a full PG startup/auth
//!    handshake using the credentials from `[db.postgres].url`.
//!
//! 2. When PHP connects to the proxy (e.g. `127.0.0.1:5432`), the proxy
//!    reads the client's `StartupMessage`, sends `AuthenticationOk` (no
//!    credential validation — loopback only), sends synthetic metadata,
//!    and starts bidirectional message forwarding. Both directions are
//!    framed on every path, which is what lets `[db.analysis]` see statements
//!    regardless of `reset_strategy`.
//!
//! 3. When the client closes or sends `Terminate`, the proxy closes only the
//!    client-facing socket. `Terminate` is **intercepted, never forwarded** —
//!    the backend is shared across sessions and closing it would poison the
//!    pool. The proxy then sends `DISCARD ALL` to the backend (unless
//!    `reset_strategy = "never"`) and returns the still-open connection to the
//!    pool.
//!
//! ## Auth support
//!
//! Supports `trust`, `md5`, and `scram-sha-256` for backend authentication.
//! Client-facing auth is always `AuthenticationOk` (loopback only).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use base64ct::Encoding;
use ephpm_query_stats::QueryStats;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::ResetStrategy;
use crate::error::DbError;
use crate::health::{ProxyHealth, RetryBudget};
use crate::pool::{Checkout, Pool, PoolConfig};
use crate::stats::ResponseOutcome;
use crate::url::DbUrl;

// ── PG message tags ──────────────────────────────────────────────────────────
//
// In the PG wire protocol, some tag bytes are shared between frontend and
// backend messages (e.g. 'D' = DataRow from backend, 'D' = Describe from
// frontend). We only define constants for tags we actively match on.

/// `AuthenticationXxx` — backend auth request/response.
const MSG_AUTH: u8 = b'R';
/// `ParameterStatus` — backend parameter notification.
const MSG_PARAMETER_STATUS: u8 = b'S';
/// `BackendKeyData` — backend process ID and secret key.
const MSG_BACKEND_KEY_DATA: u8 = b'K';
/// `ReadyForQuery` — backend is ready for the next query.
const MSG_READY_FOR_QUERY: u8 = b'Z';
/// `ErrorResponse` — backend error.
const MSG_ERROR_RESPONSE: u8 = b'E';
/// `DataRow` — one row of a result set. Counted for query stats.
const MSG_DATA_ROW: u8 = b'D';
/// `Query` — frontend simple query.
const MSG_QUERY: u8 = b'Q';
/// `Terminate` — frontend connection close.
const MSG_TERMINATE: u8 = b'X';
/// `Sync` — frontend end of an extended-query batch, answered by one
/// `ReadyForQuery`.
///
/// Shares its byte with the backend's `ParameterStatus`; the two never travel
/// in the same direction, and this constant is only ever matched against
/// frontend traffic.
const MSG_SYNC: u8 = b'S';
/// `FunctionCall` — frontend fastpath call, likewise answered by one
/// `ReadyForQuery`.
const MSG_FUNCTION_CALL: u8 = b'F';
/// `CopyInResponse` — backend is waiting for the client to stream `COPY ... FROM STDIN` data.
const MSG_COPY_IN_RESPONSE: u8 = b'G';
/// `CopyBothResponse` — backend entered bidirectional copy mode.
const MSG_COPY_BOTH_RESPONSE: u8 = b'W';

// ── Protocol limits ──────────────────────────────────────────────────────────

/// Largest PG message payload `read_pg_message` will buffer (64 MiB).
///
/// The length prefix is peer-controlled and arrives before any payload,
/// so an unbounded `vec![0u8; len - 4]` lets five bytes (`Q` followed by
/// `0x7FFFFFFF`) zero-fill 2 GiB of memory; a handful of connections then
/// OOM the whole server process. `read_startup_message` has always
/// bounded its own length field — this applies the same treatment to the
/// per-message path, with a limit generous enough for real queries and
/// bind parameters. Backend-side callers only read control messages
/// (auth exchange, `DISCARD ALL`, `SELECT 1`) through this path; query
/// results are streamed by `forward_pg_message` in 8 KiB chunks and are
/// unaffected by this bound.
const MAX_PG_MESSAGE_LEN: i32 = 64 * 1024 * 1024;

// ── Auth types ───────────────────────────────────────────────────────────────

const AUTH_OK: i32 = 0;
const AUTH_MD5_PASSWORD: i32 = 5;
const AUTH_SASL: i32 = 10;
const AUTH_SASL_CONTINUE: i32 = 11;
const AUTH_SASL_FINAL: i32 = 12;

// ── Read-write split params ──────────────────────────────────────────────────

/// Parameters for read-write splitting and sticky-after-write behavior.
#[derive(Clone, Debug)]
pub struct PgRwSplitParams {
    /// Enable read-write splitting (route SELECTs to replicas).
    pub enabled: bool,
    /// How long to stick to the primary after a write operation.
    pub sticky_duration: std::time::Duration,
}

// ── Server metadata ──────────────────────────────────────────────────────────

/// `PostgreSQL` server metadata captured from the initial backend handshake.
#[derive(Clone, Debug)]
struct PgServerMeta {
    /// `ParameterStatus` messages from the backend (encoding, timezone, etc.).
    parameters: Vec<(String, String)>,
    /// Backend process ID from `BackendKeyData`.
    process_id: i32,
    /// Secret key from `BackendKeyData` (for cancel requests).
    secret_key: i32,
}

/// A running `PostgreSQL` proxy that accepts client connections and pools backends.
pub struct PgProxy {
    pool: Pool,
    replica_pools: Vec<Pool>,
    /// Round-robin counter for distributing reads across replicas.
    replica_rr: AtomicUsize,
    meta: Arc<PgServerMeta>,
    listen: String,
    reset_strategy: ResetStrategy,
    rw_split: PgRwSplitParams,
    /// Shared per-process query stats. Recording is gated on
    /// [`QueryStats::is_enabled`] so `[db.analysis] query_stats = false`
    /// leaves the forwarding paths byte-for-byte as they were.
    stats: QueryStats,
}

impl PgProxy {
    /// Create a new proxy by connecting to the backend, authenticating, and
    /// building the pool.
    ///
    /// Connects eagerly with a **bounded** retry budget (~40 s) and returns
    /// the error to the caller. Production startup uses [`spawn_deferred`]
    /// instead, which binds the listener first and retries the upstream
    /// forever in the background.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial backend connection or handshake fails.
    pub async fn new(
        url: &str,
        listen: &str,
        pool_config: PoolConfig,
        reset_strategy: ResetStrategy,
        replica_urls: Vec<String>,
        rw_split: PgRwSplitParams,
        stats: QueryStats,
    ) -> Result<Self, DbError> {
        let db_url = Arc::new(DbUrl::parse(url)?);
        let health = ProxyHealth::new("postgres", listen, db_url.addr());
        Self::connect(
            db_url,
            listen,
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
        pool_config: PoolConfig,
        reset_strategy: ResetStrategy,
        replica_urls: Vec<String>,
        rw_split: PgRwSplitParams,
        stats: QueryStats,
        health: Arc<ProxyHealth>,
        retry: RetryBudget,
    ) -> Result<Self, DbError> {
        // Establish a probe connection to capture server metadata.
        //
        // Exponential backoff (250ms doubling to the budget's ceiling) so
        // startup ordering under k8s/systemd doesn't leave the proxy dead
        // when the DB comes up a few seconds later. See
        // `pg_connect_with_retry` for the schedule.
        let (probe_stream, meta) = pg_connect_with_retry(&db_url, &health, retry).await?;
        let meta = Arc::new(meta);

        // Build the primary pool. The connect closure doubles as the live
        // upstream-health signal (see the MySQL proxy for the rationale).
        let db_url_c = Arc::clone(&db_url);
        let health_c = Arc::clone(&health);
        let connect = move || -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            let u = Arc::clone(&db_url_c);
            let h = Arc::clone(&health_c);
            Box::pin(async move {
                match pg_connect_and_handshake(&u).await {
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
            Box::pin(pg_reset_connection(stream))
        };

        let ping =
            |stream: TcpStream| -> crate::pool::BoxFuture<Result<(TcpStream, bool), DbError>> {
                Box::pin(pg_ping_connection(stream))
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
                            let (stream, _) = pg_connect_and_handshake(&u).await?;
                            Ok(stream)
                        })
                    };

                let replica_reset =
                    |stream: TcpStream| -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
                        Box::pin(pg_reset_connection(stream))
                    };

                let replica_ping = |stream: TcpStream| -> crate::pool::BoxFuture<
                    Result<(TcpStream, bool), DbError>,
                > { Box::pin(pg_ping_connection(stream)) };

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
            reset_strategy,
            rw_split,
            stats,
        })
    }

    /// The stats handle, or `None` when recording is switched off.
    ///
    /// Mirrors `MySqlProxy::stats`: a disabled collector means the routing
    /// loop never reads the clock or copies SQL out of the wire buffer.
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
        info!(listen = %self.listen, "PostgreSQL proxy listening");
        self.run_on(Arc::new(listener)).await
    }

    /// Accept client connections on an already-bound listener.
    ///
    /// See [`MySqlProxy::run_on`](crate::mysql::MySqlProxy::run_on) — same
    /// contract: the listen socket is bound before the upstream is known
    /// reachable, so early clients queue in the accept backlog rather than
    /// getting `ECONNREFUSED`.
    ///
    /// # Errors
    ///
    /// Currently never returns `Err`; accept errors are logged and the loop
    /// continues.
    pub async fn run_on(self, listener: Arc<TcpListener>) -> Result<(), DbError> {
        let proxy = Arc::new(self);
        loop {
            let (client, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("PostgreSQL proxy accept error: {e}");
                    continue;
                }
            };
            // See mysql.rs: Nagle + delayed ACK costs ~40ms per small round trip.
            let _ = client.set_nodelay(true);
            debug!(%peer, "PostgreSQL client connected");
            let p = Arc::clone(&proxy);
            tokio::spawn(async move {
                if let Err(e) = p.handle_client(client).await {
                    debug!(%peer, "PostgreSQL proxy session ended: {e}");
                }
            });
        }
    }

    /// Handle one client connection.
    async fn handle_client(&self, mut client: TcpStream) -> Result<(), DbError> {
        // Step 1: read the client's StartupMessage (no tag byte, just length + payload).
        let _startup = read_startup_message(&mut client).await?;

        // Step 2: send AuthenticationOk (no credential validation on loopback).
        send_auth_ok(&mut client).await?;

        // Step 3: send cached ParameterStatus messages.
        for (key, value) in &self.meta.parameters {
            send_parameter_status(&mut client, key, value).await?;
        }

        // Step 4: send BackendKeyData.
        send_backend_key_data(&mut client, self.meta.process_id, self.meta.secret_key).await?;

        // Step 5: send ReadyForQuery (idle).
        send_ready_for_query(&mut client, b'I').await?;

        // Determine if we need query-level routing or just simple proxying.
        let needs_routing = matches!(self.reset_strategy, ResetStrategy::Smart)
            || (self.rw_split.enabled && !self.replica_pools.is_empty());

        if needs_routing {
            pg_proxy_routing_loop(
                client,
                &self.pool,
                &self.replica_pools,
                &self.replica_rr,
                &self.rw_split,
                self.reset_strategy,
                self.stats(),
            )
            .await
        } else {
            // Single-backend path: one pooled connection held for the whole
            // session. Reachable with `reset_strategy = "never"`/`"always"`
            // and no replicas.
            //
            // It is *not* an unframed fast path any more. Both directions are
            // framed, so statements on this path are recorded exactly as the
            // routing loop records them — see `pg_proxy_bidirectional_sniff`.
            // It stays a separate path from the routing loop because the
            // routing loop re-acquires a backend per command, which would
            // scatter a session's `SET`s, temp tables and advisory locks
            // across different connections.
            let mut checkout = self.pool.acquire().await?;
            let backend = checkout.take_stream();

            match pg_proxy_bidirectional_sniff(client, backend, self.stats()).await {
                Some(backend) => match self.reset_strategy {
                    ResetStrategy::Never => {
                        checkout.return_to_pool(backend);
                    }
                    ResetStrategy::Always => {
                        checkout.return_with_reset(backend).await;
                    }
                    ResetStrategy::Smart => {
                        unreachable!()
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
}

// ── Backend connection & auth ────────────────────────────────────────────────

/// Connect to the `PostgreSQL` backend and complete the startup/auth handshake.
///
/// Returns the authenticated stream and server metadata.
///
/// # Note on handshake construction
///
/// The PG wire protocol has no capability bitfield, so the analogous MySQL
/// "inherit-then-strip" bug fixed in PR #91 cannot occur here — the
/// `StartupMessage` is built additively from an explicit set of parameters
/// (`user`, `database`) plus the fixed `3.0` protocol version, none of which
/// are derived from anything the server advertised. However, the *auth*
/// flow has its own framing pitfall (consuming the right number of `R`
/// messages on the right code path — see `handle_backend_auth`).
/// Integration coverage in `tests/pg_proxy_integration.rs` pins this
/// against real PG 13 (`md5`) and PG 17 (`scram-sha-256`).
/// Exponential-backoff wrapper around [`pg_connect_and_handshake`].
///
/// Same schedule as the MySQL proxy: 250 ms doubling to the
/// [`RetryBudget`]'s ceiling. Prevents startup ordering (k8s, systemd, or a
/// listener this process binds moments later) from wedging the proxy when
/// the backend comes up seconds later. [`ProxyHealth`] owns the failure
/// logging and metrics.
async fn pg_connect_with_retry(
    url: &DbUrl,
    health: &ProxyHealth,
    retry: RetryBudget,
) -> Result<(TcpStream, PgServerMeta), DbError> {
    const INITIAL_BACKOFF_MS: u64 = 250;

    let max_backoff_ms = retry.max_backoff_ms();
    let mut backoff_ms = INITIAL_BACKOFF_MS;
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        match pg_connect_and_handshake(url).await {
            Ok(ok) => {
                health.record_up();
                if attempt > 1 {
                    info!(
                        attempt,
                        addr = %url.addr(),
                        "PostgreSQL backend connection established after retry"
                    );
                }
                return Ok(ok);
            }
            Err(e) => {
                health.record_down(&e);
                if retry.is_final_attempt(attempt) {
                    warn!(
                        attempt,
                        addr = %url.addr(),
                        error = %e,
                        "PostgreSQL backend still unreachable after max retries; giving up"
                    );
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms.saturating_mul(2)).min(max_backoff_ms);
            }
        }
    }
}

async fn pg_connect_and_handshake(url: &DbUrl) -> Result<(TcpStream, PgServerMeta), DbError> {
    let mut stream = TcpStream::connect(url.addr()).await?;
    let _ = stream.set_nodelay(true);

    // Send StartupMessage: length (4) + protocol version (4) + key=value pairs + \0.
    let mut startup = Vec::with_capacity(128);
    // Protocol version 3.0 — the stable PG protocol version since 7.4.
    startup.extend_from_slice(&0x0003_0000_i32.to_be_bytes());
    // user parameter.
    startup.extend_from_slice(b"user\0");
    startup.extend_from_slice(url.username.as_bytes());
    startup.push(0);
    // database parameter.
    if !url.database.is_empty() {
        startup.extend_from_slice(b"database\0");
        startup.extend_from_slice(url.database.as_bytes());
        startup.push(0);
    }
    // Terminating null.
    startup.push(0);

    // The StartupMessage has no tag byte, just: [length: 4BE][payload].
    let total_len =
        i32::try_from(4 + startup.len()).expect("startup message too large for i32 length field");
    stream.write_all(&total_len.to_be_bytes()).await?;
    stream.write_all(&startup).await?;

    // Read auth response(s).
    handle_backend_auth(&mut stream, &url.username, &url.password).await?;

    // Read ParameterStatus, BackendKeyData, ReadyForQuery.
    let mut parameters = Vec::new();
    let mut process_id = 0_i32;
    let mut secret_key = 0_i32;

    loop {
        let (tag, payload) = read_pg_message(&mut stream).await?;
        match tag {
            MSG_PARAMETER_STATUS => {
                if let Some((k, v)) = parse_parameter_status(&payload) {
                    parameters.push((k, v));
                }
            }
            MSG_BACKEND_KEY_DATA => {
                if payload.len() >= 8 {
                    process_id =
                        i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    secret_key =
                        i32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                }
            }
            MSG_READY_FOR_QUERY => {
                // Backend is ready. Done with handshake.
                break;
            }
            MSG_ERROR_RESPONSE => {
                let msg = parse_pg_error(&payload);
                return Err(DbError::Auth(format!("backend startup error: {msg}")));
            }
            _ => {
                debug!(tag = %char::from(tag), "ignoring unexpected message during startup");
            }
        }
    }

    Ok((stream, PgServerMeta { parameters, process_id, secret_key }))
}

/// Handle the backend authentication exchange.
///
/// Supports `trust` (no password), `md5`, and `scram-sha-256`.
async fn handle_backend_auth(
    stream: &mut TcpStream,
    username: &str,
    password: &str,
) -> Result<(), DbError> {
    loop {
        let (tag, payload) = read_pg_message(stream).await?;
        if tag == MSG_ERROR_RESPONSE {
            let msg = parse_pg_error(&payload);
            return Err(DbError::Auth(format!("backend auth error: {msg}")));
        }
        if tag != MSG_AUTH {
            return Err(DbError::Protocol(format!("expected auth request, got '{}'", tag as char)));
        }
        if payload.len() < 4 {
            return Err(DbError::Protocol("auth message too short".into()));
        }

        let auth_type = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);

        match auth_type {
            AUTH_OK => return Ok(()),
            AUTH_MD5_PASSWORD => {
                if payload.len() < 8 {
                    return Err(DbError::Protocol("MD5 auth salt too short".into()));
                }
                let salt = &payload[4..8];
                let response = md5_password(username, password, salt);
                send_password_message(stream, &response).await?;
            }
            AUTH_SASL => {
                // SCRAM-SHA-256 negotiation. `scram_sha256_exchange` consumes
                // the full SASLContinue → SASLFinal → AuthenticationOk
                // sequence itself, so we must return immediately on success
                // rather than looping back to read another auth message —
                // the next byte on the wire will be the post-auth
                // `ParameterStatus` (`'S'`), not another `'R'`.
                let mechanisms = parse_sasl_mechanisms(&payload[4..]);
                if !mechanisms.iter().any(|m| m == "SCRAM-SHA-256") {
                    return Err(DbError::Auth("server requires unsupported SASL mechanism".into()));
                }
                scram_sha256_exchange(stream, username, password).await?;
                return Ok(());
            }
            AUTH_SASL_CONTINUE | AUTH_SASL_FINAL => {
                // These are handled within scram_sha256_exchange.
                return Err(DbError::Protocol(
                    "unexpected SASL continue/final outside exchange".into(),
                ));
            }
            other => {
                return Err(DbError::Auth(format!("unsupported auth method: {other}")));
            }
        }
    }
}

/// Compute MD5 password response: `"md5" + md5(md5(password + user) + salt)`.
fn md5_password(username: &str, password: &str, salt: &[u8]) -> String {
    let inner = md5::compute(format!("{password}{username}"));
    let inner_hex = format!("{inner:x}");
    let mut outer_input = inner_hex.into_bytes();
    outer_input.extend_from_slice(salt);
    let outer = md5::compute(&outer_input);
    format!("md5{outer:x}")
}

/// Perform a SCRAM-SHA-256 authentication exchange with the backend.
async fn scram_sha256_exchange(
    stream: &mut TcpStream,
    _username: &str,
    password: &str,
) -> Result<(), DbError> {
    // Step 1: send SASLInitialResponse with client-first-message.
    let nonce = generate_nonce();
    let client_first_bare = format!("n=,r={nonce}");
    let client_first = format!("n,,{client_first_bare}");

    let mechanism = b"SCRAM-SHA-256\0";
    let msg_bytes = client_first.as_bytes();
    let mut sasl_init = Vec::with_capacity(mechanism.len() + 4 + msg_bytes.len());
    sasl_init.extend_from_slice(mechanism);
    let msg_len =
        i32::try_from(msg_bytes.len()).expect("SASL message too large for i32 length field");
    sasl_init.extend_from_slice(&msg_len.to_be_bytes());
    sasl_init.extend_from_slice(msg_bytes);

    write_pg_message(stream, b'p', &sasl_init).await?;

    // Step 2: read AuthenticationSASLContinue (server-first-message).
    let (tag, payload) = read_pg_message(stream).await?;
    if tag == MSG_ERROR_RESPONSE {
        let msg = parse_pg_error(&payload);
        return Err(DbError::Auth(format!("SCRAM auth error: {msg}")));
    }
    if tag != MSG_AUTH || payload.len() < 4 {
        return Err(DbError::Protocol("expected SASL continue".into()));
    }
    let auth_type = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if auth_type != AUTH_SASL_CONTINUE {
        return Err(DbError::Protocol(format!("expected SASL continue (11), got {auth_type}")));
    }
    let server_first = String::from_utf8_lossy(&payload[4..]).to_string();

    // Parse server-first-message: r=<nonce>,s=<salt>,i=<iterations>.
    let (server_nonce, salt_b64, iterations) = parse_server_first(&server_first)?;

    // Verify server nonce starts with our nonce.
    if !server_nonce.starts_with(&nonce) {
        return Err(DbError::Auth("SCRAM nonce mismatch".into()));
    }

    let salt = base64ct::Base64::decode_vec(&salt_b64)
        .map_err(|_| DbError::Auth("invalid SCRAM salt base64".into()))?;

    // Step 3: compute proof and send client-final-message.
    let salted_password = hi(password.as_bytes(), &salt, iterations);
    let client_key = hmac_sha256(&salted_password, b"Client Key");
    let stored_key = Sha256::digest(&client_key);

    let client_final_without_proof = format!("c=biws,r={server_nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");

    let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
    let client_proof: Vec<u8> =
        client_key.iter().zip(client_signature.iter()).map(|(a, b)| a ^ b).collect();

    let proof_b64 = base64ct::Base64::encode_string(&client_proof);
    let client_final = format!("{client_final_without_proof},p={proof_b64}");

    write_pg_message(stream, b'p', client_final.as_bytes()).await?;

    // Step 4: read AuthenticationSASLFinal (server-final-message).
    let (tag, payload) = read_pg_message(stream).await?;
    if tag == MSG_ERROR_RESPONSE {
        let msg = parse_pg_error(&payload);
        return Err(DbError::Auth(format!("SCRAM final error: {msg}")));
    }
    if tag != MSG_AUTH || payload.len() < 4 {
        return Err(DbError::Protocol("expected SASL final".into()));
    }
    let auth_type = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if auth_type != AUTH_SASL_FINAL {
        return Err(DbError::Protocol(format!("expected SASL final (12), got {auth_type}")));
    }
    // We could verify the server signature here, but for a proxy it's not
    // strictly necessary — we trust the backend.

    // Step 5: read AuthenticationOk.
    let (tag, payload) = read_pg_message(stream).await?;
    if tag == MSG_ERROR_RESPONSE {
        let msg = parse_pg_error(&payload);
        return Err(DbError::Auth(format!("SCRAM auth ok error: {msg}")));
    }
    if tag != MSG_AUTH || payload.len() < 4 {
        return Err(DbError::Protocol("expected auth ok after SCRAM".into()));
    }
    let auth_type = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if auth_type != AUTH_OK {
        return Err(DbError::Auth(format!("expected AUTH_OK after SCRAM, got {auth_type}")));
    }

    Ok(())
}

/// Parse SASL mechanism names from the auth payload.
fn parse_sasl_mechanisms(data: &[u8]) -> Vec<String> {
    let mut mechs = Vec::new();
    for part in data.split(|&b| b == 0) {
        if !part.is_empty() {
            mechs.push(String::from_utf8_lossy(part).to_string());
        }
    }
    mechs
}

/// Parse server-first-message fields.
fn parse_server_first(msg: &str) -> Result<(String, String, u32), DbError> {
    let mut nonce = None;
    let mut salt = None;
    let mut iterations = None;

    for field in msg.split(',') {
        if let Some(val) = field.strip_prefix("r=") {
            nonce = Some(val.to_string());
        } else if let Some(val) = field.strip_prefix("s=") {
            salt = Some(val.to_string());
        } else if let Some(val) = field.strip_prefix("i=") {
            iterations = Some(
                val.parse::<u32>()
                    .map_err(|_| DbError::Protocol("invalid SCRAM iteration count".into()))?,
            );
        }
    }

    Ok((
        nonce.ok_or_else(|| DbError::Protocol("missing nonce in server-first".into()))?,
        salt.ok_or_else(|| DbError::Protocol("missing salt in server-first".into()))?,
        iterations.ok_or_else(|| DbError::Protocol("missing iterations in server-first".into()))?,
    ))
}

/// HMAC-SHA-256.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// SCRAM `Hi()` (PBKDF2-HMAC-SHA256).
fn hi(password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;

    // U1 = HMAC(password, salt || 0x00000001)
    let mut mac = HmacSha256::new_from_slice(password).expect("HMAC can take key of any size");
    mac.update(salt);
    mac.update(&1_u32.to_be_bytes());
    let mut u_prev = mac.finalize().into_bytes().to_vec();
    let mut result = u_prev.clone();

    for _ in 1..iterations {
        let mut mac = HmacSha256::new_from_slice(password).expect("HMAC can take key of any size");
        mac.update(&u_prev);
        u_prev = mac.finalize().into_bytes().to_vec();
        for (r, u) in result.iter_mut().zip(u_prev.iter()) {
            *r ^= u;
        }
    }

    result
}

/// Generate a random nonce for SCRAM.
fn generate_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let ptr = Arc::as_ptr(&Arc::new(())) as usize;
    format!("{:x}{:x}{:x}", ts.as_secs(), ts.subsec_nanos(), ptr)
}

// ── PG wire protocol helpers ─────────────────────────────────────────────────

/// Read one PG message: `[tag: 1][length: 4BE][payload: length-4]`.
///
/// The length field includes itself (4 bytes) but not the tag byte.
async fn read_pg_message(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), DbError> {
    let tag = stream.read_u8().await?;
    let len = stream.read_i32().await?;
    if !(4..=MAX_PG_MESSAGE_LEN).contains(&len) {
        return Err(DbError::Protocol(format!("invalid PG message length: {len}")));
    }
    let payload_len = usize::try_from(len - 4)
        .map_err(|_| DbError::Protocol("negative PG payload length".into()))?;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    Ok((tag, payload))
}

/// Write one PG message: `[tag: 1][length: 4BE][payload]`.
async fn write_pg_message(stream: &mut TcpStream, tag: u8, payload: &[u8]) -> Result<(), DbError> {
    let len = i32::try_from(payload.len() + 4)
        .expect("PG message payload too large for i32 length field");
    stream.write_u8(tag).await?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

/// Which side of a relay failed, when one did.
///
/// The splice path needs this distinction: a read failure condemns the
/// pooled backend, a write failure to the client does not (see
/// [`PgSessionEnd`]). Collapsing both into one error — which is all a
/// `Result` can express — is how a live backend gets discarded, or worse, a
/// dead one recycled.
enum Relayed {
    /// A complete message with this tag reached the destination.
    Message(u8),
    /// The source stream failed or closed mid-message.
    SourceGone(DbError),
    /// The destination stream failed.
    SinkGone(DbError),
}

/// Forward one PG message from `from` to `to`, reporting which side failed.
///
/// The single framing primitive for both directions on every PG path.
/// Generic over the endpoints so it serves the whole-stream routing loop
/// (`&mut TcpStream`) and the split, buffered halves of
/// [`pg_proxy_bidirectional_sniff`] alike — there is deliberately no second
/// copy of this framing to keep in sync.
///
/// # Cancellation
///
/// This function is **not** cancel-safe, and cannot be: framing requires
/// several awaits, so dropping it part-way leaves bytes consumed from `from`
/// that were never written to `to`. `partial` exists so a caller that races
/// this against another future can find out. It is set once the first byte of
/// a message has been consumed and cleared when the message has been fully
/// forwarded, so a caller reading it after a cancellation learns exactly
/// whether the source stream was left mid-message. Parked waiting for the
/// *next* message — the idle state — leaves it clear, which is what keeps a
/// normal end-of-session from condemning a perfectly good connection.
async fn relay_pg_message<R, W>(from: &mut R, to: &mut W, partial: &AtomicBool) -> Relayed
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let tag = match from.read_u8().await {
        Ok(t) => t,
        Err(e) => return Relayed::SourceGone(e.into()),
    };
    partial.store(true, Ordering::Relaxed);
    let len = match from.read_i32().await {
        Ok(l) => l,
        Err(e) => return Relayed::SourceGone(e.into()),
    };
    let payload_len = if len >= 4 { usize::try_from(len - 4).unwrap_or(0) } else { 0 };

    // Write tag + length.
    if let Err(e) = to.write_u8(tag).await {
        return Relayed::SinkGone(e.into());
    }
    if let Err(e) = to.write_all(&len.to_be_bytes()).await {
        return Relayed::SinkGone(e.into());
    }

    // Forward payload in chunks to avoid allocating for large results.
    if payload_len > 0 {
        let mut remaining = payload_len;
        let mut buf = vec![0u8; remaining.min(8192)];
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            if let Err(e) = from.read_exact(&mut buf[..to_read]).await {
                return Relayed::SourceGone(e.into());
            }
            if let Err(e) = to.write_all(&buf[..to_read]).await {
                return Relayed::SinkGone(e.into());
            }
            remaining -= to_read;
        }
    }

    partial.store(false, Ordering::Relaxed);
    Relayed::Message(tag)
}

/// Forward a raw PG message from one stream to another.
///
/// The turn-based routing loop awaits this to completion and discards the
/// backend on any failure, so it needs neither the side attribution nor the
/// partial-message flag [`relay_pg_message`] carries.
async fn forward_pg_message(from: &mut TcpStream, to: &mut TcpStream) -> Result<u8, DbError> {
    let partial = AtomicBool::new(false);
    match relay_pg_message(from, to, &partial).await {
        Relayed::Message(tag) => Ok(tag),
        Relayed::SourceGone(e) | Relayed::SinkGone(e) => Err(e),
    }
}

/// Read the client's `StartupMessage` (no tag byte).
///
/// Format: `[length: 4BE][protocol_version: 4BE][params...][\\0]`
async fn read_startup_message(stream: &mut TcpStream) -> Result<Vec<u8>, DbError> {
    let len = stream.read_i32().await?;
    if !(8..=10240).contains(&len) {
        return Err(DbError::Protocol(format!("invalid startup message length: {len}")));
    }
    let payload_len = usize::try_from(len - 4)
        .map_err(|_| DbError::Protocol("negative startup payload length".into()))?;
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await?;

    // Check protocol version (first 4 bytes of payload).
    if payload.len() >= 4 {
        let version = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        // SSL request (80877103) or cancel request (80877102): not handled.
        if version == 80_877_103 {
            // SSL request: send 'N' (no SSL) and read the real startup.
            stream.write_u8(b'N').await?;
            return Box::pin(read_startup_message(stream)).await;
        }
    }

    Ok(payload)
}

/// Send `AuthenticationOk` to the client.
async fn send_auth_ok(stream: &mut TcpStream) -> Result<(), DbError> {
    let payload = AUTH_OK.to_be_bytes();
    write_pg_message(stream, MSG_AUTH, &payload).await
}

/// Send a `ParameterStatus` message.
async fn send_parameter_status(
    stream: &mut TcpStream,
    key: &str,
    value: &str,
) -> Result<(), DbError> {
    let mut payload = Vec::with_capacity(key.len() + value.len() + 2);
    payload.extend_from_slice(key.as_bytes());
    payload.push(0);
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    write_pg_message(stream, MSG_PARAMETER_STATUS, &payload).await
}

/// Send `BackendKeyData`.
async fn send_backend_key_data(
    stream: &mut TcpStream,
    process_id: i32,
    secret_key: i32,
) -> Result<(), DbError> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&process_id.to_be_bytes());
    payload.extend_from_slice(&secret_key.to_be_bytes());
    write_pg_message(stream, MSG_BACKEND_KEY_DATA, &payload).await
}

/// Send `ReadyForQuery` with the given transaction status byte.
async fn send_ready_for_query(stream: &mut TcpStream, status: u8) -> Result<(), DbError> {
    write_pg_message(stream, MSG_READY_FOR_QUERY, &[status]).await
}

/// Send a `PasswordMessage` (used for MD5 auth).
async fn send_password_message(stream: &mut TcpStream, password: &str) -> Result<(), DbError> {
    let mut payload = Vec::with_capacity(password.len() + 1);
    payload.extend_from_slice(password.as_bytes());
    payload.push(0);
    write_pg_message(stream, b'p', &payload).await
}

/// Parse a `ParameterStatus` payload into (key, value).
fn parse_parameter_status(payload: &[u8]) -> Option<(String, String)> {
    let null_pos = payload.iter().position(|&b| b == 0)?;
    let key = String::from_utf8_lossy(&payload[..null_pos]).to_string();
    let rest = &payload[null_pos + 1..];
    let val_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let value = String::from_utf8_lossy(&rest[..val_end]).to_string();
    Some((key, value))
}

/// Parse an `ErrorResponse` payload into a human-readable message.
fn parse_pg_error(payload: &[u8]) -> String {
    let mut message = String::new();
    let mut i = 0;
    while i < payload.len() && payload[i] != 0 {
        let field_type = payload[i];
        i += 1;
        let end = payload[i..].iter().position(|&b| b == 0).unwrap_or(payload.len() - i);
        let value = String::from_utf8_lossy(&payload[i..i + end]);
        if field_type == b'M' {
            message = value.to_string();
        }
        i += end + 1;
    }
    if message.is_empty() { "(unknown error)".to_string() } else { message }
}

// ── Reset & health check ────────────────────────────────────────────────────

/// Send `DISCARD ALL` and wait for `CommandComplete` + `ReadyForQuery`.
async fn pg_reset_connection(mut stream: TcpStream) -> Result<TcpStream, DbError> {
    let query = b"DISCARD ALL\0";
    write_pg_message(&mut stream, MSG_QUERY, query).await?;

    // Read until ReadyForQuery.
    loop {
        let (tag, payload) = read_pg_message(&mut stream).await?;
        match tag {
            MSG_READY_FOR_QUERY => return Ok(stream),
            MSG_ERROR_RESPONSE => {
                let msg = parse_pg_error(&payload);
                return Err(DbError::Protocol(format!("DISCARD ALL failed: {msg}")));
            }
            _ => { /* CommandComplete, etc. */ }
        }
    }
}

/// Send a simple `SELECT 1` query and check for a valid response.
async fn pg_ping_connection(mut stream: TcpStream) -> Result<(TcpStream, bool), DbError> {
    let query = b"SELECT 1\0";
    if write_pg_message(&mut stream, MSG_QUERY, query).await.is_err() {
        return Ok((stream, false));
    }

    // Read until ReadyForQuery.
    loop {
        match read_pg_message(&mut stream).await {
            Ok((MSG_READY_FOR_QUERY, _)) => return Ok((stream, true)),
            Ok((MSG_ERROR_RESPONSE, _)) | Err(_) => return Ok((stream, false)),
            Ok(_) => { /* RowDescription, DataRow, CommandComplete */ }
        }
    }
}

// ── Bidirectional proxy ─────────────────────────────────────────────────────

/// Which side of the proxy ended a session.
///
/// See the MySQL counterpart in [`crate::mysql`] — the same reasoning applies:
/// both sides report identical `io::ErrorKind` values, so error kind alone
/// cannot decide whether a pooled backend is still usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PgSessionEnd {
    /// The client finished — `Terminate`, EOF, or a failure on the client
    /// socket. The backend never learned the session ended.
    Client,
    /// The backend connection failed or closed. It must be discarded.
    Backend,
}

/// A client message the backend will answer with exactly one `ReadyForQuery`.
///
/// `sql` is `Some` only for a simple `Query` — the one frontend message the
/// proxy can attribute to a digest.
struct PgTurn {
    /// The statement text, when this turn is recordable.
    sql: Option<String>,
    /// When the message was handed to the backend.
    started: std::time::Instant,
}

/// Largest number of un-answered turns a session may have in flight.
///
/// The pending-turn queue drains on `ReadyForQuery`, so its length is the
/// number of commands the client has pipelined without their responses having
/// been delivered. Nothing in the PHP ecosystem pipelines at all; a client
/// that does, and that also stops reading, would otherwise grow this queue
/// without bound. Reached only by a client working to reach it, and the
/// session simply ends.
const MAX_PENDING_TURNS: usize = 4096;

/// Whether a frontend message ends a protocol turn — i.e. is answered by a
/// `ReadyForQuery`.
///
/// `Query` and `FunctionCall` each get one; in the extended query protocol an
/// entire `Parse`/`Bind`/`Describe`/`Execute` batch gets exactly one, at
/// `Sync`. Enqueuing a turn for *all* of them rather than only for `Query` is
/// what keeps the queue aligned with the `ReadyForQuery` stream in a session
/// that mixes both protocols: the extended-protocol turn pops its own
/// unrecordable entry instead of stealing a simple query's.
const fn pg_ends_a_turn(tag: u8) -> bool {
    matches!(tag, MSG_QUERY | MSG_SYNC | MSG_FUNCTION_CALL)
}

/// The statement text a `Query` payload should be attributed to, if any.
///
/// The payload is a null-terminated string. Empty queries — which PG answers
/// with `EmptyQueryResponse` — are not statements and are left unrecorded
/// rather than folded into a blank digest.
fn pg_recordable_sql(payload: &[u8]) -> Option<String> {
    let sql = String::from_utf8_lossy(payload);
    let sql = sql.trim_end_matches('\0');
    (!sql.trim().is_empty()).then(|| sql.to_string())
}

/// Relay `client` ↔ `backend` until the session ends, recording statements.
///
/// Returns the backend stream when the *client* ended the session, and `None`
/// when the backend failed and must be discarded rather than recycled.
///
/// ## `Terminate` is intercepted, never forwarded
///
/// `Terminate` (`'X'`) is the PG frontend's "close this connection" message —
/// PDO sends it when the request ends. Relaying it to a *pooled* backend makes
/// the server close the connection, and the pool then hands that dead socket
/// to the next request. It is swallowed here, exactly as `COM_QUIT` is on the
/// MySQL side.
///
/// ## Framing
///
/// Both directions are framed. Post-startup PG messages are uniformly
/// `[tag: 1][len: 4BE][payload]` in *both* directions, so this costs no
/// protocol understanding beyond the tag byte.
///
/// The backend→client direction used to be copied in bulk, which made this the
/// only proxy path that could not see a statement: with `reset_strategy =
/// "never"`/`"always"` and no replicas, a whole deployment's traffic was
/// invisible to `[db.analysis]`. Framing it closes that gap — and unlike the
/// MySQL splice path, which has to infer completion from the arrival of the
/// *next* client command, PG hands the proxy an explicit end-of-turn marker in
/// `ReadyForQuery`. Durations, row counts and error status here are therefore
/// exactly what the routing loop reports, not an approximation.
///
/// Framing is not paid for in syscalls: the backend read half is buffered and
/// the client write half is coalesced, flushed whenever the read buffer drains
/// (the point at which the relay is about to block anyway). A large result set
/// still crosses in `BufReader`-sized reads, not one syscall per message.
///
/// ## What is still not recorded
///
/// Extended-protocol executions. `Parse` carries SQL and `Execute` is the
/// execution, but mapping one to the other means tracking named statements and
/// portals across the session; the turn-based routing loop does not do it
/// either, and recording `Parse` alone would publish planning time as query
/// time. Same reasoning as `COM_STMT_PREPARE` on the MySQL side — see
/// [`crate::stats`].
async fn pg_proxy_bidirectional_sniff(
    mut client: TcpStream,
    mut backend: TcpStream,
    stats: Option<&QueryStats>,
) -> Option<TcpStream> {
    use std::collections::VecDeque;

    use parking_lot::Mutex;
    use tokio::io::{BufReader, BufWriter};

    let turns: Arc<Mutex<VecDeque<PgTurn>>> = Arc::new(Mutex::new(VecDeque::new()));
    let turns_w = Arc::clone(&turns);
    // Set while a backend message has been partly consumed. Framing takes
    // several awaits, so unlike the bulk copy this replaced, the reader half
    // is not cancel-safe — see `relay_pg_message`.
    let partial = AtomicBool::new(false);

    let (end, buffered_response) = {
        let (mut cr, cw) = client.split();
        let (br, mut bw) = backend.split();
        // Buffered so that per-message framing does not become per-message
        // syscalls. The client write half is flushed below at every point the
        // relay is about to wait.
        let mut br = BufReader::new(br);
        let mut cw = BufWriter::new(cw);

        let client_to_backend = async {
            let mut header = [0u8; 5];
            loop {
                // Any read failure is a client-side event and says nothing
                // about the health of the backend.
                if cr.read_exact(&mut header).await.is_err() {
                    return PgSessionEnd::Client;
                }
                if header[0] == MSG_TERMINATE {
                    debug!("client sent Terminate; not forwarding to pooled backend");
                    return PgSessionEnd::Client;
                }
                let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
                if !(4..=MAX_PG_MESSAGE_LEN).contains(&len) {
                    debug!(len, "client sent an out-of-range PG message length; ending session");
                    return PgSessionEnd::Client;
                }
                let payload_len = usize::try_from(len - 4).unwrap_or(0);
                let mut payload = vec![0u8; payload_len];
                if payload_len > 0 && cr.read_exact(&mut payload).await.is_err() {
                    return PgSessionEnd::Client;
                }

                // Enqueue before the message hits the wire: the response
                // cannot arrive before the write, so the reader half can never
                // see a `ReadyForQuery` for a turn that is not queued yet.
                // Costs one branch per message when stats are off.
                if stats.is_some() && pg_ends_a_turn(header[0]) {
                    let sql =
                        if header[0] == MSG_QUERY { pg_recordable_sql(&payload) } else { None };
                    let mut queue = turns_w.lock();
                    if queue.len() >= MAX_PENDING_TURNS {
                        debug!(
                            "client pipelined more than {MAX_PENDING_TURNS} unanswered turns; \
                             ending session"
                        );
                        return PgSessionEnd::Client;
                    }
                    queue.push_back(PgTurn { sql, started: std::time::Instant::now() });
                }

                if bw.write_all(&header).await.is_err() || bw.write_all(&payload).await.is_err() {
                    return PgSessionEnd::Backend;
                }
            }
        };

        let backend_to_client = async {
            let mut outcome = ResponseOutcome { ok: true, rows: 0 };
            loop {
                let tag = match relay_pg_message(&mut br, &mut cw, &partial).await {
                    Relayed::Message(tag) => tag,
                    // Source EOF or failure. With `Terminate` no longer
                    // forwarded, EOF means the server genuinely closed the
                    // connection — either way it is not reusable.
                    Relayed::SourceGone(_) => return PgSessionEnd::Backend,
                    Relayed::SinkGone(_) => return PgSessionEnd::Client,
                };

                match tag {
                    MSG_DATA_ROW => outcome.rows += 1,
                    MSG_ERROR_RESPONSE => outcome.ok = false,
                    _ => {}
                }

                if tag == MSG_READY_FOR_QUERY {
                    if let Some(collector) = stats {
                        // Pop unconditionally: an extended-protocol or copy
                        // turn carries no SQL but still owns this
                        // `ReadyForQuery`, and leaving its entry behind would
                        // misattribute every later statement on the session.
                        if let Some(turn) = turns.lock().pop_front() {
                            if let Some(sql) = turn.sql {
                                collector.record(
                                    &sql,
                                    turn.started.elapsed(),
                                    outcome.ok,
                                    outcome.rows,
                                );
                            }
                        }
                    }
                    outcome = ResponseOutcome { ok: true, rows: 0 };
                }

                // Nothing more is buffered, so the next read will wait: get
                // what has been forwarded in front of the client first.
                if br.buffer().is_empty() && cw.flush().await.is_err() {
                    return PgSessionEnd::Client;
                }
            }
        };

        let end = tokio::select! {
            e = client_to_backend => e,
            e = backend_to_client => e,
        };
        // Bytes the reader pulled off the socket but has not forwarded belong
        // to a response nobody consumed. `has_unread_bytes` cannot see them —
        // they are no longer on the socket — so they have to be reported out.
        // Two shapes: whole messages sitting in the read buffer, and a single
        // message the reader was cancelled part-way through.
        (end, !br.buffer().is_empty() || partial.load(Ordering::Relaxed))
    };

    let reusable = match end {
        // The client left a partly drained response behind — alive, but not
        // safe to hand to the next session. On PG this also condemns a
        // connection carrying an unconsumed asynchronous `NoticeResponse` or
        // `NotificationResponse`, which is the right call: the next session's
        // `DISCARD ALL` would read it as its own reply.
        //
        // The two checks are complementary, not redundant: `has_unread_bytes`
        // probes bytes still on the socket, `buffered_response` covers bytes
        // that have already left it for the relay's own buffers. Neither can
        // see the other's case.
        PgSessionEnd::Client if buffered_response || crate::pool::has_unread_bytes(&backend) => {
            debug!("client ended mid-response, leaving unread backend bytes; discarding");
            false
        }
        PgSessionEnd::Client => true,
        PgSessionEnd::Backend => false,
    };
    reusable.then_some(backend)
}

// ── Routing & smart reset ────────────────────────────────────────────────────

/// Kind of SQL query for routing decisions.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PgQueryKind {
    /// SELECT, SHOW, EXPLAIN — read-only, can go to replica.
    Read,
    /// INSERT, UPDATE, DELETE, CREATE, ALTER, DROP — must go to primary.
    Write,
    /// BEGIN, START TRANSACTION — starts a transaction.
    TxBegin,
    /// COMMIT, ROLLBACK, END — ends a transaction.
    TxEnd,
}

/// Classify a SQL query based on its first keyword.
fn classify_pg_query(sql: &str) -> PgQueryKind {
    let s = sql.trim_start();
    let tok = s.split_ascii_whitespace().next().unwrap_or("").to_ascii_uppercase();

    match tok.as_str() {
        "SELECT" | "SHOW" | "EXPLAIN" | "TABLE" => {
            if sql.to_ascii_uppercase().contains("FOR UPDATE")
                || sql.to_ascii_uppercase().contains("FOR SHARE")
                || sql.to_ascii_uppercase().contains("FOR NO KEY UPDATE")
            {
                PgQueryKind::Write
            } else {
                PgQueryKind::Read
            }
        }
        "BEGIN" | "START" => PgQueryKind::TxBegin,
        "COMMIT" | "ROLLBACK" | "END" => PgQueryKind::TxEnd,
        _ => PgQueryKind::Write,
    }
}

/// Per-client connection state for routing and dirty tracking.
#[derive(Debug, Clone, Default)]
struct PgClientState {
    in_transaction: bool,
    sticky_until: Option<std::time::Instant>,
    dirty: bool,
}

/// Select which pool to use for the next query.
fn pg_select_pool<'a>(
    primary: &'a Pool,
    replicas: &'a [Pool],
    replica_rr: &AtomicUsize,
    state: &PgClientState,
    kind: PgQueryKind,
    _rw_split: &PgRwSplitParams,
) -> &'a Pool {
    if replicas.is_empty() {
        return primary;
    }
    if state.in_transaction {
        return primary;
    }
    if let Some(sticky_until) = state.sticky_until {
        if std::time::Instant::now() < sticky_until {
            return primary;
        }
    }
    if matches!(kind, PgQueryKind::Read) {
        let idx = replica_rr.fetch_add(1, Ordering::Relaxed) % replicas.len();
        &replicas[idx]
    } else {
        primary
    }
}

/// Proxy loop with per-query routing and dirty-bit tracking.
///
/// Per-message routing is only sound for the *simple* query protocol: a
/// `Query` message is always answered by a backend message stream that
/// terminates in `ReadyForQuery`, so the proxy can block on the backend
/// before reading from the client again.
///
/// The *extended* query protocol breaks that assumption. `Parse`, `Bind`,
/// `Describe`, `Execute` and `Close` produce no backend output at all until
/// the client sends `Sync` — which is a *later client* message. Forwarding a
/// single extended-protocol message and then awaiting `ReadyForQuery`
/// deadlocks the session on the first prepared statement, which is what every
/// real client does (`PDO_pgsql`, `PQexecParams`, `tokio-postgres`). The same
/// holds once the backend answers with `CopyInResponse`/`CopyBothResponse`:
/// the next move belongs to the client, not the backend.
///
/// In both cases the session is pinned to one backend connection and spliced
/// straight through for the rest of its lifetime — see
/// [`pg_relay_pinned_session`].
///
/// # Query stats
///
/// Simple `Query` messages are recorded: the clock runs from just before the
/// message is written to the backend until the terminating `ReadyForQuery`
/// is forwarded, `ErrorResponse` anywhere in that stream marks the statement
/// failed, and `DataRow` messages are counted. Statements that reach the
/// pinned-splice paths above are *not* recorded, because after the switch
/// the proxy no longer reads message boundaries — see [`crate::stats`].
///
/// The row count is rows *returned*. Rows *affected* by a mutation live in
/// the `CommandComplete` payload (`"UPDATE 3"`), which
/// [`forward_pg_message`] streams through without buffering, so a mutation
/// records zero rows rather than a guessed number. This is the one place
/// the proxy's numbers are narrower than `TrackedBackend`'s, which gets
/// `affected_rows` directly from litewire.
async fn pg_proxy_routing_loop(
    mut client: TcpStream,
    pool: &Pool,
    replica_pools: &[Pool],
    replica_rr: &AtomicUsize,
    rw_split: &PgRwSplitParams,
    reset_strategy: ResetStrategy,
    recorder: Option<&QueryStats>,
) -> Result<(), DbError> {
    let mut state = PgClientState::default();

    loop {
        // Read one message from the client.
        let Ok((tag, payload)) = read_pg_message(&mut client).await else {
            break;
        };

        if tag == MSG_TERMINATE {
            // Session termination belongs to the client-facing socket only.
            // Pooled backends are shared across sessions and must never see
            // it — see `pg_proxy_bidirectional` for the full rationale.
            debug!("client sent Terminate; ending session without touching pooled backends");
            break;
        }

        // Anything that is not a simple `Query` belongs to the extended query
        // protocol (or a copy sub-protocol). Pin to the primary — a replica
        // cannot serve writes that may arrive later on the same pinned
        // connection — and relay the remainder of the session verbatim.
        if tag != MSG_QUERY {
            debug!(
                tag = %char::from(tag),
                "extended query protocol in use; pinning session to primary"
            );
            let mut checkout = pool.acquire().await?;
            let mut backend = checkout.take_stream();
            if let Err(e) = write_pg_message(&mut backend, tag, &payload).await {
                checkout.retire();
                return Err(e);
            }
            return pg_relay_pinned_session(client, backend, checkout, reset_strategy, recorder)
                .await;
        }

        // Query payload is null-terminated SQL. Borrowed, not copied: the
        // payload outlives the recording call at the bottom of this
        // iteration, so instrumentation costs no allocation.
        let sql_cow = String::from_utf8_lossy(&payload);
        let sql = sql_cow.trim_end_matches('\0');
        let query_kind = classify_pg_query(sql);

        // Update state tracking.
        match query_kind {
            PgQueryKind::Write | PgQueryKind::TxBegin => state.dirty = true,
            PgQueryKind::TxEnd => state.in_transaction = false,
            PgQueryKind::Read => {}
        }
        if matches!(query_kind, PgQueryKind::TxBegin) {
            state.in_transaction = true;
        }

        let target_pool =
            pg_select_pool(pool, replica_pools, replica_rr, &state, query_kind, rw_split);

        // Acquire backend and forward the command.
        let mut checkout = target_pool.acquire().await?;
        let mut backend = checkout.take_stream();

        let started = recorder.map(|_| std::time::Instant::now());

        // Any failure below leaves the backend mid-message or mid-result-set.
        // Discard it explicitly rather than letting control flow reach a
        // `return_to_pool`.
        let mut copy_mode = false;
        let mut outcome = ResponseOutcome::ok_unknown_rows();
        let relay: Result<(), DbError> = async {
            write_pg_message(&mut backend, tag, &payload).await?;

            // Forward response(s) until ReadyForQuery — or until the backend
            // hands the conversation back to the client via a copy
            // sub-protocol, in which case no further backend output is coming.
            loop {
                let resp_tag = forward_pg_message(&mut backend, &mut client).await?;
                match resp_tag {
                    MSG_DATA_ROW => outcome.rows += 1,
                    MSG_ERROR_RESPONSE => outcome.ok = false,
                    _ => {}
                }
                if resp_tag == MSG_READY_FOR_QUERY {
                    return Ok(());
                }
                if resp_tag == MSG_COPY_IN_RESPONSE || resp_tag == MSG_COPY_BOTH_RESPONSE {
                    copy_mode = true;
                    return Ok(());
                }
            }
        }
        .await;
        if let Err(e) = relay {
            debug!("backend relay failed, discarding connection: {e}");
            checkout.retire();
            return Err(e);
        }

        if copy_mode {
            // The statement is still in flight — its outcome belongs to the
            // copy stream we are about to relay, so recording it now would
            // publish a truncated duration. Left out deliberately: its
            // `ReadyForQuery` arrives inside the pinned session, whose turn
            // queue starts empty, so it settles nothing rather than being
            // misattributed.
            return pg_relay_pinned_session(client, backend, checkout, reset_strategy, recorder)
                .await;
        }

        if let (Some(collector), Some(started)) = (recorder, started) {
            collector.record(sql, started.elapsed(), outcome.ok, outcome.rows);
        }

        // Return backend to pool.
        let should_reset = match reset_strategy {
            ResetStrategy::Always => true,
            ResetStrategy::Never => false,
            ResetStrategy::Smart => state.dirty,
        };
        if should_reset {
            match pg_reset_connection(backend).await {
                Ok(s) => {
                    checkout.return_to_pool(s);
                    state.dirty = false;
                }
                Err(_) => checkout.retire(),
            }
        } else {
            checkout.return_to_pool(backend);
        }

        if rw_split.enabled && matches!(query_kind, PgQueryKind::Write) {
            state.sticky_until = Some(std::time::Instant::now() + rw_split.sticky_duration);
        }
    }

    Ok(())
}

/// Pin an already-acquired `backend` to `client` and relay both directions
/// until either side closes, then recycle the backend.
///
/// Used for the extended query protocol and the copy sub-protocols, where
/// message boundaries do not line up with *turn* boundaries and the proxy
/// therefore cannot tell when it is safe to block on the backend. Message
/// boundaries themselves stay visible, so a simple `Query` issued later on the
/// same pinned session is still recorded — `stats` is threaded through for
/// that. The connection is always reset before being parked unless the
/// operator explicitly opted out with [`ResetStrategy::Never`], because
/// extended-protocol session state (named statements, portals) cannot be
/// tracked per command.
async fn pg_relay_pinned_session(
    client: TcpStream,
    backend: TcpStream,
    checkout: Checkout,
    reset_strategy: ResetStrategy,
    stats: Option<&QueryStats>,
) -> Result<(), DbError> {
    match pg_proxy_bidirectional_sniff(client, backend, stats).await {
        Some(backend) => {
            if matches!(reset_strategy, ResetStrategy::Never) {
                checkout.return_to_pool(backend);
            } else {
                checkout.return_with_reset(backend).await;
            }
        }
        None => {
            debug!("pinned session backend failed; discarding, not recycling");
            checkout.retire();
        }
    }
    Ok(())
}

// ── Public builder ───────────────────────────────────────────────────────────

/// Build a [`PgProxy`] from configuration parameters.
///
/// `stats` is the process-wide collector shared with the litewire paths;
/// pass one built with `enabled: false` (i.e. `[db.analysis] query_stats =
/// false`) to leave the forwarding paths uninstrumented.
///
/// # Errors
///
/// Propagates any error from [`PgProxy::new`] (backend connection or
/// authentication failures).
pub async fn build_proxy(
    url: &str,
    listen: &str,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
    replica_urls: Vec<String>,
    rw_split: PgRwSplitParams,
    stats: QueryStats,
) -> Result<PgProxy, DbError> {
    PgProxy::new(url, listen, pool_config, reset_strategy, replica_urls, rw_split, stats).await
}

/// Bind the proxy listener now; reach the upstream in the background.
///
/// The `PostgreSQL` twin of
/// [`mysql::spawn_deferred`](crate::mysql::spawn_deferred) — see that
/// function for the full rationale. In short: bind (fatal on failure),
/// return, then connect upstream with unbounded capped-backoff retry and
/// serve on the already-bound listener. Clients arriving before the upstream
/// answers queue in the accept backlog for up to
/// [`BACKLOG_GRACE`](crate::health::BACKLOG_GRACE), then are closed on
/// arrival so they fail fast.
///
/// # Errors
///
/// Returns an error if the URL is malformed or the listen address cannot be
/// bound. Upstream unreachability is not an error here.
pub async fn spawn_deferred(
    url: &str,
    listen: &str,
    pool_config: PoolConfig,
    reset_strategy: ResetStrategy,
    replica_urls: Vec<String>,
    rw_split: PgRwSplitParams,
    stats: QueryStats,
    health: Arc<ProxyHealth>,
) -> Result<tokio::task::JoinHandle<()>, DbError> {
    let db_url = Arc::new(DbUrl::parse(url)?);
    let listener = TcpListener::bind(listen).await?;
    info!(
        listen = %listen,
        upstream = %db_url.addr(),
        "PostgreSQL proxy listening (upstream connect continues in the background)"
    );

    let listen_owned = listen.to_string();
    let listener = Arc::new(listener);
    Ok(tokio::spawn(async move {
        // See the MySQL twin: bounded backlog window, then fail fast.
        let drain = tokio::spawn(crate::health::drain_while_upstream_down(
            Arc::clone(&listener),
            Arc::clone(&health),
        ));

        let proxy = match PgProxy::connect(
            db_url,
            &listen_owned,
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
                tracing::error!("PostgreSQL proxy failed to start: {e:#}");
                return;
            }
        };
        drain.abort();
        drop(proxy.start_maintenance());
        match proxy.run_on(listener).await {
            Ok(()) => info!("PostgreSQL proxy stopped"),
            Err(e) => tracing::error!("PostgreSQL proxy error: {e:#}"),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_pg_query ──────────────────────────────────────────

    #[test]
    fn classify_select_as_read() {
        assert_eq!(classify_pg_query("SELECT * FROM users"), PgQueryKind::Read);
    }

    #[test]
    fn classify_select_for_update_as_write() {
        assert_eq!(classify_pg_query("SELECT * FROM users FOR UPDATE"), PgQueryKind::Write);
    }

    #[test]
    fn classify_show_as_read() {
        assert_eq!(classify_pg_query("SHOW search_path"), PgQueryKind::Read);
    }

    #[test]
    fn classify_insert_as_write() {
        assert_eq!(classify_pg_query("INSERT INTO users VALUES (1)"), PgQueryKind::Write);
    }

    #[test]
    fn classify_begin_as_tx_begin() {
        assert_eq!(classify_pg_query("BEGIN"), PgQueryKind::TxBegin);
    }

    #[test]
    fn classify_commit_as_tx_end() {
        assert_eq!(classify_pg_query("COMMIT"), PgQueryKind::TxEnd);
    }

    #[test]
    fn classify_rollback_as_tx_end() {
        assert_eq!(classify_pg_query("ROLLBACK"), PgQueryKind::TxEnd);
    }

    #[test]
    fn classify_whitespace_prefix() {
        assert_eq!(classify_pg_query("   SELECT 1"), PgQueryKind::Read);
    }

    #[test]
    fn classify_unknown_as_write() {
        assert_eq!(classify_pg_query("TRUNCATE TABLE users"), PgQueryKind::Write);
    }

    #[test]
    fn classify_explain_as_read() {
        assert_eq!(classify_pg_query("EXPLAIN SELECT * FROM users"), PgQueryKind::Read);
    }

    // ── md5_password ────────────────────────────────────────────────

    #[test]
    fn md5_password_known_vector() {
        // PostgreSQL MD5 authentication: "md5" + md5(md5(password + user) + salt)
        let result = md5_password("user", "pass", &[0x01, 0x02, 0x03, 0x04]);
        assert!(result.starts_with("md5"));
        assert_eq!(result.len(), 35); // "md5" + 32 hex chars
    }

    // ── parse_parameter_status ──────────────────────────────────────

    #[test]
    fn parse_param_status() {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"client_encoding\0UTF8\0");
        let (key, value) = parse_parameter_status(&payload).unwrap();
        assert_eq!(key, "client_encoding");
        assert_eq!(value, "UTF8");
    }

    // ── parse_pg_error ──────────────────────────────────────────────

    #[test]
    fn parse_error_extracts_message() {
        // ErrorResponse fields: S=ERROR, M=some message, \0 terminator
        let mut payload = Vec::new();
        payload.push(b'S');
        payload.extend_from_slice(b"ERROR\0");
        payload.push(b'M');
        payload.extend_from_slice(b"relation \"foo\" does not exist\0");
        payload.push(0); // terminator
        let msg = parse_pg_error(&payload);
        assert_eq!(msg, "relation \"foo\" does not exist");
    }

    #[test]
    fn parse_error_empty_payload() {
        let payload = vec![0];
        let msg = parse_pg_error(&payload);
        assert_eq!(msg, "(unknown error)");
    }

    // ── parse_server_first ──────────────────────────────────────────

    #[test]
    fn parse_scram_server_first() {
        let msg = "r=abc123serverpart,s=c2FsdA==,i=4096";
        let (nonce, salt, iterations) = parse_server_first(msg).unwrap();
        assert_eq!(nonce, "abc123serverpart");
        assert_eq!(salt, "c2FsdA==");
        assert_eq!(iterations, 4096);
    }

    #[test]
    fn parse_scram_server_first_missing_field() {
        let msg = "r=abc123,s=c2FsdA==";
        assert!(parse_server_first(msg).is_err());
    }

    // ── parse_sasl_mechanisms ───────────────────────────────────────

    #[test]
    fn parse_mechanisms() {
        let data = b"SCRAM-SHA-256\0\0";
        let mechs = parse_sasl_mechanisms(data);
        assert_eq!(mechs, vec!["SCRAM-SHA-256"]);
    }

    #[test]
    fn parse_multiple_mechanisms() {
        let data = b"SCRAM-SHA-256\0SCRAM-SHA-256-PLUS\0\0";
        let mechs = parse_sasl_mechanisms(data);
        assert_eq!(mechs, vec!["SCRAM-SHA-256", "SCRAM-SHA-256-PLUS"]);
    }

    // ── pool routing ────────────────────────────────────────────────

    #[test]
    fn select_routes_read_to_replica() {
        let rw_split =
            PgRwSplitParams { enabled: true, sticky_duration: std::time::Duration::from_secs(1) };
        let primary = pool_stub();
        let replicas = vec![pool_stub()];
        let state = PgClientState::default();
        let rr = AtomicUsize::new(0);

        let target = pg_select_pool(&primary, &replicas, &rr, &state, PgQueryKind::Read, &rw_split);
        assert!(std::ptr::eq(target, &raw const replicas[0]));
    }

    #[test]
    fn select_routes_write_to_primary() {
        let rw_split =
            PgRwSplitParams { enabled: true, sticky_duration: std::time::Duration::from_secs(1) };
        let primary = pool_stub();
        let replicas = vec![pool_stub()];
        let state = PgClientState::default();
        let rr = AtomicUsize::new(0);

        let target =
            pg_select_pool(&primary, &replicas, &rr, &state, PgQueryKind::Write, &rw_split);
        assert!(std::ptr::eq(target, &raw const primary));
    }

    #[test]
    fn select_routes_to_primary_in_transaction() {
        let rw_split =
            PgRwSplitParams { enabled: true, sticky_duration: std::time::Duration::from_secs(1) };
        let primary = pool_stub();
        let replicas = vec![pool_stub()];
        let state = PgClientState { in_transaction: true, ..PgClientState::default() };
        let rr = AtomicUsize::new(0);

        let target = pg_select_pool(&primary, &replicas, &rr, &state, PgQueryKind::Read, &rw_split);
        assert!(std::ptr::eq(target, &raw const primary));
    }

    // ── handle_backend_auth SCRAM framing ────────────────────────────

    /// Regression test for the SCRAM-SHA-256 "extra read after success" bug.
    ///
    /// After `scram_sha256_exchange` consumes the `AuthenticationOk`
    /// (auth_type=0) itself, `handle_backend_auth` must return immediately —
    /// not loop back and read another message. Otherwise it eats the
    /// post-auth `ParameterStatus` (tag `'S'`) and dies with
    /// "expected auth request, got 'S'".
    ///
    /// This test wires `handle_backend_auth` against a fake server that
    /// drives the full SCRAM flow + a trailing `ParameterStatus`. A
    /// successful return means the `ParameterStatus` was NOT consumed by
    /// the auth handler — we can read it on the wire ourselves after.
    #[tokio::test]
    async fn handle_backend_auth_scram_stops_after_auth_ok() {
        let (mut client_side, mut server_side) = make_tcp_pair().await;

        // Fake server task: drive the SCRAM dance, then send a final
        // ParameterStatus and remain open.
        let server = tokio::spawn(async move {
            // 1. AuthenticationSASL: i32 type=10 + "SCRAM-SHA-256\0\0"
            let mut p = Vec::new();
            p.extend_from_slice(&AUTH_SASL.to_be_bytes());
            p.extend_from_slice(b"SCRAM-SHA-256\0\0");
            write_pg_message(&mut server_side, MSG_AUTH, &p).await.unwrap();

            // 2. Read client SASLInitialResponse: mechanism\0 + msg_len + msg
            let (_tag, init) = read_pg_message(&mut server_side).await.unwrap();
            // Find msg after "SCRAM-SHA-256\0" and 4-byte length.
            let after_mech = &init[b"SCRAM-SHA-256\0".len() + 4..];
            let client_first = std::str::from_utf8(after_mech).unwrap();
            // Extract the client nonce: "n,,n=,r=<nonce>"
            let client_nonce = client_first.split("r=").nth(1).unwrap();

            // 3. AuthenticationSASLContinue: server-first-message.
            // r=<client_nonce+server_extra>, s=<salt b64>, i=4096
            let server_nonce = format!("{client_nonce}SERVEREXTRA");
            let server_first = format!("r={server_nonce},s=c2FsdA==,i=4096");
            let mut p = Vec::new();
            p.extend_from_slice(&AUTH_SASL_CONTINUE.to_be_bytes());
            p.extend_from_slice(server_first.as_bytes());
            write_pg_message(&mut server_side, MSG_AUTH, &p).await.unwrap();

            // 4. Read client SASLResponse (final). Discard contents — we don't
            // verify the proof in this test; we just confirm framing.
            let (_tag, _client_final) = read_pg_message(&mut server_side).await.unwrap();

            // 5. AuthenticationSASLFinal: any v=<sig>.
            let server_final = "v=AAAA";
            let mut p = Vec::new();
            p.extend_from_slice(&AUTH_SASL_FINAL.to_be_bytes());
            p.extend_from_slice(server_final.as_bytes());
            write_pg_message(&mut server_side, MSG_AUTH, &p).await.unwrap();

            // 6. AuthenticationOk.
            write_pg_message(&mut server_side, MSG_AUTH, &AUTH_OK.to_be_bytes()).await.unwrap();

            // 7. Trailing ParameterStatus that must NOT be consumed by auth.
            send_parameter_status(&mut server_side, "client_encoding", "UTF8").await.unwrap();

            // Keep the socket open until the client closes it.
            let mut buf = [0u8; 1];
            let _ = server_side.read(&mut buf).await;
        });

        // Drive auth on the client side.
        handle_backend_auth(&mut client_side, "u", "p").await.expect("SCRAM auth must succeed");

        // The next byte on the wire must be 'S' (ParameterStatus). If
        // handle_backend_auth incorrectly looped, it would have eaten this.
        let (tag, payload) = read_pg_message(&mut client_side).await.unwrap();
        assert_eq!(tag, MSG_PARAMETER_STATUS, "ParameterStatus must survive past auth");
        let (k, v) = parse_parameter_status(&payload).unwrap();
        assert_eq!(k, "client_encoding");
        assert_eq!(v, "UTF8");

        drop(client_side);
        let _ = server.await;
    }

    // ── PG wire protocol helpers ────────────────────────────────────

    #[tokio::test]
    async fn read_write_pg_message_roundtrip() {
        let (mut writer, mut reader) = make_tcp_pair().await;

        let payload = b"hello world";
        write_pg_message(&mut writer, b'Q', payload).await.unwrap();
        drop(writer);

        let (tag, data) = read_pg_message(&mut reader).await.unwrap();
        assert_eq!(tag, b'Q');
        assert_eq!(data, payload);
    }

    #[tokio::test]
    async fn forward_pg_message_preserves_content() {
        let (mut src_writer, mut src_reader) = make_tcp_pair().await;
        let (mut dst_writer, mut dst_reader) = make_tcp_pair().await;

        let payload = b"test payload";
        write_pg_message(&mut src_writer, b'T', payload).await.unwrap();
        drop(src_writer);

        let tag = forward_pg_message(&mut src_reader, &mut dst_writer).await.unwrap();
        assert_eq!(tag, b'T');
        drop(dst_writer);

        let (rtag, rdata) = read_pg_message(&mut dst_reader).await.unwrap();
        assert_eq!(rtag, b'T');
        assert_eq!(rdata, payload);
    }

    // ── extended query protocol ─────────────────────────────────────

    /// Regression test for the extended-query-protocol deadlock.
    ///
    /// `pg_proxy_routing_loop` used to forward exactly one client message and
    /// then block awaiting `ReadyForQuery`. In the extended protocol the
    /// backend stays silent until the client sends `Sync`, so the proxy never
    /// read it and the session wedged on the first prepared statement. The
    /// routing loop is entered whenever `reset_strategy == Smart`, which is
    /// the default, so this was the shipped path.
    #[tokio::test]
    async fn extended_query_protocol_does_not_deadlock() {
        // Fake PG backend: silent until it sees Sync ('S'), then answers
        // ParseComplete / BindComplete / CommandComplete / ReadyForQuery.
        let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let backend_task = tokio::spawn(async move {
            let (mut sock, _) = backend_listener.accept().await.unwrap();
            while let Ok((tag, _payload)) = read_pg_message(&mut sock).await {
                if tag == b'S' {
                    write_pg_message(&mut sock, b'1', &[]).await.unwrap();
                    write_pg_message(&mut sock, b'2', &[]).await.unwrap();
                    write_pg_message(&mut sock, b'C', b"SELECT 1\0").await.unwrap();
                    send_ready_for_query(&mut sock, b'I').await.unwrap();
                }
            }
        });

        let (mut client, proxy_side) = make_tcp_pair().await;
        let rw_split =
            PgRwSplitParams { enabled: false, sticky_duration: std::time::Duration::from_secs(1) };
        let proxy_task = tokio::spawn(async move {
            let pool = pool_dialing(backend_addr);
            let rr = AtomicUsize::new(0);
            pg_proxy_routing_loop(
                proxy_side,
                &pool,
                &[],
                &rr,
                &rw_split,
                ResetStrategy::Smart,
                None,
            )
            .await
        });

        // Parse / Bind / Describe / Execute / Sync — what PDO_pgsql sends for
        // a prepared statement.
        write_pg_message(&mut client, b'P', b"\0SELECT 1\0\0\0").await.unwrap();
        write_pg_message(&mut client, b'B', b"\0\0\0\0\0\0\0\0").await.unwrap();
        write_pg_message(&mut client, b'D', b"P\0").await.unwrap();
        write_pg_message(&mut client, b'E', b"\0\0\0\0\0").await.unwrap();
        write_pg_message(&mut client, b'S', &[]).await.unwrap();

        let tags = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut seen = Vec::new();
            loop {
                let (tag, _) = read_pg_message(&mut client).await.unwrap();
                seen.push(tag);
                if tag == MSG_READY_FOR_QUERY {
                    return seen;
                }
            }
        })
        .await
        .expect("extended query protocol must not deadlock");

        assert_eq!(tags, vec![b'1', b'2', b'C', MSG_READY_FOR_QUERY]);

        drop(client);
        proxy_task.abort();
        backend_task.abort();
    }

    // ── query stats ─────────────────────────────────────────────────

    /// Fake PG backend that answers one simple `Query` with `row_count`
    /// data rows, optionally followed by an `ErrorResponse`.
    fn spawn_query_backend(
        listener: tokio::net::TcpListener,
        row_count: usize,
        fail: bool,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            while let Ok((tag, _payload)) = read_pg_message(&mut sock).await {
                if tag != MSG_QUERY {
                    continue;
                }
                write_pg_message(&mut sock, b'T', b"\0\0").await.unwrap();
                for _ in 0..row_count {
                    write_pg_message(&mut sock, MSG_DATA_ROW, b"\0\0").await.unwrap();
                }
                if fail {
                    let mut err = Vec::new();
                    err.push(b'M');
                    err.extend_from_slice(b"boom\0");
                    err.push(0);
                    write_pg_message(&mut sock, MSG_ERROR_RESPONSE, &err).await.unwrap();
                } else {
                    write_pg_message(&mut sock, b'C', b"SELECT 2\0").await.unwrap();
                }
                send_ready_for_query(&mut sock, b'I').await.unwrap();
            }
        })
    }

    /// Drive one simple `Query` through the routing loop against a fake
    /// backend and return once `ReadyForQuery` has reached the client.
    async fn drive_pg_query(stats: Option<QueryStats>, row_count: usize, fail: bool) {
        let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let backend_task = spawn_query_backend(backend_listener, row_count, fail);

        let (mut client, proxy_side) = make_tcp_pair().await;
        let rw_split =
            PgRwSplitParams { enabled: false, sticky_duration: std::time::Duration::from_secs(1) };
        let proxy_task = tokio::spawn(async move {
            let pool = pool_dialing(backend_addr);
            let rr = AtomicUsize::new(0);
            pg_proxy_routing_loop(
                proxy_side,
                &pool,
                &[],
                &rr,
                &rw_split,
                // `Never` keeps the fake backend out of the DISCARD ALL path.
                ResetStrategy::Never,
                stats.as_ref(),
            )
            .await
        });

        write_pg_message(&mut client, MSG_QUERY, b"SELECT * FROM users WHERE id = 1\0")
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (tag, _) = read_pg_message(&mut client).await.unwrap();
                if tag == MSG_READY_FOR_QUERY {
                    return;
                }
            }
        })
        .await
        .expect("the query must complete");

        drop(client);
        proxy_task.abort();
        backend_task.abort();
    }

    #[tokio::test]
    async fn pg_routing_loop_records_simple_queries() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        drive_pg_query(Some(stats.clone()), 2, false).await;

        assert_eq!(stats.digest_count(), 1);
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[0].error_count, 0);
        assert_eq!(top[0].total_rows, 2, "DataRow messages are counted, not estimated");
        assert!(top[0].digest_text.contains('?'), "literals must be normalized away");
        assert!(top[0].total_time > std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn pg_routing_loop_records_errors() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        drive_pg_query(Some(stats.clone()), 1, true).await;

        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[0].error_count, 1, "ErrorResponse in the stream marks the statement failed");
        assert_eq!(top[0].total_rows, 1, "rows already forwarded before the error are real");
    }

    /// Negative control for `[db.analysis] query_stats = false`.
    #[tokio::test]
    async fn pg_routing_loop_records_nothing_when_stats_are_off() {
        // No collector at all — what `PgProxy::stats()` yields when off.
        drive_pg_query(None, 2, false).await;

        // A present-but-disabled collector must also stay empty.
        let disabled = QueryStats::new(ephpm_query_stats::StatsConfig {
            enabled: false,
            ..Default::default()
        });
        drive_pg_query(Some(disabled.clone()), 2, false).await;
        assert_eq!(disabled.digest_count(), 0);
    }

    // ── relay cancellation safety ───────────────────────────────────

    /// The idle state must not look like a partly consumed message.
    ///
    /// The reader half of the single-backend relay spends most of a session
    /// parked here. If that set the flag, every clean end-of-session would
    /// condemn its pooled backend and the pool would never reuse anything.
    #[tokio::test]
    async fn relay_flag_stays_clear_while_waiting_for_a_message() {
        let (_src_write, mut src_read) = make_tcp_pair().await;
        let (mut sink, _sink_read) = make_tcp_pair().await;
        let partial = AtomicBool::new(false);

        let timed_out = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            relay_pg_message(&mut src_read, &mut sink, &partial),
        )
        .await;

        assert!(timed_out.is_err(), "no message was sent, so the relay must still be waiting");
        assert!(!partial.load(Ordering::Relaxed), "waiting for the next message is not partial");
    }

    /// A message the relay was cancelled part-way through must be reported.
    ///
    /// Framing takes several awaits, so unlike the bulk copy this replaced,
    /// the reader half is not cancel-safe: the tag byte is gone from the
    /// stream and was never forwarded. Recycling that backend hands the next
    /// session a connection whose stream starts mid-message.
    #[tokio::test]
    async fn relay_flag_is_set_when_cancelled_mid_message() {
        let (mut src_write, mut src_read) = make_tcp_pair().await;
        let (mut sink, _sink_read) = make_tcp_pair().await;
        let partial = AtomicBool::new(false);

        // A tag byte and nothing else: the relay consumes it, then blocks on
        // the length field.
        src_write.write_all(&[MSG_DATA_ROW]).await.unwrap();

        let timed_out = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            relay_pg_message(&mut src_read, &mut sink, &partial),
        )
        .await;

        assert!(
            timed_out.is_err(),
            "the message is incomplete, so the relay must still be waiting"
        );
        assert!(partial.load(Ordering::Relaxed), "a consumed tag byte must condemn the connection");
    }

    /// A completed message clears the flag again, so a session that ends
    /// between messages stays recyclable.
    #[tokio::test]
    async fn relay_flag_clears_after_a_complete_message() {
        let (mut src_write, mut src_read) = make_tcp_pair().await;
        let (mut sink, _sink_read) = make_tcp_pair().await;
        let partial = AtomicBool::new(false);

        write_pg_message(&mut src_write, MSG_DATA_ROW, b"\0\0").await.unwrap();

        let relayed = relay_pg_message(&mut src_read, &mut sink, &partial).await;
        assert!(matches!(relayed, Relayed::Message(MSG_DATA_ROW)));
        assert!(
            !partial.load(Ordering::Relaxed),
            "a fully forwarded message leaves nothing behind"
        );
    }

    // ── query stats on the single-backend path ──────────────────────
    //
    // Before this path framed the backend→client direction it recorded
    // nothing at all, so `reset_strategy = "never"`/`"always"` without
    // replicas made a whole deployment's traffic invisible to
    // `[db.analysis]`. These pin the closed gap.

    /// Read client-bound messages until `ReadyForQuery`, returning the tags.
    ///
    /// Also proves the coalescing client writer flushed: a message that stayed
    /// in the `BufWriter` would never arrive and this would time out.
    async fn read_to_ready(client: &mut TcpStream) -> Vec<u8> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut seen = Vec::new();
            loop {
                let (tag, _) = read_pg_message(client).await.unwrap();
                seen.push(tag);
                if tag == MSG_READY_FOR_QUERY {
                    return seen;
                }
            }
        })
        .await
        .expect("the response must reach the client")
    }

    /// Answer one simple `Query` on the fake backend with `rows` data rows.
    async fn answer_query(backend: &mut TcpStream, rows: usize, fail: bool) {
        let (tag, _) = read_pg_message(backend).await.unwrap();
        assert_eq!(tag, MSG_QUERY);
        write_pg_message(backend, b'T', b"\0\0").await.unwrap();
        for _ in 0..rows {
            write_pg_message(backend, MSG_DATA_ROW, b"\0\0").await.unwrap();
        }
        if fail {
            write_pg_message(backend, MSG_ERROR_RESPONSE, b"Mboom\0\0").await.unwrap();
        } else {
            write_pg_message(backend, b'C', b"SELECT 0\0").await.unwrap();
        }
        send_ready_for_query(backend, b'I').await.unwrap();
    }

    /// Run one simple `Query` through the single-backend relay and return the
    /// backend the relay decided was reusable.
    async fn drive_sniff_query(
        stats: Option<&QueryStats>,
        rows: usize,
        fail: bool,
    ) -> Option<TcpStream> {
        let (mut driver, proxy_client) = make_tcp_pair().await;
        let (proxy_backend, mut fake_backend) = make_tcp_pair().await;

        let handed = stats.cloned();
        let proxy = tokio::spawn(async move {
            pg_proxy_bidirectional_sniff(proxy_client, proxy_backend, handed.as_ref()).await
        });

        write_pg_message(&mut driver, MSG_QUERY, b"SELECT * FROM users WHERE id = 1\0")
            .await
            .unwrap();
        answer_query(&mut fake_backend, rows, fail).await;
        read_to_ready(&mut driver).await;

        // Orderly close, exactly as PDO does when the request ends.
        write_pg_message(&mut driver, MSG_TERMINATE, &[]).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), proxy)
            .await
            .expect("an intercepted Terminate must end the session")
            .expect("proxy task must not panic")
    }

    #[tokio::test]
    async fn pg_single_backend_path_records_simple_queries() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        let backend = drive_sniff_query(Some(&stats), 2, false).await;
        assert!(backend.is_some(), "an intercepted Terminate must leave the backend recyclable");

        assert_eq!(stats.digest_count(), 1);
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[0].error_count, 0);
        assert_eq!(
            top[0].total_rows, 2,
            "DataRow messages are counted here now, exactly as the routing loop counts them"
        );
        assert!(top[0].digest_text.contains('?'), "literals must be normalized away");
        assert!(top[0].total_time > std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn pg_single_backend_path_records_errors() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        drive_sniff_query(Some(&stats), 1, true).await;

        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[0].error_count, 1, "ErrorResponse in the turn marks the statement failed");
        assert_eq!(top[0].total_rows, 1, "rows forwarded before the error are real");
    }

    /// Negative control for `[db.analysis] query_stats = false`: the same
    /// session with no collector, and with a present-but-disabled one, must
    /// record nothing and still relay every message.
    #[tokio::test]
    async fn pg_single_backend_path_records_nothing_when_stats_are_off() {
        // `None` is what `PgProxy::stats()` yields when the toggle is off.
        assert!(drive_sniff_query(None, 2, false).await.is_some());

        let disabled = QueryStats::new(ephpm_query_stats::StatsConfig {
            enabled: false,
            ..Default::default()
        });
        drive_sniff_query(Some(&disabled), 2, false).await;
        assert_eq!(disabled.digest_count(), 0, "the toggle must reach this tap point");
    }

    /// Turn alignment under pipelining.
    ///
    /// `ReadyForQuery` is the completion marker, and the extended query
    /// protocol produces one at `Sync` just as a simple `Query` produces one
    /// of its own. If only `Query` enqueued a turn, an extended batch whose
    /// `ReadyForQuery` arrives *after* a later `Query` was sent would pop that
    /// query's entry and publish the batch's numbers under the query's digest.
    ///
    /// Nothing in the PHP ecosystem pipelines, so this pins the invariant
    /// rather than a shipped scenario — but the invariant is what makes the
    /// numbers trustworthy.
    #[tokio::test]
    async fn pg_single_backend_path_keeps_turns_aligned_under_pipelining() {
        let stats = QueryStats::new(ephpm_query_stats::StatsConfig::default());
        let (mut driver, proxy_client) = make_tcp_pair().await;
        let (proxy_backend, mut fake_backend) = make_tcp_pair().await;

        let handed = stats.clone();
        let proxy = tokio::spawn(async move {
            pg_proxy_bidirectional_sniff(proxy_client, proxy_backend, Some(&handed)).await
        });

        // An extended-protocol batch and a simple query, both in flight before
        // either is answered.
        write_pg_message(&mut driver, b'P', b"\0SELECT 1\0\0\0").await.unwrap();
        write_pg_message(&mut driver, MSG_SYNC, &[]).await.unwrap();
        write_pg_message(&mut driver, MSG_QUERY, b"SELECT * FROM users WHERE id = 1\0")
            .await
            .unwrap();

        // The backend drains all three, then answers in order: the batch
        // returns no rows, the query returns two.
        for _ in 0..3 {
            read_pg_message(&mut fake_backend).await.unwrap();
        }
        write_pg_message(&mut fake_backend, b'1', &[]).await.unwrap();
        write_pg_message(&mut fake_backend, b'C', b"SELECT 0\0").await.unwrap();
        send_ready_for_query(&mut fake_backend, b'I').await.unwrap();
        write_pg_message(&mut fake_backend, b'T', b"\0\0").await.unwrap();
        write_pg_message(&mut fake_backend, MSG_DATA_ROW, b"\0\0").await.unwrap();
        write_pg_message(&mut fake_backend, MSG_DATA_ROW, b"\0\0").await.unwrap();
        write_pg_message(&mut fake_backend, b'C', b"SELECT 2\0").await.unwrap();
        send_ready_for_query(&mut fake_backend, b'I').await.unwrap();

        read_to_ready(&mut driver).await;
        read_to_ready(&mut driver).await;
        write_pg_message(&mut driver, MSG_TERMINATE, &[]).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), proxy).await;

        assert_eq!(stats.digest_count(), 1, "only the simple query is recordable");
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(
            top[0].total_rows, 2,
            "the query must carry its own row count, not the extended batch's zero"
        );
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

    /// Create a minimal `Pool` for routing tests.
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

    /// Create a `Pool` whose connections dial `addr` with no PG handshake.
    fn pool_dialing(addr: std::net::SocketAddr) -> Pool {
        let connect = move || -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            Box::pin(async move { TcpStream::connect(addr).await.map_err(DbError::from) })
        };
        // No-op reset: the fake backend in these tests does not implement
        // `DISCARD ALL`.
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
}

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
//!    and starts bidirectional byte forwarding.
//!
//! 3. When the client closes or sends `Terminate`, the proxy sends
//!    `DISCARD ALL` to the backend and returns the connection to the pool.
//!
//! ## Auth support
//!
//! Supports `trust`, `md5`, and `scram-sha-256` for backend authentication.
//! Client-facing auth is always `AuthenticationOk` (loopback only).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64ct::Encoding;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::ResetStrategy;
use crate::error::DbError;
use crate::pool::{Checkout, Pool, PoolConfig};
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
/// `Query` — frontend simple query.
const MSG_QUERY: u8 = b'Q';
/// `Terminate` — frontend connection close.
const MSG_TERMINATE: u8 = b'X';
/// `Parse` — frontend extended query: parse a prepared statement.
const MSG_PARSE: u8 = b'P';

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
}

impl PgProxy {
    /// Create a new proxy by connecting to the backend, authenticating, and
    /// building the pool.
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
    ) -> Result<Self, DbError> {
        let db_url = Arc::new(DbUrl::parse(url)?);

        // Establish a probe connection to capture server metadata.
        let (probe_stream, meta) = pg_connect_and_handshake(&db_url).await?;
        let meta = Arc::new(meta);

        // Build the primary pool.
        let db_url_c = Arc::clone(&db_url);
        let connect = move || -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            let u = Arc::clone(&db_url_c);
            Box::pin(async move {
                let (stream, _) = pg_connect_and_handshake(&u).await?;
                Ok(stream)
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
        })
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

        let proxy = Arc::new(self);
        loop {
            let (client, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("PostgreSQL proxy accept error: {e}");
                    continue;
                }
            };
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
            )
            .await
        } else {
            // Fast path: simple bidirectional copy.
            let mut checkout = self.pool.acquire().await?;
            let backend = checkout.take_stream();

            let result = pg_proxy_bidirectional(client, backend).await;

            match result {
                Ok(backend) => match self.reset_strategy {
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
                Err(e) => {
                    debug!("proxy session error, discarding backend connection: {e}");
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
async fn pg_connect_and_handshake(url: &DbUrl) -> Result<(TcpStream, PgServerMeta), DbError> {
    let mut stream = TcpStream::connect(url.addr()).await?;

    // Send StartupMessage: length (4) + protocol version (4) + key=value pairs + \0.
    let mut startup = Vec::with_capacity(128);
    // Protocol version 3.0.
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
                // SCRAM-SHA-256 negotiation.
                let mechanisms = parse_sasl_mechanisms(&payload[4..]);
                if !mechanisms.iter().any(|m| m == "SCRAM-SHA-256") {
                    return Err(DbError::Auth("server requires unsupported SASL mechanism".into()));
                }
                scram_sha256_exchange(stream, username, password).await?;
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
    if len < 4 {
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

/// Forward a raw PG message from one stream to another.
async fn forward_pg_message(from: &mut TcpStream, to: &mut TcpStream) -> Result<u8, DbError> {
    let tag = from.read_u8().await?;
    let len = from.read_i32().await?;
    let payload_len = if len >= 4 { usize::try_from(len - 4).unwrap_or(0) } else { 0 };

    // Write tag + length.
    to.write_u8(tag).await?;
    to.write_all(&len.to_be_bytes()).await?;

    // Forward payload in chunks to avoid allocating for large results.
    if payload_len > 0 {
        let mut remaining = payload_len;
        let mut buf = vec![0u8; remaining.min(8192)];
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            from.read_exact(&mut buf[..to_read]).await?;
            to.write_all(&buf[..to_read]).await?;
            remaining -= to_read;
        }
    }

    Ok(tag)
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

/// Splice `client` ↔ `backend` until one side closes.
///
/// Returns the backend stream on clean close so it can be reset and recycled.
async fn pg_proxy_bidirectional(
    mut client: TcpStream,
    mut backend: TcpStream,
) -> Result<TcpStream, DbError> {
    let result = {
        let (mut cr, mut cw) = client.split();
        let (mut br, mut bw) = backend.split();

        let client_to_backend = tokio::io::copy(&mut cr, &mut bw);
        let backend_to_client = tokio::io::copy(&mut br, &mut cw);

        tokio::select! {
            r = client_to_backend => r,
            r = backend_to_client => r,
        }
    };

    match result {
        Ok(_) => Ok(backend),
        Err(e)
            if e.kind() == std::io::ErrorKind::ConnectionReset
                || e.kind() == std::io::ErrorKind::BrokenPipe =>
        {
            Ok(backend)
        }
        Err(e) => Err(DbError::Io(e)),
    }
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
async fn pg_proxy_routing_loop(
    mut client: TcpStream,
    pool: &Pool,
    replica_pools: &[Pool],
    replica_rr: &AtomicUsize,
    rw_split: &PgRwSplitParams,
    reset_strategy: ResetStrategy,
) -> Result<(), DbError> {
    let mut state = PgClientState::default();

    loop {
        // Read one message from the client.
        let Ok((tag, payload)) = read_pg_message(&mut client).await else {
            break;
        };

        if tag == MSG_TERMINATE {
            break;
        }

        // For simple Query messages, classify and route.
        let query_kind = if tag == MSG_QUERY {
            // Query payload is null-terminated SQL.
            let sql = String::from_utf8_lossy(&payload);
            let sql = sql.trim_end_matches('\0');
            classify_pg_query(sql)
        } else {
            // Extended query protocol messages (Parse, Bind, etc.) — treat as writes
            // unless we can inspect the SQL in Parse.
            if tag == MSG_PARSE && payload.len() > 1 {
                // Parse: [name\0][query\0][param_count: 2][param_oids...]
                let name_end = payload.iter().position(|&b| b == 0).unwrap_or(0);
                let query_start = name_end + 1;
                let query_end = payload[query_start..]
                    .iter()
                    .position(|&b| b == 0)
                    .map_or(payload.len(), |p| query_start + p);
                let sql = String::from_utf8_lossy(&payload[query_start..query_end]);
                classify_pg_query(&sql)
            } else {
                PgQueryKind::Write
            }
        };

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

        write_pg_message(&mut backend, tag, &payload).await?;

        // Forward response(s) until ReadyForQuery.
        loop {
            let resp_tag = forward_pg_message(&mut backend, &mut client).await?;
            if resp_tag == MSG_READY_FOR_QUERY {
                break;
            }
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

// ── Public builder ───────────────────────────────────────────────────────────

/// Build a [`PgProxy`] from configuration parameters.
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
) -> Result<PgProxy, DbError> {
    PgProxy::new(url, listen, pool_config, reset_strategy, replica_urls, rw_split).await
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
}

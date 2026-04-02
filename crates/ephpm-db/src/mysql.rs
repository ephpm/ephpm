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
//! 3. When the client closes its connection, the proxy sends
//!    `COM_RESET_CONNECTION` to the backend (resets session variables,
//!    temporary tables, prepared statements, etc.) and returns the
//!    connection to the pool.
//!
//! ## Auth plugin support
//!
//! Currently supports `mysql_native_password` for backend authentication.
//! `MySQL` 8+ users should configure users with:
//! ```sql
//! ALTER USER 'user'@'%' IDENTIFIED WITH mysql_native_password BY 'pass';
//! ```
//! Support for `caching_sha2_password` is planned (TODO).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::ResetStrategy;
use crate::error::DbError;
use crate::pool::{Checkout, Pool, PoolConfig};
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
#[allow(dead_code)]
struct ServerMeta {
    server_version: String,
    capabilities: u32,
    charset: u8,
    /// Auth plugin name advertised to clients (always `mysql_native_password`).
    ///
    /// Captured from the backend handshake for use in synthetic client greetings
    /// (not yet wired up — will be used when we generate per-client handshake packets).
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    socket: Option<std::path::PathBuf>,
    reset_strategy: ResetStrategy,
    rw_split: RwSplitParams,
}

impl MySqlProxy {
    /// Create a new proxy by connecting to the backend, authenticating, and
    /// building the pool.
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
    ) -> Result<Self, DbError> {
        let db_url = Arc::new(DbUrl::parse(url)?);

        // Establish a single connection to capture server metadata.
        let (probe_stream, meta) = connect_and_handshake(&db_url).await?;
        let meta = Arc::new(meta);

        // Build the primary pool using clones of the URL and meta for closures.
        let db_url_c = Arc::clone(&db_url);
        let connect = move || -> crate::pool::BoxFuture<Result<TcpStream, DbError>> {
            let u = Arc::clone(&db_url_c);
            Box::pin(async move {
                let (stream, _) = connect_and_handshake(&u).await?;
                Ok(stream)
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
            socket,
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
        info!(listen = %self.listen, "MySQL proxy listening");

        let proxy = Arc::new(self);
        loop {
            let (client, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("MySQL proxy accept error: {e}");
                    continue;
                }
            };
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

        // Determine if we need query-level routing or just simple proxying.
        let needs_routing = matches!(self.reset_strategy, ResetStrategy::Smart)
            || (self.rw_split.enabled && !self.replica_pools.is_empty());

        if needs_routing {
            // Step 4a: per-query routing with dirty tracking.
            proxy_routing_loop(
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

            let result = proxy_bidirectional(client, backend).await;

            match result {
                Ok(backend) => {
                    match self.reset_strategy {
                        ResetStrategy::Never => {
                            checkout.return_to_pool(backend);
                        }
                        ResetStrategy::Always => match reset_connection(backend).await {
                            Ok(stream) => checkout.return_to_pool(stream),
                            Err(e) => {
                                debug!("MySQL reset failed, discarding connection: {e}");
                                checkout.retire();
                            }
                        },
                        ResetStrategy::Smart => {
                            // Smart path handled by routing loop above.
                            unreachable!()
                        }
                    }
                }
                Err(e) => {
                    debug!("proxy session error, discarding backend connection: {e}");
                    checkout.retire();
                }
            }
            Ok(())
        }
    }
}

// ── Backend connection & auth ─────────────────────────────────────────────────

/// Connect to the `MySQL` backend and complete the authentication handshake.
///
/// Returns the authenticated stream and the server metadata extracted from
/// the initial greeting.
async fn connect_and_handshake(url: &DbUrl) -> Result<(TcpStream, ServerMeta), DbError> {
    let mut stream = TcpStream::connect(url.addr()).await?;

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

    // Read OK / ERR / auth-switch.
    let (_, resp) = read_packet(&mut stream).await?;
    match resp.first() {
        Some(0x00) => { /* OK */ }
        Some(0xFE) => {
            return Err(DbError::Auth(
                "server requested auth plugin switch; only mysql_native_password is supported. \
                 Try: ALTER USER '{}' IDENTIFIED WITH mysql_native_password BY 'pass'"
                    .to_string(),
            ));
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

    if pos + 6 > payload.len() {
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
fn build_handshake_response(
    meta: &ServerMeta,
    username: &str,
    password: &str,
    challenge: &[u8; 20],
    database: Option<&str>,
) -> Vec<u8> {
    let auth_response = mysql_native_password(password, challenge);

    // Request the same capabilities as the server minus SSL.
    let mut caps = meta.capabilities & !0x0000_0800; // remove CLIENT_SSL
    // Always require protocol 4.1 features.
    caps |= CLIENT_LONG_PASSWORD
        | CLIENT_LONG_FLAG
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_MULTI_STATEMENTS
        | CLIENT_MULTI_RESULTS
        | CLIENT_PS_MULTI_RESULTS
        | CLIENT_PLUGIN_AUTH
        | CLIENT_PLUGIN_AUTH_LENENC;
    if database.is_some_and(|d| !d.is_empty()) {
        caps |= CLIENT_CONNECT_WITH_DB;
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

    buf.extend_from_slice(b"mysql_native_password");
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

// ── Client greeting & handshake ───────────────────────────────────────────────

/// Generate a non-cryptographic 20-byte challenge for the fake client greeting.
///
/// Security note: the proxy does not validate client auth responses (local
/// loopback only), so this challenge value has no security significance.
fn fresh_challenge() -> [u8; 20] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut c = [0u8; 20];
    c[..8].copy_from_slice(&ts.as_secs().to_ne_bytes());
    c[8..12].copy_from_slice(&ts.subsec_nanos().to_ne_bytes());
    // Mix in the task ID hash for uniqueness across concurrent connections.
    let ptr = Arc::as_ptr(&Arc::new(())) as usize;
    c[12..20].copy_from_slice(&ptr.to_ne_bytes());
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

/// Splice `client` ↔ `backend` until one side closes.
///
/// Returns the backend stream on clean close so it can be reset and recycled.
/// Returns `Err` if an I/O error occurs (backend is discarded).
async fn proxy_bidirectional(
    mut client: TcpStream,
    mut backend: TcpStream,
) -> Result<TcpStream, DbError> {
    // Scope the split halves so the borrows end before we move `backend`.
    let result = {
        let (mut cr, mut cw) = client.split();
        let (mut br, mut bw) = backend.split();

        let client_to_backend = tokio::io::copy(&mut cr, &mut bw);
        let backend_to_client = tokio::io::copy(&mut br, &mut cw);

        // Run both directions concurrently. When one direction closes, shut down
        // the other. `copy` returns 0 bytes on clean EOF.
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
enum QueryKind {
    /// SELECT, SHOW, EXPLAIN, DESCRIBE — read-only, can go to replica.
    Read,
    /// INSERT, UPDATE, DELETE, CREATE, ALTER, DROP — modifies data, must go to primary.
    Write,
    /// BEGIN, START TRANSACTION — starts a transaction, sticky to primary.
    TxBegin,
    /// COMMIT, ROLLBACK — ends a transaction.
    TxEnd,
}

/// Classify a SQL query based on its first keyword.
fn classify_mysql_query(sql: &str) -> QueryKind {
    let s = sql.trim_start();
    // Find the first token (word).
    let tok = s.split_ascii_whitespace().next().unwrap_or("").to_ascii_uppercase();

    match tok.as_str() {
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" => {
            // Special case: SELECT ... FOR UPDATE or SELECT ... FOR SHARE → Write
            if sql.to_ascii_uppercase().contains("FOR UPDATE")
                || sql.to_ascii_uppercase().contains("FOR SHARE")
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

/// Forward a complete `MySQL` response from backend to client.
///
/// Handles OK, ERR, EOF, and result sets by reading the full response
/// and forwarding each packet.
async fn forward_mysql_response(
    backend: &mut TcpStream,
    client: &mut TcpStream,
) -> Result<(), DbError> {
    let (seq, payload) = read_packet(backend).await?;
    write_packet(client, seq, &payload).await?;

    // Check response type.
    match payload.first().copied() {
        Some(0x00 | 0xFF) => return Ok(()), // OK or ERR packet
        Some(0xFE) if payload.len() < 9 => return Ok(()), // EOF packet
        _ => {}                             // Result set: read columns and rows
    }

    // Result set: read column definitions until EOF, then rows until EOF/OK.
    loop {
        let (seq, payload) = read_packet(backend).await?;
        write_packet(client, seq, &payload).await?;

        match payload.first().copied() {
            Some(0xFE) if payload.len() < 9 => return Ok(()), // EOF after rows
            Some(0xFF) => return Ok(()),                      // ERR
            Some(0x00) if payload.len() <= 9 => return Ok(()), // OK
            _ => {}                                           // More rows
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
fn parse_stmt_id(payload: &[u8]) -> Option<u32> {
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
    stmt_pool_map: &HashMap<u32, PoolTarget>,
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
                .and_then(|id| stmt_pool_map.get(&id).copied())
                .map_or(pool, |pt| resolve_pool_target(pt, pool, replica_pools));
            let kind = if std::ptr::eq(target, pool) { QueryKind::Write } else { QueryKind::Read };
            (target, kind)
        }
        COM_STMT_CLOSE | COM_STMT_RESET => {
            let target = parse_stmt_id(payload)
                .and_then(|id| stmt_pool_map.get(&id).copied())
                .map_or(pool, |pt| resolve_pool_target(pt, pool, replica_pools));
            (target, QueryKind::Read)
        }
        _ => (pool, QueryKind::Write),
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

/// Proxy loop with per-query routing and dirty-bit tracking.
///
/// Handles `COM_QUERY` and the full prepared statement protocol
/// (`COM_STMT_PREPARE`, `COM_STMT_EXECUTE`, `COM_STMT_CLOSE`, etc.) with
/// read/write-aware routing. Statement IDs are tracked per-connection so that
/// execute/close/reset/fetch commands are routed to the same pool that
/// compiled the statement.
async fn proxy_routing_loop(
    mut client: TcpStream,
    pool: &Pool,
    replica_pools: &[Pool],
    replica_rr: &AtomicUsize,
    rw_split: &RwSplitParams,
    reset_strategy: crate::ResetStrategy,
) -> Result<(), DbError> {
    let mut state = ClientState::default();
    // Maps statement IDs to the pool type they were prepared on, so that
    // COM_STMT_EXECUTE and friends route to the correct backend.
    let mut stmt_pool_map: HashMap<u32, PoolTarget> = HashMap::new();

    loop {
        let Ok((seq, payload)) = read_packet(&mut client).await else {
            break;
        };
        if payload.is_empty() {
            continue;
        }

        let cmd = payload[0];
        if cmd == COM_QUIT {
            break;
        }

        let (target_pool, query_kind) =
            route_command(&payload, pool, replica_pools, &state, rw_split, &stmt_pool_map, replica_rr);
        track_dirty(&mut state, &payload, query_kind);

        // Acquire backend and forward the command.
        let mut checkout = target_pool.acquire().await?;
        let mut backend = checkout.take_stream();

        if cmd == COM_STMT_PREPARE {
            write_packet(&mut backend, seq, &payload).await?;
            let pool_target = if std::ptr::eq(target_pool, pool) {
                PoolTarget::Primary
            } else {
                // Find which replica index was selected.
                let idx = replica_pools
                    .iter()
                    .position(|r| std::ptr::eq(target_pool, r))
                    .unwrap_or(0);
                PoolTarget::Replica(idx)
            };
            if let Some(stmt_id) = forward_prepare_response(&mut backend, &mut client).await? {
                stmt_pool_map.insert(stmt_id, pool_target);
                debug!(stmt_id, ?pool_target, "prepared statement registered");
            }
        } else if cmd == COM_STMT_CLOSE {
            write_packet(&mut backend, seq, &payload).await?;
            if let Some(stmt_id) = parse_stmt_id(&payload) {
                if let Some(removed) = stmt_pool_map.remove(&stmt_id) {
                    debug!(stmt_id, ?removed, "prepared statement closed");
                }
            }
        } else {
            write_packet(&mut backend, seq, &payload).await?;
            forward_mysql_response(&mut backend, &mut client).await?;
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
/// Exported so `lib.rs` can construct `MySqlProxy` from config.
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
) -> Result<MySqlProxy, DbError> {
    MySqlProxy::new(url, listen, socket, pool_config, reset_strategy, replica_urls, rw_split).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(std::ptr::eq(target, &replicas[0]));
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
        assert!(std::ptr::eq(target, &primary));
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
        assert!(std::ptr::eq(target, &primary));
    }

    #[test]
    fn stmt_pool_map_tracks_prepare_to_execute() {
        let mut map: HashMap<u32, PoolTarget> = HashMap::new();

        // Simulate: SELECT prepared on replica 0.
        map.insert(1, PoolTarget::Replica(0));
        // Simulate: INSERT prepared on primary.
        map.insert(2, PoolTarget::Primary);

        assert_eq!(map.get(&1).copied(), Some(PoolTarget::Replica(0)));
        assert_eq!(map.get(&2).copied(), Some(PoolTarget::Primary));

        // Close statement 1.
        map.remove(&1);
        assert_eq!(map.get(&1), None);
        // Statement 2 still tracked.
        assert_eq!(map.get(&2).copied(), Some(PoolTarget::Primary));
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
}

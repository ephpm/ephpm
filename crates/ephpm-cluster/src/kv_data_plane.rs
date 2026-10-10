//! TCP data plane for fetching and storing large KV values on remote nodes.
//!
//! When a key's value exceeds the gossip tier threshold, it is stored
//! locally on the owner node. Other nodes fetch or store it on demand
//! via a simple TCP protocol:
//!
//! ## Wire protocol (plaintext)
//!
//! **Request:** `[op: u8][key_len: u32 BE][key: bytes]`
//!
//! - `op = 0x00` (GET): fetch a value.
//! - `op = 0x01` (SET): store a value. Followed by `[value_len: u32 BE][value: bytes]`.
//!
//! **GET response (found):** `[value_len: u32 BE][value: bytes]`
//!
//! **GET response (not found):** `[0xFFFFFFFF: u32 BE]` (sentinel)
//!
//! **SET response:** `[status: u8]` where `0x00` = success, `0x01` = rejected (e.g., OOM).
//!
//! ### Owner-arbitrated conditional writes
//!
//! `SETNX` and compare-and-delete are decided by the key's owner node (see
//! [`ClusteredStore`](crate::ClusteredStore)'s routing), so a non-owner ships
//! them here and takes the owner's answer:
//!
//! - `op = 0x02` (SETNX): followed by `[value_len: u32 BE][value][ttl_ms: u64 BE]`
//!   (`ttl_ms = 0` = no expiry). Response `[status: u8]`: `0x00` inserted,
//!   `0x01` refused for memory, `0x02` key already exists, `0x03` this node
//!   cannot route the key (a per-site key on a non-multi-tenant node).
//! - `op = 0x03` (DEL-IF-EQ): followed by `[token_len: u32 BE][token]`.
//!   Response `[status: u8]`: `0x00` deleted, `0x02` not deleted (absent,
//!   expired, or the token differs), `0x03` cannot route the key.
//!
//! A node predating these ops drops the connection on the unknown op code
//! without answering; the caller treats that, like any transport failure, as
//! "owner unreachable" and writes nothing.
//!
//! The sentinel `u32::MAX` indicates the key was not found. This is
//! unambiguous because real values are bounded by memory limits well
//! below 4 GiB.
//!
//! ## Wire protocol (encrypted)
//!
//! When `[cluster] secret` is set, both sides derive a
//! ChaCha20-Poly1305 key via [`ClusterCipher::for_kv_data_plane`] and
//! each logical message above (the whole request, the whole response)
//! becomes one sealed frame:
//!
//! ```text
//! [frame_len: u32 BE][nonce: 12 bytes][ciphertext + 16-byte tag]
//! ```
//!
//! The plaintext inside a frame is byte-identical to the plaintext
//! protocol message. A peer without the matching secret cannot read
//! values or inject writes: frames that fail authentication cause the
//! connection to be dropped without a response.

use std::net::SocketAddr;
use std::sync::Arc;

use ephpm_kv::store::{SetNxOutcome, Store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::secure_transport::{ClusterCipher, SEAL_OVERHEAD};

/// Sentinel value indicating "key not found" in the wire protocol.
const NOT_FOUND_SENTINEL: u32 = u32::MAX;

/// Maximum key length accepted by the server (64 KiB).
const MAX_KEY_LEN: u32 = 64 * 1024;

/// Maximum value length accepted by the server (64 MiB).
const MAX_VALUE_LEN: u32 = 64 * 1024 * 1024;

/// Maximum sealed frame length accepted in encrypted mode: the largest
/// legal plaintext message (a SETNX request — a SET plus its 8-byte TTL) plus
/// sealing overhead.
const MAX_FRAME_LEN: u32 = 1 + 4 + MAX_KEY_LEN + 4 + MAX_VALUE_LEN + 8 + SEAL_OVERHEAD as u32;

/// Op code for GET requests.
const OP_GET: u8 = 0x00;

/// Op code for SET requests.
const OP_SET: u8 = 0x01;

/// Op code for an owner-arbitrated SETNX.
const OP_SET_NX: u8 = 0x02;

/// Op code for an owner-arbitrated compare-and-delete.
const OP_DEL_IF_EQ: u8 = 0x03;

/// SET response: success.
const SET_OK: u8 = 0x00;

/// SET response: rejected (e.g., memory limit with `NoEviction`).
const SET_REJECTED: u8 = 0x01;

/// SETNX / DEL-IF-EQ response: the condition did not hold (the key exists /
/// the token did not match), so nothing changed.
const COND_NOT_APPLIED: u8 = 0x02;

/// SETNX / DEL-IF-EQ response: the receiving node cannot route the key.
const COND_UNROUTABLE: u8 = 0x03;

/// Whether `op` carries a value (or token) after the key.
fn op_has_value(op: u8) -> bool {
    matches!(op, OP_SET | OP_SET_NX | OP_DEL_IF_EQ)
}

/// Start the TCP KV data plane listener on all interfaces at `port`.
///
/// Serves lookups against the local [`Store`] so remote cluster nodes
/// can fetch large values that exceed the gossip tier threshold.
///
/// When `cipher` is `Some`, every request and response is exchanged as
/// a sealed frame; connections that fail authentication are dropped.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve(
    store: Arc<Store>,
    port: u16,
    cipher: Option<Arc<ClusterCipher>>,
) -> anyhow::Result<()> {
    serve_on(store, ([0, 0, 0, 0], port).into(), cipher).await
}

/// [`serve`] with per-vhost routing for multi-tenant nodes.
///
/// Identical to [`serve`] except that a request whose key carries the per-site
/// envelope (see [`crate::site_namespace`]) is served from **that site's**
/// store rather than the global one — the large-value counterpart of the
/// gossip applier's routing. Global keys are handled byte-for-byte as before.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve_multi_tenant(
    store: Arc<Store>,
    multi_tenant: ephpm_kv::multi_tenant::MultiTenantStore,
    port: u16,
    cipher: Option<Arc<ClusterCipher>>,
) -> anyhow::Result<()> {
    serve_router(KvRouter::multi_tenant(store, multi_tenant), ([0, 0, 0, 0], port).into(), cipher)
        .await
}

/// Resolves a data-plane transport key to the [`Store`] that owns it.
///
/// On a single-keyspace node this is always the global store. On a
/// multi-tenant node a key carrying the per-site envelope resolves to that
/// site's store (creating it if this node has not served the vhost yet), and
/// everything else to the global store — so a tenant's large values replicate
/// into the same tenant's keyspace on every node and never into the global one.
#[derive(Clone)]
pub struct KvRouter {
    global: Arc<Store>,
    multi_tenant: Option<ephpm_kv::multi_tenant::MultiTenantStore>,
}

impl KvRouter {
    /// A router that sends every key to the process-wide store.
    #[must_use]
    pub fn global(store: Arc<Store>) -> Self {
        Self { global: store, multi_tenant: None }
    }

    /// A router that honours the per-site envelope.
    #[must_use]
    pub fn multi_tenant(
        store: Arc<Store>,
        multi_tenant: ephpm_kv::multi_tenant::MultiTenantStore,
    ) -> Self {
        Self { global: store, multi_tenant: Some(multi_tenant) }
    }

    /// Resolve `transport_key` to its store and the bare key within it.
    ///
    /// `None` means the key claims a vhost but cannot be honoured — either
    /// this node is not multi-tenant, or the envelope does not decode to a
    /// valid site key. Both are refused rather than flattening a key that
    /// claims to be a tenant's into the shared global keyspace.
    fn resolve<'a>(&self, transport_key: &'a str) -> Option<(Arc<Store>, &'a str)> {
        match crate::site_namespace::decode(transport_key) {
            Some((site, key)) => match &self.multi_tenant {
                Some(mt) => Some((mt.get_site_store(site), key)),
                None => None,
            },
            // Fail closed on a malformed/hostile envelope; only a key that
            // never claimed a site routes to the global store.
            None if crate::site_namespace::is_enveloped(transport_key) => None,
            None => Some((Arc::clone(&self.global), transport_key)),
        }
    }
}

/// Start the TCP KV data plane listener bound to a specific address.
///
/// Like [`serve`] but binds `addr` exactly instead of `0.0.0.0:port`.
/// Useful when several nodes share one host (e.g. in-process tests on
/// distinct `127.0.0.x` loopback addresses).
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve_on(
    store: Arc<Store>,
    addr: SocketAddr,
    cipher: Option<Arc<ClusterCipher>>,
) -> anyhow::Result<()> {
    serve_router(KvRouter::global(store), addr, cipher).await
}

/// [`serve_on`] over an explicit [`KvRouter`] — the shared body of the global
/// and multi-tenant listeners.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve_router(
    router: KvRouter,
    addr: SocketAddr,
    cipher: Option<Arc<ClusterCipher>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind KV data plane to {addr}: {e}"))?;
    tracing::info!(%addr, encrypted = cipher.is_some(), "KV data plane listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::debug!(%e, "KV data plane accept error");
                continue;
            }
        };
        // Small framed request/response protocol — same Nagle/delayed-ACK
        // stall as the DB proxies without this.
        let _ = stream.set_nodelay(true);
        let router = router.clone();
        let cipher = cipher.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &router, cipher.as_deref()).await {
                tracing::debug!(%peer, %e, "KV data plane connection error");
            }
        });
    }
}

/// A parsed data plane request.
struct Request {
    /// Op code ([`OP_GET`] or [`OP_SET`]).
    op: u8,
    /// The key being fetched or stored.
    key: String,
    /// The value for SET / SETNX requests, or the token for DEL-IF-EQ.
    value: Option<Vec<u8>>,
    /// SETNX only: TTL in milliseconds, `0` = no expiry.
    ttl_ms: u64,
}

/// Handle a single TCP connection: read one request, write one response.
async fn handle_connection(
    mut stream: TcpStream,
    router: &KvRouter,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<()> {
    let request = read_request(&mut stream, cipher).await?;

    // Route the transport key to its owning store. A per-site key arriving at
    // a node that is not multi-tenant has no store to land in; answer
    // not-found / rejected rather than flattening it into the global keyspace.
    let Some((store, key)) = router.resolve(&request.key) else {
        tracing::warn!(
            key = %request.key,
            "KV data plane received a per-site key but this node is not multi-tenant \
             ([server] sites_dir unset); refusing it"
        );
        let response = match request.op {
            OP_GET => NOT_FOUND_SENTINEL.to_be_bytes().to_vec(),
            OP_SET_NX | OP_DEL_IF_EQ => vec![COND_UNROUTABLE],
            _ => vec![SET_REJECTED],
        };
        return write_message(&mut stream, &response, cipher).await;
    };

    let response = match request.op {
        OP_GET => {
            // Look up in the resolved store and build the response.
            if let Some(value) = store.get(key) {
                let len = u32::try_from(value.len()).unwrap_or(NOT_FOUND_SENTINEL - 1);
                let mut response = Vec::with_capacity(4 + value.len());
                response.extend_from_slice(&len.to_be_bytes());
                response.extend_from_slice(&value);
                response
            } else {
                NOT_FOUND_SENTINEL.to_be_bytes().to_vec()
            }
        }
        OP_SET => {
            let value = request.value.expect("SET request always carries a value");
            // Replica-side write: we are the receiver of a fanned-out replica
            // copy, not the origin. Use `set_local` so we don't re-enter any
            // installed Replicator hook and re-fanout the same key back to
            // our peers (infinite loop).
            let ok = store.set_local(key.to_string(), value, None);
            vec![if ok { SET_OK } else { SET_REJECTED }]
        }
        OP_SET_NX => {
            let value = request.value.expect("SETNX request always carries a value");
            let ttl =
                (request.ttl_ms > 0).then(|| std::time::Duration::from_millis(request.ttl_ms));
            // We are the key's owner, asked to arbitrate. Decide on OUR map and
            // do not re-route: two nodes that briefly disagree about ownership
            // would otherwise bounce the request between them. A win is
            // published to the cluster by the store's own replicator.
            let status = match store.set_nx_outcome_local(key.to_string(), value, ttl) {
                SetNxOutcome::Inserted => SET_OK,
                SetNxOutcome::Exists => COND_NOT_APPLIED,
                SetNxOutcome::OutOfMemory => SET_REJECTED,
                // Unreachable from the local path, but answer honestly.
                SetNxOutcome::OwnerUnreachable => COND_UNROUTABLE,
            };
            vec![status]
        }
        OP_DEL_IF_EQ => {
            let token = request.value.expect("DEL-IF-EQ request always carries a token");
            // Same as SETNX: decide here, never re-route. The deletion is
            // published as a tombstone by the store's replicator.
            vec![if store.del_if_eq_local(key, &token) { SET_OK } else { COND_NOT_APPLIED }]
        }
        other => {
            anyhow::bail!("unknown op code: {other:#04x}");
        }
    };

    write_message(&mut stream, &response, cipher).await
}

/// Read and validate one request from the stream.
///
/// In plaintext mode the fields are read incrementally off the wire; in
/// encrypted mode one sealed frame is read and parsed in memory.
async fn read_request(
    stream: &mut TcpStream,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<Request> {
    if let Some(cipher) = cipher {
        let frame = read_frame(stream, cipher).await?;
        return parse_request(&frame);
    }

    // Plaintext: incremental reads, byte-compatible with older nodes.
    let op = stream.read_u8().await?;

    let key_len = stream.read_u32().await?;
    if key_len > MAX_KEY_LEN {
        anyhow::bail!("key length {key_len} exceeds maximum {MAX_KEY_LEN}");
    }
    let mut key_buf = vec![0u8; key_len as usize];
    stream.read_exact(&mut key_buf).await?;
    let key = String::from_utf8(key_buf).map_err(|_| anyhow::anyhow!("invalid UTF-8 key"))?;

    let value = if op_has_value(op) {
        let value_len = stream.read_u32().await?;
        if value_len > MAX_VALUE_LEN {
            anyhow::bail!("value length {value_len} exceeds maximum {MAX_VALUE_LEN}");
        }
        // Chunked: on the plaintext path the peer is unauthenticated, so
        // a bare `vec![0u8; value_len]` is a 64 MiB allocation for the
        // cost of a 9-byte header.
        Some(read_exact_chunked(stream, value_len as usize).await?)
    } else {
        None
    };
    let ttl_ms = if op == OP_SET_NX { stream.read_u64().await? } else { 0 };

    Ok(Request { op, key, value, ttl_ms })
}

/// Parse a plaintext request message from a decrypted frame.
fn parse_request(mut buf: &[u8]) -> anyhow::Result<Request> {
    let op = take_u8(&mut buf)?;

    let key_len = take_u32(&mut buf)?;
    if key_len > MAX_KEY_LEN {
        anyhow::bail!("key length {key_len} exceeds maximum {MAX_KEY_LEN}");
    }
    let key = String::from_utf8(take_bytes(&mut buf, key_len as usize)?.to_vec())
        .map_err(|_| anyhow::anyhow!("invalid UTF-8 key"))?;

    let value = if op_has_value(op) {
        let value_len = take_u32(&mut buf)?;
        if value_len > MAX_VALUE_LEN {
            anyhow::bail!("value length {value_len} exceeds maximum {MAX_VALUE_LEN}");
        }
        Some(take_bytes(&mut buf, value_len as usize)?.to_vec())
    } else {
        None
    };
    let ttl_ms = if op == OP_SET_NX {
        let bytes: [u8; 8] =
            take_bytes(&mut buf, 8)?.try_into().expect("take_bytes(8) returns exactly 8 bytes");
        u64::from_be_bytes(bytes)
    } else {
        0
    };

    Ok(Request { op, key, value, ttl_ms })
}

/// Write one protocol message, sealing it into a frame when encrypted.
async fn write_message(
    stream: &mut TcpStream,
    message: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<()> {
    match cipher {
        Some(cipher) => {
            let sealed = cipher.seal(message)?;
            let len = u32::try_from(sealed.len())
                .map_err(|_| anyhow::anyhow!("sealed frame too large"))?;
            stream.write_u32(len).await?;
            stream.write_all(&sealed).await?;
        }
        None => {
            stream.write_all(message).await?;
        }
    }
    stream.flush().await?;
    Ok(())
}

/// Read one sealed frame and decrypt it.
async fn read_frame(stream: &mut TcpStream, cipher: &ClusterCipher) -> anyhow::Result<Vec<u8>> {
    let frame_len = stream.read_u32().await?;
    if frame_len > MAX_FRAME_LEN {
        anyhow::bail!(
            "frame length {frame_len} exceeds maximum {MAX_FRAME_LEN} \
             (peer sending plaintext to an encrypted data plane?)"
        );
    }
    // Chunked, because this runs *before* `cipher.open()` — the peer is
    // still unauthenticated here. Allocating `frame_len` up front would
    // hand any host that can reach the port a 64 MiB allocation per
    // 4-byte header, with no need to send a single byte of payload.
    let frame = read_exact_chunked(stream, frame_len as usize).await?;
    cipher.open(&frame).ok_or_else(|| {
        anyhow::anyhow!("failed to authenticate KV data plane frame (wrong cluster secret?)")
    })
}

/// Read exactly `len` bytes, growing the buffer as data actually arrives.
///
/// Every length on this wire is peer-supplied and read before its
/// payload, so `vec![0u8; len]` charges us the peer's *claim* rather than
/// what they send. Reading in chunks bounds the cost of a lie to one
/// chunk plus whatever was really transferred, while a legitimate
/// transfer still ends up in a single contiguous `Vec`.
async fn read_exact_chunked(stream: &mut TcpStream, len: usize) -> anyhow::Result<Vec<u8>> {
    /// Bytes pulled off the socket per read before growing the buffer.
    const CHUNK: usize = 64 * 1024;

    let mut buf = Vec::with_capacity(len.min(CHUNK));
    while buf.len() < len {
        let start = buf.len();
        let want = (len - start).min(CHUNK);
        buf.resize(start + want, 0);
        stream.read_exact(&mut buf[start..]).await?;
    }
    Ok(buf)
}

/// Fetch a value from a remote node's KV data plane.
///
/// Opens a TCP connection to `addr`, sends a GET request, and reads the
/// response. Returns `None` if the remote node does not have the key.
/// `cipher` must match the remote node's setting (both `Some` with the
/// same secret, or both `None`).
///
/// # Errors
///
/// Returns an error on connection or I/O failure, or when the peer's
/// encryption setting or secret does not match.
pub async fn fetch_remote(
    addr: SocketAddr,
    key: &str,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut stream = TcpStream::connect(addr).await?;
    let _ = stream.set_nodelay(true);

    // Send GET op + key.
    write_message(&mut stream, &encode_get_request(key)?, cipher).await?;

    // Read response.
    if let Some(cipher) = cipher {
        let frame = read_frame(&mut stream, cipher).await?;
        let mut buf = frame.as_slice();
        let value_len = take_u32(&mut buf)?;
        if value_len == NOT_FOUND_SENTINEL {
            return Ok(None);
        }
        return Ok(Some(take_bytes(&mut buf, value_len as usize)?.to_vec()));
    }

    let value_len = stream.read_u32().await?;
    if value_len == NOT_FOUND_SENTINEL {
        return Ok(None);
    }
    // The encrypted path above is bounded by MAX_FRAME_LEN before anything
    // is allocated; the plaintext path had no bound at all — which is
    // exactly the deployment with no `[cluster] secret`, where the
    // responder is not authenticated either. Bound it the same way the
    // server bounds an inbound value, then read it in chunks.
    if value_len > MAX_VALUE_LEN {
        anyhow::bail!("peer advertised value length {value_len} exceeding maximum {MAX_VALUE_LEN}");
    }
    let buf = read_exact_chunked(&mut stream, value_len as usize).await?;
    Ok(Some(buf))
}

/// Store a value on a remote node's KV data plane.
///
/// Opens a TCP connection to `addr`, sends a SET request with the key
/// and value, and reads the status response. `cipher` must match the
/// remote node's setting (both `Some` with the same secret, or both
/// `None`).
///
/// # Errors
///
/// Returns an error on connection or I/O failure, or when the peer's
/// encryption setting or secret does not match.
pub async fn store_remote(
    addr: SocketAddr,
    key: &str,
    value: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<bool> {
    let mut stream = TcpStream::connect(addr).await?;
    let _ = stream.set_nodelay(true);

    // Send SET op + key + value.
    write_message(&mut stream, &encode_set_request(key, value)?, cipher).await?;

    // Read status response.
    let status = if let Some(cipher) = cipher {
        let frame = read_frame(&mut stream, cipher).await?;
        take_u8(&mut frame.as_slice())?
    } else {
        stream.read_u8().await?
    };
    Ok(status == SET_OK)
}

/// Ask the owner node at `addr` to perform `SETNX key value [ttl]` on its own
/// map and return its decision.
///
/// The owner decides under its per-key shard lock and publishes a win to the
/// cluster itself, so the caller must not apply the value anywhere.
///
/// # Errors
///
/// Returns an error on connection or I/O failure, a cipher mismatch, a peer
/// that does not speak this op (an older node closes without answering), or a
/// peer that cannot route the key. Every error means **nothing was written**.
pub async fn set_nx_remote(
    addr: SocketAddr,
    key: &str,
    value: &[u8],
    ttl: Option<std::time::Duration>,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<SetNxOutcome> {
    let ttl_ms = ttl.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1));
    let status =
        conditional_roundtrip(addr, &encode_set_nx_request(key, value, ttl_ms)?, cipher).await?;
    match status {
        SET_OK => Ok(SetNxOutcome::Inserted),
        COND_NOT_APPLIED => Ok(SetNxOutcome::Exists),
        SET_REJECTED => Ok(SetNxOutcome::OutOfMemory),
        COND_UNROUTABLE => anyhow::bail!("owner {addr} cannot route key {key:?}"),
        other => anyhow::bail!("owner {addr} answered SETNX with unknown status {other:#04x}"),
    }
}

/// Ask the owner node at `addr` to delete `key` only while its value equals
/// `token`, returning whether it did.
///
/// # Errors
///
/// As [`set_nx_remote`]: any error means nothing was deleted.
pub async fn del_if_eq_remote(
    addr: SocketAddr,
    key: &str,
    token: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<bool> {
    let status =
        conditional_roundtrip(addr, &encode_del_if_eq_request(key, token)?, cipher).await?;
    match status {
        SET_OK => Ok(true),
        COND_NOT_APPLIED => Ok(false),
        COND_UNROUTABLE => anyhow::bail!("owner {addr} cannot route key {key:?}"),
        other => anyhow::bail!("owner {addr} answered DEL-IF-EQ with unknown status {other:#04x}"),
    }
}

/// Send one conditional-write request and read its one-byte status.
async fn conditional_roundtrip(
    addr: SocketAddr,
    message: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<u8> {
    let mut stream = TcpStream::connect(addr).await?;
    let _ = stream.set_nodelay(true);
    write_message(&mut stream, message, cipher).await?;
    if let Some(cipher) = cipher {
        let frame = read_frame(&mut stream, cipher).await?;
        return take_u8(&mut frame.as_slice());
    }
    Ok(stream.read_u8().await?)
}

/// Encode a plaintext SETNX request message.
fn encode_set_nx_request(key: &str, value: &[u8], ttl_ms: u64) -> anyhow::Result<Vec<u8>> {
    let mut message = encode_set_request(key, value)?;
    message[0] = OP_SET_NX;
    message.extend_from_slice(&ttl_ms.to_be_bytes());
    Ok(message)
}

/// Encode a plaintext DEL-IF-EQ request message (the token rides where a SET
/// carries its value).
fn encode_del_if_eq_request(key: &str, token: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut message = encode_set_request(key, token)?;
    message[0] = OP_DEL_IF_EQ;
    Ok(message)
}

/// Encode a plaintext GET request message.
fn encode_get_request(key: &str) -> anyhow::Result<Vec<u8>> {
    let key_bytes = key.as_bytes();
    let key_len = u32::try_from(key_bytes.len()).map_err(|_| anyhow::anyhow!("key too long"))?;
    let mut message = Vec::with_capacity(1 + 4 + key_bytes.len());
    message.push(OP_GET);
    message.extend_from_slice(&key_len.to_be_bytes());
    message.extend_from_slice(key_bytes);
    Ok(message)
}

/// Encode a plaintext SET request message.
fn encode_set_request(key: &str, value: &[u8]) -> anyhow::Result<Vec<u8>> {
    let key_bytes = key.as_bytes();
    let key_len = u32::try_from(key_bytes.len()).map_err(|_| anyhow::anyhow!("key too long"))?;
    let value_len = u32::try_from(value.len())
        .map_err(|_| anyhow::anyhow!("value too large for TCP data plane"))?;
    let mut message = Vec::with_capacity(1 + 4 + key_bytes.len() + 4 + value.len());
    message.push(OP_SET);
    message.extend_from_slice(&key_len.to_be_bytes());
    message.extend_from_slice(key_bytes);
    message.extend_from_slice(&value_len.to_be_bytes());
    message.extend_from_slice(value);
    Ok(message)
}

/// Take one byte from the front of `buf`.
fn take_u8(buf: &mut &[u8]) -> anyhow::Result<u8> {
    let (byte, rest) = buf.split_first().ok_or_else(|| anyhow::anyhow!("truncated message"))?;
    *buf = rest;
    Ok(*byte)
}

/// Take a big-endian u32 from the front of `buf`.
fn take_u32(buf: &mut &[u8]) -> anyhow::Result<u32> {
    let bytes: [u8; 4] =
        take_bytes(buf, 4)?.try_into().expect("take_bytes(4) returns exactly 4 bytes");
    Ok(u32::from_be_bytes(bytes))
}

/// Take `len` bytes from the front of `buf`.
fn take_bytes<'a>(buf: &mut &'a [u8], len: usize) -> anyhow::Result<&'a [u8]> {
    if buf.len() < len {
        anyhow::bail!("truncated message: expected {len} bytes, got {}", buf.len());
    }
    let (bytes, rest) = buf.split_at(len);
    *buf = rest;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a data plane listener that handles a single connection.
    async fn spawn_single_handler(
        store: Arc<Store>,
        cipher: Option<Arc<ClusterCipher>>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, &KvRouter::global(store), cipher.as_deref()).await.unwrap();
        });
        addr
    }

    /// Spawn a data plane listener over an explicit [`KvRouter`], so the
    /// per-vhost routing tests exercise the same `handle_connection` the
    /// global tests do.
    async fn spawn_router_handler(
        router: KvRouter,
        cipher: Option<Arc<ClusterCipher>>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let router = router.clone();
                let cipher = cipher.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, &router, cipher.as_deref()).await;
                });
            }
        });
        addr
    }

    // ── Per-vhost routing (large-value tier) ──────────────────────────

    fn multi_tenant_fixture() -> (Arc<Store>, ephpm_kv::multi_tenant::MultiTenantStore) {
        let global = Store::new(ephpm_kv::store::StoreConfig::default());
        let mt = ephpm_kv::multi_tenant::MultiTenantStore::new(
            Arc::clone(&global),
            ephpm_kv::store::StoreConfig::default(),
        );
        (global, mt)
    }

    /// A replicated per-site large value lands in THAT site's store — not the
    /// global one, and not another tenant's.
    #[tokio::test]
    async fn per_site_set_lands_only_in_that_sites_store() {
        let (global, mt) = multi_tenant_fixture();
        let addr =
            spawn_router_handler(KvRouter::multi_tenant(Arc::clone(&global), mt.clone()), None)
                .await;

        let transport = crate::site_namespace::encode("alice.test", "shared");
        assert!(store_remote(addr, &transport, b"alice-data", None).await.unwrap());

        // Landed in alice's store under the BARE key.
        assert_eq!(
            mt.get_site_store("alice.test").get("shared").as_deref(),
            Some(&b"alice-data"[..])
        );
        // Never in the global keyspace — under either spelling.
        assert_eq!(global.get("shared"), None);
        assert_eq!(global.get(&transport), None);
        // Never in another tenant's.
        assert_eq!(mt.get_site_store("bob.test").get("shared"), None);
    }

    /// Two sites writing the same bare key stay independent end to end.
    #[tokio::test]
    async fn two_sites_same_key_do_not_collide_over_the_wire() {
        let (global, mt) = multi_tenant_fixture();
        let addr =
            spawn_router_handler(KvRouter::multi_tenant(Arc::clone(&global), mt.clone()), None)
                .await;

        store_remote(addr, &crate::site_namespace::encode("alice.test", "k"), b"A", None)
            .await
            .unwrap();
        store_remote(addr, &crate::site_namespace::encode("bob.test", "k"), b"B", None)
            .await
            .unwrap();

        assert_eq!(mt.get_site_store("alice.test").get("k").as_deref(), Some(&b"A"[..]));
        assert_eq!(mt.get_site_store("bob.test").get("k").as_deref(), Some(&b"B"[..]));
        assert_eq!(global.get("k"), None);

        // And a per-site GET reads back from the right store only.
        let got = fetch_remote(addr, &crate::site_namespace::encode("alice.test", "k"), None)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(&b"A"[..]));
    }

    /// A global key still routes to the global store on a multi-tenant node —
    /// the existing behaviour is unchanged by the routing layer.
    #[tokio::test]
    async fn global_keys_still_route_globally_on_a_multi_tenant_node() {
        let (global, mt) = multi_tenant_fixture();
        let addr =
            spawn_router_handler(KvRouter::multi_tenant(Arc::clone(&global), mt.clone()), None)
                .await;

        assert!(store_remote(addr, "plain-key", b"v", None).await.unwrap());
        assert_eq!(global.get("plain-key").as_deref(), Some(&b"v"[..]));
        // A site store created afterwards must not see it.
        assert_eq!(mt.get_site_store("alice.test").get("plain-key"), None);
    }

    /// A per-site key arriving at a node that is NOT multi-tenant is refused,
    /// never flattened into that node's global keyspace.
    #[tokio::test]
    async fn per_site_key_is_refused_by_a_single_keyspace_node() {
        let global = Store::new(ephpm_kv::store::StoreConfig::default());
        let addr = spawn_router_handler(KvRouter::global(Arc::clone(&global)), None).await;

        let transport = crate::site_namespace::encode("alice.test", "k");
        assert!(!store_remote(addr, &transport, b"v", None).await.unwrap(), "SET must be rejected");
        assert_eq!(global.get(&transport), None);
        assert_eq!(global.get("k"), None);
        assert_eq!(fetch_remote(addr, &transport, None).await.unwrap(), None);
    }

    /// Spawn a data plane listener that handles multiple connections.
    async fn spawn_multi_handler(
        store: Arc<Store>,
        cipher: Option<Arc<ClusterCipher>>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let store = Arc::clone(&store);
                let cipher = cipher.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, &KvRouter::global(store), cipher.as_deref())
                        .await;
                });
            }
        });
        addr
    }

    fn test_cipher(secret: &str) -> Arc<ClusterCipher> {
        Arc::new(ClusterCipher::for_kv_data_plane(secret))
    }

    #[tokio::test]
    async fn get_roundtrip_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        store.set("hello".to_string(), b"world".to_vec(), None);

        let addr = spawn_single_handler(Arc::clone(&store), None).await;
        let result = fetch_remote(addr, "hello", None).await.unwrap();
        assert_eq!(result, Some(b"world".to_vec()));
    }

    #[tokio::test]
    async fn get_roundtrip_not_found() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_single_handler(Arc::clone(&store), None).await;
        let result = fetch_remote(addr, "missing", None).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn set_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Store a value remotely.
        let ok = store_remote(addr, "remote_key", b"remote_val", None).await.unwrap();
        assert!(ok);

        // Verify it landed in the local store.
        let value = store.get("remote_key");
        assert_eq!(value.as_deref(), Some(&b"remote_val"[..]));
    }

    #[tokio::test]
    async fn set_then_get_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Store remotely via TCP, then fetch remotely via TCP.
        let ok = store_remote(addr, "k", b"v", None).await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "k", None).await.unwrap();
        assert_eq!(result, Some(b"v".to_vec()));
    }

    #[tokio::test]
    async fn concurrent_fetches() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        for i in 0..10 {
            store.set(format!("k{i}"), format!("v{i}").into_bytes(), None);
        }

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Launch 10 concurrent fetches.
        let mut handles = Vec::new();
        for i in 0..10 {
            let key = format!("k{i}");
            handles.push(tokio::spawn(async move { fetch_remote(addr, &key, None).await }));
        }

        for (i, handle) in handles.into_iter().enumerate() {
            let result = handle.await.unwrap().unwrap();
            assert_eq!(result, Some(format!("v{i}").into_bytes()));
        }
    }

    #[tokio::test]
    async fn connection_refused_returns_error() {
        // Connect to a port that is not listening.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let result = fetch_remote(addr, "key", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn set_large_value() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // Store a 100 KiB value.
        let big_value = vec![42u8; 100 * 1024];
        let ok = store_remote(addr, "big", &big_value, None).await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "big", None).await.unwrap();
        assert_eq!(result.as_ref().map(Vec::len), Some(big_value.len()));
        assert_eq!(result, Some(big_value));
    }

    #[tokio::test]
    async fn encrypted_get_set_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let cipher = test_cipher("data-plane-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(Arc::clone(&cipher))).await;

        let ok = store_remote(addr, "enc_key", b"enc_val", Some(&cipher)).await.unwrap();
        assert!(ok);
        assert_eq!(store.get("enc_key").as_deref(), Some(&b"enc_val"[..]));

        let result = fetch_remote(addr, "enc_key", Some(&cipher)).await.unwrap();
        assert_eq!(result, Some(b"enc_val".to_vec()));

        let missing = fetch_remote(addr, "nope", Some(&cipher)).await.unwrap();
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn encrypted_large_value_roundtrip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let cipher = test_cipher("data-plane-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(Arc::clone(&cipher))).await;

        let big_value = vec![7u8; 100 * 1024];
        let ok = store_remote(addr, "big", &big_value, Some(&cipher)).await.unwrap();
        assert!(ok);

        let result = fetch_remote(addr, "big", Some(&cipher)).await.unwrap();
        assert_eq!(result, Some(big_value));
    }

    #[tokio::test]
    async fn wrong_secret_is_rejected() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let server_cipher = test_cipher("right-secret");
        let client_cipher = test_cipher("wrong-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(server_cipher)).await;

        // Neither reads nor writes must succeed with the wrong secret.
        let fetch = fetch_remote(addr, "key", Some(&client_cipher)).await;
        assert!(fetch.is_err(), "wrong-secret fetch must fail");

        let set = store_remote(addr, "key", b"injected", Some(&client_cipher)).await;
        assert!(set.is_err(), "wrong-secret store must fail");
        assert_eq!(store.get("key"), None, "wrong-secret write must not land in the store");
    }

    #[tokio::test]
    async fn plaintext_client_rejected_by_encrypted_server() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        store.set("key".to_string(), b"secret-value".to_vec(), None);
        let server_cipher = test_cipher("server-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), Some(server_cipher)).await;

        let result = fetch_remote(addr, "key", None).await;
        assert!(result.is_err(), "plaintext client must not read from an encrypted server");
    }

    #[tokio::test]
    async fn encrypted_client_rejected_by_plaintext_server() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let client_cipher = test_cipher("client-secret");

        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // The plaintext server misparses the frame header and errors,
        // returns garbage that fails frame authentication client-side,
        // or (rarely) stalls waiting for bytes that never come — the
        // timeout covers that case. All outcomes are a rejection.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetch_remote(addr, "key", Some(&client_cipher)),
        )
        .await;
        assert!(
            !matches!(result, Ok(Ok(_))),
            "encrypted client must not accept a plaintext server"
        );
    }

    #[test]
    fn parse_request_rejects_truncated_input() {
        assert!(parse_request(&[]).is_err());
        assert!(parse_request(&[OP_GET]).is_err());
        assert!(parse_request(&[OP_GET, 0, 0, 0, 5, b'a']).is_err());
    }

    /// Values larger than one read chunk must reassemble byte-exactly:
    /// the chunked reader is on the path of every plaintext SET and GET.
    #[tokio::test]
    async fn multi_chunk_value_round_trip() {
        let store = Arc::new(Store::new(ephpm_kv::store::StoreConfig::default()));
        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        // 200 KiB spans four 64 KiB read chunks.
        let value: Vec<u8> =
            (0..200 * 1024).map(|i| u8::try_from(i % 251).expect("i % 251 < 256")).collect();

        assert!(store_remote(addr, "big", &value, None).await.unwrap());
        let fetched = fetch_remote(addr, "big", None).await.unwrap();
        assert_eq!(fetched, Some(value));
    }

    /// A peer that advertises a value length past the cap is rejected on
    /// the header, before any memory is committed on its behalf.
    #[tokio::test]
    async fn oversized_value_len_rejected_before_allocating() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream, None).await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_u8(OP_SET).await.unwrap();
        client.write_u32(3).await.unwrap();
        client.write_all(b"key").await.unwrap();
        // Claim far more than the cap, then send no payload at all.
        client.write_u32(MAX_VALUE_LEN + 1).await.unwrap();
        client.flush().await.unwrap();

        // `unwrap_err()` would require `Request: Debug` for the Ok variant, and
        // `Request` carries the key and value bytes — deriving Debug on it puts
        // stored values one stray `{:?}` away from the logs. Match instead.
        let Err(err) = server.await.unwrap() else {
            panic!("expected an oversized-length error, got a parsed request");
        };
        assert!(err.to_string().contains("exceeds maximum"), "got {err}");
    }

    // ── Owner-arbitrated conditional writes ───────────────────────────

    /// The owner's map decides SETNX: first caller inserts, every later caller
    /// sees `Exists`, and the stored value is the winner's.
    #[tokio::test]
    async fn set_nx_remote_is_decided_by_the_owner() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let addr = spawn_multi_handler(Arc::clone(&store), None).await;

        let first = set_nx_remote(addr, "lock", b"A", None, None).await.unwrap();
        let second = set_nx_remote(addr, "lock", b"B", None, None).await.unwrap();
        assert_eq!(first, SetNxOutcome::Inserted);
        assert_eq!(second, SetNxOutcome::Exists);
        assert_eq!(store.get("lock").as_deref(), Some(&b"A"[..]), "the first caller won");
    }

    /// The TTL crosses the wire: an expired grant is vacant again on the owner.
    #[tokio::test]
    async fn set_nx_remote_carries_the_ttl() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let addr = spawn_multi_handler(Arc::clone(&store), Some(test_cipher("s"))).await;
        let cipher = test_cipher("s");

        let ttl = Some(std::time::Duration::from_millis(150));
        assert_eq!(
            set_nx_remote(addr, "lock", b"A", ttl, Some(&cipher)).await.unwrap(),
            SetNxOutcome::Inserted
        );
        let pttl = store.pttl("lock").expect("owner holds the lock");
        assert!((1..=150).contains(&pttl), "owner applied the TTL, got pttl {pttl}");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert_eq!(
            set_nx_remote(addr, "lock", b"B", None, Some(&cipher)).await.unwrap(),
            SetNxOutcome::Inserted,
            "an expired grant must be vacant again"
        );
    }

    /// Compare-and-delete is compared against the OWNER's value.
    #[tokio::test]
    async fn del_if_eq_remote_compares_on_the_owner() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let addr = spawn_multi_handler(Arc::clone(&store), None).await;
        store.set_local("lock".into(), b"new-holder".to_vec(), None);

        assert!(!del_if_eq_remote(addr, "lock", b"old-holder", None).await.unwrap());
        assert_eq!(store.get("lock").as_deref(), Some(&b"new-holder"[..]), "mismatch keeps it");
        assert!(del_if_eq_remote(addr, "lock", b"new-holder", None).await.unwrap());
        assert_eq!(store.get("lock"), None);
    }

    /// A per-site conditional write lands in that site's store on the owner.
    #[tokio::test]
    async fn per_site_set_nx_lands_in_that_sites_store() {
        let (global, mt) = multi_tenant_fixture();
        let addr =
            spawn_router_handler(KvRouter::multi_tenant(Arc::clone(&global), mt.clone()), None)
                .await;
        let transport = crate::site_namespace::encode("alice.test", "lock");

        assert_eq!(
            set_nx_remote(addr, &transport, b"A", None, None).await.unwrap(),
            SetNxOutcome::Inserted
        );
        assert_eq!(mt.get_site_store("alice.test").get("lock").as_deref(), Some(&b"A"[..]));
        assert_eq!(global.get("lock"), None);
        assert_eq!(mt.get_site_store("bob.test").get("lock"), None);
        assert!(del_if_eq_remote(addr, &transport, b"A", None).await.unwrap());
        assert_eq!(mt.get_site_store("alice.test").get("lock"), None);
    }

    /// A node that cannot route a per-site key answers "unroutable", which the
    /// caller surfaces as an error (nothing written) — never as "exists".
    #[tokio::test]
    async fn conditional_write_to_an_unroutable_node_is_an_error() {
        let global = Store::new(ephpm_kv::store::StoreConfig::default());
        let addr = spawn_router_handler(KvRouter::global(Arc::clone(&global)), None).await;
        let transport = crate::site_namespace::encode("alice.test", "lock");

        assert!(set_nx_remote(addr, &transport, b"A", None, None).await.is_err());
        assert!(del_if_eq_remote(addr, &transport, b"A", None).await.is_err());
        assert_eq!(global.get("lock"), None);
    }

    #[test]
    fn parse_request_roundtrips_conditional_encoders() {
        let nx = encode_set_nx_request("k", b"v", 1500).unwrap();
        let parsed = parse_request(&nx).unwrap();
        assert_eq!(parsed.op, OP_SET_NX);
        assert_eq!(parsed.key, "k");
        assert_eq!(parsed.value.as_deref(), Some(b"v".as_slice()));
        assert_eq!(parsed.ttl_ms, 1500);

        let del = encode_del_if_eq_request("k", b"tok").unwrap();
        let parsed = parse_request(&del).unwrap();
        assert_eq!(parsed.op, OP_DEL_IF_EQ);
        assert_eq!(parsed.value.as_deref(), Some(b"tok".as_slice()));
        assert_eq!(parsed.ttl_ms, 0);

        // A SETNX missing its TTL trailer is truncated, not "no TTL".
        assert!(parse_request(&nx[..nx.len() - 1]).is_err());
    }

    #[test]
    fn parse_request_roundtrips_encoders() {
        let get = encode_get_request("mykey").unwrap();
        let parsed = parse_request(&get).unwrap();
        assert_eq!(parsed.op, OP_GET);
        assert_eq!(parsed.key, "mykey");
        assert!(parsed.value.is_none());

        let set = encode_set_request("mykey", b"myvalue").unwrap();
        let parsed = parse_request(&set).unwrap();
        assert_eq!(parsed.op, OP_SET);
        assert_eq!(parsed.key, "mykey");
        assert_eq!(parsed.value.as_deref(), Some(b"myvalue".as_slice()));
    }
}

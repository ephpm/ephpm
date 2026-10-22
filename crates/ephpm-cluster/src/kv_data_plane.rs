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
//!
//! ## Connections, timeouts and bounds
//!
//! A connection carries **any number** of request/response exchanges, one at
//! a time, until the client closes it or the server sees no request for
//! [`SERVER_IDLE_TIMEOUT`]. Clients keep a small pool of idle connections per
//! peer ([`DataPlaneClient`]) so a replication burst does not open one TCP
//! connection per value. A server predating this closes after one response;
//! the pool notices the closed connection on reuse and dials again, so the
//! two interoperate.
//!
//! Every network step is bounded: [`CONNECT_TIMEOUT`] to dial, and
//! [`REQUEST_TIMEOUT`] for a whole exchange. A peer that accepts and then
//! stalls (a SIGSTOPped process, a black-holed route) costs one timeout, not
//! a file descriptor forever — 20k large writes against a frozen peer used
//! to leave 13k sockets in `SYN-SENT` (stress finding F-13). A peer that
//! fails to connect is skipped for [`PEER_BREAKER_COOLDOWN`] before the next
//! dial, so a dead peer does not charge every replica write a full connect
//! timeout while the failure detector catches up.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ephpm_kv::store::{SetNxOutcome, Store};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
// tokio's `Instant` (not std's) so the breaker cooldown follows paused time
// in tests exactly like the timeouts around it do.
use tokio::time::Instant;

use crate::clustered_store::{AppliedWriteMap, Apply, apply_if_newer};
use crate::gossip_kv::Stamp;
use crate::secure_transport::{ClusterCipher, SEAL_OVERHEAD};

/// How long a client waits for a TCP connection to a peer to be established.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a client waits for one complete request/response exchange after
/// the connection is up. Sized for a `MAX_VALUE_LEN` value on a slow link, not
/// for the common sub-millisecond exchange; callers that block a request
/// thread wrap their call in a tighter timeout of their own.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the server keeps an idle connection open waiting for its next
/// request (or the first). A client whose request takes longer than this to
/// arrive in full is also dropped, which bounds a slowloris at one timeout.
pub const SERVER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the server allows for writing one response.
const SERVER_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle connections kept per peer by a [`DataPlaneClient`]. Anything beyond
/// this is closed on return.
const POOL_IDLE_PER_PEER: usize = 8;

/// After a failed connect, the peer is reported unreachable without dialling
/// for this long.
pub const PEER_BREAKER_COOLDOWN: Duration = Duration::from_secs(2);

/// Sentinel value indicating "key not found" in the wire protocol.
const NOT_FOUND_SENTINEL: u32 = u32::MAX;

/// Maximum key length accepted by the server (64 KiB).
const MAX_KEY_LEN: u32 = 64 * 1024;

/// Maximum value length accepted by the server (64 MiB).
const MAX_VALUE_LEN: u32 = 64 * 1024 * 1024;

/// Maximum sealed frame length accepted in encrypted mode: the largest
/// legal plaintext message (a stamped SET request — a SET plus its 8-byte TTL
/// and 16-byte stamp) plus sealing overhead.
const MAX_FRAME_LEN: u32 = 1 + 4 + MAX_KEY_LEN + 4 + MAX_VALUE_LEN + 8 + 16 + SEAL_OVERHEAD as u32;

/// Op code for GET requests.
const OP_GET: u8 = 0x00;

/// Op code for the original SET request: no TTL, no stamp. Still served for
/// a sender predating [`OP_SET_V2`]; no longer sent.
const OP_SET: u8 = 0x01;

/// Op code for a stamped SET: `[key][value][ttl_ms u64][write u64][node_tag u64]`.
///
/// `ttl_ms` is the remaining lifetime (`0` = none) — relative, so a clock
/// skew between nodes does not shift the expiry (stress finding F-9: the
/// original SET carried no TTL at all, so a replica's copy never expired).
/// `write`/`node_tag` are the origin's [`Stamp`]: the receiver applies the
/// write only when it is strictly newer than the last one it applied for
/// the key, through the same applied map the gossip applier uses, so a
/// late replica write cannot regress a newer value and the companion gossip
/// tombstone a large write carries orders correctly against it.
const OP_SET_V2: u8 = 0x04;

/// Op code for an owner-arbitrated SETNX.
const OP_SET_NX: u8 = 0x02;
/// Upper bound on a peer-supplied SETNX TTL (ten years), so `Instant + ttl`
/// on the owner cannot overflow and panic.
const MAX_REMOTE_TTL_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1000;

/// Op code for an owner-arbitrated compare-and-delete.
const OP_DEL_IF_EQ: u8 = 0x03;

/// Op code for a peer warm-up pull (`[key]` = the requester's node id, no
/// value): the server streams back every large value for which the
/// requester is in the replica set, as a sequence of [`SHARE_BATCH`]
/// messages ending with a [`SHARE_END`] message. A batch message is
/// `[SHARE_BATCH][len u32][payload]` (one sealed frame when encrypted),
/// `payload` = `[count u32]` then `count` entries of
/// `[key_len u32][transport_key][value_len u32][value][ttl_ms u64][write u64][node_tag u64]`,
/// about [`SHARE_BATCH_BYTES`] per batch — so a share of any size is bounded
/// by one batch in flight on each end, and costs a syscall per batch rather
/// than per value (one value per message pulled 400k entries at 13 MB/s;
/// batched, the same pull runs at the data plane's push rate). See
/// [`crate::kv_handoff`]. A node predating this op drops the connection,
/// which the puller reports as that peer being unable to share.
const OP_SHARE: u8 = 0x05;

/// Share stream message flag: a batch of entries follows.
const SHARE_BATCH: u8 = 0x02;

/// Share stream message flag: the stream is complete.
const SHARE_END: u8 = 0x00;

/// Target payload bytes per share batch.
const SHARE_BATCH_BYTES: usize = 256 * 1024;

/// Largest share batch a puller accepts (a batch holds at least one entry,
/// so it can exceed the target by one value).
const MAX_SHARE_BATCH: u32 = SHARE_BATCH_BYTES as u32 + MAX_KEY_LEN + MAX_VALUE_LEN + 64;

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
    matches!(op, OP_SET | OP_SET_V2 | OP_SET_NX | OP_DEL_IF_EQ)
}

/// Whether `op` carries a `ttl_ms` trailer after the value.
fn op_has_ttl(op: u8) -> bool {
    matches!(op, OP_SET_V2 | OP_SET_NX)
}

/// Start the TCP KV data plane listener on `addr`.
///
/// Serves lookups against the local [`Store`] so remote cluster nodes
/// can fetch large values that exceed the gossip tier threshold.
///
/// `addr` is the `[cluster] bind` IP with `[cluster.kv] data_port` — the
/// listener follows the gossip bind address rather than `0.0.0.0`, so a node
/// bound to one interface of a multi-homed host does not expose its KV data
/// plane on the others (stress finding F-15). An unspecified bind IP stays
/// unspecified.
///
/// When `cipher` is `Some`, every request and response is exchanged as
/// a sealed frame; connections that fail authentication are dropped.
///
/// # Errors
///
/// Returns an error if the TCP listener fails to bind.
pub async fn serve(
    store: Arc<Store>,
    addr: SocketAddr,
    cipher: Option<Arc<ClusterCipher>>,
    applied: AppliedWriteMap,
) -> anyhow::Result<()> {
    serve_router(KvRouter::global(store).with_applied(applied), addr, cipher).await
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
    addr: SocketAddr,
    cipher: Option<Arc<ClusterCipher>>,
    applied: AppliedWriteMap,
) -> anyhow::Result<()> {
    serve_router(KvRouter::multi_tenant(store, multi_tenant).with_applied(applied), addr, cipher)
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
    /// The node's last-applied-stamp map, shared with the gossip applier.
    /// A stamped SET is applied only when strictly newer than the record.
    /// `None` (tests, tools) applies every stamped SET unconditionally.
    applied: Option<AppliedWriteMap>,
    /// Answers [`OP_SHARE`] (peer warm-up pulls). `None` = the op is
    /// answered with an empty stream, so a node without handoff still
    /// interoperates with one that pulls.
    handoff: Option<Arc<crate::kv_handoff::HandoffSource>>,
}

impl KvRouter {
    /// A router that sends every key to the process-wide store.
    #[must_use]
    pub fn global(store: Arc<Store>) -> Self {
        Self { global: store, multi_tenant: None, applied: None, handoff: None }
    }

    /// A router that honours the per-site envelope.
    #[must_use]
    pub fn multi_tenant(
        store: Arc<Store>,
        multi_tenant: ephpm_kv::multi_tenant::MultiTenantStore,
    ) -> Self {
        Self { global: store, multi_tenant: Some(multi_tenant), applied: None, handoff: None }
    }

    /// Gate stamped SETs through `applied` — the same map the gossip applier
    /// and the replication hooks record into, so every path that lands a
    /// value on this node agrees on which write is newest.
    #[must_use]
    pub fn with_applied(mut self, applied: AppliedWriteMap) -> Self {
        self.applied = Some(applied);
        self
    }

    /// Answer peer warm-up pulls ([`OP_SHARE`]) from `source`.
    #[must_use]
    pub fn with_handoff(mut self, source: Arc<crate::kv_handoff::HandoffSource>) -> Self {
        self.handoff = Some(source);
        self
    }

    /// Every store this router can route to, with its site key (`""` for
    /// the global store) — the stores a handoff walks.
    #[must_use]
    pub fn stores(&self) -> Vec<(String, Arc<Store>)> {
        let mut out = vec![(String::new(), Arc::clone(&self.global))];
        if let Some(mt) = &self.multi_tenant {
            out.extend(mt.site_stores());
        }
        out
    }

    /// Read a value **together with** the stamp recorded for its key, as one
    /// consistent pair: `read` runs while this node's applied-map slot for
    /// `transport_key` is held, the same slot [`apply_if_newer`] holds across
    /// its local write, so a write cannot land between the two reads.
    ///
    /// Reading them separately is a real bug, not a theoretical one: the
    /// handoff walk once copied a value, then looked up the stamp, and a
    /// replica write that landed in between left it sending the *old* value
    /// under the *new* stamp — which then beat the real new value on every
    /// receiver (two keys of three thousand in the lab's race scenario).
    ///
    /// `None` from `read` means there is nothing to send; the stamp is
    /// `None` when the key has no record within the applied horizon.
    pub fn read_stamped<T>(
        &self,
        transport_key: &str,
        read: impl FnOnce() -> Option<T>,
    ) -> Option<(T, Option<Stamp>)> {
        match self.applied.as_ref() {
            Some(applied) => {
                // Lock order: applied slot, then (inside `read`) the store's
                // shard — the same order `apply_if_newer` uses.
                let slot = applied.get(transport_key);
                let stamp = slot.as_ref().map(|s| **s);
                let value = read()?;
                drop(slot);
                Some((value, stamp))
            }
            None => read().map(|v| (v, None)),
        }
    }

    /// Land a stamped value on this node — the [`OP_SET_V2`] path, also used
    /// by a warm-up pull for every entry a peer shares. The write is ordered
    /// through the applied map: an older stamp than the one recorded for the
    /// key is acknowledged but not applied, so a shared copy can never
    /// regress a write that landed here first.
    ///
    /// `Some(accepted)` — the store took (or skipped as stale) the write;
    /// `None` — the key claims a vhost this node cannot route.
    #[must_use]
    pub fn apply_stamped(
        &self,
        transport_key: &str,
        value: Vec<u8>,
        ttl_ms: u64,
        stamp: Stamp,
    ) -> Option<bool> {
        let (store, key) = self.resolve(transport_key)?;
        let ttl =
            (ttl_ms > 0).then(|| std::time::Duration::from_millis(ttl_ms.min(MAX_REMOTE_TTL_MS)));
        let mut ok = true;
        let verdict = match self.applied.as_ref() {
            Some(applied) => apply_if_newer(applied, transport_key, stamp, || {
                ok = store.set_local(key.to_string(), value, ttl);
            }),
            None => {
                ok = store.set_local(key.to_string(), value, ttl);
                Apply::Newer
            }
        };
        if verdict == Apply::Skip {
            tracing::trace!(key = %transport_key, ?stamp, "stamped SET skipped as stale");
            return Some(true);
        }
        Some(ok)
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
    /// SETNX and stamped SET: TTL in milliseconds, `0` = no expiry.
    ttl_ms: u64,
    /// Stamped SET only: the origin's write stamp.
    stamp: Option<Stamp>,
}

/// Serve one TCP connection: request/response exchanges until the client
/// closes it or stays silent for [`SERVER_IDLE_TIMEOUT`].
///
/// A clean close (EOF before the first byte of a request) is `Ok`; a
/// timeout, a malformed request or an I/O error ends the connection with the
/// error, and the client treats whatever it was waiting on as failed.
async fn handle_connection(
    mut stream: TcpStream,
    router: &KvRouter,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<()> {
    loop {
        let request = match tokio::time::timeout(
            SERVER_IDLE_TIMEOUT,
            read_request(&mut stream, cipher),
        )
        .await
        {
            Ok(Ok(request)) => request,
            Ok(Err(e)) => {
                return if is_clean_close(&e) { Ok(()) } else { Err(e) };
            }
            Err(_) => anyhow::bail!("idle for {SERVER_IDLE_TIMEOUT:?} without a complete request"),
        };
        if request.op == OP_SHARE {
            // A streamed answer: many messages, then the end marker.
            serve_share(&mut stream, router, &request.key, cipher).await?;
            continue;
        }
        let response = serve_request(request, router)?;
        tokio::time::timeout(SERVER_WRITE_TIMEOUT, write_message(&mut stream, &response, cipher))
            .await
            .map_err(|_| {
                anyhow::anyhow!("peer did not read the response within {SERVER_WRITE_TIMEOUT:?}")
            })??;
    }
}

/// Whether a request-read error is the client closing the connection between
/// requests, i.e. EOF before the first byte of a new request.
fn is_clean_close(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        && err.to_string().contains(CLEAN_CLOSE_MARKER)
}

/// Appended to the EOF error raised when the connection closes between
/// requests, so [`is_clean_close`] can tell it from a truncated request.
const CLEAN_CLOSE_MARKER: &str = "connection closed between requests";

/// Decide one request against the routed store and build its response.
fn serve_request(request: Request, router: &KvRouter) -> anyhow::Result<Vec<u8>> {
    // Route the transport key to its owning store. A per-site key arriving at
    // a node that is not multi-tenant has no store to land in; answer
    // not-found / rejected rather than flattening it into the global keyspace.
    let Some((store, key)) = router.resolve(&request.key) else {
        tracing::warn!(
            key = %request.key,
            "KV data plane received a per-site key but this node is not multi-tenant \
             ([server] sites_dir unset); refusing it"
        );
        return Ok(match request.op {
            OP_GET => NOT_FOUND_SENTINEL.to_be_bytes().to_vec(),
            OP_SET_NX | OP_DEL_IF_EQ => vec![COND_UNROUTABLE],
            _ => vec![SET_REJECTED],
        });
    };

    Ok(match request.op {
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
            // our peers (infinite loop). A pre-stamp sender: no TTL, no
            // ordering — applied as before.
            let ok = store.set_local(key.to_string(), value, None);
            vec![if ok { SET_OK } else { SET_REJECTED }]
        }
        OP_SET_V2 => {
            let value = request.value.expect("stamped SET always carries a value");
            let stamp = request.stamp.expect("stamped SET always carries a stamp");
            // Order against everything else that lands values on this node:
            // the record and the local write are one step under the key's
            // slot (`apply_if_newer`). A stale replica write (older than what
            // gossip or a later replica write already applied) is
            // acknowledged but not applied: the sender's write succeeded, it
            // was just overtaken. (`resolve` above already succeeded, so
            // `apply_stamped` cannot answer `None` here.)
            let ok =
                router.apply_stamped(&request.key, value, request.ttl_ms, stamp).unwrap_or(false);
            vec![if ok { SET_OK } else { SET_REJECTED }]
        }
        OP_SET_NX => {
            let value = request.value.expect("SETNX request always carries a value");
            // Clamp a peer-supplied TTL: `Instant::now() + Duration` panics on
            // overflow, so an absurd `ttl_ms` (u64::MAX) from a peer must not
            // reach the store. Ten years is "effectively forever" for a lock.
            let ttl = (request.ttl_ms > 0)
                .then(|| std::time::Duration::from_millis(request.ttl_ms.min(MAX_REMOTE_TTL_MS)));
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
        // An op this node does not know leaves an unknown number of bytes
        // on the wire, so the only safe answer is to drop the connection —
        // which is also what a pre-conditional-ops node did.
        other => anyhow::bail!("unknown op code: {other:#04x}"),
    })
}

/// One entry of a share stream ([`OP_SHARE`]).
#[derive(Debug, Clone)]
pub struct ShareEntry {
    /// The key as it travels the cluster (site-enveloped for a vhost key).
    pub transport_key: String,
    /// The decoded value.
    pub value: Vec<u8>,
    /// Remaining lifetime in milliseconds, `0` = none.
    pub ttl_ms: u64,
    /// The stamp the receiver orders the entry with.
    pub stamp: Stamp,
}

/// Append one entry to a batch payload. Returns `false` (nothing appended)
/// for an entry too large for the format.
fn push_share_entry(out: &mut Vec<u8>, entry: &ShareEntry) -> bool {
    let key = entry.transport_key.as_bytes();
    let (Ok(key_len), Ok(value_len)) = (u32::try_from(key.len()), u32::try_from(entry.value.len()))
    else {
        return false;
    };
    if key_len > MAX_KEY_LEN || value_len > MAX_VALUE_LEN {
        return false;
    }
    out.extend_from_slice(&key_len.to_be_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&value_len.to_be_bytes());
    out.extend_from_slice(&entry.value);
    out.extend_from_slice(&entry.ttl_ms.to_be_bytes());
    out.extend_from_slice(&entry.stamp.write.to_be_bytes());
    out.extend_from_slice(&entry.stamp.node_tag.to_be_bytes());
    true
}

/// Accumulates entries into one batch message.
struct ShareBatch {
    /// `[SHARE_BATCH][len u32][count u32][entries…]`; `len` and `count` are
    /// patched at [`finish`](Self::finish).
    buf: Vec<u8>,
    count: u32,
    bytes: u64,
}

impl ShareBatch {
    fn new() -> Self {
        let mut buf = Vec::with_capacity(SHARE_BATCH_BYTES + 4096);
        buf.push(SHARE_BATCH);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        Self { buf, count: 0, bytes: 0 }
    }

    fn push(&mut self, entry: &ShareEntry) -> bool {
        if !push_share_entry(&mut self.buf, entry) {
            return false;
        }
        self.count += 1;
        self.bytes += entry.value.len() as u64;
        true
    }

    fn is_full(&self) -> bool {
        self.buf.len() >= SHARE_BATCH_BYTES + 9
    }

    /// The finished message, or `None` when empty.
    fn finish(mut self) -> Option<(Vec<u8>, u32, u64)> {
        if self.count == 0 {
            return None;
        }
        let len = u32::try_from(self.buf.len() - 5).ok()?;
        self.buf[1..5].copy_from_slice(&len.to_be_bytes());
        self.buf[5..9].copy_from_slice(&self.count.to_be_bytes());
        Some((self.buf, self.count, self.bytes))
    }
}

/// Parse a batch payload (`[count u32][entries…]`) into its entries.
fn parse_share_batch(mut buf: &[u8]) -> anyhow::Result<Vec<ShareEntry>> {
    let count = take_u32(&mut buf)? as usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let key_len = take_u32(&mut buf)?;
        if key_len > MAX_KEY_LEN {
            anyhow::bail!("key length {key_len} exceeds maximum {MAX_KEY_LEN}");
        }
        let transport_key = String::from_utf8(take_bytes(&mut buf, key_len as usize)?.to_vec())
            .map_err(|_| anyhow::anyhow!("invalid UTF-8 key"))?;
        let value_len = take_u32(&mut buf)?;
        if value_len > MAX_VALUE_LEN {
            anyhow::bail!("value length {value_len} exceeds maximum {MAX_VALUE_LEN}");
        }
        let value = take_bytes(&mut buf, value_len as usize)?.to_vec();
        let ttl_ms = take_u64(&mut buf)?;
        let stamp = Stamp { write: take_u64(&mut buf)?, node_tag: take_u64(&mut buf)? };
        out.push(ShareEntry { transport_key, value, ttl_ms, stamp });
    }
    if !buf.is_empty() {
        anyhow::bail!("trailing bytes in share batch");
    }
    Ok(out)
}

/// How many share entries may wait between the store walk and the socket.
const SHARE_QUEUE: usize = 1024;

/// Answer an [`OP_SHARE`] from `requester`: stream every large value for
/// which the requester is in the replica set, then the end marker.
///
/// The store walk runs on a blocking thread and feeds a bounded channel, so
/// a share of any size holds at most [`SHARE_QUEUE`] values in flight and
/// never a shard lock while the socket is slow. A requester that hangs up
/// closes the channel, which ends the walk.
async fn serve_share(
    stream: &mut TcpStream,
    router: &KvRouter,
    requester: &str,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<()> {
    let Some(source) = router.handoff.clone() else {
        tracing::debug!(requester, "KV share requested but this node has no handoff source");
        return write_message(stream, &[SHARE_END], cipher).await;
    };
    let membership = source.membership_with(requester).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ShareEntry>(SHARE_QUEUE);
    let walker = {
        let router = router.clone();
        let requester = requester.to_string();
        let source = Arc::clone(&source);
        tokio::task::spawn_blocking(move || {
            let mut sent = 0u64;
            let mut closed = false;
            for (site, store) in router.stores() {
                if closed {
                    break;
                }
                // The scan only enumerates candidate keys; the value and
                // its stamp are re-read as a consistent pair per key.
                store.scan_strings_decoded(source.large_min_len(), |key, _value, _ttl| {
                    if closed {
                        return;
                    }
                    let transport_key = crate::kv_handoff::transport_key(&site, key);
                    if !source.replica_includes(&membership, &requester, &transport_key) {
                        return;
                    }
                    let Some(entry) =
                        crate::kv_handoff::stamped_entry(&router, &store, &source, &site, key)
                    else {
                        return;
                    };
                    if tx.blocking_send(entry).is_err() {
                        closed = true;
                    } else {
                        sent += 1;
                    }
                });
            }
            sent
        })
    };
    let mut streamed = 0u64;
    let mut bytes = 0u64;
    let mut batch = ShareBatch::new();
    loop {
        // Fill a batch from what the walk has queued; send it when it is
        // full, or when the queue runs dry so the socket never idles waiting
        // for a batch to fill while the walk is the slow side.
        let entry = match rx.try_recv() {
            Ok(e) => Some(e),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                if let Some((message, count, b)) =
                    std::mem::replace(&mut batch, ShareBatch::new()).finish()
                {
                    send_share(stream, &message, cipher).await?;
                    streamed += u64::from(count);
                    bytes += b;
                }
                rx.recv().await
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => None,
        };
        let Some(entry) = entry else { break };
        if !batch.push(&entry) {
            tracing::warn!(key = %entry.transport_key, "KV share: entry too large for the wire; skipped");
        }
        if batch.is_full()
            && let Some((message, count, b)) =
                std::mem::replace(&mut batch, ShareBatch::new()).finish()
        {
            send_share(stream, &message, cipher).await?;
            streamed += u64::from(count);
            bytes += b;
        }
    }
    if let Some((message, count, b)) = batch.finish() {
        send_share(stream, &message, cipher).await?;
        streamed += u64::from(count);
        bytes += b;
    }
    let _ = walker.await;
    write_message(stream, &[SHARE_END], cipher).await?;
    tracing::info!(requester, keys = streamed, bytes, "KV warm-up share served");
    Ok(())
}

/// Write one share message, bounded by [`SERVER_WRITE_TIMEOUT`].
async fn send_share(
    stream: &mut TcpStream,
    message: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<()> {
    tokio::time::timeout(SERVER_WRITE_TIMEOUT, write_message(stream, message, cipher))
        .await
        .map_err(|_| {
            anyhow::anyhow!("peer did not read the share within {SERVER_WRITE_TIMEOUT:?}")
        })?
}

/// Tag an EOF on the first byte of a request as a clean close.
fn mark_clean_close(err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
        anyhow::Error::from(err).context(CLEAN_CLOSE_MARKER)
    } else {
        err.into()
    }
}

/// Read and validate one request from the stream.
///
/// In plaintext mode the fields are read incrementally off the wire; in
/// encrypted mode one sealed frame is read and parsed in memory. EOF before
/// the first byte is reported with [`CLEAN_CLOSE_MARKER`] so the connection
/// loop can tell "client hung up between requests" from a truncated request.
async fn read_request(
    stream: &mut TcpStream,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<Request> {
    if let Some(cipher) = cipher {
        let frame_len = stream.read_u32().await.map_err(mark_clean_close)?;
        let frame = read_frame_body(stream, cipher, frame_len).await?;
        return parse_request(&frame);
    }

    // Plaintext: incremental reads, byte-compatible with older nodes.
    let op = stream.read_u8().await.map_err(mark_clean_close)?;

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
    let ttl_ms = if op_has_ttl(op) { stream.read_u64().await? } else { 0 };
    let stamp = if op == OP_SET_V2 {
        let write = stream.read_u64().await?;
        let node_tag = stream.read_u64().await?;
        Some(Stamp { write, node_tag })
    } else {
        None
    };

    Ok(Request { op, key, value, ttl_ms, stamp })
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
    let ttl_ms = if op_has_ttl(op) { take_u64(&mut buf)? } else { 0 };
    let stamp = if op == OP_SET_V2 {
        Some(Stamp { write: take_u64(&mut buf)?, node_tag: take_u64(&mut buf)? })
    } else {
        None
    };

    Ok(Request { op, key, value, ttl_ms, stamp })
}

/// Take a big-endian u64 from the front of `buf`.
fn take_u64(buf: &mut &[u8]) -> anyhow::Result<u64> {
    let bytes: [u8; 8] =
        take_bytes(buf, 8)?.try_into().expect("take_bytes(8) returns exactly 8 bytes");
    Ok(u64::from_be_bytes(bytes))
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
    read_frame_body(stream, cipher, frame_len).await
}

/// Read the body of a sealed frame whose length prefix has already been read,
/// and decrypt it.
async fn read_frame_body(
    stream: &mut TcpStream,
    cipher: &ClusterCipher,
    frame_len: u32,
) -> anyhow::Result<Vec<u8>> {
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

// ─────────────────────────────────────────────────────────────────────
// Client side
// ─────────────────────────────────────────────────────────────────────

/// The shape of the response a request expects, so the plaintext reader
/// knows how many bytes make up one response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseKind {
    /// One status byte (SET, SETNX, DEL-IF-EQ).
    Status,
    /// `[value_len: u32][value]` or the not-found sentinel (GET).
    Value,
}

/// Dial `addr` within [`CONNECT_TIMEOUT`].
async fn connect(addr: SocketAddr) -> anyhow::Result<TcpStream> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("connect to {addr} timed out after {CONNECT_TIMEOUT:?}"))??;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// One request/response exchange on an open connection, bounded by
/// [`REQUEST_TIMEOUT`]. Returns the plaintext response message.
async fn exchange_on(
    stream: &mut TcpStream,
    message: &[u8],
    kind: ResponseKind,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<Vec<u8>> {
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        write_message(stream, message, cipher).await?;
        if let Some(cipher) = cipher {
            return read_frame(stream, cipher).await;
        }
        match kind {
            ResponseKind::Status => Ok(vec![stream.read_u8().await?]),
            ResponseKind::Value => {
                let value_len = stream.read_u32().await?;
                let mut response = value_len.to_be_bytes().to_vec();
                if value_len == NOT_FOUND_SENTINEL {
                    return Ok(response);
                }
                // The encrypted path is bounded by MAX_FRAME_LEN before anything
                // is allocated; the plaintext path had no bound at all — which
                // is exactly the deployment with no `[cluster] secret`, where
                // the responder is not authenticated either. Bound it the same
                // way the server bounds an inbound value, then read in chunks.
                if value_len > MAX_VALUE_LEN {
                    anyhow::bail!(
                        "peer advertised value length {value_len} exceeding maximum {MAX_VALUE_LEN}"
                    );
                }
                response.extend(read_exact_chunked(stream, value_len as usize).await?);
                Ok(response)
            }
        }
    })
    .await
    .map_err(|elapsed| {
        // Keep `Elapsed` as the source so the pool can tell a stalled peer
        // (do not retry) from a closed pooled connection (retry once).
        anyhow::Error::new(elapsed)
            .context(format!("KV data plane exchange timed out after {REQUEST_TIMEOUT:?}"))
    })?
}

/// Whether an exchange error is the [`REQUEST_TIMEOUT`] firing.
fn is_exchange_timeout(err: &anyhow::Error) -> bool {
    err.downcast_ref::<tokio::time::error::Elapsed>().is_some()
}

/// Decode a GET response message into the value, or `None` for the sentinel.
fn decode_get_response(response: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let mut buf = response;
    let value_len = take_u32(&mut buf)?;
    if value_len == NOT_FOUND_SENTINEL {
        return Ok(None);
    }
    Ok(Some(take_bytes(&mut buf, value_len as usize)?.to_vec()))
}

/// Idle connections and breaker state for one peer.
#[derive(Default)]
struct PeerState {
    /// Connections that completed their last exchange cleanly, ready to reuse.
    idle: Vec<TcpStream>,
    /// Set after a failed connect; dialling is skipped until it passes.
    unreachable_until: Option<Instant>,
}

/// A client for peers' KV data planes with per-peer connection reuse and a
/// per-peer connect breaker.
///
/// One instance is shared by everything on a node that talks to peers
/// (replica writes, read-through fetches, routed conditional writes). Each
/// exchange checks out one connection, so concurrency per peer is bounded by
/// the number of concurrent callers, and idle connections beyond
/// [`POOL_IDLE_PER_PEER`] are closed rather than kept.
pub struct DataPlaneClient {
    cipher: Option<Arc<ClusterCipher>>,
    peers: Mutex<HashMap<SocketAddr, PeerState>>,
}

impl std::fmt::Debug for DataPlaneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneClient")
            .field("encrypted", &self.cipher.is_some())
            .finish_non_exhaustive()
    }
}

impl DataPlaneClient {
    /// A client sealing every exchange with `cipher` (or plaintext when
    /// `None`); must match the peers' listeners.
    #[must_use]
    pub fn new(cipher: Option<Arc<ClusterCipher>>) -> Self {
        Self { cipher, peers: Mutex::new(HashMap::new()) }
    }

    /// The cipher this client seals with, for callers that still use the
    /// one-shot functions.
    #[must_use]
    pub fn cipher(&self) -> Option<&ClusterCipher> {
        self.cipher.as_deref()
    }

    /// Take an idle connection to `addr` out of the pool, if any.
    fn checkout(&self, addr: SocketAddr) -> anyhow::Result<Option<TcpStream>> {
        let mut peers = self.peers.lock();
        let state = peers.entry(addr).or_default();
        if let Some(until) = state.unreachable_until {
            if Instant::now() < until {
                anyhow::bail!("peer {addr} is in connect-failure cooldown");
            }
            state.unreachable_until = None;
        }
        Ok(state.idle.pop())
    }

    /// Return a healthy connection to the pool, closing it if the pool is full.
    fn checkin(&self, addr: SocketAddr, stream: TcpStream) {
        let mut peers = self.peers.lock();
        let state = peers.entry(addr).or_default();
        if state.idle.len() < POOL_IDLE_PER_PEER {
            state.idle.push(stream);
        }
    }

    /// Record a failed connect so the next [`PEER_BREAKER_COOLDOWN`] of calls
    /// fail fast instead of each paying [`CONNECT_TIMEOUT`].
    fn trip_breaker(&self, addr: SocketAddr) {
        let mut peers = self.peers.lock();
        let state = peers.entry(addr).or_default();
        state.unreachable_until = Some(Instant::now() + PEER_BREAKER_COOLDOWN);
        // Whatever was idle is suspect too.
        state.idle.clear();
    }

    /// Number of idle connections currently pooled for `addr` (observability
    /// and tests).
    #[must_use]
    pub fn idle_connections(&self, addr: SocketAddr) -> usize {
        self.peers.lock().get(&addr).map_or(0, |s| s.idle.len())
    }

    /// Run one exchange against `addr`, reusing a pooled connection when one
    /// exists.
    ///
    /// A reused connection may have been closed by the peer since its last
    /// use (an idle timeout, or a pre-pooling server that closes after one
    /// response), so an I/O failure on a reused connection is retried
    /// **once** on a fresh one. A *timeout* on a reused connection is not
    /// retried: the peer is stalled, and a second attempt would only double
    /// the wait. The retry can duplicate a request whose first attempt
    /// reached the peer and died on the response leg; for a SET that is
    /// idempotent, and for a conditional write the second answer is the same
    /// "not applied" the caller would have treated as unknown.
    async fn exchange(
        &self,
        addr: SocketAddr,
        message: &[u8],
        kind: ResponseKind,
    ) -> anyhow::Result<Vec<u8>> {
        let cipher = self.cipher.as_deref();
        if let Some(mut stream) = self.checkout(addr)? {
            match exchange_on(&mut stream, message, kind, cipher).await {
                Ok(response) => {
                    self.checkin(addr, stream);
                    return Ok(response);
                }
                Err(e) if is_exchange_timeout(&e) => return Err(e),
                Err(e) => {
                    tracing::debug!(%addr, %e, "pooled KV data plane connection failed; redialling");
                }
            }
        }
        let mut stream = match connect(addr).await {
            Ok(stream) => stream,
            Err(e) => {
                self.trip_breaker(addr);
                return Err(e);
            }
        };
        let response = exchange_on(&mut stream, message, kind, cipher).await?;
        self.checkin(addr, stream);
        Ok(response)
    }

    /// Fetch `key` from the peer at `addr`; `None` when it has no such key.
    ///
    /// # Errors
    ///
    /// Connect/I-O failure, timeout, or a cipher mismatch.
    pub async fn fetch(&self, addr: SocketAddr, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let response = self.exchange(addr, &encode_get_request(key)?, ResponseKind::Value).await?;
        decode_get_response(&response)
    }

    /// Store `value` under `key` on the peer at `addr`, returning whether it
    /// accepted the write.
    ///
    /// # Errors
    ///
    /// Connect/I-O failure, timeout, or a cipher mismatch.
    pub async fn store(&self, addr: SocketAddr, key: &str, value: &[u8]) -> anyhow::Result<bool> {
        let response =
            self.exchange(addr, &encode_set_request(key, value)?, ResponseKind::Status).await?;
        Ok(take_u8(&mut response.as_slice())? == SET_OK)
    }

    /// Store `value` under `key` on the peer at `addr` with its remaining
    /// TTL and the origin's stamp ([`OP_SET_V2`]), returning whether the peer
    /// accepted it. An accepted-but-stale write (the peer already holds a
    /// newer stamp) also answers `true`.
    ///
    /// A peer predating the stamped op drops the connection on it; that is
    /// reported as an error and counted as a failed replica write.
    ///
    /// # Errors
    ///
    /// Connect/I-O failure, timeout, a cipher mismatch, or an old peer.
    pub async fn store_stamped(
        &self,
        addr: SocketAddr,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
        stamp: Stamp,
    ) -> anyhow::Result<bool> {
        let ttl_ms = ttl.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1));
        let response = self
            .exchange(
                addr,
                &encode_set_v2_request(key, value, ttl_ms, stamp)?,
                ResponseKind::Status,
            )
            .await?;
        Ok(take_u8(&mut response.as_slice())? == SET_OK)
    }

    /// Pull this node's share of large values from the peer at `addr`
    /// ([`OP_SHARE`]): every entry the peer streams is handed to `apply`,
    /// one at a time, until the end marker or `deadline`. Returns
    /// `(entries, value bytes)` received. Uses its own connection (a share is
    /// one long exchange, not a pooled request).
    ///
    /// # Errors
    ///
    /// Connect/I-O failure, the deadline passing mid-stream (entries applied
    /// so far stay applied), a cipher mismatch, or a peer predating the op
    /// (it drops the connection).
    pub async fn share_from(
        &self,
        addr: SocketAddr,
        requester_id: &str,
        deadline: Instant,
        mut apply: impl FnMut(ShareEntry),
    ) -> anyhow::Result<(u64, u64)> {
        let cipher = self.cipher.as_deref();
        let request = encode_get_request(requester_id)?;
        let mut message = request;
        message[0] = OP_SHARE;
        let mut stream = connect(addr).await.inspect_err(|_| self.trip_breaker(addr))?;
        tokio::time::timeout_at(deadline, async {
            write_message(&mut stream, &message, cipher).await?;
            let mut entries = 0u64;
            let mut bytes = 0u64;
            loop {
                // One message per batch: `[flag][len u32][payload]`, read
                // whole and parsed in memory on both the sealed and the
                // plaintext path.
                let (flag, payload) = match cipher {
                    Some(cipher) => {
                        let frame = read_frame(&mut stream, cipher).await?;
                        let (flag, rest) = frame
                            .split_first()
                            .ok_or_else(|| anyhow::anyhow!("empty share frame"))?;
                        if *flag == SHARE_END {
                            break;
                        }
                        let mut rest = rest;
                        let len = take_u32(&mut rest)?;
                        (*flag, take_bytes(&mut rest, len as usize)?.to_vec())
                    }
                    None => {
                        let flag = stream.read_u8().await?;
                        if flag == SHARE_END {
                            break;
                        }
                        let len = stream.read_u32().await?;
                        if len > MAX_SHARE_BATCH {
                            anyhow::bail!("share batch of {len} bytes exceeds {MAX_SHARE_BATCH}");
                        }
                        (flag, read_exact_chunked(&mut stream, len as usize).await?)
                    }
                };
                if flag != SHARE_BATCH {
                    anyhow::bail!("unknown share flag {flag:#04x}");
                }
                for entry in parse_share_batch(&payload)? {
                    entries += 1;
                    bytes += entry.value.len() as u64;
                    apply(entry);
                }
            }
            Ok((entries, bytes))
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("KV warm-up pull from {addr} did not finish before the deadline")
        })?
    }

    /// Owner-arbitrated `SETNX`; see [`set_nx_remote`] for the contract.
    ///
    /// # Errors
    ///
    /// As [`set_nx_remote`].
    pub async fn set_nx(
        &self,
        addr: SocketAddr,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> anyhow::Result<SetNxOutcome> {
        let ttl_ms = ttl.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1));
        let response = self
            .exchange(addr, &encode_set_nx_request(key, value, ttl_ms)?, ResponseKind::Status)
            .await?;
        decode_set_nx_status(addr, key, take_u8(&mut response.as_slice())?)
    }

    /// Owner-arbitrated compare-and-delete; see [`del_if_eq_remote`].
    ///
    /// # Errors
    ///
    /// As [`del_if_eq_remote`].
    pub async fn del_if_eq(
        &self,
        addr: SocketAddr,
        key: &str,
        token: &[u8],
    ) -> anyhow::Result<bool> {
        let response = self
            .exchange(addr, &encode_del_if_eq_request(key, token)?, ResponseKind::Status)
            .await?;
        decode_del_if_eq_status(addr, key, take_u8(&mut response.as_slice())?)
    }
}

/// Fetch a value from a remote node's KV data plane over a one-shot
/// connection.
///
/// Dials `addr`, sends a GET request, reads the response and closes. Returns
/// `None` if the remote node does not have the key. `cipher` must match the
/// remote node's setting (both `Some` with the same secret, or both `None`).
/// Nodes use a pooled [`DataPlaneClient`] instead; this is the
/// dependency-free form for tools and tests.
///
/// # Errors
///
/// Returns an error on connection or I/O failure, a timeout, or when the
/// peer's encryption setting or secret does not match.
pub async fn fetch_remote(
    addr: SocketAddr,
    key: &str,
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut stream = connect(addr).await?;
    let response =
        exchange_on(&mut stream, &encode_get_request(key)?, ResponseKind::Value, cipher).await?;
    decode_get_response(&response)
}

/// Store a value on a remote node's KV data plane over a one-shot
/// connection. See [`fetch_remote`].
///
/// # Errors
///
/// Returns an error on connection or I/O failure, a timeout, or when the
/// peer's encryption setting or secret does not match.
pub async fn store_remote(
    addr: SocketAddr,
    key: &str,
    value: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<bool> {
    let mut stream = connect(addr).await?;
    let response =
        exchange_on(&mut stream, &encode_set_request(key, value)?, ResponseKind::Status, cipher)
            .await?;
    Ok(take_u8(&mut response.as_slice())? == SET_OK)
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
/// peer that cannot route the key. A connect/refused error means nothing was
/// written; an error after the request was sent (timeout, reset) leaves the
/// outcome **unknown**.
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
    decode_set_nx_status(addr, key, status)
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
    decode_del_if_eq_status(addr, key, status)
}

/// Map a SETNX status byte to the owner's decision.
fn decode_set_nx_status(addr: SocketAddr, key: &str, status: u8) -> anyhow::Result<SetNxOutcome> {
    match status {
        SET_OK => Ok(SetNxOutcome::Inserted),
        COND_NOT_APPLIED => Ok(SetNxOutcome::Exists),
        SET_REJECTED => Ok(SetNxOutcome::OutOfMemory),
        COND_UNROUTABLE => anyhow::bail!("owner {addr} cannot route key {key:?}"),
        other => anyhow::bail!("owner {addr} answered SETNX with unknown status {other:#04x}"),
    }
}

/// Map a DEL-IF-EQ status byte to whether the owner deleted the key.
fn decode_del_if_eq_status(addr: SocketAddr, key: &str, status: u8) -> anyhow::Result<bool> {
    match status {
        SET_OK => Ok(true),
        COND_NOT_APPLIED => Ok(false),
        COND_UNROUTABLE => anyhow::bail!("owner {addr} cannot route key {key:?}"),
        other => anyhow::bail!("owner {addr} answered DEL-IF-EQ with unknown status {other:#04x}"),
    }
}

/// Send one conditional-write request over a one-shot connection and read
/// its one-byte status.
async fn conditional_roundtrip(
    addr: SocketAddr,
    message: &[u8],
    cipher: Option<&ClusterCipher>,
) -> anyhow::Result<u8> {
    let mut stream = connect(addr).await?;
    let response = exchange_on(&mut stream, message, ResponseKind::Status, cipher).await?;
    take_u8(&mut response.as_slice())
}

/// Encode a plaintext SETNX request message.
fn encode_set_nx_request(key: &str, value: &[u8], ttl_ms: u64) -> anyhow::Result<Vec<u8>> {
    let mut message = encode_set_request(key, value)?;
    message[0] = OP_SET_NX;
    message.extend_from_slice(&ttl_ms.to_be_bytes());
    Ok(message)
}

/// Encode a plaintext stamped SET request message ([`OP_SET_V2`]).
fn encode_set_v2_request(
    key: &str,
    value: &[u8],
    ttl_ms: u64,
    stamp: Stamp,
) -> anyhow::Result<Vec<u8>> {
    let mut message = encode_set_request(key, value)?;
    message[0] = OP_SET_V2;
    message.extend_from_slice(&ttl_ms.to_be_bytes());
    message.extend_from_slice(&stamp.write.to_be_bytes());
    message.extend_from_slice(&stamp.node_tag.to_be_bytes());
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

    /// The handoff reads a value and its stamp as one pair: with a writer
    /// racing through `apply_stamped` (stamp n, value "n"), a reader must
    /// never observe a value from a different write than the stamp.
    #[test]
    fn read_stamped_never_pairs_a_value_with_another_writes_stamp() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let applied = crate::clustered_store::new_applied_write_map();
        let router = KvRouter::global(Arc::clone(&store)).with_applied(applied);
        let key = "race";
        let writer = {
            let router = router.clone();
            std::thread::spawn(move || {
                for n in 1..=20_000u64 {
                    let stamp = Stamp { write: n, node_tag: 1 };
                    let _ = router.apply_stamped(key, n.to_string().into_bytes(), 0, stamp);
                }
            })
        };
        let mut seen = 0u64;
        while !writer.is_finished() {
            if let Some(((value, _ttl), stamp)) =
                router.read_stamped(key, || store.get_with_ttl(key))
            {
                let v: u64 = std::str::from_utf8(&value).unwrap().parse().unwrap();
                let s = stamp.expect("a recorded write has a stamp").write;
                assert_eq!(v, s, "value {v} observed under stamp {s}");
                seen += 1;
            }
        }
        writer.join().unwrap();
        assert!(seen > 0, "the reader must have overlapped the writer");
    }

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

    /// A peer-supplied `ttl_ms = u64::MAX` is clamped, not an `Instant`
    /// overflow panic on the owner.
    #[tokio::test]
    async fn set_nx_remote_clamps_an_absurd_ttl() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let addr = spawn_multi_handler(Arc::clone(&store), None).await;
        let huge = Some(std::time::Duration::MAX); // encodes as ttl_ms = u64::MAX
        assert_eq!(
            set_nx_remote(addr, "lock", b"A", huge, None).await.unwrap(),
            SetNxOutcome::Inserted
        );
        let pttl = store.pttl("lock").expect("owner holds the lock");
        assert!(pttl > 0 && pttl <= i64::try_from(MAX_REMOTE_TTL_MS).unwrap(), "pttl {pttl}");
        // The handler task is still alive and answering.
        assert_eq!(
            set_nx_remote(addr, "lock", b"B", None, None).await.unwrap(),
            SetNxOutcome::Exists
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

    // ── Stamped SET (OP_SET_V2) ───────────────────────────────────────

    #[test]
    fn parse_request_roundtrips_stamped_set() {
        let stamp = Stamp { write: 0x1234_5678_9abc_def0, node_tag: 42 };
        let msg = encode_set_v2_request("k", b"v", 1500, stamp).unwrap();
        let parsed = parse_request(&msg).unwrap();
        assert_eq!(parsed.op, OP_SET_V2);
        assert_eq!(parsed.key, "k");
        assert_eq!(parsed.value.as_deref(), Some(b"v".as_slice()));
        assert_eq!(parsed.ttl_ms, 1500);
        assert_eq!(parsed.stamp, Some(stamp));
        // Missing any trailer byte is truncated, not "no stamp".
        assert!(parse_request(&msg[..msg.len() - 1]).is_err());
        // The largest stamped message still fits the sealed-frame cap.
        assert!(MAX_FRAME_LEN as usize >= 1 + 4 + 4 + 8 + 16 + SEAL_OVERHEAD);
    }

    /// A stamped SET lands with its TTL (F-9: the original SET carried none,
    /// so a replica's copy lived forever).
    #[tokio::test]
    async fn stamped_set_carries_the_ttl_and_stamp() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let applied = crate::clustered_store::new_applied_write_map();
        let addr = spawn_router_handler(
            KvRouter::global(Arc::clone(&store)).with_applied(Arc::clone(&applied)),
            None,
        )
        .await;
        let client = DataPlaneClient::new(None);
        let clock = crate::gossip_kv::StampClock::new("origin");

        let stamp = clock.next();
        assert!(
            client
                .store_stamped(addr, "big", b"value", Some(Duration::from_millis(300)), stamp)
                .await
                .unwrap()
        );
        assert_eq!(store.get("big").as_deref(), Some(b"value".as_slice()));
        let pttl = store.pttl("big").expect("replica holds the key");
        assert!((1..=300).contains(&pttl), "TTL applied on the replica, got {pttl}");
        assert_eq!(*applied.get("big").unwrap(), stamp, "the replica records the stamp");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(store.get("big"), None, "the replica copy expires");
    }

    /// A replica applies stamped SETs in stamp order, not arrival order: a
    /// late older write is acknowledged but does not regress the value.
    #[tokio::test]
    async fn stale_stamped_set_is_acknowledged_but_not_applied() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let applied = crate::clustered_store::new_applied_write_map();
        let addr =
            spawn_router_handler(KvRouter::global(Arc::clone(&store)).with_applied(applied), None)
                .await;
        let client = DataPlaneClient::new(None);
        let clock = crate::gossip_kv::StampClock::new("origin");
        let older = clock.next();
        let newer = clock.next();

        assert!(client.store_stamped(addr, "k", b"new", None, newer).await.unwrap());
        assert!(client.store_stamped(addr, "k", b"old", None, older).await.unwrap());
        assert_eq!(store.get("k").as_deref(), Some(b"new".as_slice()));
        // The same stamp again (a retry) is a no-op too.
        assert!(client.store_stamped(addr, "k", b"dup", None, newer).await.unwrap());
        assert_eq!(store.get("k").as_deref(), Some(b"new".as_slice()));
    }

    /// Without an applied map (tools, tests) every stamped SET applies, and
    /// the pre-stamp OP_SET is still served for an old sender.
    #[tokio::test]
    async fn stamped_set_without_applied_map_and_legacy_set_both_apply() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let addr = spawn_multi_handler(Arc::clone(&store), None).await;
        let client = DataPlaneClient::new(None);
        let clock = crate::gossip_kv::StampClock::new("origin");
        let newer = clock.next();
        let older = Stamp { write: newer.write - 1, node_tag: newer.node_tag };
        assert!(client.store_stamped(addr, "k", b"new", None, newer).await.unwrap());
        assert!(client.store_stamped(addr, "k", b"old", None, older).await.unwrap());
        assert_eq!(store.get("k").as_deref(), Some(b"old".as_slice()), "ungated: last arrival");
        assert!(client.store(addr, "k", b"legacy").await.unwrap());
        assert_eq!(store.get("k").as_deref(), Some(b"legacy".as_slice()));
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

#[cfg(test)]
mod connection_tests {
    //! Connection reuse, timeouts and the per-peer breaker (stress finding
    //! F-13: a frozen peer used to pin one half-open socket per write).

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A listener that counts accepted connections and serves each one with
    /// the production multi-request loop.
    async fn counting_server(store: Arc<Store>) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, &KvRouter::global(store), None).await;
                });
            }
        });
        (addr, accepted)
    }

    /// Several exchanges through the pooled client ride one TCP connection,
    /// and the connection goes back to the pool between them.
    #[tokio::test]
    async fn pooled_client_reuses_one_connection() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let (addr, accepted) = counting_server(Arc::clone(&store)).await;
        let client = DataPlaneClient::new(None);

        for i in 0..20 {
            assert!(client.store(addr, &format!("k{i}"), b"v").await.unwrap());
            assert_eq!(client.fetch(addr, &format!("k{i}")).await.unwrap(), Some(b"v".to_vec()));
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "40 exchanges must share one connection");
        assert_eq!(client.idle_connections(addr), 1);
    }

    /// A server that closes after one response — a node predating connection
    /// reuse — is transparently redialled by the pool.
    #[tokio::test]
    async fn pooled_client_tolerates_a_one_shot_server() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_store = Arc::clone(&store);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                let store = Arc::clone(&server_store);
                tokio::spawn(async move {
                    // One request, one response, close — the old behaviour.
                    let request = read_request(&mut stream, None).await.unwrap();
                    let response = serve_request(request, &KvRouter::global(store)).unwrap();
                    write_message(&mut stream, &response, None).await.unwrap();
                });
            }
        });

        let client = DataPlaneClient::new(None);
        assert!(client.store(addr, "a", b"1").await.unwrap());
        // The pooled connection is dead by now; the second call must still succeed.
        assert_eq!(client.fetch(addr, "a").await.unwrap(), Some(b"1".to_vec()));
        assert!(client.store(addr, "b", b"2").await.unwrap());
        assert_eq!(store.get("b").as_deref(), Some(&b"2"[..]));
    }

    // The multi-second timeouts below are exercised under tokio's paused
    // clock, which auto-advances to the next timer whenever the runtime would
    // otherwise block. That is exactly right for a stall (nothing will ever
    // arrive, so jump to the deadline) and exactly wrong for real loopback
    // I/O: the clock is advanced after a single zero-length poll of the I/O
    // driver, so a connect whose readiness the kernel has not delivered yet
    // "times out after 2s" in microseconds. Linux reports a loopback connect
    // as ready on that first poll; macOS does not always, which is how
    // `stalled_pooled_connection_is_not_retried` failed on the macOS CI leg
    // with `connect to 127.0.0.1:49409 timed out after 2s` after 0.010 s.
    // So every test here does its real I/O (connect, accept, the first
    // exchange) with the clock running and calls `tokio::time::pause()` only
    // once the next thing to wait for is the stall itself.

    /// A peer that accepts and then never answers costs one bounded timeout,
    /// after which the socket is closed — not a socket parked forever.
    #[tokio::test]
    async fn frozen_peer_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept and hold the connection open without reading or writing.
        let accepted = Arc::new(tokio::sync::Notify::new());
        let frozen = tokio::spawn({
            let accepted = Arc::clone(&accepted);
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                accepted.notify_one();
                std::future::pending::<()>().await;
                drop(stream);
            }
        });

        let client = Arc::new(DataPlaneClient::new(None));
        let exchange = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.store(addr, "k", b"v").await }
        });
        // The connection is established once the peer has accepted it; from
        // here on the only thing left to wait for is the request timeout.
        accepted.notified().await;
        tokio::time::pause();
        let err = exchange.await.unwrap().unwrap_err();
        assert!(is_exchange_timeout(&err), "the request, not the connect, timed out: {err}");
        assert_eq!(client.idle_connections(addr), 0, "a failed connection is never pooled");
        frozen.abort();
    }

    /// A peer that answered once and then froze costs the pooled client one
    /// request timeout, not a timeout plus a redial and a second one.
    #[tokio::test]
    async fn stalled_pooled_connection_is_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Serve exactly one request, then stop reading forever.
            let request = read_request(&mut stream, None).await.unwrap();
            let response = serve_request(request, &KvRouter::global(store)).unwrap();
            write_message(&mut stream, &response, None).await.unwrap();
            std::future::pending::<()>().await;
        });

        let client = DataPlaneClient::new(None);
        assert!(client.store(addr, "k", b"v").await.unwrap());
        assert_eq!(client.idle_connections(addr), 1);

        // The pooled connection is up; the second exchange only ever waits
        // on a response that never comes.
        tokio::time::pause();
        let started = Instant::now();
        let err = client.store(addr, "k", b"v2").await.unwrap_err();
        assert!(is_exchange_timeout(&err), "got: {err}");
        let waited = started.elapsed();
        assert!(
            waited >= REQUEST_TIMEOUT && waited < REQUEST_TIMEOUT * 2,
            "one timeout, no redial: waited {waited:?}"
        );
        server.abort();
    }

    /// After a refused connect the peer is not redialled for the cooldown,
    /// so a dead peer does not charge every caller a connect attempt.
    #[tokio::test]
    async fn breaker_skips_dial_after_connect_failure() {
        // Bind then drop: the port is closed and refuses connections.
        let addr = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let client = DataPlaneClient::new(None);
        let first = client.fetch(addr, "k").await.unwrap_err();
        assert!(!first.to_string().contains("cooldown"), "first failure is the real connect error");
        let second = client.fetch(addr, "k").await.unwrap_err();
        assert!(second.to_string().contains("cooldown"), "got: {second}");
        // Once the cooldown passes, dialling resumes (and fails for real
        // again). Only the wait itself runs on the paused clock: the refused
        // connect that follows is real I/O again.
        tokio::time::pause();
        tokio::time::sleep(PEER_BREAKER_COOLDOWN + Duration::from_millis(1)).await;
        tokio::time::resume();
        let third = client.fetch(addr, "k").await.unwrap_err();
        assert!(!third.to_string().contains("cooldown"), "got: {third}");
    }

    /// The server drops a connection that sends nothing for the idle timeout,
    /// bounding what a stalled client can hold.
    #[tokio::test]
    async fn server_drops_idle_connections() {
        let store = Store::new(ephpm_kv::store::StoreConfig::default());
        let (addr, accepted) = counting_server(store).await;
        let mut client = TcpStream::connect(addr).await.unwrap();
        // Let the server pick the connection up with the clock running, so
        // its idle timer exists before the clock starts auto-advancing.
        while accepted.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::pause();
        // Say nothing; the server must hang up on its own.
        let mut byte = [0u8; 1];
        let n = tokio::time::timeout(SERVER_IDLE_TIMEOUT * 2, client.read(&mut byte))
            .await
            .expect("the server must close within the idle timeout")
            .unwrap();
        assert_eq!(n, 0, "EOF from the server, not data");
    }

    /// A clean close between requests is not an error, a truncated request is.
    #[test]
    fn clean_close_is_only_eof_before_a_request() {
        let eof = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof");
        assert!(is_clean_close(&mark_clean_close(eof)));
        let truncated: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof").into();
        assert!(!is_clean_close(&truncated));
        let other = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "rst");
        assert!(!is_clean_close(&mark_clean_close(other)));
    }
}

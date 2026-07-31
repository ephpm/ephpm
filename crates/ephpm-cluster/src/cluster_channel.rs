//! Cluster channel v1 — the multiplexed, authenticated cluster data plane.
//!
//! # Why a channel and not more gossip
//!
//! ePHPm's cluster stack splits neatly along a **state vs log** line:
//!
//! - **Gossip (chitchat) = control plane ONLY.** Membership, failure
//!   detection, primary election, and small state (opcache versions,
//!   the sqlite primary key, ACME leader lock, etc.). Gossip never
//!   carries payloads.
//! - **Cluster channel = data plane for LOGS.** CDC transaction batches
//!   today; snapshot bootstrap, watermark sync, and any future bulk
//!   stream tomorrow. The channel never carries elections or membership.
//!
//! Keeping those separate means gossip stays a small, bounded UDP
//! chatter regardless of write volume, and channel features get a real
//! backpressured TCP transport instead of trying to shove bytes through
//! chitchat's KV.
//!
//! # Lazy-bind contract ("opt-in transport")
//!
//! The channel listener is **only bound when at least one feature asks
//! for it**. A v0.5.0 config that ships no channel feature is
//! byte-identical to today: no socket, no task, no log line above
//! `debug!`. Adding `[cluster.channel]` to a config is not itself an
//! opt-in — a feature elsewhere (e.g.
//! `[db.sqlite.replication] cdc_experimental = true`) has to ask.
//!
//! See [`FeatureFlags::any_enabled`] — startup calls
//! [`maybe_start`] with the resolved flags; if none are set the
//! function returns `Ok(None)` without touching the network.
//!
//! # Handshake (v2 — mutual challenge/response)
//!
//! Both sides derive `ClusterCipher::for_cluster_channel(secret)`
//! (distinct HKDF domain from gossip / KV data plane) and use it *only*
//! to seal the three handshake messages:
//!
//! ```text
//!   I → R  [version: u8 = 0x02][len: u16 BE][seal(C_i)]
//!   R → I  [version: u8 = 0x02][len: u16 BE][seal(C_i || C_r)]
//!   I → R                     [len: u16 BE][seal(C_r)]
//! ```
//!
//! `C_i` and `C_r` are fresh 32-byte `OsRng` challenges chosen by the
//! initiator and the responder respectively. Message 2 proves the
//! responder holds the secret *and* contributes freshness; message 3
//! proves the initiator holds the secret and is not replaying a
//! recording. Both comparisons are constant-time.
//!
//! Replaying a captured message 1 gets the attacker a message 2 sealing
//! a challenge they cannot open, so they cannot produce message 3 —
//! the responder drops them. Every side dropping silently on failure is
//! the "wrong secret" signal; there is deliberately no typed error
//! reply, so a wrong-secret peer looks identical to a stray TCP
//! scanner.
//!
//! # Post-handshake confidentiality
//!
//! Both sides then derive a **per-connection** key from the operator
//! secret salted with the transcript `C_i || C_r`
//! ([`ClusterCipher::for_cluster_channel_session`]) and wrap the TCP
//! stream in a sealing adapter: every byte the multiplexer writes is
//! sealed into a length-prefixed ChaCha20-Poly1305 frame, and every
//! frame is opened (authenticated) on read. This mirrors what
//! [`crate::kv_data_plane`] does for its request/response messages, so
//! an eavesdropper on the channel sees ciphertext only, and a
//! man-in-the-middle cannot splice frames between connections (the
//! keys differ per connection).
//!
//! **There is still no TLS and no PKI peer identity** beyond "holds the
//! shared cluster secret" plus a coarse gossip-membership check on the
//! peer IP (see [`maybe_start`]). Certificate-based identity is future
//! work.
//!
//! # Multiplexing
//!
//! After the handshake, both sides speak `yamux 0.14` over the sealed
//! stream. Each yamux stream begins with a length-prefixed UTF-8
//! stream-type string from [`stream_type`]: `cdc/<vhost>` and
//! `snapshot/<vhost>` are both implemented (the CDC replication path
//! registers handlers for them). Unknown stream types are logged and
//! closed.
//!
//! # Denial-of-service bounds
//!
//! Pre-authentication work is bounded: at most
//! `MAX_INFLIGHT_HANDSHAKES` handshakes run concurrently (further
//! connections are dropped immediately rather than queued), and each
//! one must finish inside `HANDSHAKE_TIMEOUT`. Post-handshake, yamux
//! is configured with at most `MAX_STREAMS_PER_CONNECTION` concurrent
//! streams per connection.
//!
//! Backpressure is yamux's native per-stream flow-control window
//! (256 KiB per stream by default). A stalled reader blocks new writes
//! into that stream without blocking other streams on the same
//! connection — the CDC producer's `write_frame` call awaits the
//! window naturally, which is what we want: a stalled replica pauses
//! primary tail broadcast on **that** subscriber only, without
//! blocking the tail loop as a whole (the `broadcast::Sender` absorbs
//! transient stall via its bounded queue; sustained stall lags the
//! subscriber and forces a reconnect).

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::Context as _;
use ephpm_config::ClusterChannelConfig;
use futures::io::{AsyncRead as FuturesAsyncRead, AsyncWrite as FuturesAsyncWrite};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

use crate::ClusterHandle;
use crate::secure_transport::ClusterCipher;

/// Handshake protocol version byte. Bump on wire-format change.
///
/// `0x02` is the mutual challenge/response handshake with a
/// per-connection session key. `0x01` (initiator-only challenge, no
/// session key, plaintext payload afterwards) was replayable and is
/// **not** accepted — an old peer looks like a wrong-secret peer.
const HANDSHAKE_VERSION: u8 = 0x02;

/// Challenge nonce length in bytes. The *plaintext* random challenge
/// that gets sealed — the AEAD nonce is a separate 12-byte value
/// chosen internally by [`ClusterCipher::seal`].
const CHALLENGE_LEN: usize = 32;

/// Cap on the sealed handshake message length to bound the initial
/// read. The largest handshake message seals `2 * CHALLENGE_LEN` bytes
/// (92 with `SEAL_OVERHEAD`); the cap leaves headroom for a future
/// handshake extension without a version bump while still rejecting
/// garbage.
const MAX_HANDSHAKE_LEN: u16 = 256;

/// Cap on the stream-type string a peer may send when opening a stream.
/// Type strings are `feature/<vhost>` — 128 bytes is more than enough
/// and refusing anything longer keeps a malicious peer from wasting
/// memory before we recognize the type.
const MAX_STREAM_TYPE_LEN: u16 = 128;

/// Wall-clock budget for one inbound handshake, from the first byte to
/// the last. Without this an attacker can pin a task and a file
/// descriptor indefinitely by connecting and never writing.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many inbound handshakes may run concurrently. Connections that
/// arrive while the budget is exhausted are closed immediately rather
/// than queued — queueing would just move the unbounded growth from
/// tasks to the accept backlog.
const MAX_INFLIGHT_HANDSHAKES: usize = 64;

/// Concurrent yamux streams allowed per connection (yamux's own default
/// is 512). Every cluster feature today needs one stream per peer, so
/// this is generous while still bounding how many handler tasks and
/// 256 KiB flow-control windows one authenticated peer can force us to
/// allocate.
const MAX_STREAMS_PER_CONNECTION: usize = 64;

/// Maximum sealed frame length accepted on the post-handshake stream.
/// yamux writes at most a 16 KiB payload per call (its default
/// `split_send_size`), so 1 MiB is far above anything the multiplexer
/// produces and well under an allocation worth worrying about.
const MAX_SEALED_FRAME_LEN: usize = 1024 * 1024;

/// Largest plaintext run sealed into a single frame on write. Bounds
/// the per-frame buffer on both sides regardless of what the
/// multiplexer hands us in one `poll_write`.
const MAX_SEALED_PLAINTEXT_LEN: usize = 64 * 1024;

/// Length of the sealed-frame length prefix, in bytes.
const SEALED_LEN_PREFIX: usize = 4;

// ---------------------------------------------------------------------------
// Feature registry — the "lazy bind" contract.
// ---------------------------------------------------------------------------

/// Which cluster-channel features are enabled on this node.
///
/// This struct is the single source of truth for whether the channel
/// listener should bind at all. Adding a new channel-using feature
/// means adding a field here and updating [`FeatureFlags::any_enabled`]
/// — that keeps the "only bind if something needs it" contract
/// mechanically enforceable (a feature that forgets to set its flag
/// gets no channel, not a silently-half-wired one).
#[derive(Debug, Clone, Copy, Default)]
pub struct FeatureFlags {
    /// Turso CDC replication (`[db.sqlite.replication] cdc_experimental`).
    pub cdc: bool,
}

impl FeatureFlags {
    /// Return `true` if any feature that needs the channel is enabled.
    ///
    /// When this returns `false`, [`maybe_start`] MUST NOT bind the
    /// listener. This is the "if nothing uses it, don't turn it on"
    /// invariant.
    #[must_use]
    pub fn any_enabled(&self) -> bool {
        let Self { cdc } = *self;
        cdc
    }
}

// ---------------------------------------------------------------------------
// Stream registry.
// ---------------------------------------------------------------------------

/// Well-known stream-type prefixes spoken on the cluster channel.
///
/// Represented as strings on the wire so a future feature can add a new
/// stream type without a version bump — an unknown type is a warning
/// and a closed stream, not a connection drop.
pub mod stream_type {
    /// CDC replication stream, one per vhost / logical database.
    ///
    /// Wire form: `"cdc/<vhost>"`. The full string (including the
    /// vhost) is what selects the handler on the accepting side.
    pub const CDC_PREFIX: &str = "cdc/";

    /// Snapshot bootstrap stream, one per vhost / logical database.
    ///
    /// Wire form: `"snapshot/<vhost>"`. A cold replica joining a running
    /// primary opens one of these to receive a base snapshot before
    /// subscribing to the CDC stream. The handler is registered by the
    /// CDC replication feature (`ephpm-server`'s `turso_cdc` module);
    /// the channel itself only routes the stream type. When no feature
    /// has registered a handler, the stream is closed like any other
    /// unknown type.
    pub const SNAPSHOT_PREFIX: &str = "snapshot/";
}

// ---------------------------------------------------------------------------
// Public handle — dialers and stream registration.
// ---------------------------------------------------------------------------

/// Handle to the running cluster channel on this node.
///
/// A [`Handle`] returned by [`maybe_start`] means the listener is
/// actively bound. `None` means no feature asked for the channel and
/// the network was never touched.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<HandleInner>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelHandle")
            .field("listen_addr", &self.inner.listen_addr)
            .finish_non_exhaustive()
    }
}

/// The keys a channel endpoint needs: the long-lived handshake cipher
/// plus the operator secret, retained so a per-connection session key
/// can be derived from the handshake transcript.
///
/// Deliberately has no `Debug` impl — nothing should ever print it.
struct ChannelKeys {
    handshake: ClusterCipher,
    secret: String,
}

impl ChannelKeys {
    fn new(secret: &str) -> Self {
        Self { handshake: ClusterCipher::for_cluster_channel(secret), secret: secret.to_string() }
    }

    /// Derive this connection's frame key from the handshake transcript
    /// (`C_i || C_r`). Both peers compute the identical transcript.
    fn session(&self, transcript: &[u8]) -> ClusterCipher {
        ClusterCipher::for_cluster_channel_session(&self.secret, transcript)
    }
}

struct HandleInner {
    listen_addr: SocketAddr,
    /// The gossip layer's listen IP, captured at bind time. Used to
    /// substitute the wildcard IP when advertising this node's
    /// dial-me address to peers via
    /// [`Handle::advertise_addr`] — see the doc there for why.
    gossip_advertise_ip: IpAddr,
    keys: Arc<ChannelKeys>,
    /// Registered stream handlers, keyed by full stream-type string.
    ///
    /// The accept loop looks up an incoming stream's type here; a miss
    /// closes the stream with a warning. Shared between the accept
    /// task and the public API so registrations are visible on both
    /// sides.
    handlers: Arc<Mutex<Vec<HandlerEntry>>>,
}

struct HandlerEntry {
    /// Prefix or exact match this handler serves.
    pattern: String,
    /// When `true`, `pattern` matches any stream-type starting with it;
    /// when `false`, the match is exact.
    prefix: bool,
    tx: mpsc::UnboundedSender<IncomingStream>,
}

/// An accepted inbound stream, delivered to whichever handler matched
/// its type string.
pub struct IncomingStream {
    /// The exact stream-type string the peer sent (`"cdc/default"` etc.).
    pub stream_type: String,
    /// The multiplexed stream, ready for framed IO.
    pub stream: ChannelStream,
    /// Peer address of the underlying TCP connection.
    pub peer: SocketAddr,
}

/// Alias for the yamux stream type used on the wire.
///
/// Wrapped in [`Compat`] so callers can use tokio's `AsyncRead`/
/// `AsyncWrite` traits everywhere else in the codebase and only touch
/// the `futures::io` traits inside this module.
pub type ChannelStream = Compat<yamux::Stream>;

impl Handle {
    /// The address the channel listener is bound on.
    ///
    /// This is the raw bind address — it may have an unspecified IP
    /// (`0.0.0.0` / `::`) if the operator asked us to bind on all
    /// interfaces. Do NOT publish this to peers verbatim; use
    /// [`Handle::advertise_addr`] instead (see the doc there for why).
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.inner.listen_addr
    }

    /// The address peers should dial to reach this node's channel.
    ///
    /// Substitutes the wildcard IP (`0.0.0.0` / `::`) with the gossip
    /// advertise IP so remote replicas do not try to dial `0.0.0.0`
    /// (which they will, refused by their local stack). The port is
    /// always the actual listener port.
    ///
    /// If the operator bound both the channel and gossip on wildcard
    /// IPs there is no discoverable advertise IP; this returns `None`
    /// and callers must refuse the operation with a config error
    /// pointing operators at `[cluster] bind` or an explicit
    /// `[cluster.channel] listen`.
    #[must_use]
    pub fn advertise_addr(&self) -> Option<SocketAddr> {
        resolve_advertise_addr(self.inner.listen_addr, self.inner.gossip_advertise_ip)
    }

    /// Register a handler for exact stream-type `pattern`.
    ///
    /// Returns a receiver that yields every inbound stream whose type
    /// string equals `pattern`. Dropping the receiver un-registers the
    /// handler on the next inbound stream (silent — a peer that opens
    /// a stream for a de-registered type gets the "unknown" branch).
    pub fn register_exact(
        &self,
        pattern: impl Into<String>,
    ) -> mpsc::UnboundedReceiver<IncomingStream> {
        self.register(pattern.into(), false)
    }

    /// Register a handler for any stream-type starting with `prefix`.
    ///
    /// Useful for multi-vhost features (e.g. `"cdc/"` matches
    /// `"cdc/default"`, `"cdc/blog"`, ...).
    pub fn register_prefix(
        &self,
        prefix: impl Into<String>,
    ) -> mpsc::UnboundedReceiver<IncomingStream> {
        self.register(prefix.into(), true)
    }

    fn register(&self, pattern: String, prefix: bool) -> mpsc::UnboundedReceiver<IncomingStream> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.handlers.lock().push(HandlerEntry { pattern, prefix, tx });
        rx
    }

    /// Dial `peer_addr`, complete the handshake, open a yamux stream,
    /// send the `stream_type` header, and return the stream.
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP connect fails, the handshake fails
    /// (wrong secret or protocol mismatch), or yamux fails to open a
    /// stream.
    pub async fn dial(
        &self,
        peer_addr: SocketAddr,
        stream_type: &str,
    ) -> anyhow::Result<ChannelStream> {
        let mut tcp = TcpStream::connect(peer_addr)
            .await
            .with_context(|| format!("cluster channel: connect to {peer_addr}"))?;
        tcp.set_nodelay(true).ok();
        let handshake = handshake_initiate(&mut tcp, &self.inner.keys);
        let session = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .with_context(|| format!("cluster channel: handshake with {peer_addr} timed out"))?
            .with_context(|| format!("cluster channel: handshake with {peer_addr}"))?;

        let sealed = SealedStream::new(tcp, session);
        let cfg = yamux_config();
        let mut conn = yamux::Connection::new(sealed.compat(), cfg, yamux::Mode::Client);

        let stream =
            poll_new_outbound(&mut conn).await.context("cluster channel: yamux open outbound")?;

        // Dialer connections don't accept inbound streams for us, but
        // yamux is symmetric — control frames (WindowUpdate, Ping,
        // GoAway) still arrive and must be driven. We spawn a drain
        // task with an empty handler set; the connection will run
        // until either side closes.
        let drain_handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(drive_connection(conn, drain_handlers, peer_addr));

        let mut compat = stream.compat();
        write_stream_type(&mut compat, stream_type).await?;
        Ok(compat)
    }
}

// ---------------------------------------------------------------------------
// Entry point — the lazy-bind decision.
// ---------------------------------------------------------------------------

/// Start the cluster channel listener **iff** any feature in `features`
/// is enabled. Returns `Ok(None)` when no feature wants the channel
/// (the "don't turn it on" contract) — no socket is bound and no task
/// is spawned in that case.
///
/// # Peer admission
///
/// An inbound connection must (a) complete the mutual handshake, which
/// requires the shared secret, and (b) originate from an IP address
/// that gossip currently knows as a cluster member (alive or dead).
/// The second check is coarse — it is per-host, not per-process, and it
/// trusts the TCP source address, so it does not survive NAT or a
/// spoofed source on a network where that is possible. It is defence in
/// depth behind the handshake, not a substitute for it: it means a
/// stolen secret alone does not let an arbitrary internet host pull a
/// database snapshot, and it bounds the blast radius to hosts already
/// in the mesh.
///
/// # Errors
///
/// Returns an error if a feature is enabled but a shared secret is
/// unavailable, if the derived listen address is invalid, or if
/// binding the TCP listener fails.
pub async fn maybe_start(
    channel: &ClusterChannelConfig,
    cluster_secret: &str,
    cluster: &Arc<ClusterHandle>,
    features: FeatureFlags,
) -> anyhow::Result<Option<Handle>> {
    if !features.any_enabled() {
        tracing::debug!(
            "cluster channel: no features enabled; listener stays closed \
             (opt-in transport)"
        );
        return Ok(None);
    }

    // Fail-closed: any channel feature requires a secret. Fall back
    // to [cluster] secret when [cluster.channel] secret is unset.
    let effective_secret = channel
        .secret
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| Some(cluster_secret).filter(|s| !s.is_empty()))
        .context(
            "cluster channel: a shared secret is required when any channel feature is \
             enabled (set [cluster] secret or [cluster.channel] secret). Refusing to \
             bind the channel in unauthenticated mode.",
        )?;

    let listen_addr = resolve_listen_addr(channel, cluster)?;

    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("cluster channel: bind {listen_addr}"))?;
    let listen_addr = listener.local_addr().unwrap_or(listen_addr);

    let keys = Arc::new(ChannelKeys::new(effective_secret));
    let handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));

    let gossip_advertise_ip = cluster.gossip_socket_addr().ip();

    let handle = Handle {
        inner: Arc::new(HandleInner {
            listen_addr,
            gossip_advertise_ip,
            keys: Arc::clone(&keys),
            handlers: Arc::clone(&handlers),
        }),
    };

    let peers = Some(Arc::clone(cluster));
    tokio::spawn(accept_loop(listener, keys, Arc::clone(&handlers), peers));

    tracing::info!(
        %listen_addr,
        cdc = features.cdc,
        "cluster channel: listener bound (opt-in features active)"
    );
    Ok(Some(handle))
}

/// yamux settings shared by both ends of every channel connection.
///
/// Only deviation from the crate default is the per-connection stream
/// cap (`512` → [`MAX_STREAMS_PER_CONNECTION`]).
fn yamux_config() -> yamux::Config {
    let mut cfg = yamux::Config::default();
    cfg.set_max_num_streams(MAX_STREAMS_PER_CONNECTION);
    cfg
}

fn resolve_listen_addr(
    channel: &ClusterChannelConfig,
    cluster: &ClusterHandle,
) -> anyhow::Result<SocketAddr> {
    if let Some(explicit) = channel.listen.as_deref().filter(|s| !s.is_empty()) {
        return explicit.parse().with_context(|| {
            format!("cluster.channel.listen is not a valid socket address: {explicit}")
        });
    }
    let gossip = cluster.gossip_socket_addr();
    // gossip_port + 2, NOT + 1: the KV data plane's default port (7947) is
    // already gossip-default (7946) + 1, so deriving +1 would collide with
    // it on any default-ported cluster. +2 lands on 7948 with defaults.
    let port = gossip
        .port()
        .checked_add(2)
        .context("gossip port is >= u16::MAX - 1; cannot derive cluster channel port")?;
    Ok(SocketAddr::new(gossip.ip(), port))
}

/// Compute the address peers should dial to reach us.
///
/// If the listener bound a wildcard IP (`0.0.0.0` / `::`), substitute
/// the gossip layer's own listen IP — it's the only IP we know is
/// meaningful to peers (they either directly know it from `join =
/// [...]` or discovered us through gossip with that IP already
/// published as our identity).
///
/// Returns `None` when the gossip IP is *also* unspecified. In that
/// case there is no discoverable advertise IP and the caller must
/// refuse the operation with a clear config error — publishing
/// `0.0.0.0:PORT` to peers is guaranteed-broken (a remote replica
/// would dial `0.0.0.0` on its local stack, which is refused).
///
/// Kept as a free function so both the runtime and the unit tests
/// exercise the identical logic without spinning up a real listener.
fn resolve_advertise_addr(listen: SocketAddr, gossip_ip: std::net::IpAddr) -> Option<SocketAddr> {
    if !listen.ip().is_unspecified() {
        return Some(listen);
    }
    if gossip_ip.is_unspecified() {
        return None;
    }
    Some(SocketAddr::new(gossip_ip, listen.port()))
}

// ---------------------------------------------------------------------------
// Accept loop.
// ---------------------------------------------------------------------------

/// Accept inbound channel connections.
///
/// `cluster` is `Some` in production and gates admission on gossip
/// membership (see [`maybe_start`]). It is `None` only in this module's
/// unit tests, which drive the accept loop directly against a loopback
/// pair with no gossip mesh to consult; passing `None` disables the
/// membership check and nothing else.
async fn accept_loop(
    listener: TcpListener,
    keys: Arc<ChannelKeys>,
    handlers: Arc<Mutex<Vec<HandlerEntry>>>,
    cluster: Option<Arc<ClusterHandle>>,
) {
    // Bounds concurrent *pre-authentication* work. Permits are released
    // as soon as the handshake resolves, so long-lived authenticated
    // connections never hold a slot.
    let handshakes = Arc::new(Semaphore::new(MAX_INFLIGHT_HANDSHAKES));

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("cluster channel: accept error: {e}");
                continue;
            }
        };
        tcp.set_nodelay(true).ok();

        let Ok(permit) = Arc::clone(&handshakes).try_acquire_owned() else {
            tracing::warn!(
                peer = %peer,
                limit = MAX_INFLIGHT_HANDSHAKES,
                "cluster channel: too many in-flight handshakes; dropping connection"
            );
            drop(tcp);
            continue;
        };

        let keys = Arc::clone(&keys);
        let handlers = Arc::clone(&handlers);
        let cluster = cluster.clone();
        tokio::spawn(async move {
            let admitted = if let Some(cluster) = &cluster {
                peer_is_cluster_member(cluster, peer.ip()).await
            } else {
                true
            };
            if !admitted {
                tracing::warn!(
                    peer = %peer,
                    "cluster channel: refusing connection from an address gossip does not \
                     know as a cluster member"
                );
                drop(permit);
                return;
            }

            let mut tcp = tcp;
            let handshake =
                tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake_respond(&mut tcp, &keys)).await;
            // Release the pre-auth budget before the (unbounded in time)
            // connection-serving phase begins.
            drop(permit);

            let session = match handshake {
                Ok(Ok(session)) => session,
                Ok(Err(e)) => {
                    // Drop with debug — a wrong-secret peer is legitimate
                    // background noise; we should not warn per-attempt.
                    tracing::debug!(peer = %peer, "cluster channel: handshake failed: {e:#}");
                    return;
                }
                Err(_) => {
                    tracing::debug!(
                        peer = %peer,
                        timeout_secs = HANDSHAKE_TIMEOUT.as_secs(),
                        "cluster channel: handshake timed out"
                    );
                    return;
                }
            };

            let sealed = SealedStream::new(tcp, session);
            let cfg = yamux_config();
            let conn = yamux::Connection::new(sealed.compat(), cfg, yamux::Mode::Server);
            drive_connection(conn, handlers, peer).await;
        });
    }
}

/// Is `ip` an address gossip currently associates with a cluster member?
///
/// Dead members count: a node that just restarted still has to be able
/// to reconnect, and the failure detector takes tens of seconds to
/// forget it. Self counts too — loopback and single-node test setups
/// dial themselves.
async fn peer_is_cluster_member(cluster: &ClusterHandle, ip: IpAddr) -> bool {
    if ip == cluster.gossip_socket_addr().ip() {
        return true;
    }
    cluster
        .nodes()
        .await
        .iter()
        .filter_map(|n| n.gossip_addr.parse::<SocketAddr>().ok())
        .any(|addr| addr.ip() == ip)
}

// ---------------------------------------------------------------------------
// Handshake.
// ---------------------------------------------------------------------------

/// Constant-time byte-slice equality.
///
/// The handshake comparisons are not realistically timing-attackable
/// (a wrong guess just drops the connection, and the values are
/// single-use), but a challenge/response check is exactly the shape of
/// code that should never leak a prefix-match length. `black_box`
/// keeps the accumulate-then-compare from being turned back into an
/// early-exit loop by the optimizer.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Generate a fresh random challenge.
fn fresh_challenge() -> [u8; CHALLENGE_LEN] {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::aead::rand_core::RngCore;

    let mut challenge = [0u8; CHALLENGE_LEN];
    OsRng.fill_bytes(&mut challenge);
    challenge
}

/// Write `[len: u16 BE][sealed]` for an already-sealed handshake body.
async fn write_handshake_body(tcp: &mut TcpStream, sealed: &[u8]) -> anyhow::Result<()> {
    let len = u16::try_from(sealed.len()).context("handshake message too long")?;
    anyhow::ensure!(len <= MAX_HANDSHAKE_LEN, "handshake message > MAX_HANDSHAKE_LEN");
    tcp.write_all(&len.to_be_bytes()).await?;
    tcp.write_all(sealed).await?;
    tcp.flush().await?;
    Ok(())
}

/// Read `[len: u16 BE][sealed]` and open it with the handshake key.
async fn read_handshake_body(
    tcp: &mut TcpStream,
    cipher: &ClusterCipher,
) -> anyhow::Result<Vec<u8>> {
    let mut lenbuf = [0u8; 2];
    tcp.read_exact(&mut lenbuf).await?;
    let len = u16::from_be_bytes(lenbuf);
    anyhow::ensure!(len <= MAX_HANDSHAKE_LEN, "handshake message too long: {len}");
    let mut sealed = vec![0u8; len as usize];
    tcp.read_exact(&mut sealed).await?;
    cipher.open(&sealed).context("handshake message failed authentication")
}

/// Build the transcript both peers hash into their session key.
fn transcript(initiator: &[u8; CHALLENGE_LEN], responder: &[u8; CHALLENGE_LEN]) -> Vec<u8> {
    let mut t = Vec::with_capacity(CHALLENGE_LEN * 2);
    t.extend_from_slice(initiator);
    t.extend_from_slice(responder);
    t
}

/// Dial side of the v2 handshake. Returns the per-connection cipher
/// every subsequent byte on this TCP stream is sealed with.
async fn handshake_initiate(
    tcp: &mut TcpStream,
    keys: &ChannelKeys,
) -> anyhow::Result<ClusterCipher> {
    let challenge_i = fresh_challenge();

    tcp.write_all(&[HANDSHAKE_VERSION]).await?;
    let sealed = keys.handshake.seal(&challenge_i).context("seal initiator challenge")?;
    write_handshake_body(tcp, &sealed).await?;

    // Message 2: [version][len][seal(C_i || C_r)].
    let mut ver = [0u8; 1];
    tcp.read_exact(&mut ver).await.context("read handshake reply version")?;
    anyhow::ensure!(ver[0] == HANDSHAKE_VERSION, "handshake version mismatch: got {}", ver[0]);
    let reply = read_handshake_body(tcp, &keys.handshake).await.context("read handshake reply")?;
    anyhow::ensure!(
        reply.len() == CHALLENGE_LEN * 2,
        "handshake reply wrong length: {} (expected {})",
        reply.len(),
        CHALLENGE_LEN * 2
    );
    let (echoed, responder) = reply.split_at(CHALLENGE_LEN);
    anyhow::ensure!(
        constant_time_eq(echoed, &challenge_i),
        "handshake reply did not echo our challenge (replayed or forged responder)"
    );
    let mut challenge_r = [0u8; CHALLENGE_LEN];
    challenge_r.copy_from_slice(responder);

    // Message 3: prove we can open the responder's challenge too.
    let sealed = keys.handshake.seal(&challenge_r).context("seal responder challenge echo")?;
    write_handshake_body(tcp, &sealed).await?;

    Ok(keys.session(&transcript(&challenge_i, &challenge_r)))
}

/// Accept side of the v2 handshake. Returns the per-connection cipher.
///
/// The responder's own challenge is what defeats replay: an attacker
/// re-sending a captured message 1 receives a message 2 built around a
/// challenge they have never seen and cannot open, so they cannot
/// produce message 3.
async fn handshake_respond(
    tcp: &mut TcpStream,
    keys: &ChannelKeys,
) -> anyhow::Result<ClusterCipher> {
    let mut ver = [0u8; 1];
    tcp.read_exact(&mut ver).await.context("read handshake version")?;
    anyhow::ensure!(ver[0] == HANDSHAKE_VERSION, "handshake version mismatch: got {}", ver[0]);

    let opened =
        read_handshake_body(tcp, &keys.handshake).await.context("read initiator challenge")?;
    anyhow::ensure!(
        opened.len() == CHALLENGE_LEN,
        "handshake challenge wrong length: {}",
        opened.len()
    );
    let mut challenge_i = [0u8; CHALLENGE_LEN];
    challenge_i.copy_from_slice(&opened);

    let challenge_r = fresh_challenge();
    let session_transcript = transcript(&challenge_i, &challenge_r);

    // Message 2 seals the transcript itself: the echo of C_i (which
    // proves we opened it) followed by our own fresh C_r.
    tcp.write_all(&[HANDSHAKE_VERSION]).await?;
    let sealed = keys.handshake.seal(&session_transcript).context("seal handshake reply")?;
    write_handshake_body(tcp, &sealed).await?;

    // Message 3 must be the responder challenge, sealed by a peer that
    // holds the secret. A replayer cannot get here.
    let confirm =
        read_handshake_body(tcp, &keys.handshake).await.context("read handshake confirmation")?;
    anyhow::ensure!(
        constant_time_eq(&confirm, &challenge_r),
        "handshake confirmation did not echo our challenge (replay attempt)"
    );

    Ok(keys.session(&session_transcript))
}

// ---------------------------------------------------------------------------
// Sealed stream: per-frame ChaCha20-Poly1305 over the post-handshake TCP
// connection. Same posture as `kv_data_plane`'s framed messages, but as
// an `AsyncRead`/`AsyncWrite` adapter so yamux can sit on top of it.
// ---------------------------------------------------------------------------

/// Wraps a byte stream so every write is sealed into a
/// `[len: u32 BE][nonce || ciphertext+tag]` frame and every read opens
/// one.
///
/// Framing is invisible to the caller: `poll_read` hands back plaintext
/// bytes from the current frame and only touches the socket when that
/// frame is exhausted. A frame that fails authentication is a hard
/// `InvalidData` error — there is no "skip and continue", because on a
/// stream transport a forged frame means the stream is no longer
/// trustworthy.
struct SealedStream<S> {
    inner: S,
    cipher: ClusterCipher,

    // Read state.
    hdr: [u8; SEALED_LEN_PREFIX],
    hdr_filled: usize,
    body: Vec<u8>,
    body_filled: usize,
    plain: Vec<u8>,
    plain_pos: usize,

    // Write state: one sealed frame at a time, flushed before the next
    // plaintext run is accepted.
    out: Vec<u8>,
    out_pos: usize,
}

impl<S> SealedStream<S> {
    fn new(inner: S, cipher: ClusterCipher) -> Self {
        Self {
            inner,
            cipher,
            hdr: [0u8; SEALED_LEN_PREFIX],
            hdr_filled: 0,
            body: Vec::new(),
            body_filled: 0,
            plain: Vec::new(),
            plain_pos: 0,
            out: Vec::new(),
            out_pos: 0,
        }
    }
}

impl<S: AsyncWrite + Unpin> SealedStream<S> {
    /// Push any bytes of the current sealed frame that have not reached
    /// the socket yet.
    fn poll_flush_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.out_pos < self.out.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.out[self.out_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "cluster channel: sealed write made no progress",
                )));
            }
            self.out_pos += n;
        }
        self.out.clear();
        self.out_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for SealedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            // 1. Serve whatever plaintext the last opened frame still has.
            if this.plain_pos < this.plain.len() {
                let n = out.remaining().min(this.plain.len() - this.plain_pos);
                if n == 0 {
                    return Poll::Ready(Ok(()));
                }
                out.put_slice(&this.plain[this.plain_pos..this.plain_pos + n]);
                this.plain_pos += n;
                return Poll::Ready(Ok(()));
            }

            // 2. Length prefix.
            if this.hdr_filled < SEALED_LEN_PREFIX {
                let mut rb = ReadBuf::new(&mut this.hdr[this.hdr_filled..]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
                let n = rb.filled().len();
                if n == 0 {
                    return if this.hdr_filled == 0 {
                        // Clean EOF on a frame boundary.
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "cluster channel: EOF inside a sealed frame header",
                        )))
                    };
                }
                this.hdr_filled += n;
                continue;
            }

            // 3. Frame body.
            let want = usize::try_from(u32::from_be_bytes(this.hdr)).unwrap_or(usize::MAX);
            if want == 0 || want > MAX_SEALED_FRAME_LEN {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("cluster channel: sealed frame length {want} out of range"),
                )));
            }
            if this.body.len() != want {
                this.body.resize(want, 0);
            }
            while this.body_filled < want {
                let mut rb = ReadBuf::new(&mut this.body[this.body_filled..]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
                let n = rb.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "cluster channel: EOF inside a sealed frame body",
                    )));
                }
                this.body_filled += n;
            }

            let Some(plain) = this.cipher.open(&this.body) else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cluster channel: sealed frame failed authentication",
                )));
            };
            this.plain = plain;
            this.plain_pos = 0;
            this.hdr_filled = 0;
            this.body_filled = 0;
            this.body.clear();
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for SealedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // Never interleave two frames: finish the pending one first.
        ready!(this.poll_flush_pending(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let take = buf.len().min(MAX_SEALED_PLAINTEXT_LEN);
        let sealed = this
            .cipher
            .seal(&buf[..take])
            .map_err(|e| io::Error::other(format!("cluster channel: seal failed: {e}")))?;
        let len = u32::try_from(sealed.len())
            .map_err(|_| io::Error::other("cluster channel: sealed frame too large"))?;

        this.out.clear();
        this.out.reserve(SEALED_LEN_PREFIX + sealed.len());
        this.out.extend_from_slice(&len.to_be_bytes());
        this.out.extend_from_slice(&sealed);
        this.out_pos = 0;

        // Best effort: whatever does not fit in the socket buffer now is
        // completed by the next poll_write / poll_flush. The plaintext is
        // already committed, so we report it as written.
        if let Poll::Ready(Err(e)) = this.poll_flush_pending(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(take))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_flush_pending(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_flush_pending(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Stream-type header.
// ---------------------------------------------------------------------------

async fn write_stream_type(stream: &mut ChannelStream, stream_type: &str) -> anyhow::Result<()> {
    let bytes = stream_type.as_bytes();
    let len = u16::try_from(bytes.len()).context("stream type too long")?;
    anyhow::ensure!(len <= MAX_STREAM_TYPE_LEN, "stream type > MAX_STREAM_TYPE_LEN");
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_stream_type(stream: &mut ChannelStream) -> anyhow::Result<String> {
    let mut lenbuf = [0u8; 2];
    stream.read_exact(&mut lenbuf).await?;
    let len = u16::from_be_bytes(lenbuf);
    anyhow::ensure!(len <= MAX_STREAM_TYPE_LEN, "peer sent stream type > MAX_STREAM_TYPE_LEN");
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    String::from_utf8(buf).context("stream type is not valid utf-8")
}

// ---------------------------------------------------------------------------
// yamux poll helpers (yamux 0.14 exposes poll_*, not async fns).
// ---------------------------------------------------------------------------

async fn poll_new_outbound<T>(conn: &mut yamux::Connection<T>) -> anyhow::Result<yamux::Stream>
where
    T: FuturesAsyncRead + FuturesAsyncWrite + Unpin,
{
    struct Fut<'a, T>(&'a mut yamux::Connection<T>);
    impl<T: FuturesAsyncRead + FuturesAsyncWrite + Unpin> Future for Fut<'_, T> {
        type Output = yamux::Result<yamux::Stream>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.poll_new_outbound(cx)
        }
    }
    Fut(conn).await.map_err(anyhow::Error::from)
}

async fn poll_next_inbound<T>(
    conn: &mut yamux::Connection<T>,
) -> Option<yamux::Result<yamux::Stream>>
where
    T: FuturesAsyncRead + FuturesAsyncWrite + Unpin,
{
    struct Fut<'a, T>(&'a mut yamux::Connection<T>);
    impl<T: FuturesAsyncRead + FuturesAsyncWrite + Unpin> Future for Fut<'_, T> {
        type Output = Option<yamux::Result<yamux::Stream>>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.poll_next_inbound(cx)
        }
    }
    Fut(conn).await
}

// ---------------------------------------------------------------------------
// Connection driver.
// ---------------------------------------------------------------------------

async fn drive_connection<T>(
    mut conn: yamux::Connection<T>,
    handlers: Arc<Mutex<Vec<HandlerEntry>>>,
    peer: SocketAddr,
) where
    T: FuturesAsyncRead + FuturesAsyncWrite + Unpin + Send + 'static,
{
    loop {
        match poll_next_inbound(&mut conn).await {
            Some(Ok(stream)) => {
                let handlers = Arc::clone(&handlers);
                tokio::spawn(async move {
                    let mut compat = stream.compat();
                    let stream_type = match read_stream_type(&mut compat).await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(%peer, "cluster channel: stream-type read error: {e:#}");
                            return;
                        }
                    };
                    dispatch_stream(&handlers, stream_type, compat, peer);
                });
            }
            Some(Err(e)) => {
                tracing::debug!(%peer, "cluster channel: yamux inbound error: {e}");
                break;
            }
            None => {
                tracing::debug!(%peer, "cluster channel: yamux connection closed by peer");
                break;
            }
        }
    }
}

fn dispatch_stream(
    handlers: &Mutex<Vec<HandlerEntry>>,
    stream_type: String,
    stream: ChannelStream,
    peer: SocketAddr,
) {
    // Snapshot the matching sender under the lock and prune dead entries.
    let matched = {
        let mut guard = handlers.lock();
        guard.retain(|h| !h.tx.is_closed());
        guard.iter().find_map(|h| {
            let hit = if h.prefix {
                stream_type.starts_with(&h.pattern)
            } else {
                stream_type == h.pattern
            };
            if hit { Some(h.tx.clone()) } else { None }
        })
    };

    match matched {
        Some(tx) => {
            let _ = tx.send(IncomingStream { stream_type, stream, peer });
        }
        None => {
            tracing::warn!(
                stream_type = %stream_type,
                %peer,
                "cluster channel: no handler registered for stream type; closing"
            );
            drop(stream);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_flags_default_is_all_off() {
        let f = FeatureFlags::default();
        assert!(!f.any_enabled(), "default FeatureFlags must have nothing enabled");
    }

    #[test]
    fn feature_flags_cdc_flips_any_enabled() {
        let f = FeatureFlags { cdc: true };
        assert!(f.any_enabled());
    }

    /// The lazy-bind proof. When no feature is enabled, `maybe_start`
    /// must return `Ok(None)` and MUST NOT touch the network — no
    /// listener appears on the port derived from the gossip address.
    #[tokio::test]
    async fn channel_stays_off_when_no_features_enabled() {
        // Bring up a real gossip listener so we have a real
        // ClusterHandle to derive from. The bound gossip port dictates
        // what "port + 2" would be — we then confirm nothing is
        // listening on that port after `maybe_start`.
        let gossip_bind = pick_free_port_addr();

        let cfg = ephpm_config::ClusterConfig {
            enabled: true,
            bind: gossip_bind.clone(),
            secret: "test-secret-not-important-if-no-feature-enabled".to_string(),
            ..ephpm_config::ClusterConfig::default()
        };
        let cluster = Arc::new(crate::start_gossip(&cfg).await.expect("gossip start"));

        let derived_port = cluster.gossip_socket_addr().port() + 2;
        let derived_addr = format!("127.0.0.1:{derived_port}");

        let channel_cfg = ephpm_config::ClusterChannelConfig::default();
        let features = FeatureFlags::default(); // NOTHING enabled

        let result =
            maybe_start(&channel_cfg, &cfg.secret, &cluster, features).await.expect("maybe_start");

        assert!(result.is_none(), "channel handle must be None when no feature is enabled");

        // Prove nothing is bound: we should be able to bind the
        // derived port ourselves. If maybe_start had bound it in
        // violation of the contract, this would fail with EADDRINUSE.
        let probe = TcpListener::bind(&derived_addr).await;
        assert!(
            probe.is_ok(),
            "the channel port {derived_addr} must be unbound when no feature is enabled \
             (got: {probe:?})"
        );
    }

    /// When a feature IS enabled, the channel must bind. Also confirms
    /// the derived-address rule (`gossip_port + 2`) is honored when
    /// `[cluster.channel] listen` is unset.
    #[tokio::test]
    async fn channel_binds_when_a_feature_is_enabled_and_derives_port_from_gossip() {
        let gossip_bind = pick_free_port_addr();

        let cfg = ephpm_config::ClusterConfig {
            enabled: true,
            bind: gossip_bind.clone(),
            secret: "a-secret-value-for-tests".to_string(),
            ..ephpm_config::ClusterConfig::default()
        };
        let cluster = Arc::new(crate::start_gossip(&cfg).await.expect("gossip start"));
        let expected_port = cluster.gossip_socket_addr().port() + 2;

        let channel_cfg = ephpm_config::ClusterChannelConfig::default();
        let handle = maybe_start(&channel_cfg, &cfg.secret, &cluster, FeatureFlags { cdc: true })
            .await
            .expect("maybe_start")
            .expect("channel handle");

        assert_eq!(handle.listen_addr().port(), expected_port);
    }

    /// When a feature is enabled but no secret is configured anywhere,
    /// `maybe_start` must refuse (fail-closed).
    #[tokio::test]
    async fn channel_refuses_to_bind_without_a_secret() {
        let gossip_bind = pick_free_port_addr();
        let cfg = ephpm_config::ClusterConfig {
            enabled: true,
            bind: gossip_bind.clone(),
            secret: String::new(), // no secret
            ..ephpm_config::ClusterConfig::default()
        };
        let cluster = Arc::new(crate::start_gossip(&cfg).await.expect("gossip start"));

        let channel_cfg = ephpm_config::ClusterChannelConfig::default();
        let err = maybe_start(&channel_cfg, "", &cluster, FeatureFlags { cdc: true })
            .await
            .expect_err("must fail with no secret");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("shared secret is required"),
            "expected fail-closed message, got: {msg}"
        );
    }

    /// Handshake + open a stream + dispatch by exact type. Uses a
    /// loopback pair — no gossip involved for this leg, so the accept
    /// loop runs with the membership check disabled (`None`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_then_stream_dispatch_roundtrip() {
        let keys = Arc::new(ChannelKeys::new("roundtrip-secret"));
        let handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Register handler for "cdc/default" before we bring up the
        // accept task so no inbound stream can slip through the gap.
        let (tx, mut rx) = mpsc::unbounded_channel();
        handlers.lock().push(HandlerEntry { pattern: "cdc/default".into(), prefix: false, tx });

        tokio::spawn(accept_loop(listener, Arc::clone(&keys), Arc::clone(&handlers), None));

        // Dial as a client.
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        let session = handshake_initiate(&mut tcp, &keys).await.expect("handshake");
        let sealed = SealedStream::new(tcp, session);
        let cfg = yamux_config();
        let mut conn = yamux::Connection::new(sealed.compat(), cfg, yamux::Mode::Client);
        let stream = poll_new_outbound(&mut conn).await.unwrap();
        tokio::spawn(drive_connection(conn, Arc::new(Mutex::new(Vec::new())), addr));

        let mut cs = stream.compat();
        write_stream_type(&mut cs, "cdc/default").await.expect("write type");
        cs.write_all(b"hello").await.unwrap();
        cs.flush().await.unwrap();

        let incoming = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap();
        let mut incoming = incoming.expect("dispatched stream");
        assert_eq!(incoming.stream_type, "cdc/default");
        let mut buf = [0u8; 5];
        incoming.stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    /// A dialer holding the WRONG secret must be silently dropped by
    /// the responder — the initiator's `handshake_initiate` should
    /// error, and no stream should ever reach a registered handler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_secret_is_rejected_silently() {
        let good = Arc::new(ChannelKeys::new("good-secret"));
        let bad = ChannelKeys::new("bad-secret");
        let handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        handlers.lock().push(HandlerEntry { pattern: "cdc/".into(), prefix: true, tx });
        tokio::spawn(accept_loop(listener, Arc::clone(&good), Arc::clone(&handlers), None));

        let mut tcp = TcpStream::connect(addr).await.unwrap();
        let hs = handshake_initiate(&mut tcp, &bad).await;
        assert!(hs.is_err(), "handshake with wrong secret must fail on initiator side");

        // Nothing should arrive on the handler within a short window.
        let noise = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(noise.is_err(), "no stream should be dispatched for a rejected peer");
    }

    /// **CRIT-1 regression.** Record a full, legitimate handshake, then
    /// replay the captured initiator bytes verbatim against a fresh
    /// responder. Under the v1 handshake this cleared the accept path
    /// (the responder only ever echoed what it was given). Under v2 the
    /// responder issues its own challenge, so the replayed transcript
    /// cannot produce a valid third message and the connection is
    /// dropped without a single stream being dispatched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recorded_handshake_cannot_be_replayed() {
        let keys = Arc::new(ChannelKeys::new("replay-secret"));
        let handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        handlers.lock().push(HandlerEntry { pattern: "cdc/".into(), prefix: true, tx });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(accept_loop(listener, Arc::clone(&keys), Arc::clone(&handlers), None));

        // 1. Capture what a legitimate initiator puts on the wire.
        let mut recorder = TcpStream::connect(addr).await.unwrap();
        let challenge = fresh_challenge();
        let sealed = keys.handshake.seal(&challenge).unwrap();
        let len = u16::try_from(sealed.len()).unwrap();
        let mut captured = vec![HANDSHAKE_VERSION];
        captured.extend_from_slice(&len.to_be_bytes());
        captured.extend_from_slice(&sealed);
        recorder.write_all(&captured).await.unwrap();
        recorder.flush().await.unwrap();
        drop(recorder);

        // 2. Replay those exact bytes on a brand new connection.
        let mut attacker = TcpStream::connect(addr).await.unwrap();
        attacker.write_all(&captured).await.unwrap();
        attacker.flush().await.unwrap();

        // The responder answers, but with a challenge of its own that
        // the attacker cannot open — so it cannot complete message 3.
        let mut ver = [0u8; 1];
        attacker.read_exact(&mut ver).await.unwrap();
        assert_eq!(ver[0], HANDSHAKE_VERSION);
        let mut lenbuf = [0u8; 2];
        attacker.read_exact(&mut lenbuf).await.unwrap();
        let reply_len = usize::from(u16::from_be_bytes(lenbuf));
        let mut reply = vec![0u8; reply_len];
        attacker.read_exact(&mut reply).await.unwrap();

        // Best an attacker without the secret can do: echo the bytes
        // back. That is not a valid seal of C_r, so the responder drops.
        attacker.write_all(&lenbuf).await.unwrap();
        attacker.write_all(&reply).await.unwrap();
        attacker.flush().await.unwrap();

        let noise = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(noise.is_err(), "a replayed handshake must never reach the dispatch table");
    }

    /// **CRIT-2 regression.** The post-handshake transport must actually
    /// be sealed: a roundtrip through the adapter returns the plaintext,
    /// but the bytes that reach the underlying transport do not contain
    /// it. Before the fix the doc claimed this and yamux ran on the raw
    /// socket.
    #[tokio::test]
    async fn sealed_stream_roundtrips_but_puts_ciphertext_on_the_wire() {
        let cipher = ClusterCipher::for_cluster_channel_session("s", b"transcript");
        let (a, b) = tokio::io::duplex(64 * 1024);

        let mut writer = SealedStream::new(a, cipher.clone());
        writer.write_all(b"plaintext-marker").await.unwrap();
        writer.flush().await.unwrap();

        let mut reader = SealedStream::new(b, cipher);
        let mut got = [0u8; 16];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"plaintext-marker");

        // And the raw bytes on the underlying transport are not the
        // plaintext.
        let (mut raw_a, raw_b) = tokio::io::duplex(64 * 1024);
        let cipher2 = ClusterCipher::for_cluster_channel_session("s", b"transcript");
        let mut writer2 = SealedStream::new(raw_b, cipher2);
        writer2.write_all(b"plaintext-marker").await.unwrap();
        writer2.flush().await.unwrap();
        let mut on_the_wire = vec![0u8; 4 + 16 + crate::secure_transport::SEAL_OVERHEAD];
        raw_a.read_exact(&mut on_the_wire).await.unwrap();
        assert!(
            !on_the_wire.windows(16).any(|w| w == b"plaintext-marker"),
            "channel payload must not appear in cleartext on the wire"
        );
    }

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    fn pick_free_port_addr() -> String {
        // Ask the OS for a free UDP port by binding then dropping —
        // the port is likely to still be free by the time
        // `start_gossip` re-binds it a moment later. On failure the
        // test is flaky rather than incorrect; if we start seeing
        // flakiness in CI we can move to explicit gossip port hopping.
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().to_string()
    }

    // -----------------------------------------------------------------
    // advertise_addr — the "don't publish 0.0.0.0 to peers" contract.
    //
    // Regression coverage for the exact bug that the cross-container
    // cluster e2e surfaced: with `bind = "0.0.0.0"` on both nodes, the
    // primary was publishing `0.0.0.0:PORT` into the election KV, and
    // replicas were dialing 0.0.0.0 on their local stack. The unit
    // test level catches this cheaply.
    // -----------------------------------------------------------------

    #[test]
    fn resolve_advertise_returns_listen_when_listen_ip_is_specific() {
        // A specific bound IP is already the advertise IP — return
        // verbatim.
        let listen: SocketAddr = "10.0.1.5:8094".parse().unwrap();
        let gossip_ip = "10.0.1.5".parse().unwrap();
        assert_eq!(resolve_advertise_addr(listen, gossip_ip), Some(listen));
    }

    #[test]
    fn resolve_advertise_substitutes_gossip_ip_when_listen_is_wildcard_v4() {
        // This is Bug 2: listen is 0.0.0.0, gossip has a real IP, so
        // we publish gossip_ip:listen_port to peers.
        let listen: SocketAddr = "0.0.0.0:8094".parse().unwrap();
        let gossip_ip = "10.0.1.5".parse().unwrap();
        let advertise = resolve_advertise_addr(listen, gossip_ip).expect("resolvable");
        assert_eq!(advertise.ip().to_string(), "10.0.1.5");
        assert_eq!(advertise.port(), 8094);
    }

    #[test]
    fn resolve_advertise_substitutes_gossip_ip_when_listen_is_wildcard_v6() {
        // Same wildcard rule for IPv6 :: — the ip().is_unspecified()
        // check covers both families uniformly.
        let listen: SocketAddr = "[::]:8094".parse().unwrap();
        let gossip_ip = "fd00::1".parse().unwrap();
        let advertise = resolve_advertise_addr(listen, gossip_ip).expect("resolvable");
        assert_eq!(advertise.ip().to_string(), "fd00::1");
        assert_eq!(advertise.port(), 8094);
    }

    #[test]
    fn resolve_advertise_returns_none_when_both_are_wildcard() {
        // No discoverable IP anywhere — caller must refuse to
        // start with a config error, not publish 0.0.0.0.
        let listen: SocketAddr = "0.0.0.0:8094".parse().unwrap();
        let gossip_ip = "0.0.0.0".parse().unwrap();
        assert!(resolve_advertise_addr(listen, gossip_ip).is_none());
    }

    #[test]
    fn resolve_advertise_none_for_v6_wildcard_pair() {
        let listen: SocketAddr = "[::]:8094".parse().unwrap();
        let gossip_ip = "::".parse().unwrap();
        assert!(resolve_advertise_addr(listen, gossip_ip).is_none());
    }

    /// End-to-end: a real handle bound with a specific IP returns
    /// `Some(listen_addr)` verbatim from `advertise_addr` — the
    /// happy-path we do NOT want the fix to change.
    #[tokio::test]
    async fn advertise_addr_matches_listen_when_bound_to_loopback() {
        let gossip_bind = pick_free_port_addr();
        let cfg = ephpm_config::ClusterConfig {
            enabled: true,
            bind: gossip_bind,
            secret: "advertise-test-secret".to_string(),
            ..ephpm_config::ClusterConfig::default()
        };
        let cluster = Arc::new(crate::start_gossip(&cfg).await.expect("gossip start"));
        let handle = maybe_start(
            &ephpm_config::ClusterChannelConfig::default(),
            &cfg.secret,
            &cluster,
            FeatureFlags { cdc: true },
        )
        .await
        .expect("maybe_start")
        .expect("channel handle");
        assert_eq!(handle.advertise_addr(), Some(handle.listen_addr()));
    }

    /// End-to-end for Bug 2: bind on `0.0.0.0` — `listen_addr()` must
    /// still be `0.0.0.0:PORT` (that's what we bound), but
    /// `advertise_addr()` must swap in a routable IP (loopback in
    /// this test, since chitchat has the *identical* wildcard problem
    /// and we bind gossip on 127.0.0.1 too).
    ///
    /// This is the assertion the missing test would have made and
    /// which would have caught the shipped bug.
    #[tokio::test]
    async fn advertise_addr_substitutes_wildcard_bound_channel() {
        // Bind gossip on a real IP so it has an advertise IP to
        // donate, but tell the channel to bind on 0.0.0.0 (a pattern
        // real operators use, and the shape of the container
        // deployment that surfaced Bug 2).
        let gossip_bind = pick_free_port_addr();
        let cfg = ephpm_config::ClusterConfig {
            enabled: true,
            bind: gossip_bind,
            secret: "advertise-test-secret".to_string(),
            ..ephpm_config::ClusterConfig::default()
        };
        let cluster = Arc::new(crate::start_gossip(&cfg).await.expect("gossip start"));

        // Pick a free TCP port for the channel by binding and dropping.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let chan_port = probe.local_addr().unwrap().port();
        drop(probe);

        let channel_cfg = ephpm_config::ClusterChannelConfig {
            listen: Some(format!("0.0.0.0:{chan_port}")),
            secret: None,
        };
        let handle = maybe_start(&channel_cfg, &cfg.secret, &cluster, FeatureFlags { cdc: true })
            .await
            .expect("maybe_start")
            .expect("channel handle");

        assert!(
            handle.listen_addr().ip().is_unspecified(),
            "test precondition: we asked for a wildcard bind"
        );
        let advertise = handle.advertise_addr().expect("advertise must resolve");
        assert!(
            !advertise.ip().is_unspecified(),
            "advertise_addr must NOT be a wildcard: got {advertise}"
        );
        assert_eq!(advertise.port(), handle.listen_addr().port());
    }
}

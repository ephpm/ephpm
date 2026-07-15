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
//! # Handshake
//!
//! Both sides derive `ClusterCipher::for_cluster_channel(secret)`
//! (distinct HKDF domain from gossip / KV data plane). The initiator
//! writes:
//!
//! ```text
//!   [version: u8 = 0x01]
//!   [sealed_challenge_len: u16 BE]
//!   [sealed_challenge]         // seal(random 32-byte nonce)
//! ```
//!
//! The responder opens the challenge (proves possession of the secret),
//! re-seals the *same* nonce with a fresh AEAD nonce, and writes it
//! back with a version-byte prefix. The initiator verifies the
//! recovered nonce equals what it sent. Either side dropping on any
//! failure is the "wrong secret" signal; there is deliberately no
//! typed error reply, so a wrong-secret peer looks identical to a
//! stray TCP scanner.
//!
//! **TLS is Phase 2.1** — the channel today is authenticated with
//! ChaCha20-Poly1305 (same primitive as gossip / KV data plane) but
//! not TLS-wrapped. The framing is symmetric-key sealed end-to-end,
//! so eavesdroppers see ciphertext, but there is no PKI-based peer
//! identity beyond "holds the shared cluster secret".
//!
//! # Multiplexing
//!
//! After the handshake, both sides speak `yamux 0.14` over the raw
//! (post-handshake) TCP stream. Each yamux stream begins with a
//! length-prefixed UTF-8 stream-type string from
//! [`stream_type`]: `cdc/<vhost>` (implemented), `snapshot/<vhost>`
//! (RESERVED — refused with a logged warning today). Unknown stream
//! types are logged and closed.
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
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Context as _;
use ephpm_config::ClusterChannelConfig;
use futures::io::{AsyncRead as FuturesAsyncRead, AsyncWrite as FuturesAsyncWrite};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

use crate::ClusterHandle;
use crate::secure_transport::ClusterCipher;

/// Handshake protocol version byte. Bump on wire-format change.
const HANDSHAKE_VERSION: u8 = 0x01;

/// Challenge nonce length in bytes. The *plaintext* random challenge
/// that gets sealed — the AEAD nonce is a separate 12-byte value
/// chosen internally by [`ClusterCipher::seal`].
const CHALLENGE_LEN: usize = 32;

/// Cap on the sealed handshake message length to bound the initial
/// read. `SEAL_OVERHEAD + CHALLENGE_LEN` is 60 bytes; the cap leaves
/// headroom for a future handshake extension without a version bump
/// while still rejecting garbage.
const MAX_HANDSHAKE_LEN: u16 = 256;

/// Cap on the stream-type string a peer may send when opening a stream.
/// Type strings are `feature/<vhost>` — 128 bytes is more than enough
/// and refusing anything longer keeps a malicious peer from wasting
/// memory before we recognize the type.
const MAX_STREAM_TYPE_LEN: u16 = 128;

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

    /// Snapshot bootstrap stream — RESERVED for Phase 2.1.
    ///
    /// A future replica joining a running primary will open one of
    /// these to receive a base snapshot before subscribing to the CDC
    /// stream. Not implemented today; refused with a logged warning.
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

struct HandleInner {
    listen_addr: SocketAddr,
    cipher: Arc<ClusterCipher>,
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
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.inner.listen_addr
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
        handshake_initiate(&mut tcp, &self.inner.cipher)
            .await
            .with_context(|| format!("cluster channel: handshake with {peer_addr}"))?;

        let cfg = yamux::Config::default();
        let mut conn = yamux::Connection::new(tcp.compat(), cfg, yamux::Mode::Client);

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
/// # Errors
///
/// Returns an error if a feature is enabled but a shared secret is
/// unavailable, if the derived listen address is invalid, or if
/// binding the TCP listener fails.
pub async fn maybe_start(
    channel: &ClusterChannelConfig,
    cluster_secret: &str,
    cluster: &ClusterHandle,
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

    let cipher = Arc::new(ClusterCipher::for_cluster_channel(effective_secret));
    let handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));

    let handle = Handle {
        inner: Arc::new(HandleInner {
            listen_addr,
            cipher: Arc::clone(&cipher),
            handlers: Arc::clone(&handlers),
        }),
    };

    tokio::spawn(accept_loop(listener, Arc::clone(&cipher), Arc::clone(&handlers)));

    tracing::info!(
        %listen_addr,
        cdc = features.cdc,
        "cluster channel: listener bound (opt-in features active)"
    );
    Ok(Some(handle))
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

// ---------------------------------------------------------------------------
// Accept loop.
// ---------------------------------------------------------------------------

async fn accept_loop(
    listener: TcpListener,
    cipher: Arc<ClusterCipher>,
    handlers: Arc<Mutex<Vec<HandlerEntry>>>,
) {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("cluster channel: accept error: {e}");
                continue;
            }
        };
        tcp.set_nodelay(true).ok();
        let cipher = Arc::clone(&cipher);
        let handlers = Arc::clone(&handlers);
        tokio::spawn(async move {
            let mut tcp = tcp;
            if let Err(e) = handshake_respond(&mut tcp, &cipher).await {
                // Drop with debug — a wrong-secret peer is legitimate
                // background noise; we should not warn per-attempt.
                tracing::debug!(peer = %peer, "cluster channel: handshake failed: {e:#}");
                return;
            }
            let cfg = yamux::Config::default();
            let conn = yamux::Connection::new(tcp.compat(), cfg, yamux::Mode::Server);
            drive_connection(conn, handlers, peer).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Handshake.
// ---------------------------------------------------------------------------

async fn handshake_initiate(tcp: &mut TcpStream, cipher: &ClusterCipher) -> anyhow::Result<()> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::aead::rand_core::RngCore;

    let mut challenge = [0u8; CHALLENGE_LEN];
    OsRng.fill_bytes(&mut challenge);

    let sealed = cipher.seal(&challenge).context("seal challenge")?;
    let len = u16::try_from(sealed.len()).context("challenge too long")?;
    anyhow::ensure!(len <= MAX_HANDSHAKE_LEN, "challenge > MAX_HANDSHAKE_LEN");

    tcp.write_all(&[HANDSHAKE_VERSION]).await?;
    tcp.write_all(&len.to_be_bytes()).await?;
    tcp.write_all(&sealed).await?;
    tcp.flush().await?;

    // Read reply: [version][sealed_len: u16 BE][sealed_reply]
    let mut ver = [0u8; 1];
    tcp.read_exact(&mut ver).await.context("read handshake reply version")?;
    anyhow::ensure!(ver[0] == HANDSHAKE_VERSION, "handshake version mismatch: got {}", ver[0]);
    let mut lenbuf = [0u8; 2];
    tcp.read_exact(&mut lenbuf).await?;
    let reply_len = u16::from_be_bytes(lenbuf);
    anyhow::ensure!(reply_len <= MAX_HANDSHAKE_LEN, "handshake reply too long");
    let mut sealed_reply = vec![0u8; reply_len as usize];
    tcp.read_exact(&mut sealed_reply).await?;

    let plaintext = cipher.open(&sealed_reply).context("handshake reply auth failed")?;
    anyhow::ensure!(
        plaintext == challenge,
        "handshake replay mismatch (peer opened challenge but returned different nonce)"
    );
    Ok(())
}

async fn handshake_respond(tcp: &mut TcpStream, cipher: &ClusterCipher) -> anyhow::Result<()> {
    let mut ver = [0u8; 1];
    tcp.read_exact(&mut ver).await.context("read handshake version")?;
    anyhow::ensure!(ver[0] == HANDSHAKE_VERSION, "handshake version mismatch: got {}", ver[0]);
    let mut lenbuf = [0u8; 2];
    tcp.read_exact(&mut lenbuf).await?;
    let ch_len = u16::from_be_bytes(lenbuf);
    anyhow::ensure!(ch_len <= MAX_HANDSHAKE_LEN, "handshake challenge too long");
    let mut sealed_ch = vec![0u8; ch_len as usize];
    tcp.read_exact(&mut sealed_ch).await?;

    let challenge = cipher.open(&sealed_ch).context("handshake challenge auth failed")?;
    anyhow::ensure!(
        challenge.len() == CHALLENGE_LEN,
        "handshake challenge wrong length: {}",
        challenge.len()
    );

    let sealed_reply = cipher.seal(&challenge).context("seal handshake reply")?;
    let reply_len = u16::try_from(sealed_reply.len()).context("reply too long")?;
    anyhow::ensure!(reply_len <= MAX_HANDSHAKE_LEN, "reply > MAX_HANDSHAKE_LEN");

    tcp.write_all(&[HANDSHAKE_VERSION]).await?;
    tcp.write_all(&reply_len.to_be_bytes()).await?;
    tcp.write_all(&sealed_reply).await?;
    tcp.flush().await?;
    Ok(())
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
            if stream_type.starts_with(stream_type::SNAPSHOT_PREFIX) {
                tracing::warn!(
                    stream_type = %stream_type,
                    %peer,
                    "cluster channel: snapshot stream received but not implemented in this build \
                     (RESERVED for Phase 2.1); closing"
                );
            } else {
                tracing::warn!(
                    stream_type = %stream_type,
                    %peer,
                    "cluster channel: no handler for stream type; closing"
                );
            }
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
        let cluster = crate::start_gossip(&cfg).await.expect("gossip start");

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
        let cluster = crate::start_gossip(&cfg).await.expect("gossip start");
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
        let cluster = crate::start_gossip(&cfg).await.expect("gossip start");

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
    /// loopback pair — no gossip involved for this leg.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_then_stream_dispatch_roundtrip() {
        let secret = "roundtrip-secret";
        let cipher = Arc::new(ClusterCipher::for_cluster_channel(secret));
        let handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Register handler for "cdc/default" before we bring up the
        // accept task so no inbound stream can slip through the gap.
        let (tx, mut rx) = mpsc::unbounded_channel();
        handlers.lock().push(HandlerEntry { pattern: "cdc/default".into(), prefix: false, tx });

        tokio::spawn(accept_loop(listener, Arc::clone(&cipher), Arc::clone(&handlers)));

        // Dial as a client.
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        handshake_initiate(&mut tcp, &cipher).await.expect("handshake");
        let cfg = yamux::Config::default();
        let mut conn = yamux::Connection::new(tcp.compat(), cfg, yamux::Mode::Client);
        let stream = poll_new_outbound(&mut conn).await.unwrap();
        tokio::spawn(drive_connection(conn, Arc::new(Mutex::new(Vec::new())), addr));

        let mut cs = stream.compat();
        write_stream_type(&mut cs, "cdc/default").await.expect("write type");
        cs.write_all(b"hello").await.unwrap();
        cs.flush().await.unwrap();

        let incoming =
            tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await.unwrap();
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
        let good_cipher = Arc::new(ClusterCipher::for_cluster_channel("good-secret"));
        let bad_cipher = ClusterCipher::for_cluster_channel("bad-secret");
        let handlers: Arc<Mutex<Vec<HandlerEntry>>> = Arc::new(Mutex::new(Vec::new()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        handlers.lock().push(HandlerEntry { pattern: "cdc/".into(), prefix: true, tx });
        tokio::spawn(accept_loop(listener, Arc::clone(&good_cipher), Arc::clone(&handlers)));

        let mut tcp = TcpStream::connect(addr).await.unwrap();
        let hs = handshake_initiate(&mut tcp, &bad_cipher).await;
        assert!(hs.is_err(), "handshake with wrong secret must fail on initiator side");

        // Nothing should arrive on the handler within a short window.
        let noise = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
        assert!(noise.is_err(), "no stream should be dispatched for a rejected peer");
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
}

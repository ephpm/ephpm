//! Site-scoped WebSocket connection registry.
//!
//! ePHPm owns every WebSocket socket in Rust; PHP never holds one. This crate
//! is the shared table that lets the two halves meet:
//!
//! * `ephpm-server` accepts the HTTP upgrade, registers the connection here,
//!   and runs a per-connection session task that drains this connection's
//!   outbound queue onto the socket.
//! * `ephpm-php` exposes `ephpm_ws_send` / `ephpm_ws_broadcast` /
//!   `ephpm_ws_subscribe` / `ephpm_ws_unsubscribe` / `ephpm_ws_close` to
//!   userland, which resolve through this registry.
//!
//! It deliberately knows nothing about the WebSocket wire protocol — no
//! framing, no masking, no handshake. [`OutFrame`] is the whole vocabulary, so
//! this crate builds and unit-tests in stub mode (no PHP, no tungstenite).
//!
//! # The two invariants
//!
//! **1. Every lookup is site-scoped.** Connection IDs and channel names are
//! only meaningful *within* a site. [`Registry::send`], [`Registry::broadcast`],
//! [`Registry::subscribe`], [`Registry::unsubscribe`] and [`Registry::close`]
//! all take the **calling** request's site scope, and a connection registered
//! under site `a` is unreachable from site `b` even when `b` knows the exact
//! connection ID. The scope is never taken from a function argument: the bridge
//! reads it from the per-thread context the router installed for the request
//! that is executing (mirroring `ephpm_db_*`), and a request with no site
//! identity gets no scope and therefore no capability at all.
//!
//! A cross-site attempt is reported as "not found", identical to a stale ID —
//! there is no oracle that tells site `b` whether an ID exists elsewhere.
//!
//! **2. No queue is unbounded.** Each connection gets a bounded
//! [`tokio::sync::mpsc`] channel sized by [`Limits::send_queue`]. Producers use
//! `try_send` only. A producer that finds the queue full does **not** block and
//! does **not** grow a buffer: it marks the connection for shedding and returns
//! failure, and the session task closes that socket with WebSocket status
//! `1013 Try Again Later`. One slow reader costs one connection, never the
//! server's memory.

mod id;
mod registry;

use bytes::Bytes;
pub use id::{CONNECTION_ID_LEN, new_connection_id};
pub use registry::{
    Control, MAX_CHANNEL_NAME_LEN, MAX_CHANNELS_PER_CONNECTION, Registered, Registry, RegistryStats,
};

/// WebSocket close code sent to a connection whose outbound queue overflowed.
///
/// `1013 Try Again Later` (RFC 6455 §7.4.1 / IANA registry) is the honest
/// signal: the server is not at fault and the client may reconnect. It is
/// deliberately distinct from a policy close so an operator can tell
/// back-pressure shedding apart from an application-initiated close in logs.
pub const CLOSE_QUEUE_OVERFLOW: u16 = 1013;

/// Default close code for [`Registry::close`] when userland passes none.
pub const CLOSE_NORMAL: u16 = 1000;

/// An outbound frame queued for one connection.
///
/// Cheap to clone: the payload is [`Bytes`], so fanning one broadcast out to
/// N subscribers bumps a refcount N times instead of copying N payloads.
#[derive(Debug, Clone)]
pub enum OutFrame {
    /// A text frame. The payload is **not** validated as UTF-8 here; the
    /// server-side session task rejects non-UTF-8 text before it reaches the
    /// wire.
    Text(Bytes),
    /// A binary frame.
    Binary(Bytes),
}

impl OutFrame {
    /// Build a frame from a payload and userland's `binary` flag.
    #[must_use]
    pub fn new(payload: Bytes, binary: bool) -> Self {
        if binary { Self::Binary(payload) } else { Self::Text(payload) }
    }

    /// Payload byte length, for metrics and limit checks.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Text(b) | Self::Binary(b) => b.len(),
        }
    }

    /// Whether the payload is empty. (An empty frame is legal on the wire.)
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Resource bounds for the registry, resolved from `[server.websocket]`.
///
/// `0` means "no limit" for both connection caps. [`Limits::send_queue`] has no
/// unlimited setting on purpose — an unbounded outbound queue is exactly the
/// failure this crate exists to prevent — so `0` is normalized to `1`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum concurrent WebSocket connections across all sites. `0` =
    /// unlimited.
    pub max_connections: usize,
    /// Maximum concurrent WebSocket connections for any single site. `0` =
    /// unlimited. Enforced in addition to [`Limits::max_connections`], so one
    /// tenant cannot consume a shared deployment's whole budget.
    pub max_connections_per_site: usize,
    /// Capacity of each connection's outbound frame queue, in frames. Overflow
    /// closes the connection with [`CLOSE_QUEUE_OVERFLOW`].
    pub send_queue: usize,
    /// Maximum payload size, in bytes, of a frame PHP may push. `0` =
    /// unlimited.
    ///
    /// This is the **outbound** half of `[server.websocket] max_message_size`;
    /// the inbound half is enforced by the WebSocket codec in `ephpm-server`.
    /// Checked here rather than at the socket so an oversized payload is
    /// refused before it is queued — a 100 MiB frame must not be admitted into
    /// a 64-deep queue and only discovered on write.
    pub max_message_size: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 10_000,
            max_connections_per_site: 1_000,
            send_queue: 64,
            max_message_size: 1024 * 1024,
        }
    }
}

/// Why [`Registry::register`] refused a new connection.
///
/// The server maps each variant to an HTTP status on the upgrade request, so a
/// client learns it was shed rather than silently getting a dead socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    /// The global [`Limits::max_connections`] cap is full.
    #[error("websocket connection limit reached")]
    ServerFull,
    /// This site's [`Limits::max_connections_per_site`] cap is full.
    #[error("websocket connection limit reached for this site")]
    SiteFull,
    /// The OS entropy source was unavailable, so no unguessable connection ID
    /// could be minted.
    ///
    /// Deliberately fatal for the connection rather than falling back to a
    /// counter: a predictable ID is a cross-connection send capability handed
    /// to anyone who can count.
    #[error("failed to draw connection-id entropy from the OS")]
    Entropy,
}

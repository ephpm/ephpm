//! Generic async connection pool for TCP streams.
//!
//! The pool manages a bounded set of pre-authenticated backend connections.
//! Connections are returned to the idle queue after use (provided the caller
//! runs a reset first). Background tasks maintain `min_connections` and
//! enforce `idle_timeout` / `max_lifetime`.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use crate::error::DbError;

/// A boxed, send future. Used as the return type for closures stored in the pool.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Ping an idle connection at checkout when it has been parked at least this
/// long.
///
/// A pooled backend can die while it sits in the idle queue — the server
/// restarts, an idle timeout fires, a firewall reaps the flow — and nothing
/// in the pool notices until the next caller writes to it and gets
/// `BrokenPipe`. The background health check only samples every
/// `health_check_interval` (30s by default), which leaves a window in which
/// every checkout hands out a corpse.
///
/// Validating on *every* checkout would be correct but costs a full backend
/// round trip (`COM_PING` / `SELECT 1`, roughly 50–100µs on loopback) on the
/// hot path, which is the same order as the query it precedes. A connection
/// handed back and immediately re-acquired cannot have died in the interim
/// for any reason the ping would detect, so the check is skipped below this
/// threshold: under sustained load the pool never pays for it, and a quiet
/// site — the exact shape that exposes dead idle connections — always does.
const VALIDATE_AFTER_IDLE: Duration = Duration::from_millis(500);

/// Whether a pooled backend still has unread bytes waiting on it.
///
/// A client that ends its session without draining the response to its last
/// command leaves the backend mid-conversation. The connection is perfectly
/// alive, so nothing on the liveness path notices, but the leftover bytes
/// would be read as the *next* session's response. Neither a `MySQL` server
/// nor a pooled `PostgreSQL` connection sends unsolicited traffic in normal
/// operation, so anything readable at end-of-session is leftover data and the
/// connection must be discarded rather than recycled.
///
/// Non-blocking: a single `try_read`. It consumes a byte when data is present,
/// which is harmless precisely because the caller discards the connection in
/// that case.
///
/// Best-effort, and callers must treat it as such: `try_read` consults tokio's
/// readiness cache and can report `WouldBlock` for data that has arrived but
/// whose readiness event was lost when the reader half was cancelled. It may
/// therefore miss a desynchronised connection; it will never falsely condemn a
/// clean one.
pub(crate) fn has_unread_bytes(stream: &TcpStream) -> bool {
    let mut probe = [0u8; 1];
    match stream.try_read(&mut probe) {
        // Nothing waiting — the only case in which the connection is reusable.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        // `Ok(n > 0)` is leftover data, `Ok(0)` is EOF, and any other error
        // means the socket is unhealthy. None of them are reusable.
        Ok(_) | Err(_) => true,
    }
}

/// Configuration parameters for the pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Keep at least this many connections open at all times.
    pub min_connections: u32,
    /// Never exceed this many concurrent checkouts.
    ///
    /// This also bounds live backend connections: a connection is only ever
    /// dialled while its caller holds a permit, so the number in existence
    /// cannot outgrow the peak number of simultaneous checkouts.
    pub max_connections: u32,
    /// Close idle connections that have been idle longer than this.
    pub idle_timeout: Duration,
    /// Close connections older than this regardless of activity.
    pub max_lifetime: Duration,
    /// How long to wait for a connection before returning [`DbError::PoolTimeout`].
    pub pool_timeout: Duration,
    /// Interval between background health checks on idle connections.
    pub health_check_interval: Duration,
}

/// An idle slot in the pool.
///
/// Deliberately holds no semaphore permit. A permit represents *a checkout in
/// progress*, not *a connection that exists* — see [`Pool::acquire`] for why
/// the distinction is load-bearing.
struct Slot {
    stream: TcpStream,
    /// Time the backend connection was first established.
    created_at: Instant,
    /// Time the connection was last returned to the pool.
    last_used: Instant,
}

struct PoolState {
    idle: Mutex<VecDeque<Slot>>,
    closed: AtomicBool,
}

/// Shared connection pool.
///
/// Clone is cheap — it shares the same internal state via [`Arc`].
#[derive(Clone)]
pub struct Pool {
    state: Arc<PoolState>,
    pub semaphore: Arc<Semaphore>,
    config: Arc<PoolConfig>,
    /// Called to create a new authenticated backend connection.
    connect: Arc<dyn Fn() -> BoxFuture<Result<TcpStream, DbError>> + Send + Sync>,
    /// Called to reset a connection before returning it to idle.
    ///
    /// Used by [`Pool::recycle_with_reset`] to run a protocol-level reset
    /// (e.g. `COM_RESET_CONNECTION`) before parking the connection.
    reset: Arc<dyn Fn(TcpStream) -> BoxFuture<Result<TcpStream, DbError>> + Send + Sync>,
    /// Called to check whether an idle connection is still alive.
    /// Returns `(stream, is_alive)` on success, or an error on I/O failure.
    #[allow(clippy::type_complexity)]
    ping: Arc<dyn Fn(TcpStream) -> BoxFuture<Result<(TcpStream, bool), DbError>> + Send + Sync>,
}

impl Pool {
    /// Create a new pool.
    ///
    /// - `connect` — async closure that establishes and authenticates a fresh backend connection.
    /// - `reset` — async closure that resets session state (e.g. `COM_RESET_CONNECTION`).
    ///   Returns the stream on success; on failure the connection is discarded.
    /// - `ping` — async closure that verifies the connection is alive.
    ///   Returns `(stream, true)` if healthy, `(stream, false)` if dead.
    pub fn new(
        config: PoolConfig,
        connect: impl Fn() -> BoxFuture<Result<TcpStream, DbError>> + Send + Sync + 'static,
        reset: impl Fn(TcpStream) -> BoxFuture<Result<TcpStream, DbError>> + Send + Sync + 'static,
        ping: impl Fn(TcpStream) -> BoxFuture<Result<(TcpStream, bool), DbError>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let max = config.max_connections as usize;
        Self {
            state: Arc::new(PoolState {
                idle: Mutex::new(VecDeque::with_capacity(config.min_connections as usize)),
                closed: AtomicBool::new(false),
            }),
            semaphore: Arc::new(Semaphore::new(max)),
            config: Arc::new(config),
            connect: Arc::new(connect),
            reset: Arc::new(reset),
            ping: Arc::new(ping),
        }
    }

    /// Start background maintenance tasks (min-connections warmer, health checker).
    ///
    /// Returns a handle to an [`tokio::task::JoinHandle`] that runs until the pool
    /// is closed. Call [`Pool::close`] to stop it.
    #[must_use]
    pub fn start_background_tasks(&self) -> tokio::task::JoinHandle<()> {
        let pool = self.clone();
        let interval = pool.config.health_check_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if pool.state.closed.load(Ordering::Acquire) {
                    break;
                }
                pool.maintenance().await;
            }
        })
    }

    /// Number of connections currently parked in the idle queue.
    ///
    /// Does not count connections that are checked out. Intended for tests and
    /// diagnostics — the value is a snapshot and may be stale the moment it is
    /// returned.
    #[must_use]
    pub fn idle_len(&self) -> usize {
        self.state.idle.lock().len()
    }

    /// Acquire a connection from the pool, waiting up to `pool_timeout`.
    ///
    /// Tries the idle queue first; creates a new connection if the queue is empty
    /// (within `max_connections` limit).
    ///
    /// Idle connections that have been parked longer than
    /// [`VALIDATE_AFTER_IDLE`] are pinged before being handed out; one that
    /// fails is discarded and the next candidate (or a fresh dial) is used
    /// instead. A caller therefore never receives a connection the pool has
    /// already observed to be dead.
    ///
    /// # Permit accounting
    ///
    /// A permit means "a checkout is in progress", not "a connection exists".
    /// Idle connections hold none. This matters: permits were once parked with
    /// idle slots, which made returning a connection *consume* a permit rather
    /// than release one. Once `in-use + idle` reached `max_connections`,
    /// nothing on the healthy path could free a permit again and every later
    /// acquire blocked until `pool_timeout` — a pool full of perfectly good
    /// connections that it could not hand out. That deadlock was masked for as
    /// long as the proxy was killing connections on every request, because the
    /// resulting discards released permits continuously.
    ///
    /// # Errors
    ///
    /// - [`DbError::PoolClosed`] if the pool has been shut down.
    /// - [`DbError::PoolTimeout`] if no connection became available in time.
    /// - Propagates connection errors from the `connect` closure.
    pub async fn acquire(&self) -> Result<Checkout, DbError> {
        if self.state.closed.load(Ordering::Acquire) {
            return Err(DbError::PoolClosed);
        }

        // Fast path: try_acquire_owned skips the tokio timer + notify machinery
        // when a permit is immediately available (the overwhelmingly common
        // case under a warm, correctly-sized pool). Falls back to the timeout
        // wait when the pool is saturated.
        let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(p) => p,
            Err(tokio::sync::TryAcquireError::NoPermits) => tokio::time::timeout(
                self.config.pool_timeout,
                Arc::clone(&self.semaphore).acquire_owned(),
            )
            .await
            .map_err(|_| DbError::PoolTimeout { max: self.config.max_connections })?
            .map_err(|_| DbError::PoolClosed)?,
            Err(tokio::sync::TryAcquireError::Closed) => return Err(DbError::PoolClosed),
        };

        // Try an idle connection. Skip expired ones.
        loop {
            let slot = self.state.idle.lock().pop_front();
            let Some(slot) = slot else { break };

            // Drop connections that have exceeded max_lifetime or idle_timeout.
            let age_ok = slot.created_at.elapsed() < self.config.max_lifetime;
            let idle_ok = slot.last_used.elapsed() < self.config.idle_timeout;

            if age_ok && idle_ok {
                // Validate before handing out. A connection that has been
                // parked for a while may have been closed by the server with
                // no local indication; writing to it is how the caller finds
                // out, and by then the request has already failed.
                let stream = if slot.last_used.elapsed() >= VALIDATE_AFTER_IDLE {
                    match (self.ping)(slot.stream).await {
                        Ok((stream, true)) => stream,
                        Ok((_, false)) => {
                            debug!("idle connection failed checkout validation, discarding");
                            continue;
                        }
                        Err(e) => {
                            debug!("idle connection validation I/O error, discarding: {e}");
                            continue;
                        }
                    }
                } else {
                    slot.stream
                };

                return Ok(Checkout {
                    stream: Some(stream),
                    permit: Some(permit),
                    created_at: slot.created_at,
                    pool: self.clone(),
                });
            }
            // Expired: discard the stream and try the next candidate.
            debug!("discarding expired idle connection");
        }

        // No suitable idle connection — open a new one.
        let stream = (self.connect)().await?;
        Ok(Checkout {
            stream: Some(stream),
            permit: Some(permit),
            created_at: Instant::now(),
            pool: self.clone(),
        })
    }

    /// Return a stream to the idle pool after a successful reset.
    ///
    /// The caller must run the protocol-level reset (`COM_RESET_CONNECTION` /
    /// `DISCARD ALL`) before calling this. Pass the post-reset stream.
    ///
    /// The caller must also have established that the connection is still
    /// live. Re-parking a connection the backend has already closed poisons
    /// the idle queue: the next caller draws the corpse, fails, and — if its
    /// failure path also recycles — puts it straight back. See
    /// [`crate::mysql`] for the session-termination interception that keeps
    /// this invariant on the proxy paths.
    pub(crate) fn recycle(
        &self,
        stream: TcpStream,
        created_at: Instant,
        permit: OwnedSemaphorePermit,
    ) {
        let slot = Slot { stream, created_at, last_used: Instant::now() };
        self.state.idle.lock().push_back(slot);
        // Release the permit *after* the connection is visible in the idle
        // queue, so a caller woken by this release is guaranteed to find it.
        // Dropping it is the point: returning a connection has to hand a
        // permit back to whoever is waiting, not consume one.
        drop(permit);
    }

    /// Reset a connection using the pool's reset closure and return it to idle.
    ///
    /// Runs the protocol-level reset (e.g. `COM_RESET_CONNECTION`) before
    /// parking the connection. If the reset fails, the connection is discarded
    /// and the permit is freed.
    pub(crate) async fn recycle_with_reset(
        &self,
        stream: TcpStream,
        created_at: Instant,
        permit: OwnedSemaphorePermit,
    ) {
        match (self.reset)(stream).await {
            Ok(stream) => {
                self.recycle(stream, created_at, permit);
            }
            Err(e) => {
                debug!("pool reset failed, discarding connection: {e}");
                // permit dropped → semaphore slot freed
            }
        }
    }

    /// Shut down the pool: drain idle connections and reject new `acquire()` calls.
    pub fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
        let mut idle = self.state.idle.lock();
        idle.clear(); // drops streams + permits
    }

    /// Background: warm the pool to `min_connections` and check health.
    async fn maintenance(&self) {
        self.prune_idle();
        self.warm().await;
        self.health_check().await;
    }

    /// Remove idle connections that have exceeded `idle_timeout` or `max_lifetime`.
    fn prune_idle(&self) {
        let mut idle = self.state.idle.lock();
        idle.retain(|s| {
            s.created_at.elapsed() < self.config.max_lifetime
                && s.last_used.elapsed() < self.config.idle_timeout
        });
    }

    /// Create new connections until we reach `min_connections`.
    async fn warm(&self) {
        let current_idle = u32::try_from(self.state.idle.lock().len()).unwrap_or(u32::MAX);
        if current_idle >= self.config.min_connections {
            return;
        }
        // Warm-up connections are parked directly rather than checked out, so
        // they are not covered by a permit. Bound them by the semaphore's free
        // capacity so warming can never push the live connection count past
        // `max_connections` while checkouts are in flight.
        let headroom = self.semaphore.available_permits().saturating_sub(current_idle as usize);
        let needed = usize::try_from(self.config.min_connections - current_idle)
            .unwrap_or(usize::MAX)
            .min(headroom);
        for _ in 0..needed {
            if self.state.closed.load(Ordering::Acquire) {
                break;
            }
            match (self.connect)().await {
                Ok(stream) => {
                    self.state.idle.lock().push_back(Slot {
                        stream,
                        created_at: Instant::now(),
                        last_used: Instant::now(),
                    });
                }
                Err(e) => {
                    warn!("pool warm-up connection failed: {e}");
                }
            }
        }
    }

    /// Ping idle connections and remove any that are dead.
    async fn health_check(&self) {
        // Collect all idle slots for checking.
        let slots: Vec<Slot> = self.state.idle.lock().drain(..).collect();
        let mut healthy = VecDeque::with_capacity(slots.len());

        for slot in slots {
            match (self.ping)(slot.stream).await {
                Ok((stream, true)) => {
                    healthy.push_back(Slot {
                        stream,
                        created_at: slot.created_at,
                        last_used: slot.last_used,
                    });
                }
                Ok((_, false)) => {
                    debug!("health check failed: dropping connection");
                }
                Err(e) => {
                    debug!("health check I/O error: {e}");
                }
            }
        }

        *self.state.idle.lock() = healthy;
    }
}

/// A connection checked out from the pool.
///
/// When dropped without calling [`Checkout::return_to_pool`], the connection
/// is discarded and the pool slot is freed.
pub struct Checkout {
    /// The TCP stream. `None` only after [`Checkout::take_stream`] has been called.
    pub stream: Option<TcpStream>,
    pub permit: Option<OwnedSemaphorePermit>,
    /// When the underlying backend connection was first opened.
    pub created_at: Instant,
    pub pool: Pool,
}

impl Checkout {
    /// Extract the stream for bidirectional proxying.
    ///
    /// The caller is responsible for calling [`Checkout::retire`] or
    /// [`Checkout::return_to_pool`] after use.
    ///
    /// # Panics
    ///
    /// Panics if called more than once on the same [`Checkout`].
    #[must_use]
    pub fn take_stream(&mut self) -> TcpStream {
        self.stream.take().expect("stream already taken")
    }

    /// Return the (reset) stream to the pool for reuse.
    ///
    /// Consumes the checkout. The caller must have already run the
    /// protocol-level reset on `stream`.
    pub fn return_to_pool(mut self, stream: TcpStream) {
        if let Some(permit) = self.permit.take() {
            self.pool.recycle(stream, self.created_at, permit);
        }
    }

    /// Reset the connection using the pool's reset closure and return it.
    ///
    /// Runs the protocol-level reset (e.g. `COM_RESET_CONNECTION`) before
    /// parking the connection. If the reset fails, the connection is
    /// discarded and the pool slot is freed.
    pub async fn return_with_reset(mut self, stream: TcpStream) {
        if let Some(permit) = self.permit.take() {
            self.pool.recycle_with_reset(stream, self.created_at, permit).await;
        }
    }

    /// Discard the connection and free the pool slot.
    pub fn retire(self) {
        // permit dropped → semaphore slot freed
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        // If the stream was not taken, it is dropped here (connection closed).
        // The permit is also dropped, freeing the pool slot.
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::*;

    /// Connect a client/server socket pair over loopback.
    async fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).await.expect("connect");
        let (server, _) = listener.accept().await.expect("accept");
        (client, server)
    }

    /// A connection whose peer has sent nothing is clean and must be reusable.
    /// A false positive here would discard every healthy pooled connection.
    #[tokio::test]
    async fn quiet_connection_has_no_unread_bytes() {
        let (client, _server) = socket_pair().await;
        assert!(!has_unread_bytes(&client), "an idle connection must not be condemned");
    }

    /// Leftover response data must condemn the connection — that is the whole
    /// point: those bytes would be read as the next session's reply.
    #[tokio::test]
    async fn pending_data_is_detected() {
        let (client, mut server) = socket_pair().await;
        server.write_all(b"leftover").await.expect("write");
        client.readable().await.expect("readable");
        assert!(has_unread_bytes(&client), "buffered peer data must condemn the connection");
    }

    /// A closed peer reads as EOF, which is likewise not reusable.
    #[tokio::test]
    async fn closed_peer_is_detected() {
        let (client, server) = socket_pair().await;
        drop(server);
        client.readable().await.expect("readable");
        assert!(has_unread_bytes(&client), "a peer that closed must condemn the connection");
    }
}

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

/// Configuration parameters for the pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Keep at least this many connections open at all times.
    pub min_connections: u32,
    /// Never exceed this many connections (in-use + idle).
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
struct Slot {
    stream: TcpStream,
    /// Time the backend connection was first established.
    created_at: Instant,
    /// Time the connection was last returned to the pool.
    last_used: Instant,
    /// The semaphore permit "parked" with this idle connection.
    permit: OwnedSemaphorePermit,
}

struct PoolState {
    idle: Mutex<VecDeque<Slot>>,
    closed: AtomicBool,
}

/// Shared connection pool.
///
/// Clone is cheap — it shares the same internal state via [`Arc`].
#[derive(Clone)]
#[allow(dead_code)]
pub struct Pool {
    state: Arc<PoolState>,
    pub semaphore: Arc<Semaphore>,
    config: Arc<PoolConfig>,
    /// Called to create a new authenticated backend connection.
    connect: Arc<dyn Fn() -> BoxFuture<Result<TcpStream, DbError>> + Send + Sync>,
    /// Called to reset a connection before returning it to idle.
    ///
    /// Currently the caller runs the protocol-level reset externally before
    /// calling [`Pool::recycle`]. This closure will be used once the pool
    /// manages reset internally (automatic reset-on-return).
    #[allow(dead_code)]
    reset: Arc<dyn Fn(TcpStream) -> BoxFuture<Result<TcpStream, DbError>> + Send + Sync>,
    /// Called to check whether an idle connection is still alive.
    /// Returns `(stream, is_alive)` on success, or an error on I/O failure.
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
        ping: impl Fn(TcpStream) -> BoxFuture<Result<(TcpStream, bool), DbError>> + Send + Sync + 'static,
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

    /// Acquire a connection from the pool, waiting up to `pool_timeout`.
    ///
    /// Tries the idle queue first; creates a new connection if the queue is empty
    /// (within `max_connections` limit).
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

        let permit = tokio::time::timeout(
            self.config.pool_timeout,
            Arc::clone(&self.semaphore).acquire_owned(),
        )
        .await
        .map_err(|_| DbError::PoolTimeout { max: self.config.max_connections })?
        .map_err(|_| DbError::PoolClosed)?; // semaphore closed

        // Try an idle connection. Skip expired ones.
        loop {
            let slot = self.state.idle.lock().pop_front();
            let Some(slot) = slot else { break };

            // Drop connections that have exceeded max_lifetime or idle_timeout.
            let age_ok = slot.created_at.elapsed() < self.config.max_lifetime;
            let idle_ok = slot.last_used.elapsed() < self.config.idle_timeout;

            if age_ok && idle_ok {
                // Discard the parked permit (we now hold `permit` from above).
                drop(slot.permit);
                return Ok(Checkout {
                    stream: Some(slot.stream),
                    permit: Some(permit),
                    created_at: slot.created_at,
                    pool: self.clone(),
                });
            }
            // Expired: discard both the stream and its parked permit.
            debug!("discarding expired idle connection");
            drop(slot.permit);
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
    pub(crate) fn recycle(&self, stream: TcpStream, created_at: Instant, permit: OwnedSemaphorePermit) {
        let slot = Slot {
            stream,
            created_at,
            last_used: Instant::now(),
            permit,
        };
        self.state.idle.lock().push_back(slot);
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
        let current_idle = self.state.idle.lock().len() as u32;
        if current_idle >= self.config.min_connections {
            return;
        }
        let needed = self.config.min_connections - current_idle;
        for _ in 0..needed {
            if self.state.closed.load(Ordering::Acquire) {
                break;
            }
            // Try to grab a permit without blocking.
            let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() else {
                break; // at max_connections
            };
            match (self.connect)().await {
                Ok(stream) => {
                    self.state.idle.lock().push_back(Slot {
                        stream,
                        created_at: Instant::now(),
                        last_used: Instant::now(),
                        permit,
                    });
                }
                Err(e) => {
                    warn!("pool warm-up connection failed: {e}");
                    // permit is dropped, freeing the slot
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
                        permit: slot.permit,
                    });
                }
                Ok((_, false)) => {
                    debug!("health check failed: dropping connection");
                    // permit dropped here
                }
                Err(e) => {
                    debug!("health check I/O error: {e}");
                    // permit dropped here
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

//! Dedicated per-request FPM execution pool (experimental `[php] fpm_engine =
//! "pool"`).
//!
//! An **opt-in** alternative to the default `spawn_blocking` FPM path. A fixed
//! pool of dedicated OS threads (`std::thread`, NOT `spawn_blocking`) each pull
//! one per-request job off a bounded [`async_channel`] dispatch queue, run
//! exactly one PHP request, reply on a [`tokio::sync::oneshot`], and loop. This
//! is the php-fpm model — every request runs a full
//! `php_request_startup`/`shutdown` cycle inside [`PhpRuntime::execute`], so
//! framework state never leaks across requests — but on threads ePHPm owns
//! rather than tokio's shared blocking pool.
//!
//! # Why dedicated threads
//!
//! Parking N threads forever in tokio's `spawn_blocking` pool would starve the
//! shared pool that also serves static-file I/O and other blocking work. A
//! dedicated pool bounds PHP concurrency to its own thread count without
//! touching that shared resource — the same reason [`crate::worker_pool`] uses
//! `std::thread`.
//!
//! # Parity with the default engine
//!
//! The pool is deliberately dumb about *what* a request does: the router hands
//! it a boxed closure that is the **identical** per-request body the
//! `spawn_blocking` path runs (per-site DB session swap, KV keyspace,
//! `open_basedir`/temp/session INI, OPcache invalidation, and the
//! `PhpRuntime::execute` bailout crash guard). Parity is therefore guaranteed
//! by construction — the same code runs; only the thread it runs on differs.
//!
//! # Concurrency, backpressure, and failure mapping (mirrors `worker_pool`)
//!
//! - **Concurrency cap:** the pool size ([`PhpConfig::effective_worker_count`])
//!   is the cap. The `[php] workers` semaphore is redundant and bypassed.
//! - **Backpressure → 504:** the dispatch queue is bounded. When it is full,
//!   [`FpmPool::dispatch`] suspends; the outer request timeout turns a starved
//!   queue into a 504.
//! - **Draining / no live threads → 503:** [`DispatchClosed`] from `dispatch`.
//! - **Wedged thread → 504 + replace:** the router bounds the `oneshot` wait; a
//!   timeout calls [`FpmPool::note_hung`], which spawns a replacement and
//!   abandons the stuck thread (a wedged PHP thread cannot be killed without
//!   corrupting the ZMM — replace, don't kill).
//! - **Graceful drain:** [`FpmPool::drain`] closes the dispatch sender; each
//!   thread's `recv_blocking` then returns `Err`, the loop ends, and the thread
//!   releases its TSRM slot ([`PhpRuntime::worker_thread_shutdown`]) before
//!   exiting so `php_embed_shutdown()` is safe once `live` reaches 0 (#266).
//!
//! Actual poison-thread *retirement* — replacing a thread whose PHP context is
//! corrupt after a contained crash — is a deferred follow-up. For now a
//! contained bailout is surfaced to the caller as a 500 exactly as the
//! `spawn_blocking` path does (via `PhpRuntime::execute`'s `Result`), and the
//! thread keeps serving. The one exception is a *Rust* panic inside the job
//! (not a PHP bailout, which is caught in C): that leaves the context in an
//! unknown state, so the thread retires and a replacement is spawned.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ephpm_php::response::PhpResponse;
use ephpm_php::{PhpError, PhpRuntime};
use metrics::{counter, gauge};
use tokio::sync::oneshot;

/// The outcome of one PHP request — exactly what [`PhpRuntime::execute`]
/// returns. A contained bailout is `Err(PhpError::Bailout)`, which the router
/// turns into a 500 via `build_php_response` (the same arm the `spawn_blocking`
/// path uses).
pub type FpmExecOutput = Result<PhpResponse, PhpError>;

/// The per-request work handed to a pool thread: the router-built closure that
/// runs the identical body the `spawn_blocking` path runs.
pub type FpmTask = Box<dyn FnOnce() -> FpmExecOutput + Send + 'static>;

/// The dispatch channel is closed (pool draining or all threads gone). The
/// router turns this into a 503.
#[derive(Debug, Clone, Copy)]
pub struct DispatchClosed;

/// One queued request: the work to run plus where to send its response.
struct FpmJob {
    run: FpmTask,
    respond_to: oneshot::Sender<FpmExecOutput>,
}

/// Handle to the running FPM pool. Cheap to clone via `Arc`.
pub struct FpmPool {
    /// Dispatch queue: the hyper handler `send().await`s jobs here; pool
    /// threads `recv_blocking()`. Bounded — a full queue applies HTTP
    /// backpressure (the outer request timeout turns a starved queue into 504).
    dispatch_tx: async_channel::Sender<FpmJob>,
    /// Kept alive so the channel never closes while the supervisor respawns
    /// threads. Cloned into each pool thread.
    dispatch_rx: async_channel::Receiver<FpmJob>,
    /// Jobs enqueued but not yet pulled. Incremented on enqueue, decremented by
    /// a thread right after `recv_blocking`. Backs `ephpm_fpm_pool_queue_depth`.
    queue_depth: Arc<AtomicUsize>,
    /// Shared runtime state (readiness, liveness, drain flag).
    state: Arc<PoolState>,
    /// Target number of live pool threads (the concurrency cap).
    thread_count: usize,
}

/// Shared, atomically-updated pool state.
struct PoolState {
    /// Threads that have registered with TSRM and are pulling work.
    ready: AtomicUsize,
    /// Live pool threads (registered or registering). Decremented only *after*
    /// a thread releases its TSRM slot, so `0` during drain means every thread
    /// is fully retired and `php_embed_shutdown()` is safe (#266).
    live: AtomicUsize,
    /// Consecutive TSRM-init failures, for respawn backoff.
    boot_failures: AtomicUsize,
    /// Set when draining — supervisors stop respawning.
    draining: AtomicBool,
    /// Monotonic id source for threads (logging / metric context).
    next_id: AtomicUsize,
}

impl FpmPool {
    /// Spawn the pool with `thread_count` dedicated OS threads.
    ///
    /// Returns immediately; [`FpmPool::ready_count`] rises to `thread_count` as
    /// threads register with TSRM. `thread_count == 0` spawns no threads (used
    /// by the plumbing tests) — every dispatch then queues until backpressure /
    /// the request timeout answers it.
    #[must_use]
    pub fn spawn(thread_count: usize, backlog: usize) -> Arc<Self> {
        let (dispatch_tx, dispatch_rx) = async_channel::bounded(backlog.max(1));
        let state = Arc::new(PoolState {
            ready: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            boot_failures: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            next_id: AtomicUsize::new(0),
        });

        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_fpm_pool_size").set(thread_count as f64);
        gauge!("ephpm_fpm_pool_queue_depth").set(0.0);

        let pool = Arc::new(Self {
            dispatch_tx,
            dispatch_rx,
            queue_depth: Arc::new(AtomicUsize::new(0)),
            state,
            thread_count,
        });

        for _ in 0..thread_count {
            pool.spawn_thread();
        }

        tracing::info!(
            thread_count,
            backlog = backlog.max(1),
            "fpm execution pool started (experimental [php] fpm_engine = \"pool\")"
        );

        pool
    }

    /// Number of threads registered with TSRM and serving. Drives readiness.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.state.ready.load(Ordering::Acquire)
    }

    /// Number of live pool OS threads (including registering / retiring ones).
    /// Reaches `0` only after every thread has released its TSRM slot, so it is
    /// the signal graceful shutdown waits on before PHP teardown (#266).
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.state.live.load(Ordering::Acquire)
    }

    /// Current dispatch-queue depth (enqueued, not yet pulled). Test-only.
    #[cfg(test)]
    fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Dispatch one request to the pool and return the receiver for its
    /// response.
    ///
    /// `send().await` suspends when the bounded queue is full (backpressure);
    /// the caller wraps the whole thing in the outer request timeout, so a
    /// starved queue becomes a 504 rather than an unbounded wait.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchClosed`] if the pool is draining / all threads are gone
    /// (the dispatch channel is closed) — the caller should 503.
    pub async fn dispatch(
        &self,
        run: FpmTask,
    ) -> Result<oneshot::Receiver<FpmExecOutput>, DispatchClosed> {
        let (tx, rx) = oneshot::channel();
        let job = FpmJob { run, respond_to: tx };
        // Account the enqueue before the (awaitable) send so a thread pulling
        // concurrently can only ever decrement a value we already added.
        let depth = self.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_fpm_pool_queue_depth").set(depth as f64);
        match self.dispatch_tx.send(job).await {
            Ok(()) => Ok(rx),
            Err(_) => {
                // Channel closed — the job never entered the queue and no thread
                // will pull it, so undo the accounting.
                self.queue_depth.fetch_sub(1, Ordering::Relaxed);
                Err(DispatchClosed)
            }
        }
    }

    /// Record that a thread appears hung (its `oneshot` timed out). The stuck
    /// thread is abandoned and a replacement is spawned to keep the pool at
    /// `thread_count` live pullers.
    pub fn note_hung(self: &Arc<Self>) {
        counter!("ephpm_fpm_pool_recycles_total", "reason" => "hung").increment(1);
        // The stuck thread still holds its dispatch-receiver clone and may
        // eventually finish; we simply add capacity. A brief over-provision is
        // preferable to a wedged pool.
        if !self.state.draining.load(Ordering::Acquire) {
            self.spawn_thread();
            tracing::warn!(
                "fpm pool thread appeared hung — spawned replacement, abandoned stuck thread"
            );
        }
    }

    /// Begin graceful drain: stop accepting new jobs and let threads exit once
    /// their in-flight request (if any) completes. Idempotent.
    pub fn drain(&self) {
        if self.state.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        // Closing the sender makes each thread's recv_blocking return Err, so
        // the loop ends after any in-flight request completes.
        self.dispatch_tx.close();
        tracing::info!("fpm execution pool draining — dispatch closed");
    }

    /// Spawn one dedicated OS thread that registers with TSRM, serves requests
    /// until drain / panic-retire, then releases its TSRM slot and exits.
    fn spawn_thread(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        let thread_id = pool.state.next_id.fetch_add(1, Ordering::Relaxed);
        let rx = pool.dispatch_rx.clone();

        // Count the thread as live BEFORE spawning so the respawn gate can't
        // over-provision in the window before it starts. Undone on spawn
        // failure below, and by thread_main on every exit path.
        pool.state.live.fetch_add(1, Ordering::AcqRel);

        let builder = std::thread::Builder::new().name(format!("ephpm-fpm-{thread_id}"));
        let spawn_result = builder.spawn(move || {
            thread_main(&pool, thread_id, &rx);
        });

        if let Err(e) = spawn_result {
            self.state.live.fetch_sub(1, Ordering::AcqRel);
            tracing::error!(thread_id, %e, "failed to spawn fpm pool thread");
            counter!("ephpm_fpm_pool_boot_failures_total").increment(1);
        }
    }
}

/// Body of one pool thread: register with TSRM, then loop pulling jobs and
/// running one PHP request each, until the dispatch channel closes (drain) or a
/// job panics (retire). On every exit it releases its TSRM slot and lets the
/// supervisor respawn a replacement unless draining.
fn thread_main(pool: &Arc<FpmPool>, thread_id: usize, rx: &async_channel::Receiver<FpmJob>) {
    // `live` was incremented in spawn_thread before this thread started; every
    // exit path below decrements it.

    // Register this thread with TSRM once (same guard the spawn_blocking path
    // uses lazily, so a thread never double-registers). Unlike worker mode this
    // does NOT boot a framework — each request runs its own fpm cycle.
    if let Err(e) = PhpRuntime::worker_thread_init() {
        tracing::error!(thread_id, ?e, "fpm pool thread TSRM init failed");
        pool.state.boot_failures.fetch_add(1, Ordering::AcqRel);
        counter!("ephpm_fpm_pool_boot_failures_total").increment(1);
        pool.state.live.fetch_sub(1, Ordering::AcqRel);
        respawn_if_running(pool);
        return;
    }
    pool.state.ready.fetch_add(1, Ordering::AcqRel);
    pool.state.boot_failures.store(0, Ordering::Release);
    tracing::debug!(thread_id, "fpm pool thread ready");

    let mut retire = false;
    loop {
        let job = match rx.recv_blocking() {
            Ok(job) => job,
            // Dispatch closed — graceful drain. Exit the loop cleanly.
            Err(_) => break,
        };
        // Pulled from the queue: mirror the enqueue increment in `dispatch`.
        pool.queue_depth.fetch_sub(1, Ordering::Relaxed);

        let FpmJob { run, respond_to } = job;
        // A PHP bailout is caught in C and returned as `Err(PhpError)` — a
        // normal `FpmExecOutput`. A *Rust* panic (e.g. an unexpected `.expect()`
        // in the request path) is different: it would unwind and silently kill
        // this thread. Catch it so the request still gets an answer (500) and
        // the thread retires cleanly with a replacement, rather than leaking a
        // `live` slot on a half-unwound stack.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
            Ok(out) => {
                // The receiver is gone only if the router already timed out this
                // request (504) and moved on; dropping the response is correct.
                let _ = respond_to.send(out);
            }
            Err(_) => {
                counter!("ephpm_fpm_pool_panics_total").increment(1);
                let _ = respond_to.send(Err(PhpError::ExecutionFailed(
                    "fpm pool thread panicked during PHP execution".into(),
                )));
                tracing::error!(
                    thread_id,
                    "fpm pool thread panicked mid-request — 500 delivered, retiring thread"
                );
                retire = true;
                break;
            }
        }
    }

    pool.state.ready.fetch_sub(1, Ordering::AcqRel);
    // Release this thread's TSRM slot (php_request_shutdown + ts_free_thread)
    // and roll back any transaction a bailed-out request left open, on this
    // thread, before the PHP context goes away.
    PhpRuntime::worker_thread_shutdown();
    pool.state.live.fetch_sub(1, Ordering::AcqRel);

    if retire {
        counter!("ephpm_fpm_pool_recycles_total", "reason" => "panic").increment(1);
    }
    respawn_if_running(pool);
}

/// Spawn a replacement thread unless the pool is draining or already at target.
///
/// Gating on `live < thread_count` makes hung-thread replacement
/// self-balancing: `note_hung` spawns a replacement (live -> count+1) while the
/// stuck thread is abandoned; the extra puller is harmless and the pool holds
/// `thread_count` healthy pullers.
fn respawn_if_running(pool: &Arc<FpmPool>) {
    if pool.state.draining.load(Ordering::Acquire) {
        return;
    }
    if pool.state.live.load(Ordering::Acquire) >= pool.thread_count {
        return;
    }
    // Basic boot-storm backoff: if TSRM init keeps failing, pause before
    // respawning so a broken PHP context doesn't spin the CPU.
    let failures = pool.state.boot_failures.load(Ordering::Acquire);
    if failures > 0 {
        let shift = u32::try_from(failures.min(6)).unwrap_or(6);
        let backoff = std::time::Duration::from_millis(100u64.saturating_mul(1u64 << shift));
        std::thread::sleep(backoff.min(std::time::Duration::from_secs(10)));
    }
    pool.spawn_thread();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_task(status: u16) -> FpmTask {
        Box::new(move || {
            Ok(PhpResponse {
                status,
                headers: vec![("content-type".to_string(), "text/plain".to_string())],
                body: b"hello".to_vec(),
            })
        })
    }

    /// A pool with zero threads spawns nothing (safe in stub mode) and is never
    /// ready. Exercises the non-PHP plumbing: readiness, drain, and
    /// dispatch-after-drain error.
    #[tokio::test]
    async fn zero_thread_pool_never_ready_and_drains() {
        let pool = FpmPool::spawn(0, 4);
        assert_eq!(pool.ready_count(), 0, "no thread registered, so not ready");
        assert_eq!(pool.live_count(), 0);

        pool.drain();
        assert!(pool.dispatch(ok_task(200)).await.is_err(), "dispatch after drain must error");

        // drain() is idempotent and note_hung() while draining must not spawn.
        pool.drain();
        pool.note_hung();
        assert_eq!(pool.ready_count(), 0);
        assert_eq!(pool.live_count(), 0);
    }

    /// The counter-pair depth gauge tracks enqueues. With no thread to pull
    /// (0-thread pool), each successful dispatch leaves the job in the channel
    /// and the depth reflects it.
    #[tokio::test]
    async fn dispatch_increments_queue_depth() {
        let pool = FpmPool::spawn(0, 4);
        assert_eq!(pool.queue_depth(), 0);
        assert!(pool.dispatch(ok_task(200)).await.is_ok());
        assert!(pool.dispatch(ok_task(200)).await.is_ok());
        assert_eq!(pool.queue_depth(), 2, "each enqueue must increment depth");
    }

    /// A dispatch that fails because the channel is closed (draining) must NOT
    /// leak an increment into the depth counter.
    #[tokio::test]
    async fn failed_dispatch_does_not_leak_queue_depth() {
        let pool = FpmPool::spawn(0, 4);
        pool.drain();
        assert!(pool.dispatch(ok_task(200)).await.is_err());
        assert_eq!(pool.queue_depth(), 0, "failed enqueue must roll back the increment");
    }

    /// Full pipeline in stub mode: real threads pull a job, run the closure, and
    /// reply on the oneshot. `PhpRuntime::init()` sets the stub `PHP_INITIALIZED`
    /// flag so `worker_thread_init()` succeeds without libphp.
    #[tokio::test]
    async fn threads_execute_task_and_reply() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(2, 4);

        // Threads register asynchronously; wait (bounded) for readiness.
        for _ in 0..200 {
            if pool.ready_count() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(pool.ready_count(), 2, "both threads should register with TSRM");

        let rx = pool.dispatch(ok_task(202)).await.expect("dispatch ok");
        let out = rx.await.expect("thread replied");
        let resp = out.expect("stub task returns Ok");
        assert_eq!(resp.status, 202, "the router-built closure's response round-trips");

        // Draining retires every thread; live must reach 0 so PHP teardown is
        // safe (#266).
        pool.drain();
        for _ in 0..200 {
            if pool.live_count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(pool.live_count(), 0, "drain retires every thread");
    }
}

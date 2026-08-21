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
//! - **Shed → 503 + `Retry-After`:** with `[php] overload_policy = "shed"` the
//!   router calls [`FpmPool::try_dispatch`] instead, which refuses to queue
//!   behind a full backlog (after an optional `[php] shed_after_ms` grace) and
//!   returns [`DispatchRejected::Full`]. Overload then costs one cheap 503
//!   rather than a request that occupies a client until its own timeout
//!   (issue #301).
//! - **Draining / no live threads → 503:** [`DispatchClosed`] from `dispatch`.
//! - **Wedged thread → 504 + replace:** the router bounds the `oneshot` wait; a
//!   timeout calls [`FpmPool::note_hung`], which spawns a replacement and
//!   abandons the stuck thread (a wedged PHP thread cannot be killed without
//!   corrupting the ZMM — replace, don't kill).
//! - **Graceful drain:** [`FpmPool::drain`] closes the dispatch sender; each
//!   thread's `recv_blocking` then returns `Err`, the loop ends, and the thread
//!   releases its TSRM slot ([`PhpRuntime::worker_thread_shutdown`]) before
//!   exiting so `php_embed_shutdown()` is safe once `live` reaches 0 (#266).
//! - **Rust panic mid-job → 500 + retire:** a PHP bailout is caught in C and
//!   returned as `Err(PhpError::Bailout)`, an ordinary outcome. A *Rust* panic
//!   is not: it leaves the context in an unknown state, so the thread retires
//!   (with normal PHP teardown) and a replacement is spawned.
//!
//! # Poison-thread retirement (`[php] crash_containment`)
//!
//! Owning the threads is what makes crash containment safe, and this is the
//! part tokio's shared blocking pool could not provide. When
//! [`PhpRuntime::execute`] contains a C-stack overflow it answers
//! [`PhpError::Contained`] and the thread's Zend context is **poisoned** — its
//! ZMM free lists and `EG(objects_store)` were abandoned mid-mutation. This
//! pool then, in order: delivers the 500 to the waiting request, runs **no**
//! further job on that thread, exits the job loop, and spawns a replacement.
//!
//! **A poisoned thread never runs [`PhpRuntime::worker_thread_shutdown`].**
//! That call is `php_request_shutdown()` + `ts_free_thread()`, both of which
//! walk the poisoned per-thread heap and would turn a contained crash straight
//! back into a process abort. The thread is therefore *abandoned* — its TSRM
//! slot and Zend context are leaked deliberately, exactly as the
//! [`FpmPool::note_hung`] path abandons a wedged thread rather than killing it.
//! The invariant that follows: **once any crash has been contained, this
//! process must never run `php_embed_shutdown()`** — `ts_free_id()` would walk
//! the abandoned entry and `SIGABRT`. `ephpm-php` enforces that end of the
//! bargain by latching a flag that makes `PhpRuntime::shutdown()` skip module
//! teardown.
//!
//! The cost is a leak that **plateaus** rather than growing without bound:
//! measured on a 4-thread pool (PHP 8.5.7 ZTS), RSS rose about 90 MiB over the
//! first ~1000 contained crashes and then stopped moving over 400 more — the
//! allocator reuses the abandoned thread stacks and ZMM chunks. Live thread
//! count stayed flat throughout, so retired threads really do exit. The
//! alternative is the whole server dying on crash number one.
//!
//! Respawning after a poison is rate-limited ([`FpmPool::note_poisoned`]): a
//! crash costs a thread teardown plus a fresh TSRM registration, far more than a
//! normal request, so an attacker looping the crash script would otherwise get
//! free CPU amplification. Consecutive poisons inside a 10 s window back off
//! exponentially to a 500 ms ceiling — low enough that the pool refills quickly,
//! high enough to cap the respawn rate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

/// Why [`FpmPool::try_dispatch`] refused to enqueue a request
/// (`[php] overload_policy = "shed"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchRejected {
    /// Dispatch closed — pool draining or all threads gone. Same condition
    /// [`DispatchClosed`] reports; the router answers 503 either way.
    Closed,
    /// The bounded backlog was still full after the configured grace window.
    /// The router answers 503 + `Retry-After` — this is the load-shed arm.
    Full,
}

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
    /// Pool start time — the epoch the crash-storm backoff measures against.
    /// An `Instant` cannot live in an atomic, so poison timestamps are stored as
    /// milliseconds elapsed from here.
    started: Instant,
}

/// Sliding window for the crash-storm backoff: poisons further apart than this
/// are unrelated events and reset the streak.
const POISON_WINDOW: Duration = Duration::from_secs(10);

/// Base and ceiling of the per-poison respawn backoff. Small enough that the
/// pool refills promptly after an isolated crash, large enough that a sustained
/// crash storm cannot spin thread-spawn + TSRM-registration work at request rate.
const POISON_BACKOFF_BASE: Duration = Duration::from_millis(25);
/// Upper bound on the respawn backoff (reached after ~5 consecutive poisons).
const POISON_BACKOFF_MAX: Duration = Duration::from_millis(500);

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
    /// Consecutive contained-crash retirements inside [`POISON_WINDOW`], for
    /// the crash-storm respawn backoff. Reset by the first poison that arrives
    /// after a quiet window.
    poison_streak: AtomicUsize,
    /// Milliseconds (from [`FpmPool::started`]) of the most recent poison,
    /// stored **biased by +1** so `0` unambiguously means "none yet".
    last_poison_ms: AtomicU64,
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
            poison_streak: AtomicUsize::new(0),
            last_poison_ms: AtomicU64::new(0),
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
            started: Instant::now(),
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

    /// Total pool threads spawned since startup, including replacements.
    /// Monotonic, so a test can wait for a respawn without racing the
    /// `ready`/`live` counters as they dip through the retirement. Test-only.
    #[cfg(test)]
    fn threads_spawned(&self) -> usize {
        self.state.next_id.load(Ordering::Acquire)
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

    /// Dispatch one request, refusing to queue behind a full backlog
    /// (`[php] overload_policy = "shed"`).
    ///
    /// The load-shedding counterpart to [`FpmPool::dispatch`]. Where `dispatch`
    /// awaits a send permit for as long as it takes — turning overload into
    /// latency, and then into a client timeout with no server-side error ever
    /// emitted (issue #301) — this waits at most `grace` and then gives up so
    /// the caller can answer `503` immediately.
    ///
    /// `grace` of zero is a plain non-blocking `try_send`: the backlog
    /// (`[php] worker_backlog`, default = pool size) is already the buffer, so
    /// "full" means every thread is busy *and* the queue behind them is full.
    ///
    /// Cancelling the wait is sound because a queued-but-unstarted job is
    /// genuinely never run: nothing has been handed to a pool thread yet, so
    /// dropping the send future removes it from the channel's waiter list and
    /// no PHP work leaks. That is precisely what the `spawn_blocking` engine
    /// cannot do with tokio's blocking queue.
    ///
    /// # Errors
    ///
    /// [`DispatchRejected::Full`] when the backlog stayed full for `grace`
    /// (shed), [`DispatchRejected::Closed`] when the pool is draining.
    pub async fn try_dispatch(
        &self,
        run: FpmTask,
        grace: Duration,
    ) -> Result<oneshot::Receiver<FpmExecOutput>, DispatchRejected> {
        let (tx, rx) = oneshot::channel();
        let job = FpmJob { run, respond_to: tx };
        // Same accounting discipline as `dispatch`: count the enqueue before
        // the attempt, roll it back on every path that does not enqueue.
        let depth = self.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_fpm_pool_queue_depth").set(depth as f64);

        let outcome = if grace.is_zero() {
            match self.dispatch_tx.try_send(job) {
                Ok(()) => Ok(()),
                Err(async_channel::TrySendError::Full(_)) => Err(DispatchRejected::Full),
                Err(async_channel::TrySendError::Closed(_)) => Err(DispatchRejected::Closed),
            }
        } else {
            match tokio::time::timeout(grace, self.dispatch_tx.send(job)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(DispatchRejected::Closed),
                Err(_elapsed) => Err(DispatchRejected::Full),
            }
        };

        match outcome {
            Ok(()) => Ok(rx),
            Err(reason) => {
                self.queue_depth.fetch_sub(1, Ordering::Relaxed);
                Err(reason)
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

    /// Record a contained-crash retirement and return how long the retiring
    /// thread should wait before its replacement is spawned.
    ///
    /// Crash-storm rate limit. A contained crash is expensive on both sides —
    /// the poisoned context is abandoned and the replacement pays a fresh
    /// `ts_resource()` + `php_request_startup()` — so a script that crashes on
    /// every hit would otherwise buy an attacker a large CPU multiplier for a
    /// cheap request. Consecutive poisons inside [`POISON_WINDOW`] double the
    /// wait from [`POISON_BACKOFF_BASE`] up to [`POISON_BACKOFF_MAX`]; a quiet
    /// window resets the streak so an isolated crash costs almost nothing.
    ///
    /// Returns the backoff and the current streak length (for logging).
    fn note_poisoned(&self) -> (Duration, usize) {
        // Stored biased by +1 so that `0` unambiguously means "no poison yet".
        // Without the bias two crashes inside the same millisecond as pool
        // startup would both read `0` and each look like the first.
        let now_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX - 1);
        let last_raw = self.state.last_poison_ms.swap(now_ms + 1, Ordering::AcqRel);

        let window_ms = u64::try_from(POISON_WINDOW.as_millis()).unwrap_or(u64::MAX);
        let streak = if last_raw == 0 || now_ms.saturating_sub(last_raw - 1) > window_ms {
            self.state.poison_streak.store(1, Ordering::Release);
            1
        } else {
            self.state.poison_streak.fetch_add(1, Ordering::AcqRel) + 1
        };

        // 25ms, 50, 100, 200, 400, then pinned at the 500ms ceiling.
        let shift = u32::try_from(streak.saturating_sub(1).min(5)).unwrap_or(5);
        let backoff = POISON_BACKOFF_BASE.saturating_mul(1u32 << shift).min(POISON_BACKOFF_MAX);
        (backoff, streak)
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

        // Explicit stack size — see `ephpm_php::PHP_THREAD_STACK`. PHP's own
        // C-stack guard is bounded by the running thread's stack, so this is
        // what makes the pool's recursion ceiling match php-fpm's (#116).
        let builder = std::thread::Builder::new()
            .name(format!("ephpm-fpm-{thread_id}"))
            .stack_size(ephpm_php::PHP_THREAD_STACK);
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

/// Why a pool thread left its job loop. Determines whether PHP teardown is safe
/// on the way out and how the replacement is scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadExit {
    /// Dispatch closed — graceful drain. Normal PHP teardown.
    Drain,
    /// A Rust panic escaped the job. The Zend context is in an unknown but not
    /// known-corrupt state, so normal PHP teardown still runs.
    Panicked,
    /// A C-stack overflow was contained on this thread ([`PhpError::Contained`]).
    /// The Zend context is KNOWN corrupt: teardown is skipped and the thread is
    /// abandoned. See the module docs.
    Poisoned,
}

/// Body of one pool thread: register with TSRM, then loop pulling jobs and
/// running one PHP request each, until the dispatch channel closes (drain), a
/// job panics, or a crash is contained on this thread. On exit it releases its
/// TSRM slot — unless poisoned, where doing so would re-crash — and lets the
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

    let mut exit = ThreadExit::Drain;
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
                // A contained C-stack overflow poisons this thread's Zend
                // context. Deliver the 500 first — the request is still waiting
                // and the answer costs nothing — then leave the loop so this
                // thread runs no further PHP. Detected here rather than inside
                // `PhpRuntime` because retirement is the pool's job.
                let poisoned = matches!(&out, Err(PhpError::Contained(_)));
                // The receiver is gone only if the router already timed out this
                // request (504) and moved on; dropping the response is correct.
                let _ = respond_to.send(out);
                if poisoned {
                    counter!("ephpm_fpm_pool_contained_crashes_total").increment(1);
                    tracing::error!(
                        thread_id,
                        "fpm pool thread contained a PHP C-stack overflow — 500 delivered, \
                         thread poisoned; abandoning it (no PHP teardown) and spawning a \
                         replacement"
                    );
                    exit = ThreadExit::Poisoned;
                    break;
                }
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
                exit = ThreadExit::Panicked;
                break;
            }
        }
    }

    pool.state.ready.fetch_sub(1, Ordering::AcqRel);
    if exit == ThreadExit::Poisoned {
        // DO NOT run worker_thread_shutdown() here. It is php_request_shutdown()
        // + ts_free_thread(), and both walk this thread's ZMM free lists and
        // EG(objects_store) — which the contained `siglongjmp` left mid-mutation.
        // Running them would free out of a corrupt heap and abort the process,
        // undoing the containment. The TSRM slot and Zend context are abandoned
        // instead (a bounded leak); `ephpm-php` correspondingly skips
        // php_embed_shutdown() at process exit so module teardown never walks
        // this entry either. Same trade as the hung-thread path: a wedged or
        // poisoned PHP thread is replaced, never cleaned up.
        counter!("ephpm_fpm_pool_recycles_total", "reason" => "poisoned").increment(1);
    } else {
        // Release this thread's TSRM slot (php_request_shutdown + ts_free_thread)
        // and roll back any transaction a bailed-out request left open, on this
        // thread, before the PHP context goes away.
        PhpRuntime::worker_thread_shutdown();
        if exit == ThreadExit::Panicked {
            counter!("ephpm_fpm_pool_recycles_total", "reason" => "panic").increment(1);
        }
    }
    pool.state.live.fetch_sub(1, Ordering::AcqRel);

    if exit == ThreadExit::Poisoned {
        // Rate-limit the replacement so a crash-looping client cannot spin
        // thread-spawn + TSRM-registration work at request rate.
        let (backoff, streak) = pool.note_poisoned();
        tracing::warn!(
            thread_id,
            streak,
            backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
            "backing off before replacing a poisoned fpm pool thread"
        );
        if !pool.state.draining.load(Ordering::Acquire) {
            std::thread::sleep(backoff);
        }
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

    /// Wait (bounded) for `cond` to hold, polling on the tokio timer.
    async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..400 {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        cond()
    }

    /// A task that reports a contained C-stack overflow — exactly what
    /// `PhpRuntime::execute` returns when the crash guard recovers. Lets the
    /// retirement path be exercised in stub mode, with no libphp and no real
    /// crash.
    fn contained_task() -> FpmTask {
        Box::new(|| Err(PhpError::Contained("stack_overflow.php (GET /boom)".into())))
    }

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

    /// Poison-thread retirement, end to end in stub mode. A job that reports a
    /// contained crash must (1) still answer the waiting request, (2) take that
    /// thread out of service, and (3) be replaced, leaving the pool serving at
    /// full strength.
    #[tokio::test]
    async fn contained_crash_retires_thread_and_pool_recovers() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 4);
        assert!(wait_for(|| pool.ready_count() == 1).await, "thread should register");
        let spawned_before = pool.threads_spawned();

        let rx = pool.dispatch(contained_task()).await.expect("dispatch ok");
        let out = rx.await.expect("the 500 must reach the waiting request");
        assert!(
            matches!(out, Err(PhpError::Contained(_))),
            "a contained crash is delivered to the caller, not swallowed"
        );

        // The poisoned thread exits and a replacement is spawned (after the
        // crash-storm backoff, ~25ms for an isolated crash).
        assert!(
            wait_for(|| pool.threads_spawned() > spawned_before).await,
            "a replacement thread must be spawned for the poisoned one"
        );
        assert!(wait_for(|| pool.ready_count() == 1).await, "the pool must refill to its size");
        assert_eq!(pool.live_count(), 1, "exactly one live thread — the replacement");

        // And the pool serves normally again on the fresh thread.
        let rx = pool.dispatch(ok_task(200)).await.expect("dispatch ok");
        let resp = rx.await.expect("replacement replied").expect("stub task returns Ok");
        assert_eq!(resp.status, 200);

        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires the replacement");
    }

    // ── Load shedding (`[php] overload_policy = "shed"`, issue #301) ────

    /// The shed contract: with no grace window, a request that finds the
    /// bounded backlog full is rejected immediately instead of queueing. A
    /// 0-thread pool never drains, so "full" is deterministic here.
    #[tokio::test]
    async fn try_dispatch_sheds_when_the_backlog_is_full() {
        let pool = FpmPool::spawn(0, 1);

        assert!(
            pool.try_dispatch(ok_task(200), Duration::ZERO).await.is_ok(),
            "the first request fills the single backlog slot"
        );
        assert_eq!(
            pool.try_dispatch(ok_task(200), Duration::ZERO).await.err(),
            Some(DispatchRejected::Full),
            "a full backlog must reject, not queue — this is the 503 the client sees"
        );
    }

    /// A non-zero `shed_after_ms` is a grace window, not a queue: it waits for a
    /// slot and *then* sheds. Nothing drains a 0-thread pool, so the wait must
    /// actually elapse before the rejection.
    #[tokio::test]
    async fn try_dispatch_grace_waits_then_sheds() {
        let pool = FpmPool::spawn(0, 1);
        assert!(pool.try_dispatch(ok_task(200), Duration::ZERO).await.is_ok());

        let grace = Duration::from_millis(120);
        let started = Instant::now();
        assert_eq!(
            pool.try_dispatch(ok_task(200), grace).await.err(),
            Some(DispatchRejected::Full),
            "the grace window ends in a shed, not an unbounded wait"
        );
        assert!(
            started.elapsed() >= grace,
            "the grace window must actually be honoured before shedding: {:?}",
            started.elapsed()
        );
    }

    /// A drained pool reports `Closed`, not `Full` — a shutting-down server and
    /// a saturated one are different conditions and must stay distinguishable
    /// (only the latter is load shedding).
    #[tokio::test]
    async fn try_dispatch_after_drain_reports_closed() {
        let pool = FpmPool::spawn(0, 4);
        pool.drain();

        assert_eq!(
            pool.try_dispatch(ok_task(200), Duration::ZERO).await.err(),
            Some(DispatchRejected::Closed)
        );
        assert_eq!(
            pool.try_dispatch(ok_task(200), Duration::from_millis(20)).await.err(),
            Some(DispatchRejected::Closed),
            "a closed channel short-circuits the grace window"
        );
    }

    /// A shed request never entered the queue, so it must not leave an
    /// increment behind in the depth gauge — the same accounting invariant
    /// `failed_dispatch_does_not_leak_queue_depth` pins for the waiting path.
    #[tokio::test]
    async fn shed_dispatch_does_not_leak_queue_depth() {
        let pool = FpmPool::spawn(0, 1);
        assert!(pool.try_dispatch(ok_task(200), Duration::ZERO).await.is_ok());
        assert_eq!(pool.queue_depth(), 1);

        for _ in 0..5 {
            assert_eq!(
                pool.try_dispatch(ok_task(200), Duration::ZERO).await.err(),
                Some(DispatchRejected::Full)
            );
        }
        assert_eq!(pool.queue_depth(), 1, "shed requests must not inflate the queue depth");

        pool.drain();
        assert_eq!(
            pool.try_dispatch(ok_task(200), Duration::ZERO).await.err(),
            Some(DispatchRejected::Closed)
        );
        assert_eq!(pool.queue_depth(), 1, "a closed-channel rejection must not inflate it either");
    }

    /// Shedding is admission-only: a request that *does* get a slot is served
    /// exactly as the waiting path serves it.
    #[tokio::test]
    async fn shed_policy_still_serves_requests_that_fit() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 2);
        assert!(wait_for(|| pool.ready_count() == 1).await, "thread should register");

        let rx = pool.try_dispatch(ok_task(204), Duration::ZERO).await.expect("slot available");
        let resp = rx.await.expect("thread replied").expect("stub task returns Ok");
        assert_eq!(resp.status, 204);

        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires the thread");
    }

    /// The crash-storm backoff grows with consecutive poisons and is capped, so
    /// a client looping a crash script cannot spin thread-spawn + TSRM work at
    /// request rate. Pure bookkeeping — no threads involved.
    #[tokio::test]
    async fn poison_backoff_escalates_and_caps() {
        let pool = FpmPool::spawn(0, 1);

        let (first, streak) = pool.note_poisoned();
        assert_eq!(streak, 1, "the first poison starts a streak");
        assert_eq!(first, POISON_BACKOFF_BASE, "an isolated crash barely waits");

        let mut previous = first;
        for expected_streak in 2..=5 {
            let (backoff, streak) = pool.note_poisoned();
            assert_eq!(streak, expected_streak, "poisons inside the window accumulate");
            assert!(
                backoff > previous,
                "backoff must grow while crashes keep coming: {backoff:?} <= {previous:?}"
            );
            previous = backoff;
        }

        // Sustained storm: pinned at the ceiling, never unbounded.
        for _ in 0..10 {
            let (backoff, _) = pool.note_poisoned();
            assert!(backoff <= POISON_BACKOFF_MAX, "backoff must stay capped: {backoff:?}");
        }
        let (backoff, streak) = pool.note_poisoned();
        assert!(streak > 5);
        assert_eq!(backoff, POISON_BACKOFF_MAX, "a long storm sits at the ceiling");
    }
}

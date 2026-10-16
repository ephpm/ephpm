//! Dedicated per-request PHP execution pool (`[php] mode = "per_request"`,
//! the default execution model).
//!
//! A fixed pool of dedicated OS threads (`std::thread`, NOT tokio's
//! `spawn_blocking`) each take one per-request job from a per-thread
//! [`Dispatcher`] mailbox (an idle thread gets the job directly; a shared
//! bounded FIFO holds the overflow while every thread is busy), run exactly one
//! PHP request, reply on a [`tokio::sync::oneshot`], finish that request, and
//! loop. This is the
//! php-fpm model — every request runs a full `php_request_startup`/`shutdown`
//! cycle, so framework state never leaks across requests — but on threads
//! ePHPm owns rather than tokio's shared blocking pool.
//!
//! # Request end: reply first, then `php_request_shutdown`
//!
//! [`PhpRuntime::execute`] returns with the PHP request still open (shutdown
//! functions have run and the response is complete). The thread sends the
//! response and only then calls [`PhpRuntime::finish_request`] —
//! `php_request_shutdown()`: destructors, extension `RSHUTDOWN`, INI restore —
//! the same order as php-fpm's `fastcgi_finish_request()`. The teardown never
//! adds to response latency, and it is never deferred to the thread's *next*
//! request: before v0.11.3 it was, and because the session module writes the
//! session and releases its lock in `RSHUTDOWN`, a second request on the same
//! session landed on another thread and blocked on a lock held by an idle one.
//!
//! Destructors run tenant code, so the request's per-thread scope must still be
//! in place during the finish. The KV/DB/WebSocket scopes persist until the
//! next request overwrites them; anything scope-shaped that the job would
//! otherwise drop on return (the eBPF tenant tag) is parked with
//! [`hold_until_finished`] and released after the finish. A finish that runs
//! longer than [`SLOW_FINISH`] is logged: the thread is idle-but-busy then, with
//! no request attached that a timeout could charge it to.
//!
//! # Why dedicated threads
//!
//! Running PHP on tokio's `spawn_blocking` pool would tie PHP concurrency to
//! the shared pool that also serves static-file I/O and other blocking work —
//! either uncapped (one thread and one full ZTS PHP context per in-flight
//! request under flood) or starving that shared pool. A dedicated pool bounds
//! PHP concurrency to its own thread count without touching the shared
//! resource, so slow PHP scripts can never starve static-file serving — the
//! same reason [`crate::worker_pool`] uses `std::thread`.
//!
//! # Concurrency, backpressure, and failure mapping (mirrors `worker_pool`)
//!
//! - **Concurrency cap:** the pool size ([`PhpConfig::effective_concurrency`])
//!   is the cap.
//! - **Backpressure → 504:** the dispatch queue (`[php] queue_depth`) is
//!   bounded. When it is full, [`FpmPool::dispatch`] suspends on the FIFO-fair
//!   admission semaphore ([`FpmPool::admission`], issue #442); the outer
//!   request timeout turns a starved queue into a 504.
//! - **Shed → 503 + `Retry-After`:** with `[php] overload = "shed"` the
//!   router calls [`FpmPool::try_dispatch`] instead, which refuses to queue
//!   behind a full queue (after an optional `[php] shed_after_ms` grace) and
//!   returns [`DispatchRejected::Full`]. Overload then costs one cheap 503
//!   rather than a request that occupies a client until its own timeout
//!   (issue #301).
//! - **Draining / no live threads → 503:** [`DispatchClosed`] from `dispatch`.
//! - **Wedged thread → 504 + replace:** the router bounds the `oneshot` wait; a
//!   timeout calls [`FpmPool::note_hung`], which spawns a replacement and
//!   abandons the stuck thread (a wedged PHP thread cannot be killed without
//!   corrupting the ZMM — replace, don't kill).
//! - **Graceful drain:** [`FpmPool::drain`] closes the dispatcher; each
//!   thread's `recv` then returns `Err` once the overflow queue is empty, the
//!   loop ends, and the thread
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
//! [`PhpRuntime::execute`] — or the request's teardown,
//! [`PhpRuntime::finish_request`] — contains a C-stack overflow it answers
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

use ephpm_config::AdmissionPolicy;
use ephpm_php::handoff::{Dispatcher, SpinPolicy, TryPushError};
use ephpm_php::response::PhpResponse;
use ephpm_php::{PhpError, PhpRuntime};
use metrics::{counter, gauge};
use tokio::sync::oneshot;

/// The outcome of one PHP request — exactly what [`PhpRuntime::execute`]
/// returns. A contained bailout is `Err(PhpError::Bailout)`, which the router
/// turns into a 500 via `build_php_response`.
pub type FpmExecOutput = Result<PhpResponse, PhpError>;

/// The per-request work handed to a pool thread: the router-built closure that
/// runs the full per-request body (per-site DB session swap, KV keyspace,
/// `open_basedir`/temp/session INI, OPcache invalidation, and the
/// `PhpRuntime::execute` bailout crash guard).
pub type FpmTask = Box<dyn FnOnce() -> FpmExecOutput + Send + 'static>;

/// A request teardown ([`PhpRuntime::finish_request`]) slower than this is
/// logged at WARN. A hanging destructor or `RSHUTDOWN` ties up a pool thread
/// with no request attached — invisible to the request timeout, which has
/// already answered the client — so it gets a line of its own.
const SLOW_FINISH: Duration = Duration::from_secs(5);

/// The per-request pool's idle spin policy: park at once unless the
/// environment overrides it (see [`SpinPolicy::fpm_from_env`]). Read once per
/// process.
fn fpm_dispatch_spin() -> SpinPolicy {
    static POLICY: std::sync::OnceLock<SpinPolicy> = std::sync::OnceLock::new();
    *POLICY.get_or_init(|| SpinPolicy::fpm_from_env(Duration::ZERO))
}

thread_local! {
    /// Per-request scope parked by [`hold_until_finished`] on this pool
    /// thread, released once the request's teardown has run.
    static HELD_UNTIL_FINISHED: std::cell::RefCell<Vec<Box<dyn std::any::Any>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Keep `scope` alive until the current request's PHP teardown
/// ([`PhpRuntime::finish_request`]) has run on this pool thread, then drop it.
///
/// For per-request, per-thread state the [`FpmTask`] would otherwise drop when
/// it returns — but that must still cover the destructors and `RSHUTDOWN`
/// handlers that run in the teardown after the response is sent, because they
/// execute the same tenant's code. The router parks the eBPF tenant tag guard
/// here. Released on every exit path of the job (normal, contained crash,
/// panic), after any teardown that path runs. Dropping `scope` must not call
/// into PHP.
pub fn hold_until_finished(scope: impl std::any::Any) {
    HELD_UNTIL_FINISHED.with(|held| held.borrow_mut().push(Box::new(scope)));
}

/// Drop everything parked by [`hold_until_finished`] on this thread.
fn release_held_scope() {
    // Take first, drop outside the borrow: a guard's Drop must never observe
    // the cell mid-borrow.
    let held = HELD_UNTIL_FINISHED.with(|held| std::mem::take(&mut *held.borrow_mut()));
    drop(held);
}

/// Run the teardown of the request this pool thread just answered, timing it.
/// Returns `false` if a crash was contained during it (the thread is then
/// poisoned and must be retired without any further PHP teardown).
fn finish_request(thread_id: usize) -> bool {
    let started = Instant::now();
    let finished = PhpRuntime::finish_request();
    let elapsed = started.elapsed();
    if elapsed >= SLOW_FINISH {
        tracing::warn!(
            thread_id,
            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "PHP request teardown (destructors / extension RSHUTDOWN, after the response \
             was sent) was slow — this pool thread was unavailable meanwhile"
        );
    }
    !matches!(finished, Err(PhpError::Contained(_)))
}

/// The dispatcher is closed (pool draining or all threads gone). The router
/// turns this into a 503.
#[derive(Debug, Clone, Copy)]
pub struct DispatchClosed;

/// Why [`FpmPool::try_dispatch`] refused to enqueue a request
/// (`[php] overload = "shed"`).
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
    /// Fair-admission permit held while the job occupies the dispatch queue
    /// (issue #442) — dropped by the pool thread the moment it pulls the job,
    /// handing the freed slot to the longest-waiting dispatcher. See
    /// [`FpmPool::admission`].
    admission: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// Handle to the running FPM pool. Cheap to clone via `Arc`.
pub struct FpmPool {
    /// Per-thread job handoff: the hyper handler pushes jobs here after
    /// passing [`FpmPool::admission`]. An idle thread gets the job straight
    /// into its own mailbox; with every thread busy it waits in the shared
    /// FIFO overflow queue, whose capacity equals the admission permit count,
    /// so a permit holder's `try_push` can never find it full. `queued()`
    /// backs `ephpm_fpm_pool_queue_depth`.
    dispatcher: Dispatcher<FpmJob>,
    /// FIFO admission gate in front of the dispatch queue (issue #442).
    ///
    /// One permit == one dispatch-queue slot (capacity == permit count), and
    /// the pool thread releases the permit as it pulls the job. Waiting
    /// happens here — in strict arrival order — instead of in
    /// [`Dispatcher::push`] (formerly a bounded channel's `send().await`),
    /// whose try/wait/retry loop lets fresh senders barge past parked ones and
    /// re-queues a raced-out waiter at the back, producing a multi-lap
    /// starvation tail under saturation. See
    /// `worker_pool::WorkerPool::admission` for the full mechanism and the
    /// measurements; the two pools share the defect and the fix.
    admission: Arc<tokio::sync::Semaphore>,
    /// Which admission discipline dispatch uses (`[php] admission`): `Fifo`
    /// waits on [`FpmPool::admission`] in strict arrival order (the #443
    /// fix); `Barge` restores the pre-#443 path — wait for queue space in
    /// [`Dispatcher::push`], which lets fresh dispatchers steal freed slots
    /// ahead of parked waiters.
    admission_policy: AdmissionPolicy,
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
    pub fn spawn(
        thread_count: usize,
        backlog: usize,
        admission_policy: AdmissionPolicy,
    ) -> Arc<Self> {
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
            dispatcher: Dispatcher::new(backlog.max(1)),
            admission: Arc::new(tokio::sync::Semaphore::new(backlog.max(1))),
            admission_policy,
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
            admission = admission_policy.as_str(),
            "php execution pool started"
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

    /// Current overflow-queue depth (jobs waiting for a busy thread; a job
    /// handed straight to an idle thread never counts). Test-only.
    #[cfg(test)]
    fn queue_depth(&self) -> usize {
        self.dispatcher.queued()
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
    /// The acquire on [`FpmPool::admission`] suspends when the queue is full
    /// (backpressure) and admits waiters in strict arrival order (#442); the
    /// caller wraps the whole thing in the outer request timeout, so a starved
    /// queue becomes a 504 rather than an unbounded wait.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchClosed`] if the pool is draining / all threads are gone
    /// (admission and dispatcher are closed) — the caller should 503.
    pub async fn dispatch(
        &self,
        run: FpmTask,
    ) -> Result<oneshot::Receiver<FpmExecOutput>, DispatchClosed> {
        if self.admission_policy == AdmissionPolicy::Barge {
            return self.dispatch_barge(run).await;
        }
        // Wait for a queue slot in strict arrival order. Errors only when
        // `drain()` closed the semaphore.
        let Ok(permit) = Arc::clone(&self.admission).acquire_owned().await else {
            return Err(DispatchClosed);
        };
        self.enqueue_with_permit(run, permit).map_err(|_reason| DispatchClosed)
    }

    /// The pre-#443 dispatch path (`[php] admission = "barge"`): wait for a
    /// queue slot inside [`Dispatcher::push`] instead of the FIFO admission
    /// semaphore. Admission order is then a race — a fresh dispatcher tries
    /// before waiting behind parked ones — which is
    /// exactly the behaviour this escape hatch exists to restore. See
    /// `worker_pool::WorkerPool::dispatch_barge`; the two pools share the
    /// discipline. The job carries `admission: None`, so the pool thread's
    /// permit release is a no-op.
    async fn dispatch_barge(
        &self,
        run: FpmTask,
    ) -> Result<oneshot::Receiver<FpmExecOutput>, DispatchClosed> {
        let (tx, rx) = oneshot::channel();
        let job = FpmJob { run, respond_to: tx, admission: None };
        // Cancellation mid-wait (the outer request timeout) drops the job
        // unqueued; the dispatcher's depth only ever counts queued jobs.
        let pushed = self.dispatcher.push(job).await;
        self.record_queue_depth();
        match pushed {
            Ok(()) => Ok(rx),
            Err(_job) => Err(DispatchClosed),
        }
    }

    /// Publish the overflow-queue depth to `ephpm_fpm_pool_queue_depth`.
    fn record_queue_depth(&self) {
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_fpm_pool_queue_depth").set(self.dispatcher.queued() as f64);
    }

    /// Enqueue a job whose admission permit is already held.
    ///
    /// Holding a permit guarantees a free overflow slot (capacity == permit
    /// count, and every queued or handed-off job holds one permit until a
    /// thread takes it), so the `try_push` cannot see `Full` — no await,
    /// nothing to cancel mid-way.
    fn enqueue_with_permit(
        &self,
        run: FpmTask,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<oneshot::Receiver<FpmExecOutput>, DispatchRejected> {
        let (tx, rx) = oneshot::channel();
        let job = FpmJob { run, respond_to: tx, admission: Some(permit) };
        let pushed = self.dispatcher.try_push(job);
        self.record_queue_depth();
        match pushed {
            Ok(()) => Ok(rx),
            Err(e) => {
                // Closed (draining) — or, defensively, a Full the permit
                // invariant says cannot happen. No thread will see the job;
                // the permit inside the dropped job frees itself.
                debug_assert!(
                    e.is_closed(),
                    "fpm dispatch queue full while holding an admission permit"
                );
                Err(DispatchRejected::Closed)
            }
        }
    }

    /// Dispatch one request, refusing to queue behind a full backlog
    /// (`[php] overload = "shed"`).
    ///
    /// The load-shedding counterpart to [`FpmPool::dispatch`]. Where `dispatch`
    /// awaits a send permit for as long as it takes — turning overload into
    /// latency, and then into a client timeout with no server-side error ever
    /// emitted (issue #301) — this waits at most `grace` and then gives up so
    /// the caller can answer `503` immediately.
    ///
    /// `grace` of zero is a plain non-blocking permit try-acquire: the backlog
    /// (`[php] queue_depth`, default = pool size) is already the buffer, so
    /// "full" means every thread is busy *and* the queue behind them is full.
    ///
    /// A non-zero `grace` waits on the same FIFO-fair admission semaphore
    /// `dispatch` uses, so shed-mode waiters queue in arrival order too.
    /// Cancelling the wait is sound because a queued-but-unstarted job is
    /// genuinely never run: nothing has been handed to a pool thread yet, so
    /// dropping the acquire future removes it from the semaphore's waiter list
    /// and no PHP work leaks. (Tokio's own blocking queue could never offer
    /// this: its entries are unbounded and uncancellable.)
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
        if self.admission_policy == AdmissionPolicy::Barge {
            return self.try_dispatch_barge(run, grace).await;
        }
        let permit = if grace.is_zero() {
            match Arc::clone(&self.admission).try_acquire_owned() {
                Ok(permit) => permit,
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    return Err(DispatchRejected::Full);
                }
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(DispatchRejected::Closed);
                }
            }
        } else {
            match tokio::time::timeout(grace, Arc::clone(&self.admission).acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_closed)) => return Err(DispatchRejected::Closed),
                Err(_elapsed) => return Err(DispatchRejected::Full),
            }
        };
        self.enqueue_with_permit(run, permit)
    }

    /// The pre-#443 shed path (`[php] admission = "barge"`): `try_push` /
    /// `timeout(grace, push())` straight on the dispatcher. Shed-mode waiters
    /// race for freed slots exactly as `dispatch_barge` waiters do.
    async fn try_dispatch_barge(
        &self,
        run: FpmTask,
        grace: Duration,
    ) -> Result<oneshot::Receiver<FpmExecOutput>, DispatchRejected> {
        let (tx, rx) = oneshot::channel();
        let job = FpmJob { run, respond_to: tx, admission: None };
        // A rejected or timed-out job is dropped unqueued, so the
        // dispatcher's depth only ever counts queued jobs.
        let outcome = if grace.is_zero() {
            match self.dispatcher.try_push(job) {
                Ok(()) => Ok(()),
                Err(TryPushError::Full(_)) => Err(DispatchRejected::Full),
                Err(TryPushError::Closed(_)) => Err(DispatchRejected::Closed),
            }
        } else {
            match tokio::time::timeout(grace, self.dispatcher.push(job)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_job)) => Err(DispatchRejected::Closed),
                Err(_elapsed) => Err(DispatchRejected::Full),
            }
        };
        self.record_queue_depth();
        outcome.map(|()| rx)
    }

    /// Record that a thread appears hung (its `oneshot` timed out). The stuck
    /// thread is abandoned and a replacement is spawned to keep the pool at
    /// `thread_count` live pullers.
    pub fn note_hung(self: &Arc<Self>) {
        counter!("ephpm_fpm_pool_recycles_total", "reason" => "hung").increment(1);
        // The stuck thread is busy, so it is not on the dispatcher's idle
        // stack and no new job can be handed to it; if it ever finishes it
        // simply rejoins as an extra puller. A brief over-provision is
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
        // Closing the dispatcher wakes every idle thread; once the overflow
        // queue is drained each thread's recv returns Err and its loop ends
        // after any in-flight request completes. Closing the admission
        // semaphore wakes every dispatcher parked in `dispatch` /
        // `try_dispatch` with `Closed` (503).
        self.dispatcher.close();
        self.admission.close();
        tracing::info!("fpm execution pool draining — dispatch closed");
    }

    /// Spawn one dedicated OS thread that registers with TSRM, serves requests
    /// until drain / panic-retire, then releases its TSRM slot and exits.
    fn spawn_thread(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        let thread_id = pool.state.next_id.fetch_add(1, Ordering::Relaxed);

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
            thread_main(&pool, thread_id);
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
fn thread_main(pool: &Arc<FpmPool>, thread_id: usize) {
    // `live` was incremented in spawn_thread before this thread started; every
    // exit path below decrements it.

    // This thread's mailbox on the pool's dispatcher. Offered to dispatchers
    // only while the thread waits in `recv` below.
    let mailbox = pool.dispatcher.register_worker();

    // Register this thread with TSRM once (the per-thread registration guard
    // ensures a thread never double-registers). Unlike worker mode this does
    // NOT boot a framework — each request runs its own per-request cycle.
    if let Err(e) = PhpRuntime::worker_thread_init() {
        drop(mailbox);
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
        // Park straight away when idle (no spin-poll) by default: a
        // per-request job costs milliseconds, so the wake latency a poll would
        // save is noise here, and an idle per-request pool should not burn
        // CPU. `EPHPM_FPM_DISPATCH_SPIN_US` overrides it for experiments.
        let job = match mailbox.recv_with(fpm_dispatch_spin()) {
            Ok(job) => job,
            // Dispatch closed — graceful drain. Exit the loop cleanly.
            Err(_) => break,
        };
        // Release the admission permit so the FIFO admission semaphore hands
        // the freed slot to the longest-waiting dispatcher (#442).
        let FpmJob { run, respond_to, admission } = job;
        drop(admission);
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
                // The client has its answer; now end the PHP request
                // (destructors, session write + lock release, extension
                // RSHUTDOWN, INI restore) while this request's scope is still
                // in place. A crash contained here poisons the thread exactly
                // like one contained during execution.
                if !finish_request(thread_id) {
                    counter!("ephpm_fpm_pool_contained_crashes_total").increment(1);
                    tracing::error!(
                        thread_id,
                        "fpm pool thread contained a PHP C-stack overflow during request \
                         teardown (response already delivered) — thread poisoned; abandoning \
                         it (no PHP teardown) and spawning a replacement"
                    );
                    exit = ThreadExit::Poisoned;
                    break;
                }
                release_held_scope();
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

    // Takes no more jobs: unregister the mailbox before any teardown.
    drop(mailbox);
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
    // A panicked job's request was torn down (if at all) by the shutdown
    // above; a poisoned one never will be. Either way nothing of its scope is
    // needed any more.
    release_held_scope();
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
        let pool = FpmPool::spawn(0, 4, AdmissionPolicy::Fifo);
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

    /// The depth gauge tracks the overflow queue. With no thread to pull
    /// (0-thread pool), each successful dispatch leaves the job queued and the
    /// depth reflects it.
    #[tokio::test]
    async fn dispatch_increments_queue_depth() {
        let pool = FpmPool::spawn(0, 4, AdmissionPolicy::Fifo);
        assert_eq!(pool.queue_depth(), 0);
        assert!(pool.dispatch(ok_task(200)).await.is_ok());
        assert!(pool.dispatch(ok_task(200)).await.is_ok());
        assert_eq!(pool.queue_depth(), 2, "each enqueue must increment depth");
    }

    /// A dispatch that fails because the channel is closed (draining) must NOT
    /// leak an increment into the depth counter.
    #[tokio::test]
    async fn failed_dispatch_does_not_leak_queue_depth() {
        let pool = FpmPool::spawn(0, 4, AdmissionPolicy::Fifo);
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
        let pool = FpmPool::spawn(2, 4, AdmissionPolicy::Fifo);

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

    /// Sets its flag when dropped — stands in for the eBPF tag guard.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Scope parked with `hold_until_finished` outlives the job closure — it
    /// must still be alive when the response is delivered (the teardown that
    /// runs tenant code comes after) — and is released once the request is
    /// finished, on the normal path and on the contained-crash path alike.
    #[tokio::test]
    async fn held_scope_outlives_the_job_and_is_released_after_finish() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 4, AdmissionPolicy::Fifo);
        assert!(wait_for(|| pool.ready_count() == 1).await, "thread should register");

        for contained in [false, true] {
            let released = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&released);
            let observed_at_return = Arc::new(AtomicBool::new(true));
            let observed = Arc::clone(&observed_at_return);
            let task: FpmTask = Box::new(move || {
                let probe = Arc::clone(&flag);
                hold_until_finished(DropFlag(flag));
                // Still held when the job returns — the closure's own frame is
                // gone, the scope is not.
                observed.store(probe.load(Ordering::SeqCst), Ordering::SeqCst);
                if contained {
                    Err(PhpError::Contained("teardown test".into()))
                } else {
                    Ok(PhpResponse { status: 200, headers: Vec::new(), body: Vec::new() })
                }
            });
            let rx = pool.dispatch(task).await.expect("dispatch ok");
            let _ = rx.await.expect("thread replied");
            assert!(
                !observed_at_return.load(Ordering::SeqCst),
                "the parked scope must survive the job closure's return"
            );
            assert!(
                wait_for(|| released.load(Ordering::SeqCst)).await,
                "the parked scope must be released after the request is finished \
                 (contained = {contained})"
            );
            assert!(wait_for(|| pool.ready_count() == 1).await, "pool back to strength");
        }

        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires the thread");
    }

    /// Poison-thread retirement, end to end in stub mode. A job that reports a
    /// contained crash must (1) still answer the waiting request, (2) take that
    /// thread out of service, and (3) be replaced, leaving the pool serving at
    /// full strength.
    #[tokio::test]
    async fn contained_crash_retires_thread_and_pool_recovers() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 4, AdmissionPolicy::Fifo);
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

    // ── Load shedding (`[php] overload = "shed"`, issue #301) ────────────

    /// The shed contract: with no grace window, a request that finds the
    /// bounded backlog full is rejected immediately instead of queueing. A
    /// 0-thread pool never drains, so "full" is deterministic here.
    #[tokio::test]
    async fn try_dispatch_sheds_when_the_backlog_is_full() {
        let pool = FpmPool::spawn(0, 1, AdmissionPolicy::Fifo);

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
        let pool = FpmPool::spawn(0, 1, AdmissionPolicy::Fifo);
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
        let pool = FpmPool::spawn(0, 4, AdmissionPolicy::Fifo);
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
        let pool = FpmPool::spawn(0, 1, AdmissionPolicy::Fifo);
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
        let pool = FpmPool::spawn(1, 2, AdmissionPolicy::Fifo);
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
        let pool = FpmPool::spawn(0, 1, AdmissionPolicy::Fifo);

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

    // ── `[php] admission = "barge"` (the pre-#443 escape hatch) ─────────

    /// The barge mirror of `worker_pool`'s
    /// `admission_is_strictly_fifo_and_barge_proof`: with a waiter parked and
    /// a slot freshly freed, a brand-new dispatcher completes *inline* by
    /// stealing the slot — under FIFO the same inline dispatch would park
    /// behind the waiter and this single-threaded test would deadlock.
    #[tokio::test]
    async fn admission_barge_lets_a_newcomer_steal_a_freed_slot() {
        let pool = FpmPool::spawn(0, 1, AdmissionPolicy::Barge);

        // Fills the single queue slot immediately.
        let _rx_a = pool.dispatch(ok_task(200)).await.expect("first dispatch fits");

        // w1 parks inside the dispatcher's push().await.
        let w1 = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move {
                let _rx = pool.dispatch(ok_task(201)).await.expect("eventually admitted");
            })
        };
        tokio::task::yield_now().await;
        assert!(!w1.is_finished(), "w1 must be parked behind the full queue");

        // Free the slot; the newcomer's inline dispatch steals it before the
        // notified-but-not-yet-run w1 can retry.
        let job = pool.dispatcher.try_pop_queued().expect("the first job is queued");
        drop(job);
        let _rx_late = pool
            .dispatch(ok_task(202))
            .await
            .expect("a newcomer takes the freed slot ahead of the parked waiter");

        // w1 wakes, loses the race (queue full again), and re-parks.
        tokio::task::yield_now().await;
        assert!(!w1.is_finished(), "the parked waiter must still be waiting after being lapped");

        // Only the next freed slot admits it.
        let job = pool.dispatcher.try_pop_queued().expect("the stolen-slot job is queued");
        drop(job);
        tokio::task::yield_now().await;
        w1.await.expect("the lapped waiter is eventually admitted");
    }

    /// Barge shed semantics match the FIFO path's contract: a full backlog
    /// rejects with `Full`, a drained pool with `Closed`, and neither leaks a
    /// depth increment.
    #[tokio::test]
    async fn barge_try_dispatch_sheds_and_does_not_leak_depth() {
        let pool = FpmPool::spawn(0, 1, AdmissionPolicy::Barge);

        assert!(pool.try_dispatch(ok_task(200), Duration::ZERO).await.is_ok());
        assert_eq!(
            pool.try_dispatch(ok_task(200), Duration::ZERO).await.err(),
            Some(DispatchRejected::Full),
            "a full backlog must shed under barge too"
        );
        assert_eq!(pool.queue_depth(), 1, "shed requests must not inflate the queue depth");

        pool.drain();
        assert_eq!(
            pool.try_dispatch(ok_task(200), Duration::ZERO).await.err(),
            Some(DispatchRejected::Closed)
        );
        assert!(pool.dispatch(ok_task(200)).await.is_err(), "dispatch after drain must error");
        assert_eq!(pool.queue_depth(), 1, "failed barge enqueues must roll back their increments");
    }

    // ── Per-thread handoff properties (real threads, stub mode) ─────────

    /// A task that announces it started, then blocks until released. Returns
    /// the task plus (started, release) handles.
    fn gated_task(
        status: u16,
    ) -> (FpmTask, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let task: FpmTask = Box::new(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Ok(PhpResponse { status, headers: Vec::new(), body: Vec::new() })
        });
        (task, started_rx, release_tx)
    }

    /// Wait (blocking, bounded) for a gated task to report that it started.
    async fn started(rx: std::sync::mpsc::Receiver<()>) {
        tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)))
            .await
            .expect("join")
            .expect("the gated task must start");
    }

    async fn reply(rx: oneshot::Receiver<FpmExecOutput>) -> u16 {
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("the request must be answered, not stranded")
            .expect("thread replied")
            .expect("stub task returns Ok")
            .status
    }

    /// No head-of-line blocking: while one thread is stuck on a slow request,
    /// every new request goes to the idle thread — none is parked behind the
    /// busy one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fast_requests_never_wait_behind_a_slow_one_while_a_thread_is_idle() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(2, 8, AdmissionPolicy::Fifo);
        assert!(wait_for(|| pool.ready_count() == 2).await, "threads should register");

        let (slow, slow_started, release) = gated_task(201);
        let slow_rx = pool.dispatch(slow).await.expect("dispatch ok");
        started(slow_started).await;

        for _ in 0..50 {
            let rx = pool.dispatch(ok_task(200)).await.expect("dispatch ok");
            assert_eq!(reply(rx).await, 200, "a fast request must be served by the idle thread");
        }

        release.send(()).expect("release");
        assert_eq!(reply(slow_rx).await, 201);
        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires every thread");
    }

    /// Overload is bounded exactly as before: with every thread busy, at most
    /// `queue_depth` requests wait; the next one is shed (`try_dispatch`) or
    /// waits in admission without being queued (`dispatch`); the queued ones
    /// are then served in arrival order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overload_is_bounded_by_queue_depth_and_drains_in_arrival_order() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 3, AdmissionPolicy::Fifo);
        assert!(wait_for(|| pool.ready_count() == 1).await, "thread should register");

        let (blocker, blocker_started, release) = gated_task(299);
        let blocker_rx = pool.dispatch(blocker).await.expect("dispatch ok");
        started(blocker_started).await;

        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut queued = Vec::new();
        for id in 1..=3u16 {
            let order = Arc::clone(&order);
            let task: FpmTask = Box::new(move || {
                order.lock().unwrap().push(id);
                Ok(PhpResponse { status: 200 + id, headers: Vec::new(), body: Vec::new() })
            });
            queued.push(pool.dispatch(task).await.expect("fits in the backlog"));
        }
        assert_eq!(pool.queue_depth(), 3, "every thread busy: the backlog holds the overflow");

        assert_eq!(
            pool.try_dispatch(ok_task(200), Duration::ZERO).await.err(),
            Some(DispatchRejected::Full),
            "a full backlog sheds (503) instead of growing"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pool.dispatch(ok_task(200)))
                .await
                .is_err(),
            "a waiting dispatch parks in admission while the backlog is full"
        );
        assert_eq!(pool.queue_depth(), 3, "neither the shed nor the waiter was queued");

        release.send(()).expect("release");
        assert_eq!(reply(blocker_rx).await, 299);
        let mut statuses = Vec::new();
        for rx in queued {
            statuses.push(reply(rx).await);
        }
        assert_eq!(statuses, vec![201, 202, 203]);
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3], "queued requests run in arrival order");
        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires the thread");
    }

    /// Hung-thread replacement: requests queued while the only thread is
    /// wedged are served by the replacement — none stays stranded with the
    /// stuck thread — and the stuck thread rejoins if it ever finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_thread_replacement_serves_the_queued_requests() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 4, AdmissionPolicy::Fifo);
        assert!(wait_for(|| pool.ready_count() == 1).await, "thread should register");

        let (hung, hung_started, release) = gated_task(504);
        let hung_rx = pool.dispatch(hung).await.expect("dispatch ok");
        started(hung_started).await;

        let a = pool.dispatch(ok_task(200)).await.expect("dispatch ok");
        let b = pool.dispatch(ok_task(200)).await.expect("dispatch ok");
        assert_eq!(pool.queue_depth(), 2, "both wait while the only thread is stuck");

        // What the router does when the request timeout fires.
        pool.note_hung();
        assert_eq!(reply(a).await, 200, "the replacement must pick up queued work");
        assert_eq!(reply(b).await, 200);
        assert_eq!(pool.live_count(), 2, "the stuck thread is abandoned, not killed");

        release.send(()).expect("release");
        assert_eq!(reply(hung_rx).await, 504);
        let rx = pool.dispatch(ok_task(200)).await.expect("dispatch ok");
        assert_eq!(reply(rx).await, 200);

        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires both threads");
    }

    /// `admission = "barge"` with real threads: dispatchers waiting for queue
    /// space are woken by threads going idle as well as by queue pops, so a
    /// burst far larger than the backlog is fully served (none stranded
    /// until its request timeout while threads sit idle).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn barge_burst_is_fully_served_by_idle_threads() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(2, 1, AdmissionPolicy::Barge);
        assert!(wait_for(|| pool.ready_count() == 2).await, "threads should register");
        for _ in 0..20 {
            let mut tasks = Vec::new();
            for _ in 0..16 {
                let pool = Arc::clone(&pool);
                tasks.push(tokio::spawn(async move {
                    let task: FpmTask = Box::new(|| {
                        std::thread::sleep(Duration::from_micros(300));
                        Ok(PhpResponse { status: 200, headers: Vec::new(), body: Vec::new() })
                    });
                    let rx = pool.dispatch(task).await.expect("dispatch ok");
                    reply(rx).await
                }));
            }
            for t in tasks {
                let status = tokio::time::timeout(Duration::from_secs(5), t)
                    .await
                    .expect("a barge dispatcher was stranded")
                    .expect("join");
                assert_eq!(status, 200);
            }
        }
        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires every thread");
    }

    /// Fairness under a slow/fast mix: queued requests are handed out in
    /// arrival order whatever their cost — a fast request is never overtaken
    /// (or starved) by later ones, and a slow one never jumps the queue. One
    /// thread makes hand-out order observable as start order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixed_slow_and_fast_requests_are_served_in_arrival_order() {
        PhpRuntime::init().expect("stub init");
        let pool = FpmPool::spawn(1, 32, AdmissionPolicy::Fifo);
        assert!(wait_for(|| pool.ready_count() == 1).await, "thread should register");

        let (blocker, blocker_started, release) = gated_task(299);
        let blocker_rx = pool.dispatch(blocker).await.expect("dispatch ok");
        started(blocker_started).await;

        let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut pending = Vec::new();
        for id in 0..24usize {
            let starts = Arc::clone(&starts);
            let slow = id % 3 == 0;
            let task: FpmTask = Box::new(move || {
                starts.lock().unwrap().push(id);
                if slow {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(PhpResponse { status: 200, headers: Vec::new(), body: Vec::new() })
            });
            pending.push(pool.dispatch(task).await.expect("dispatch ok"));
        }
        release.send(()).expect("release");
        assert_eq!(reply(blocker_rx).await, 299);
        for rx in pending {
            assert_eq!(reply(rx).await, 200);
        }
        assert_eq!(
            *starts.lock().unwrap(),
            (0..24).collect::<Vec<_>>(),
            "queued requests must be served strictly in arrival order"
        );
        pool.drain();
        assert!(wait_for(|| pool.live_count() == 0).await, "drain retires the thread");
    }
}

//! Persistent-worker engine pool (worker mode — `worker-mode-design.md` §2, §5).
//!
//! A fixed pool of dedicated OS threads (NOT `spawn_blocking` — parking N
//! threads forever would starve the shared tokio blocking pool). Each thread
//! boots the framework once via [`PhpRuntime::run_worker`], then loops over
//! HTTP requests handed to it by a per-worker
//! [`Dispatcher`](ephpm_php::handoff::Dispatcher) (an idle worker gets the job
//! directly in its own mailbox; a shared bounded FIFO holds the overflow while
//! every worker is busy) guarded by a FIFO-fair admission semaphore (issue
//! #442 — see [`WorkerPool::admission`]), replying on a `tokio::sync::oneshot`.
//!
//! Lifecycle guarantees (design §5):
//! - **Boot-once:** the framework bootstrap runs once per worker thread; the
//!   worker then loops in `\Ephpm\Worker\take_request()`.
//! - **Recycle after N requests:** the C bridge returns shutdown once the
//!   per-worker counter hits `[php.worker] max_requests`; the thread exits and the
//!   supervisor spawns a replacement with a fresh boot.
//! - **Crash recovery:** a fatal bailout unwinds past `send_response`; the
//!   parked `oneshot::Sender` is still stashed, so the thread fulfils it with a
//!   500 (the in-flight request never hangs) and the worker is recycled.
//! - **Hung-worker replacement:** when a request the worker had already pulled
//!   is still unanswered at the request deadline, the router calls
//!   [`WorkerPool::note_hung`]; the pool spawns a replacement and abandons the
//!   stuck thread (a wedged PHP thread cannot be killed without corrupting the
//!   ZMM — replace, don't kill; matches RoadRunner / FrankenPHP). If the stuck
//!   worker answers after all, [`WorkerPool::note_recovered`] lets one surplus
//!   worker retire at its next `take_request()` (see `crate::php_wait`).
//! - **Graceful drain:** [`WorkerPool::drain`] closes the dispatcher; each
//!   worker's `take_request()` then returns null once the overflow queue is
//!   empty, the loop ends, and the thread exits after any in-flight request
//!   completes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ephpm_config::AdmissionPolicy;
use ephpm_php::PhpRuntime;
use ephpm_php::handoff::Dispatcher;
use ephpm_php::worker_bridge::{WorkerJob, WorkerRequestOwned, WorkerResponse};
use metrics::{counter, gauge, histogram};
use tokio::sync::oneshot;

/// The dispatcher is closed (pool draining or all workers gone). The router
/// turns this into a 503.
#[derive(Debug, Clone, Copy)]
pub struct DispatchClosed;

/// Handle to the running worker pool. Cloneable-cheap via `Arc`.
pub struct WorkerPool {
    /// Per-worker job handoff: the hyper handler pushes jobs here after
    /// passing [`WorkerPool::admission`]. An idle worker gets the job straight
    /// into its own mailbox; with every worker busy it waits in the shared
    /// FIFO overflow queue, whose capacity equals the admission permit count,
    /// so a permit holder's `try_push` can never find it full. Each worker
    /// thread registers its own mailbox when it starts. `queued()` backs
    /// `ephpm_worker_dispatch_queue_depth`.
    dispatcher: Dispatcher<WorkerJob>,
    /// FIFO admission gate in front of the dispatch queue (issue #442).
    ///
    /// Why not just wait for queue space ([`Dispatcher::push`], formerly a
    /// bounded `async_channel`'s `send().await`)? That is a try/listen/retry
    /// loop: a **new** sender always tries
    /// before ever queueing behind the parked ones (barging), and a notified
    /// waiter that loses that race re-registers at the **back** of the waiter
    /// queue. Under saturation (c ≫ backlog) that is a starvation engine:
    /// throughput stays maximal (a freed slot is always taken instantly) but
    /// an unlucky request is lapped by the entire waiter queue every time it
    /// loses — the measured signature was 43% of requests admitted in <1 ms
    /// while the P99 waited 5-6 full queue laps (~250-500 ms at 2.3k req/s).
    ///
    /// `tokio::sync::Semaphore` is documented FIFO-fair: permits go to
    /// waiters in acquire order and a new acquirer queues behind existing
    /// waiters even when a permit is free. One permit == one dispatch-queue
    /// slot; the worker releases it (via [`WorkerJob::admission`]) the moment
    /// it takes the job (from the overflow queue or its own mailbox), keeping
    /// the queue refill pipeline — and therefore throughput — identical to
    /// the old barging behaviour.
    admission: Arc<tokio::sync::Semaphore>,
    /// Which admission discipline [`WorkerPool::dispatch`] uses
    /// (`[php] admission`): `Fifo` waits on [`WorkerPool::admission`] in
    /// strict arrival order (the #443 fix); `Barge` restores the pre-#443
    /// path — wait for queue space in [`Dispatcher::push`], which lets fresh
    /// dispatchers steal freed slots ahead of parked waiters.
    admission_policy: AdmissionPolicy,
    /// Shared runtime state (readiness, liveness, drain flag).
    state: Arc<PoolState>,
    /// Worker entrypoint script (absolute, validated under document_root).
    worker_script: PathBuf,
    /// Requests-per-worker recycle threshold (`0` = never).
    max_requests: u64,
    /// Target number of live worker threads.
    worker_count: usize,
    /// Time each worker gets to reach its first `take_request()`.
    boot_timeout: Duration,
    /// How long `response_chunk` waits for a stalled client before aborting a
    /// streaming response (see `worker_bridge::set_stream_send_timeout`).
    stream_send_timeout: Duration,
}

/// Shared, atomically-updated pool state.
struct PoolState {
    /// Workers that have booted and reached their first `take_request()`.
    ready: AtomicUsize,
    /// Live worker threads (running `worker_main`, booted or booting). Used to
    /// self-balance respawns: a hung worker's replacement over-provisions by 1
    /// until the stuck thread finally exits, which then skips its own respawn.
    live: AtomicUsize,
    /// Consecutive boot failures (for boot-storm protection / degraded ready).
    boot_failures: AtomicUsize,
    /// Set when the pool is draining — supervisors stop respawning.
    draining: AtomicBool,
    /// Monotonic id source for worker threads (metric label / logging).
    next_id: AtomicUsize,
    /// Workers marked hung ([`WorkerPool::note_hung`]) that have not answered
    /// yet: live, but not serving.
    hung: AtomicUsize,
    /// Surplus workers that have claimed retirement
    /// ([`WorkerPool::claim_surplus`]) but not yet left `live`.
    retiring: AtomicUsize,
}

impl WorkerPool {
    /// Spawn the worker pool and block server readiness contract on it: this
    /// returns immediately, but [`WorkerPool::ready_count`] stays `0` until at
    /// least one worker finishes booting.
    ///
    /// `worker_script` must be the resolved absolute path (see
    /// [`ephpm_config::Config::resolve_worker_script`]).
    #[must_use]
    pub fn spawn(
        worker_script: PathBuf,
        worker_count: usize,
        max_requests: u64,
        backlog: usize,
        boot_timeout: Duration,
        stream_send_timeout: Duration,
        admission_policy: AdmissionPolicy,
    ) -> Arc<Self> {
        let state = Arc::new(PoolState {
            ready: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            boot_failures: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            next_id: AtomicUsize::new(0),
            hung: AtomicUsize::new(0),
            retiring: AtomicUsize::new(0),
        });

        gauge!("ephpm_worker_pool_size").set(worker_count as f64);
        gauge!("ephpm_worker_idle").set(0.0);
        gauge!("ephpm_worker_busy").set(0.0);

        let pool = Arc::new(Self {
            dispatcher: Dispatcher::new(backlog.max(1)),
            admission: Arc::new(tokio::sync::Semaphore::new(backlog.max(1))),
            admission_policy,
            state,
            worker_script,
            max_requests,
            worker_count,
            boot_timeout,
            stream_send_timeout,
        });

        for _ in 0..worker_count {
            pool.spawn_worker();
        }

        let recycle_policy = if max_requests == 0 {
            "disabled (leak-free framework loops)".to_string()
        } else {
            format!("recycle after {max_requests} requests per worker")
        };
        tracing::info!(
            worker_count,
            max_requests,
            recycle_policy = %recycle_policy,
            backlog = backlog.max(1),
            admission = admission_policy.as_str(),
            script = %pool.worker_script.display(),
            "worker pool started"
        );

        pool
    }

    /// Number of workers that have booted and are serving. Drives readiness.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.state.ready.load(Ordering::Acquire)
    }

    /// Number of worker OS threads still alive (including ones mid-boot or
    /// mid-teardown). Decremented only *after* a worker has released its
    /// TSRM slot ([`PhpRuntime::worker_thread_shutdown`]), so `0` during a
    /// drain means every worker thread's PHP context is fully retired and
    /// `php_embed_shutdown()` is safe to run (issue #266).
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.state.live.load(Ordering::Acquire)
    }

    /// Play a worker in tests: pull the oldest queued job (as `take_request`
    /// would — the `started` flag included) without answering it.
    #[cfg(test)]
    pub(crate) fn pull_job_for_test(&self) -> Option<WorkerJob> {
        let job = self.dispatcher.try_pop_queued()?;
        if let Some(started) = &job.started {
            started.store(true, Ordering::Release);
        }
        Some(job)
    }

    /// Current overflow-queue depth (jobs waiting for a busy worker to free
    /// up; a job handed straight to an idle worker never counts). Test-only.
    #[cfg(test)]
    fn queue_depth(&self) -> usize {
        self.dispatcher.queued()
    }

    /// Dispatch a request to the pool and return the receiver for its response.
    ///
    /// Admission is **FIFO-fair**: the acquire on [`WorkerPool::admission`]
    /// suspends when every dispatch-queue slot is taken (backpressure) and
    /// grants slots strictly in arrival order — see the field docs for why
    /// waiting on the bounded channel's own `send().await` instead produced a
    /// multi-lap starvation tail (issue #442). The caller wraps the whole
    /// thing in the outer request timeout, so a starved queue becomes a 504
    /// rather than an unbounded wait; a cancelled acquire leaves no trace
    /// (tokio removes the waiter, and the depth accounting below has no await
    /// point between increment and enqueue, so it cannot leak either).
    ///
    /// # Errors
    ///
    /// Returns [`DispatchClosed`] if the pool is draining / all workers gone
    /// (admission and dispatcher are closed) — the caller should 503.
    pub async fn dispatch(
        &self,
        request: WorkerRequestOwned,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        self.dispatch_tracked(request, None).await
    }

    /// [`WorkerPool::dispatch`], plus a flag the worker raises when it pulls
    /// the job — how the router tells a request that was *running* at its
    /// deadline (the worker may be hung) from one still queued (it is not).
    ///
    /// # Errors
    ///
    /// As [`WorkerPool::dispatch`].
    pub(crate) async fn dispatch_tracked(
        &self,
        request: WorkerRequestOwned,
        started: Option<Arc<AtomicBool>>,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        if self.admission_policy == AdmissionPolicy::Barge {
            return self.dispatch_barge(request, started).await;
        }
        // Wait for a queue slot in strict arrival order. Errors only when
        // `drain()` closed the semaphore.
        let Ok(permit) = Arc::clone(&self.admission).acquire_owned().await else {
            return Err(DispatchClosed);
        };
        let (tx, rx) = oneshot::channel();
        let job = WorkerJob { request, respond_to: tx, admission: Some(permit), started };
        // Holding a permit guarantees a free overflow slot (capacity == permit
        // count, and every queued or handed-off job holds one permit until a
        // worker takes it), so this `try_push` cannot see `Full` — no await,
        // no suspension, nothing to cancel mid-way.
        let pushed = self.dispatcher.try_push(job);
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_worker_dispatch_queue_depth").set(self.dispatcher.queued() as f64);
        match pushed {
            Ok(()) => Ok(rx),
            Err(e) => {
                // Closed (draining) — or, defensively, a Full that the permit
                // invariant says cannot happen. Either way no worker will see
                // the job; the permit inside the dropped job frees itself.
                debug_assert!(
                    e.is_closed(),
                    "dispatch queue full while holding an admission permit"
                );
                Err(DispatchClosed)
            }
        }
    }

    /// The pre-#443 dispatch path (`[php] admission = "barge"`): wait for a
    /// queue slot inside [`Dispatcher::push`] instead of the FIFO admission
    /// semaphore.
    ///
    /// `push` is a try/wait/retry loop — a fresh dispatcher `try_push`es
    /// before ever waiting behind parked ones, and a woken waiter that loses
    /// that race waits again — so admission order is a race, not a queue
    /// (what the old bounded-channel `send().await` did). Kept selectable as
    /// an operator escape hatch; see [`WorkerPool::admission`] for why FIFO is
    /// the default. The job carries `admission: None`, so the worker-side permit
    /// release in the bridge is a no-op.
    async fn dispatch_barge(
        &self,
        request: WorkerRequestOwned,
        started: Option<Arc<AtomicBool>>,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        let (tx, rx) = oneshot::channel();
        let job = WorkerJob { request, respond_to: tx, admission: None, started };
        // Cancellation (the outer request timeout dropping this future) drops
        // the job unqueued; the dispatcher's depth count only ever reflects
        // jobs actually queued.
        let pushed = self.dispatcher.push(job).await;
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_worker_dispatch_queue_depth").set(self.dispatcher.queued() as f64);
        match pushed {
            Ok(()) => Ok(rx),
            Err(_job) => Err(DispatchClosed),
        }
    }

    /// Record that a worker appears hung (its request was running at the
    /// request deadline and had not answered). The stuck thread is abandoned
    /// and a replacement is spawned to keep the pool at `worker_count` serving
    /// workers (design §5.4). Pair every call with
    /// [`WorkerPool::note_recovered`] once the worker answers.
    ///
    /// Replacements are capped at `worker_count` outstanding hung workers (at
    /// most twice the configured workers alive), so requests that outlast the
    /// deadline cannot grow the pool — one framework boot each — without
    /// bound. Past the cap the worker is counted hung but not replaced.
    pub fn note_hung(self: &Arc<Self>) {
        let hung = self.state.hung.fetch_add(1, Ordering::AcqRel) + 1;
        counter!("ephpm_worker_recycles_total", "reason" => "hung").increment(1);
        if hung > self.worker_count {
            tracing::error!(
                hung,
                cap = self.worker_count,
                "worker appeared hung, but as many hung workers as the pool size are already \
                 outstanding — not spawning a replacement"
            );
            return;
        }
        // The stuck thread is busy, so it is not on the dispatcher's idle
        // stack and no new job can be handed to it; if it ever finishes it
        // rejoins as an extra worker until `note_recovered` lets one surplus
        // worker retire. A brief over-provision is preferable to a wedged
        // pool.
        if !self.state.draining.load(Ordering::Acquire) {
            self.spawn_worker();
            tracing::warn!("worker appeared hung — spawned replacement, abandoned stuck thread");
        }
    }

    /// A worker passed to [`WorkerPool::note_hung`] answered (or dropped) its
    /// request at last. If it is still serving, the pool is one worker over
    /// target and the next worker to ask for a request retires
    /// ([`WorkerPool::claim_surplus`]).
    pub fn note_recovered(&self) {
        crate::php_wait::release_hung(&self.state.hung);
    }

    /// Claim retirement for the calling worker if the pool is over target
    /// (live workers, minus hung and already-retiring ones, exceed
    /// `worker_count`). At most one worker claims each surplus slot. Called by
    /// the worker bridge between requests.
    fn claim_surplus(&self) -> bool {
        !self.state.draining.load(Ordering::Acquire)
            && crate::php_wait::claim_surplus(
                &self.state.live,
                &self.state.hung,
                &self.state.retiring,
                self.worker_count,
            )
    }

    /// Begin graceful drain: stop accepting new jobs and let workers exit once
    /// their in-flight request (if any) completes. Idempotent.
    pub fn drain(&self) {
        if self.state.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        // Closing the dispatcher wakes every idle worker; once the overflow
        // queue is drained their take_request() returns null and the
        // framework loop ends. Closing the admission semaphore wakes every
        // dispatcher parked in `dispatch()` with an error (503).
        self.dispatcher.close();
        self.admission.close();
        tracing::info!("worker pool draining — dispatch closed");
    }

    /// Spawn one worker OS thread that boots the framework once, serves until
    /// recycle/bailout/drain, then exits. The supervisor respawns a
    /// replacement unless the pool is draining.
    fn spawn_worker(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        let worker_id = pool.state.next_id.fetch_add(1, Ordering::Relaxed);
        let script = pool.worker_script.clone();
        let max_requests = pool.max_requests;
        let boot_timeout = pool.boot_timeout;

        // Count the worker as live BEFORE spawning so the respawn gate can't
        // over-provision in the window before the thread starts. Undone on
        // spawn failure below, and by worker_main on normal exit.
        pool.state.live.fetch_add(1, Ordering::AcqRel);

        // An explicit stack size, not Rust's 2 MiB default: PHP's C-stack guard
        // bounds recursion at this thread's stack, so the size is what decides
        // how deep a nested render may go before PHP raises `Maximum call stack
        // size ... reached`. `PHP_THREAD_STACK` matches php-fpm (#116).
        let builder = std::thread::Builder::new()
            .name(format!("ephpm-worker-{worker_id}"))
            .stack_size(ephpm_php::PHP_THREAD_STACK);
        let spawn_result = builder.spawn(move || {
            worker_main(&pool, worker_id, &script, max_requests, boot_timeout);
        });

        if let Err(e) = spawn_result {
            self.state.live.fetch_sub(1, Ordering::AcqRel);
            tracing::error!(worker_id, %e, "failed to spawn worker thread");
            counter!("ephpm_worker_boot_failures_total").increment(1);
        }
    }
}

/// Body of one worker OS thread: register with TSRM, install its dispatch
/// mailbox + recycle counter, boot the framework, serve, then exit (the
/// supervisor spawns the replacement).
fn worker_main(
    pool: &Arc<WorkerPool>,
    worker_id: usize,
    script: &std::path::Path,
    max_requests: u64,
    boot_timeout: Duration,
) {
    // `live` was incremented in spawn_worker before this thread started; we
    // decrement it on every exit path below.

    // Register this thread's mailbox and install the recycle quota BEFORE
    // booting, so the very first take_request() inside the framework loop can
    // pull work. The mailbox is only offered to dispatchers while the worker
    // waits in take_request(), so a booting worker is never handed a job.
    ephpm_php::worker_bridge::set_dispatch_worker(pool.dispatcher.register_worker());
    ephpm_php::worker_bridge::set_max_requests(max_requests);
    ephpm_php::worker_bridge::set_stream_send_timeout(pool.stream_send_timeout);
    // Between requests the bridge asks whether this worker is surplus (a hung
    // worker's replacement still running after the hung one came back).
    {
        let pool = Arc::clone(pool);
        ephpm_php::worker_bridge::set_retire_check(Box::new(move || pool.claim_surplus()));
    }

    // TSRM register + start the one long-lived request the whole loop runs in.
    if let Err(e) = PhpRuntime::worker_thread_init() {
        ephpm_php::worker_bridge::clear_dispatch_source();
        tracing::error!(worker_id, ?e, "worker TSRM init failed");
        pool.state.boot_failures.fetch_add(1, Ordering::AcqRel);
        counter!("ephpm_worker_boot_failures_total").increment(1);
        pool.state.live.fetch_sub(1, Ordering::AcqRel);
        respawn_if_running(pool);
        return;
    }

    // Boot completion is signalled by the worker's FIRST take_request() — the
    // framework has finished booting and is asking for work. run_worker itself
    // blocks for the worker's entire life, so it cannot distinguish boot from
    // serving: readiness, the boot-duration metric, and the backoff reset all
    // hang off this notifier, not off run_worker returning.
    let boot_start = Instant::now();
    let booted = Arc::new(AtomicBool::new(false));
    {
        let pool = Arc::clone(pool);
        let booted = Arc::clone(&booted);
        ephpm_php::worker_bridge::set_boot_notifier(Box::new(move || {
            booted.store(true, Ordering::Release);
            let boot_elapsed = boot_start.elapsed().as_secs_f64();
            histogram!("ephpm_worker_boot_duration_seconds").record(boot_elapsed);
            pool.state.ready.fetch_add(1, Ordering::AcqRel);
            pool.state.boot_failures.store(0, Ordering::Release);
            tracing::info!(worker_id, boot_elapsed, "worker booted");
        }));
    }

    // Boot watchdog: a wedged boot (framework hangs before its first
    // take_request) never returns from run_worker, so readiness would sit at
    // 0 with no diagnostic. The watchdog cannot kill the thread (a PHP thread
    // cannot be terminated without corrupting the ZMM) — it makes the stall
    // visible and counts it.
    {
        let booted = Arc::clone(&booted);
        let _ = std::thread::Builder::new()
            .name(format!("ephpm-worker-{worker_id}-bootwatch"))
            .spawn(move || {
                std::thread::sleep(boot_timeout);
                if !booted.load(Ordering::Acquire) {
                    counter!("ephpm_worker_boot_timeouts_total").increment(1);
                    tracing::error!(
                        worker_id,
                        timeout_secs = boot_timeout.as_secs(),
                        "worker has not finished booting within [php.worker] boot_timeout \
                         (thread cannot be killed; it becomes ready if the boot completes)"
                    );
                }
            });
    }

    tracing::info!(worker_id, "worker booting framework");

    // run_worker blocks until the framework's take_request() loop ends.
    let outcome = PhpRuntime::run_worker(script);

    // This thread takes no more jobs: unregister its mailbox now rather than
    // at thread exit, after the (possibly slow) PHP teardown below.
    ephpm_php::worker_bridge::clear_dispatch_source();

    // The worker is no longer serving (only if it ever was).
    let was_booted = booted.load(Ordering::Acquire);
    let retired_surplus = ephpm_php::worker_bridge::retired_as_surplus();
    if was_booted {
        pool.state.ready.fetch_sub(1, Ordering::AcqRel);
    }

    if was_booted {
        match outcome {
            Ok(ephpm_php::WorkerExit::Clean) => {
                // Clean loop end: graceful drain or max_requests recycle.
                let requests_served = ephpm_php::worker_bridge::requests_handled();
                let uptime_secs = boot_start.elapsed().as_secs_f64();
                if retired_surplus {
                    tracing::debug!(
                        worker_id,
                        requests_served,
                        uptime_secs,
                        "worker retired — pool back to target after a hung worker recovered",
                    );
                } else if pool.state.draining.load(Ordering::Acquire) {
                    tracing::debug!(
                        worker_id,
                        requests_served,
                        uptime_secs,
                        "worker exited on drain",
                    );
                } else {
                    counter!("ephpm_worker_recycles_total", "reason" => "max_requests")
                        .increment(1);
                    tracing::debug!(
                        worker_id,
                        requests_served,
                        uptime_secs,
                        "worker recycled (max_requests) — respawning",
                    );
                }
            }
            Ok(ephpm_php::WorkerExit::ScriptExit) => {
                // The script exit()ed mid-request; the C layer synthesized and
                // delivered the response from SAPI state. Defensive: if the
                // sender is somehow still parked, 500 it rather than hang.
                if let Some(sender) = ephpm_php::worker_bridge::take_pending_sender() {
                    let _ = sender.send(WorkerResponse::internal_error());
                }
                counter!("ephpm_worker_recycles_total", "reason" => "script_exit").increment(1);
                tracing::debug!(worker_id, "worker script exited mid-request — recycling");
            }
            Ok(ephpm_php::WorkerExit::ScriptFatal) => {
                // The request died on a PHP fatal. The C layer synthesized the
                // response (500 unless the script chose a status) and delivered
                // it, so the oneshot is already fulfilled; the drain below is
                // the same defensive path ScriptExit uses.
                if let Some(sender) = ephpm_php::worker_bridge::take_pending_sender() {
                    let _ = sender.send(WorkerResponse::internal_error());
                }
                counter!("ephpm_worker_recycles_total", "reason" => "fatal").increment(1);
                tracing::warn!(
                    worker_id,
                    "worker request ended on a PHP fatal — 500 delivered, recycling"
                );
            }
            Ok(ephpm_php::WorkerExit::Fatal) => {
                // A zend_bailout() killed the worker. Nothing was delivered
                // (the C layer refuses to synthesize a response from truncated
                // capture buffers), so the request must be terminated here —
                // and it must not look like a success. Two shapes:
                let sender = ephpm_php::worker_bridge::take_pending_sender();
                let had_parked_sender = sender.is_some();
                //   1. Nothing on the wire yet: the oneshot is still parked, so
                //      the request becomes a clean 500.
                if let Some(sender) = sender {
                    let _ = sender.send(WorkerResponse::internal_error());
                    tracing::error!(
                        worker_id,
                        "PHP worker terminated in a Zend bailout mid-request — \
                         500 sent, recycling worker"
                    );
                }
                //   2. `send_response_stream` already put status+headers on the
                //      wire, so the 200 cannot be retracted. Setting the abort
                //      flag makes the body end in an error instead of a clean
                //      EOF: the client's transfer fails rather than completing
                //      with a partial document.
                let aborted_stream = ephpm_php::worker_bridge::clear_in_flight_streams();
                if aborted_stream {
                    counter!("ephpm_worker_stream_aborts_total").increment(1);
                    tracing::error!(
                        worker_id,
                        "PHP worker terminated in a Zend bailout after its response \
                         headers were already sent — response body deliberately \
                         aborted (no terminating chunk), recycling worker"
                    );
                }
                //   3. Neither: the bailout landed between requests. No client
                //      is waiting, but it still must not be silent.
                if !had_parked_sender && !aborted_stream {
                    tracing::error!(
                        worker_id,
                        "PHP worker terminated in a Zend bailout with no request in \
                         flight — recycling worker"
                    );
                }
                counter!("ephpm_worker_recycles_total", "reason" => "fatal").increment(1);
            }
            Err(e) => {
                // run_worker refused after a successful boot — should not
                // happen (boot implies init); recycle defensively.
                if let Some(sender) = ephpm_php::worker_bridge::take_pending_sender() {
                    let _ = sender.send(WorkerResponse::internal_error());
                }
                tracing::error!(worker_id, ?e, "worker run failed after boot");
                counter!("ephpm_worker_recycles_total", "reason" => "fatal").increment(1);
            }
        }
    } else {
        // The worker exited without ever reaching take_request(): the
        // framework failed to boot (fatal during bootstrap, script error, a
        // script that returns without looping, or run_worker refusing). This
        // MUST count as a boot failure — it is what drives respawn backoff;
        // without it a broken worker.php respawns in a zero-delay hot loop.
        if let Some(sender) = ephpm_php::worker_bridge::take_pending_sender() {
            let _ = sender.send(WorkerResponse::internal_error());
        }
        // A worker that never booted cannot have an open stream, but abort
        // whatever is there rather than letting it end cleanly.
        let _ = ephpm_php::worker_bridge::clear_in_flight_streams();
        pool.state.boot_failures.fetch_add(1, Ordering::AcqRel);
        counter!("ephpm_worker_boot_failures_total").increment(1);
        match outcome {
            Ok(exit) => tracing::error!(
                worker_id,
                ?exit,
                "worker exited before completing boot (framework never reached \
                 take_request) — check the worker script's error log"
            ),
            Err(e) => tracing::error!(worker_id, ?e, "worker boot failed"),
        }
    }

    // Free this thread's TSRM slot + booted framework so the replacement boots
    // clean. Safe: this thread is done executing PHP.
    PhpRuntime::worker_thread_shutdown();

    pool.state.live.fetch_sub(1, Ordering::AcqRel);
    if retired_surplus {
        pool.state.retiring.fetch_sub(1, Ordering::AcqRel);
    }
    respawn_if_running(pool);
}

/// Spawn a replacement worker unless the pool is draining or already at target.
///
/// The gate counts *serving* workers: `live` minus workers marked hung (alive
/// but stuck). Gating on plain `live` would let a hung worker stand in for a
/// serving one, so a routine `max_requests` recycle after a hung worker's
/// replacement would leave the pool a worker short. Replacement still cannot
/// compound: `note_hung` spawns a replacement while the stuck thread is
/// abandoned. When that stuck thread eventually exits, its reply or dropped
/// sender came first, so `note_recovered` has normally run and it finds
/// serving == count and does NOT respawn (if the watcher has not caught up,
/// one extra worker spawns and later retires as surplus); when it instead
/// goes back to serving,
/// `note_recovered` + `claim_surplus` retire one worker at its next
/// `take_request()`. Either way the pool converges back to target.
fn respawn_if_running(pool: &Arc<WorkerPool>) {
    if pool.state.draining.load(Ordering::Acquire) {
        return;
    }
    let serving = pool
        .state
        .live
        .load(Ordering::Acquire)
        .saturating_sub(pool.state.hung.load(Ordering::Acquire));
    if serving >= pool.worker_count {
        return;
    }
    // Basic boot-storm backoff: if boots keep failing, pause before respawning
    // so a broken worker.php doesn't spin the CPU. Readiness reports 0 anyway.
    let failures = pool.state.boot_failures.load(Ordering::Acquire);
    if failures > 0 {
        let shift = u32::try_from(failures.min(6)).unwrap_or(6);
        let backoff = Duration::from_millis(100u64.saturating_mul(1u64 << shift));
        std::thread::sleep(backoff.min(Duration::from_secs(10)));
    }
    pool.spawn_worker();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool with zero workers spawns no OS threads (safe in stub mode) and
    /// is never ready. Exercises the non-PHP pool plumbing: readiness, drain,
    /// and dispatch-after-drain error.
    #[tokio::test]
    async fn zero_worker_pool_never_ready_and_drains() {
        let pool = WorkerPool::spawn(
            PathBuf::from("/nonexistent/worker.php"),
            0, // no worker threads spawned
            500,
            4,
            Duration::from_secs(30),
            Duration::from_secs(60),
            AdmissionPolicy::Fifo,
        );

        assert_eq!(pool.ready_count(), 0, "no worker booted, so not ready");

        // Draining closes the dispatch sender; a subsequent dispatch must error
        // (the router turns this into a 503) rather than hang.
        pool.drain();
        let req = ephpm_php::worker_bridge::WorkerRequestOwned {
            method: "GET".into(),
            uri: "/".into(),
            query_string: String::new(),
            cookie_data: String::new(),
            content_type: None,
            body: ephpm_php::worker_bridge::WorkerBody::Buffered(Vec::new()),
            server_vars: Vec::new(),
            headers: Vec::new(),
        };
        assert!(pool.dispatch(req).await.is_err(), "dispatch after drain must error");

        // drain() is idempotent and note_hung() while draining is a no-op
        // (must not spawn a replacement thread).
        pool.drain();
        pool.note_hung();
        assert_eq!(pool.ready_count(), 0);
    }

    fn dummy_request() -> ephpm_php::worker_bridge::WorkerRequestOwned {
        request_with_uri("/")
    }

    fn request_with_uri(uri: &str) -> ephpm_php::worker_bridge::WorkerRequestOwned {
        ephpm_php::worker_bridge::WorkerRequestOwned {
            method: "GET".into(),
            uri: uri.into(),
            query_string: String::new(),
            cookie_data: String::new(),
            content_type: None,
            body: ephpm_php::worker_bridge::WorkerBody::Buffered(Vec::new()),
            server_vars: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Admission is strictly FIFO (issue #442): waiters are admitted in
    /// arrival order, and a dispatcher that arrives while others are parked
    /// queues behind them instead of barging into a freshly freed slot — the
    /// exact behaviour the old bounded-channel `send().await` could not
    /// provide (its try/listen/retry loop let new senders steal slots and
    /// re-queued raced-out waiters at the back, producing the multi-lap P99).
    ///
    /// Runs on the default current-thread test runtime, where an explicit
    /// `yield_now` deterministically lets a just-spawned dispatcher run until
    /// it parks on the admission semaphore.
    #[tokio::test]
    async fn admission_is_strictly_fifo_and_barge_proof() {
        let pool = WorkerPool::spawn(
            PathBuf::from("/nonexistent/worker.php"),
            0, // no workers: this test plays the worker by popping the queue
            500,
            1, // backlog of one queue slot => everyone else parks in admission
            Duration::from_secs(30),
            Duration::from_secs(60),
            AdmissionPolicy::Fifo,
        );

        // Fills the single queue slot immediately.
        let _rx_a = pool.dispatch(request_with_uri("/a")).await.expect("first dispatch fits");

        // Three dispatchers park on admission, in a deterministic order.
        let mut parked = Vec::new();
        for uri in ["/w1", "/w2", "/w3"] {
            let pool = Arc::clone(&pool);
            parked.push(tokio::spawn(async move {
                pool.dispatch(request_with_uri(uri)).await.expect("admitted after a slot frees");
            }));
            // Let the spawned task run until it parks on the semaphore.
            tokio::task::yield_now().await;
        }

        // Play the worker: pull one job (freeing its slot when the job — and
        // the permit inside it — drops) and record the order jobs arrive in.
        let mut served = Vec::new();
        let mut pull = || {
            let job = pool.dispatcher.try_pop_queued().expect("a job is queued");
            served.push(job.request.uri.clone());
            drop(job); // releases the admission permit -> admits ONE waiter
        };

        pull(); // "/a" leaves; w1 must be admitted, not anyone newer...
        tokio::task::yield_now().await;

        // ...and a brand-new dispatcher arriving NOW must queue behind w2/w3
        // even though slots keep freeing up (barge-proofing).
        let late = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move {
                pool.dispatch(request_with_uri("/late")).await.expect("admitted last");
            })
        };
        tokio::task::yield_now().await;

        for _ in 0..3 {
            pull();
            tokio::task::yield_now().await;
        }
        pull();

        for handle in parked {
            handle.await.expect("parked dispatcher completed");
        }
        late.await.expect("late dispatcher completed");

        assert_eq!(
            served,
            vec!["/a", "/w1", "/w2", "/w3", "/late"],
            "admission must be strict arrival order with no barging"
        );
    }

    /// The mirror of `admission_is_strictly_fifo_and_barge_proof` for
    /// `[php] admission = "barge"`: the escape hatch must genuinely restore
    /// the pre-#443 racing admission, not silently keep FIFO. With a parked
    /// waiter and a freshly freed slot, a brand-new dispatcher's inline
    /// `push()` tries first and steals the slot — under FIFO the same
    /// inline dispatch would park behind the waiter (and this single-threaded
    /// test would deadlock), so the inline completion *is* the barge proof.
    #[tokio::test]
    async fn admission_barge_lets_a_newcomer_steal_a_freed_slot() {
        let pool = WorkerPool::spawn(
            PathBuf::from("/nonexistent/worker.php"),
            0, // no workers: this test plays the worker by popping the queue
            500,
            1, // one queue slot => the second dispatcher parks in push()
            Duration::from_secs(30),
            Duration::from_secs(60),
            AdmissionPolicy::Barge,
        );

        // Fills the single queue slot immediately.
        let _rx_a = pool.dispatch(request_with_uri("/a")).await.expect("first dispatch fits");

        // w1 parks inside the dispatcher's push().await.
        let w1 = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move {
                pool.dispatch(request_with_uri("/w1")).await.expect("eventually admitted");
            })
        };
        tokio::task::yield_now().await;

        let mut served = Vec::new();
        let mut pull = || {
            let job = pool.dispatcher.try_pop_queued().expect("a job is queued");
            served.push(job.request.uri.clone());
            drop(job);
        };

        // Free the slot. w1 is *notified* but has not run yet — and a
        // brand-new dispatcher arriving right now steals the freed slot
        // inline, completing without ever waiting.
        pull();
        let _rx_late = pool
            .dispatch(request_with_uri("/late"))
            .await
            .expect("a newcomer takes the freed slot ahead of the parked waiter");

        // w1 wakes, loses the race (queue full again), and re-parks; only the
        // next freed slot admits it.
        tokio::task::yield_now().await;
        assert!(!w1.is_finished(), "the parked waiter must still be waiting after being lapped");
        pull();
        tokio::task::yield_now().await;
        pull();
        w1.await.expect("the lapped waiter is eventually admitted");

        assert_eq!(
            served,
            vec!["/a", "/late", "/w1"],
            "barge admission must let the newcomer overtake the parked waiter"
        );
    }

    /// Barge dispatch against a drained pool errors and leaves the depth
    /// untouched — the same accounting invariant the FIFO path pins.
    #[tokio::test]
    async fn barge_failed_dispatch_does_not_leak_queue_depth() {
        let pool = WorkerPool::spawn(
            PathBuf::from("/nonexistent/worker.php"),
            0,
            500,
            4,
            Duration::from_secs(30),
            Duration::from_secs(60),
            AdmissionPolicy::Barge,
        );
        pool.drain();
        assert!(pool.dispatch(dummy_request()).await.is_err());
        assert_eq!(pool.queue_depth(), 0, "failed barge enqueue must roll back the increment");
    }

    /// The depth gauge tracks the overflow queue. With no workers to pull
    /// (stub mode), each successful dispatch leaves the job queued and the
    /// depth reflects it.
    #[tokio::test]
    async fn dispatch_increments_queue_depth() {
        let pool = WorkerPool::spawn(
            PathBuf::from("/nonexistent/worker.php"),
            0,
            500,
            4, // backlog: room for a few queued jobs
            Duration::from_secs(30),
            Duration::from_secs(60),
            AdmissionPolicy::Fifo,
        );
        assert_eq!(pool.queue_depth(), 0);

        // Two successful enqueues (no worker pulls them) bump the depth to 2.
        assert!(pool.dispatch(dummy_request()).await.is_ok());
        assert!(pool.dispatch(dummy_request()).await.is_ok());
        assert_eq!(pool.queue_depth(), 2, "each enqueue must increment depth");
    }

    /// A dispatch that fails because the channel is closed (draining) must NOT
    /// leak an increment into the depth counter.
    #[tokio::test]
    async fn failed_dispatch_does_not_leak_queue_depth() {
        let pool = WorkerPool::spawn(
            PathBuf::from("/nonexistent/worker.php"),
            0,
            500,
            4,
            Duration::from_secs(30),
            Duration::from_secs(60),
            AdmissionPolicy::Fifo,
        );
        pool.drain(); // closes the sender
        assert!(pool.dispatch(dummy_request()).await.is_err());
        assert_eq!(pool.queue_depth(), 0, "failed enqueue must roll back the increment");
    }

    #[test]
    fn internal_error_response_is_500() {
        match WorkerResponse::internal_error() {
            WorkerResponse::Buffered { status, .. } => assert_eq!(status, 500),
            WorkerResponse::Streaming { .. } => panic!("internal_error must be buffered"),
        }
    }
}

//! Persistent-worker engine pool (worker mode — `worker-mode-design.md` §2, §5).
//!
//! A fixed pool of dedicated OS threads (NOT `spawn_blocking` — parking N
//! threads forever would starve the shared tokio blocking pool). Each thread
//! boots the framework once via [`PhpRuntime::run_worker`], then loops over
//! HTTP requests handed to it through an `async_channel::bounded` dispatch
//! queue guarded by a FIFO-fair admission semaphore (issue #442 — see
//! [`WorkerPool::admission`]), replying on a `tokio::sync::oneshot`.
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
//! - **Hung-worker replacement:** on an `oneshot` timeout the router calls
//!   [`WorkerPool::note_hung`]; the pool spawns a replacement and abandons the
//!   stuck thread (a wedged PHP thread cannot be killed without corrupting the
//!   ZMM — replace, don't kill; matches RoadRunner / FrankenPHP).
//! - **Graceful drain:** [`WorkerPool::drain`] closes the dispatch sender;
//!   each worker's `take_request()` then returns null, the loop ends, and the
//!   thread exits after any in-flight request completes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ephpm_config::AdmissionPolicy;
use ephpm_php::PhpRuntime;
use ephpm_php::worker_bridge::{WorkerJob, WorkerRequestOwned, WorkerResponse};
use metrics::{counter, gauge, histogram};
use tokio::sync::oneshot;

/// Rolls back a queue-depth increment on drop unless [`Self::defuse`]d.
///
/// The barge admission path (`[php] admission = "barge"`) increments the
/// depth counter *before* an awaitable `send()` — the pre-#443 discipline, so
/// a worker pulling concurrently can only ever decrement a value already
/// added — but that `send().await` is a cancellation point (the outer request
/// timeout drops the dispatch future). Without this guard a cancelled send
/// would leak the increment into the depth gauge forever. The FIFO path has
/// no await between increment and enqueue and does not need it.
pub(crate) struct DepthRollback<'a>(Option<&'a AtomicUsize>);

impl<'a> DepthRollback<'a> {
    /// Arm a rollback of one increment on `depth`.
    pub(crate) fn new(depth: &'a AtomicUsize) -> Self {
        Self(Some(depth))
    }

    /// The job entered the queue — the increment stands (the puller undoes it).
    pub(crate) fn defuse(mut self) {
        self.0 = None;
    }
}

impl Drop for DepthRollback<'_> {
    fn drop(&mut self) {
        if let Some(depth) = self.0 {
            depth.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// The dispatch channel is closed (pool draining or all workers gone). The
/// router turns this into a 503.
#[derive(Debug, Clone, Copy)]
pub struct DispatchClosed;

/// Handle to the running worker pool. Cloneable-cheap via `Arc`.
pub struct WorkerPool {
    /// Dispatch queue: the hyper handler enqueues jobs here after passing
    /// [`WorkerPool::admission`]; worker threads `recv_blocking()`. Capacity
    /// equals the admission permit count, so a permit holder's `try_send`
    /// can never find it full.
    dispatch_tx: async_channel::Sender<WorkerJob>,
    /// Kept alive so the channel never closes while the supervisor respawns
    /// workers between boots. Cloned into each worker thread.
    dispatch_rx: async_channel::Receiver<WorkerJob>,
    /// Jobs currently sitting in the dispatch channel. Incremented on enqueue
    /// ([`WorkerPool::dispatch`]) and decremented by each worker in the bridge
    /// after `recv_blocking`. Backs `ephpm_worker_dispatch_queue_depth` with a
    /// single `Relaxed` load, replacing a per-dispatch `async_channel::len()`
    /// (a SeqCst spin-loop over head/tail).
    queue_depth: Arc<AtomicUsize>,
    /// FIFO admission gate in front of the dispatch queue (issue #442).
    ///
    /// Why not just `dispatch_tx.send().await`? A bounded `async_channel`
    /// send is a try/listen/retry loop: a **new** sender always `try_send`s
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
    /// it pulls the job, keeping the queue refill pipeline — and therefore
    /// throughput — identical to the old barging behaviour.
    admission: Arc<tokio::sync::Semaphore>,
    /// Which admission discipline [`WorkerPool::dispatch`] uses
    /// (`[php] admission`): `Fifo` waits on [`WorkerPool::admission`] in
    /// strict arrival order (the #443 fix); `Barge` restores the pre-#443
    /// path — wait in the bounded channel's own `send().await`, which lets
    /// fresh dispatchers steal freed slots ahead of parked waiters.
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
        let (dispatch_tx, dispatch_rx) = async_channel::bounded(backlog.max(1));
        let state = Arc::new(PoolState {
            ready: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            boot_failures: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            next_id: AtomicUsize::new(0),
        });

        gauge!("ephpm_worker_pool_size").set(worker_count as f64);
        gauge!("ephpm_worker_idle").set(0.0);
        gauge!("ephpm_worker_busy").set(0.0);

        let pool = Arc::new(Self {
            dispatch_tx,
            dispatch_rx,
            queue_depth: Arc::new(AtomicUsize::new(0)),
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

    /// Current dispatch-queue depth (jobs enqueued, not yet pulled by a
    /// worker). Test-only accessor for the counter-pair accounting.
    #[cfg(test)]
    fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
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
    /// (admission and dispatch channel are closed) — the caller should 503.
    pub async fn dispatch(
        &self,
        request: WorkerRequestOwned,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        if self.admission_policy == AdmissionPolicy::Barge {
            return self.dispatch_barge(request).await;
        }
        // Wait for a queue slot in strict arrival order. Errors only when
        // `drain()` closed the semaphore.
        let Ok(permit) = Arc::clone(&self.admission).acquire_owned().await else {
            return Err(DispatchClosed);
        };
        let (tx, rx) = oneshot::channel();
        let job = WorkerJob { request, respond_to: tx, admission: Some(permit) };
        // Holding a permit guarantees a free channel slot (capacity == permit
        // count, and every enqueued job holds one permit until a worker pulls
        // it), so this `try_send` cannot see `Full` — no await, no suspension,
        // and the increment-to-enqueue window cannot be cancelled mid-way.
        let depth = self.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_worker_dispatch_queue_depth").set(depth as f64);
        match self.dispatch_tx.try_send(job) {
            Ok(()) => Ok(rx),
            Err(e) => {
                // Closed (draining) — or, defensively, a Full that the permit
                // invariant says cannot happen. Either way the job never
                // entered the queue and no worker will pull it, so undo the
                // accounting; the permit inside the dropped job frees itself.
                debug_assert!(
                    matches!(e, async_channel::TrySendError::Closed(_)),
                    "dispatch queue full while holding an admission permit"
                );
                self.queue_depth.fetch_sub(1, Ordering::Relaxed);
                Err(DispatchClosed)
            }
        }
    }

    /// The pre-#443 dispatch path (`[php] admission = "barge"`): wait for a
    /// queue slot inside the bounded channel's `send().await` instead of the
    /// FIFO admission semaphore.
    ///
    /// A bounded `async_channel` send is a try/listen/retry loop — a fresh
    /// sender `try_send`s before ever queueing behind parked ones, and a
    /// notified waiter that loses that race re-registers at the back — so
    /// admission order is a race, not a queue. Kept selectable as an operator
    /// escape hatch; see [`WorkerPool::admission`] for why FIFO is the
    /// default. The job carries `admission: None`, so the worker-side permit
    /// release in the bridge is a no-op.
    async fn dispatch_barge(
        &self,
        request: WorkerRequestOwned,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        let (tx, rx) = oneshot::channel();
        let job = WorkerJob { request, respond_to: tx, admission: None };
        // Account the enqueue before the (awaitable) send so a worker pulling
        // concurrently can only ever decrement a value we already added. The
        // rollback guard undoes it if the job never enters the queue — on the
        // closed-channel error below, or when the outer request timeout
        // cancels us mid-`send().await`.
        let depth = self.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        #[allow(clippy::cast_precision_loss)]
        gauge!("ephpm_worker_dispatch_queue_depth").set(depth as f64);
        let rollback = DepthRollback::new(&self.queue_depth);
        match self.dispatch_tx.send(job).await {
            Ok(()) => {
                rollback.defuse();
                Ok(rx)
            }
            Err(_) => Err(DispatchClosed),
        }
    }

    /// Record that a worker appears hung (its `oneshot` timed out). The stuck
    /// thread is abandoned and a replacement is spawned to keep the pool at
    /// `worker_count` live pullers (design §5.4).
    pub fn note_hung(self: &Arc<Self>) {
        counter!("ephpm_worker_recycles_total", "reason" => "hung").increment(1);
        // The stuck thread still holds its dispatch-receiver clone and may
        // eventually finish; we simply add capacity. A brief over-provision is
        // preferable to a wedged pool.
        if !self.state.draining.load(Ordering::Acquire) {
            self.spawn_worker();
            tracing::warn!("worker appeared hung — spawned replacement, abandoned stuck thread");
        }
    }

    /// Begin graceful drain: stop accepting new jobs and let workers exit once
    /// their in-flight request (if any) completes. Idempotent.
    pub fn drain(&self) {
        if self.state.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        // Closing the sender makes each worker's recv_blocking return Err, so
        // take_request() returns null and the framework loop ends. Closing the
        // admission semaphore wakes every dispatcher parked in `dispatch()`
        // with an error (503) — mirroring how parked `send().await`s used to
        // fail when the channel closed.
        self.dispatch_tx.close();
        self.admission.close();
        tracing::info!("worker pool draining — dispatch closed");
    }

    /// Spawn one worker OS thread that boots the framework once, serves until
    /// recycle/bailout/drain, then exits. The supervisor respawns a
    /// replacement unless the pool is draining.
    fn spawn_worker(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        let worker_id = pool.state.next_id.fetch_add(1, Ordering::Relaxed);
        let rx = pool.dispatch_rx.clone();
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
            worker_main(&pool, worker_id, &rx, &script, max_requests, boot_timeout);
        });

        if let Err(e) = spawn_result {
            self.state.live.fetch_sub(1, Ordering::AcqRel);
            tracing::error!(worker_id, %e, "failed to spawn worker thread");
            counter!("ephpm_worker_boot_failures_total").increment(1);
        }
    }
}

/// Body of one worker OS thread: register with TSRM, install its dispatch
/// receiver + recycle counter, boot the framework, serve, then exit (the
/// supervisor spawns the replacement).
fn worker_main(
    pool: &Arc<WorkerPool>,
    worker_id: usize,
    rx: &async_channel::Receiver<WorkerJob>,
    script: &std::path::Path,
    max_requests: u64,
    boot_timeout: Duration,
) {
    // `live` was incremented in spawn_worker before this thread started; we
    // decrement it on every exit path below.

    // Install this thread's dispatch receiver and recycle quota BEFORE booting,
    // so the very first take_request() inside the framework loop can pull work.
    ephpm_php::worker_bridge::set_dispatch_receiver(rx.clone());
    ephpm_php::worker_bridge::set_dispatch_depth_counter(Arc::clone(&pool.queue_depth));
    ephpm_php::worker_bridge::set_max_requests(max_requests);
    ephpm_php::worker_bridge::set_stream_send_timeout(pool.stream_send_timeout);

    // TSRM register + start the one long-lived request the whole loop runs in.
    if let Err(e) = PhpRuntime::worker_thread_init() {
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

    // The worker is no longer serving (only if it ever was).
    let was_booted = booted.load(Ordering::Acquire);
    if was_booted {
        pool.state.ready.fetch_sub(1, Ordering::AcqRel);
    }

    if was_booted {
        match outcome {
            Ok(ephpm_php::WorkerExit::Clean) => {
                // Clean loop end: graceful drain or max_requests recycle.
                let requests_served = ephpm_php::worker_bridge::requests_handled();
                let uptime_secs = boot_start.elapsed().as_secs_f64();
                if pool.state.draining.load(Ordering::Acquire) {
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
    respawn_if_running(pool);
}

/// Spawn a replacement worker unless the pool is draining or already at target.
///
/// Gating on `live < worker_count` is what makes hung-worker replacement
/// self-balancing: `note_hung` spawns a replacement (live -> count+1) while the
/// stuck thread is abandoned; when that stuck thread eventually exits it finds
/// `live == count` and does NOT respawn, so the pool converges back to target.
fn respawn_if_running(pool: &Arc<WorkerPool>) {
    if pool.state.draining.load(Ordering::Acquire) {
        return;
    }
    if pool.state.live.load(Ordering::Acquire) >= pool.worker_count {
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
            0, // no workers: this test plays the worker by pulling dispatch_rx
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
            let job = pool.dispatch_rx.try_recv().expect("a job is queued");
            pool.queue_depth.fetch_sub(1, Ordering::Relaxed);
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
    /// `send()` `try_send`s first and steals the slot — under FIFO the same
    /// inline dispatch would park behind the waiter (and this single-threaded
    /// test would deadlock), so the inline completion *is* the barge proof.
    #[tokio::test]
    async fn admission_barge_lets_a_newcomer_steal_a_freed_slot() {
        let pool = WorkerPool::spawn(
            PathBuf::from("/nonexistent/worker.php"),
            0, // no workers: this test plays the worker by pulling dispatch_rx
            500,
            1, // one queue slot => the second dispatcher parks in send()
            Duration::from_secs(30),
            Duration::from_secs(60),
            AdmissionPolicy::Barge,
        );

        // Fills the single queue slot immediately.
        let _rx_a = pool.dispatch(request_with_uri("/a")).await.expect("first dispatch fits");

        // w1 parks inside the bounded channel's send().await.
        let w1 = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move {
                pool.dispatch(request_with_uri("/w1")).await.expect("eventually admitted");
            })
        };
        tokio::task::yield_now().await;

        let mut served = Vec::new();
        let mut pull = || {
            let job = pool.dispatch_rx.try_recv().expect("a job is queued");
            pool.queue_depth.fetch_sub(1, Ordering::Relaxed);
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

    /// Barge dispatch against a drained pool errors and rolls back its depth
    /// increment — the same accounting invariant the FIFO path pins, but on
    /// the barge path the increment happens *before* an awaitable send, so
    /// the rollback guard is what keeps the gauge honest.
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

    /// The counter-pair depth gauge tracks enqueues. With no workers to pull
    /// (stub mode), each successful dispatch leaves the job in the channel and
    /// the depth counter reflects it — replacing the per-dispatch
    /// `async_channel::len()` read.
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

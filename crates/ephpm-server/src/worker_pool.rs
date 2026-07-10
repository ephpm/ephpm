//! Persistent-worker engine pool (worker mode — `worker-mode-design.md` §2, §5).
//!
//! A fixed pool of dedicated OS threads (NOT `spawn_blocking` — parking N
//! threads forever would starve the shared tokio blocking pool). Each thread
//! boots the framework once via [`PhpRuntime::run_worker`], then loops over
//! HTTP requests handed to it through an `async_channel::bounded` dispatch
//! queue, replying on a `tokio::sync::oneshot`.
//!
//! Lifecycle guarantees (design §5):
//! - **Boot-once:** the framework bootstrap runs once per worker thread; the
//!   worker then loops in `\Ephpm\Worker\take_request()`.
//! - **Recycle after N requests:** the C bridge returns shutdown once the
//!   per-worker counter hits `worker_max_requests`; the thread exits and the
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

use ephpm_php::PhpRuntime;
use ephpm_php::worker_bridge::{WorkerJob, WorkerRequestOwned, WorkerResponse};
use metrics::{counter, gauge, histogram};
use tokio::sync::oneshot;

/// The dispatch channel is closed (pool draining or all workers gone). The
/// router turns this into a 503.
#[derive(Debug, Clone, Copy)]
pub struct DispatchClosed;

/// Handle to the running worker pool. Cloneable-cheap via `Arc`.
pub struct WorkerPool {
    /// Dispatch queue: the hyper handler `send().await`s jobs here; worker
    /// threads `recv_blocking()`. Bounded — a full queue applies HTTP
    /// backpressure (the outer request timeout turns a starved queue into 504).
    dispatch_tx: async_channel::Sender<WorkerJob>,
    /// Kept alive so the channel never closes while the supervisor respawns
    /// workers between boots. Cloned into each worker thread.
    dispatch_rx: async_channel::Receiver<WorkerJob>,
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

    /// Dispatch a request to the pool and return the receiver for its response.
    ///
    /// `send().await` suspends when the bounded queue is full (backpressure);
    /// the caller wraps the whole thing in the outer request timeout, so a
    /// starved queue becomes a 504 rather than an unbounded wait.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchClosed`] if the pool is draining / all workers gone
    /// (the dispatch channel is closed) — the caller should 503.
    pub async fn dispatch(
        &self,
        request: WorkerRequestOwned,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        let (tx, rx) = oneshot::channel();
        let job = WorkerJob { request, respond_to: tx };
        gauge!("ephpm_worker_dispatch_queue_depth").set(self.dispatch_tx.len() as f64);
        match self.dispatch_tx.send(job).await {
            Ok(()) => Ok(rx),
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
        // take_request() returns null and the framework loop ends.
        self.dispatch_tx.close();
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

        let builder = std::thread::Builder::new().name(format!("ephpm-worker-{worker_id}"));
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
                        "worker has not finished booting within worker_boot_timeout \
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
                // Clean loop end: graceful drain or worker_max_requests recycle.
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
            Ok(ephpm_php::WorkerExit::Fatal) => {
                // Fatal bailout unwound past send_response. Fulfil the parked
                // oneshot with a 500 so the in-flight request never hangs, then
                // recycle (never resume on a possibly-corrupt kernel).
                if let Some(sender) = ephpm_php::worker_bridge::take_pending_sender() {
                    let _ = sender.send(WorkerResponse::internal_error());
                    tracing::warn!(
                        worker_id,
                        "worker bailed out mid-request — 500 sent, recycling"
                    );
                } else {
                    // No parked oneshot: the response was already begun. For a
                    // mid-stream bailout, closing the chunk channel below
                    // truncates the body without hanging.
                    tracing::warn!(
                        worker_id,
                        "worker bailed out between/after response — recycling"
                    );
                }
                // Close any still-open streaming channels so a mid-download
                // client sees EOF rather than hanging.
                ephpm_php::worker_bridge::clear_in_flight_streams();
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
        ephpm_php::worker_bridge::clear_in_flight_streams();
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

    #[test]
    fn internal_error_response_is_500() {
        match WorkerResponse::internal_error() {
            WorkerResponse::Buffered { status, .. } => assert_eq!(status, 500),
            WorkerResponse::Streaming { .. } => panic!("internal_error must be buffered"),
        }
    }
}

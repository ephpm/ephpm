//! Worker-mode **Xdebug debug lane** (prototype — `exp/xdebug-worker-mode`).
//!
//! ## The problem
//!
//! To PHP — and therefore to Xdebug — a persistent worker's whole life is
//! ONE request: `ephpm_thread_init()` starts it, the framework boots, the
//! `take_request()` loop serves thousands of HTTP requests inside it, and
//! `php_request_shutdown()` only runs when the thread retires. Xdebug decides
//! whether to open a DBGp session at RINIT (`xdebug.start_with_request =
//! trigger` looks for `XDEBUG_TRIGGER` / `XDEBUG_SESSION` in
//! `$_GET`/`$_POST`/`$_COOKIE`) and closes it at RSHUTDOWN. Both happen once
//! per worker thread, and the request they see carries no HTTP data at all.
//! Resetting Xdebug between loop iterations is not an option either: the
//! worker's own PHP frames are live while it sits in `take_request()`.
//!
//! ## The design
//!
//! Make the constraint the mechanism. A request carrying an Xdebug trigger
//! is routed here instead of the warm pool, and the lane spawns a dedicated
//! OS thread that:
//!
//! 1. registers with TSRM and starts its long-lived request with
//!    `SG(request_info)` **pre-seeded** from the triggering HTTP request
//!    ([`PhpRuntime::worker_thread_init_preseeded`]), so Xdebug's RINIT sees
//!    the trigger natively and opens the DBGp session *before any PHP runs*;
//! 2. boots the framework inside that session — breakpoints in boot code hit;
//! 3. serves exactly that one request through the ordinary
//!    `take_request()` / `send_response()` loop (the job is pre-queued on a
//!    private one-slot channel, `max_requests = 1`);
//! 4. lets the loop end, so [`PhpRuntime::worker_thread_shutdown`]'s
//!    `php_request_shutdown()` runs Xdebug's RSHUTDOWN: the session ends with
//!    a clean DBGp `stopping`, exactly as under php-fpm.
//!
//! Untriggered requests never come near this lane; the warm pool keeps
//! serving them with zero boot cost, even while a debug thread is parked on
//! a breakpoint. The price is one framework boot per *debugged* request —
//! the same cost php-fpm pays on every request — paid only by the developer
//! who asked for it. Nothing here is Xdebug-specific: the lane only
//! guarantees "this thread's PHP request is this HTTP request", and any
//! extension with per-request RINIT/RSHUTDOWN semantics benefits.
//!
//! Concurrency is capped ([`DebugLane::new`]'s `max_workers`) and sits
//! outside `[php] concurrency`: a developer paused on a breakpoint must not
//! consume a production serving slot, and a second triggered request queues
//! behind the first rather than spawning unboundedly.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ephpm_php::PhpRuntime;
use ephpm_php::worker_bridge::{DebugSeed, WorkerJob, WorkerRequestOwned, WorkerResponse};
use metrics::counter;
use tokio::sync::oneshot;

use crate::worker_pool::{DispatchClosed, WorkerPool};

/// Query-string / cookie names that make Xdebug (`start_with_request =
/// trigger`) open a session at RINIT. `XDEBUG_TRIGGER` is the Xdebug 3 name;
/// `XDEBUG_SESSION_START` (GET/POST) and `XDEBUG_SESSION` (cookie, also
/// what browser helper extensions set) are the legacy spellings Xdebug 3
/// still honours. `XDEBUG_SESSION_STOP*` deliberately does not count.
const TRIGGER_NAMES: [&str; 3] = ["XDEBUG_TRIGGER", "XDEBUG_SESSION_START", "XDEBUG_SESSION"];

/// Whether a request carries an Xdebug step-debug trigger in its query
/// string or `Cookie` header.
///
/// This mirrors *where* Xdebug looks, not its full policy: the value is not
/// checked against `xdebug.trigger_value` (Xdebug does that itself at RINIT —
/// a mismatched value costs one wasted boot on the lane, nothing else) and a
/// trigger carried only in a POST body is not detected (the lane does not
/// read request bodies at startup; see `ephpm_thread_init_preseeded`).
#[must_use]
pub fn xdebug_triggered(query_string: &str, headers: &[(String, String)]) -> bool {
    let in_query = query_string
        .split('&')
        .map(|pair| pair.split_once('=').map_or(pair, |(k, _)| k))
        .any(|key| TRIGGER_NAMES.contains(&key));
    if in_query {
        return true;
    }
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        .flat_map(|(_, value)| value.split(';'))
        .map(|pair| pair.trim().split_once('=').map_or(pair.trim(), |(k, _)| k.trim()))
        .any(|key| TRIGGER_NAMES.contains(&key))
}

/// Where a worker-mode request is executed: the warm pool, or a fresh
/// debug-lane thread for a request that carries an Xdebug trigger.
pub enum WorkerTarget {
    /// The persistent warm pool (every untriggered request).
    Pool(Arc<WorkerPool>),
    /// A one-request debug thread (triggered requests only).
    Debug(Arc<DebugLane>),
}

impl WorkerTarget {
    /// Dispatch to whichever lane this is. Same contract as
    /// [`WorkerPool::dispatch`].
    ///
    /// # Errors
    ///
    /// [`DispatchClosed`] when the lane cannot accept the request (pool
    /// draining, or the debug lane could not spawn its thread).
    pub async fn dispatch(
        &self,
        request: WorkerRequestOwned,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        match self {
            Self::Pool(pool) => pool.dispatch(request).await,
            Self::Debug(lane) => lane.dispatch(request).await,
        }
    }

    /// The request's `oneshot` timed out. The pool replaces the stuck worker;
    /// the debug lane has nothing to replace (its thread was never part of
    /// the serving set) and simply keeps its permit until the thread exits.
    pub fn note_hung(&self) {
        match self {
            Self::Pool(pool) => pool.note_hung(),
            Self::Debug(_) => {
                counter!("ephpm_xdebug_debug_requests_total", "outcome" => "timeout").increment(1);
                tracing::warn!(
                    "debug-lane request timed out (still paused in the debugger?) — raise \
                     [server.timeouts] request or set it to 0 while debugging; the thread \
                     keeps its debug slot until the session ends"
                );
            }
        }
    }
}

/// The debug lane: a capped supply of one-request debug threads.
pub struct DebugLane {
    /// One permit per concurrently running debug thread.
    permits: Arc<tokio::sync::Semaphore>,
    /// Worker entrypoint script (absolute; the same one the pool boots).
    script: PathBuf,
    /// Streaming-response send timeout handed to each debug thread.
    stream_send_timeout: Duration,
    /// Monotonic id source for thread names / log fields.
    next_id: AtomicUsize,
    /// Configured cap, for logging.
    max_workers: usize,
}

impl DebugLane {
    /// Create the lane. `max_workers` is the cap on simultaneously alive
    /// debug threads (a triggered request beyond it waits, FIFO, for a slot).
    #[must_use]
    pub fn new(script: PathBuf, max_workers: usize, stream_send_timeout: Duration) -> Arc<Self> {
        let max_workers = max_workers.max(1);
        tracing::info!(
            max_workers,
            script = %script.display(),
            "worker mode: Xdebug debug lane armed — requests carrying XDEBUG_TRIGGER / \
             XDEBUG_SESSION run on a fresh one-request worker thread"
        );
        Arc::new(Self {
            permits: Arc::new(tokio::sync::Semaphore::new(max_workers)),
            script,
            stream_send_timeout,
            next_id: AtomicUsize::new(0),
            max_workers,
        })
    }

    /// Configured cap on concurrent debug threads.
    #[must_use]
    pub fn max_workers(&self) -> usize {
        self.max_workers
    }

    /// Run `request` on a fresh debug thread and return the receiver for its
    /// response. Waits (FIFO) for a free debug slot first.
    ///
    /// # Errors
    ///
    /// [`DispatchClosed`] if the OS thread could not be spawned (the request
    /// never ran; the caller 503s).
    pub async fn dispatch(
        self: &Arc<Self>,
        request: WorkerRequestOwned,
    ) -> Result<oneshot::Receiver<WorkerResponse>, DispatchClosed> {
        let Ok(permit) = Arc::clone(&self.permits).acquire_owned().await else {
            return Err(DispatchClosed);
        };
        counter!("ephpm_xdebug_debug_requests_total", "outcome" => "dispatched").increment(1);

        // The seed is copied out BEFORE the request moves into the job: it is
        // what php_request_startup() sees; the job is what take_request()
        // hands to the framework. Same request, two consumers.
        let seed = DebugSeed::from(&request);
        let (respond_to, rx) = oneshot::channel();
        let (job_tx, job_rx) = async_channel::bounded(1);
        let job = WorkerJob { request, respond_to, admission: None };
        // Capacity 1 on a fresh channel: cannot be Full; a Closed error is
        // impossible while `job_rx` is held below.
        if job_tx.try_send(job).is_err() {
            return Err(DispatchClosed);
        }
        // Nothing else will ever be queued: after this one job the worker's
        // take_request() sees a closed channel (and max_requests = 1), ends
        // its loop, and the thread retires — which is what ends the session.
        job_tx.close();

        let lane = Arc::clone(self);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name(format!("ephpm-debug-worker-{id}"))
            // Same stack as pool workers: PHP's C-stack guard is sized from it.
            .stack_size(ephpm_php::PHP_THREAD_STACK)
            .spawn(move || {
                debug_worker_main(&lane, id, &seed, &job_rx);
                // Release the debug slot only once the thread is fully
                // retired (session closed, TSRM slot freed).
                drop(permit);
            });
        match spawned {
            Ok(_) => Ok(rx),
            Err(e) => {
                tracing::error!(debug_worker = id, %e, "failed to spawn debug worker thread");
                counter!("ephpm_xdebug_debug_requests_total", "outcome" => "spawn_failed")
                    .increment(1);
                // Dropping `job_rx` drops the job and its oneshot sender; the
                // caller sees RecvError → 500. 503 is more honest here.
                Err(DispatchClosed)
            }
        }
    }
}

/// Body of one debug thread: pre-seeded TSRM init, framework boot, serve the
/// one queued request, retire. Mirrors `worker_pool::worker_main` minus the
/// supervisor bookkeeping (no readiness, no respawn — this thread is not
/// part of the serving set).
fn debug_worker_main(
    lane: &Arc<DebugLane>,
    id: usize,
    seed: &DebugSeed,
    rx: &async_channel::Receiver<WorkerJob>,
) {
    ephpm_php::worker_bridge::set_dispatch_receiver(rx.clone());
    ephpm_php::worker_bridge::set_max_requests(1);
    ephpm_php::worker_bridge::set_stream_send_timeout(lane.stream_send_timeout);

    let start = Instant::now();
    ephpm_php::worker_bridge::set_boot_notifier(Box::new(move || {
        let boot_secs = start.elapsed().as_secs_f64();
        tracing::info!(
            debug_worker = id,
            boot_secs,
            "debug worker booted inside the DBGp session — serving the triggered request"
        );
    }));

    tracing::info!(
        debug_worker = id,
        method = %seed.method,
        uri = %seed.uri,
        "debug worker starting: pre-seeded request startup, then framework boot"
    );

    // TSRM register + php_request_startup with the HTTP request pre-seeded,
    // so Xdebug's RINIT honours the trigger before the framework boots.
    if let Err(e) = PhpRuntime::worker_thread_init_preseeded(seed, &lane.script) {
        tracing::error!(debug_worker = id, ?e, "debug worker request startup failed");
        counter!("ephpm_xdebug_debug_requests_total", "outcome" => "startup_failed").increment(1);
        // The unpulled job drops with `rx` at return → oneshot sender dropped
        // → the router answers 500.
        return;
    }

    let outcome = PhpRuntime::run_worker(&lane.script);

    // Crash seam, same as the pool: a response never sent becomes a 500, an
    // unfinished streamed body is aborted rather than ended cleanly.
    let unsent = ephpm_php::worker_bridge::take_pending_sender();
    let had_unsent = unsent.is_some();
    if let Some(sender) = unsent {
        let _ = sender.send(WorkerResponse::internal_error());
    }
    let aborted_stream = ephpm_php::worker_bridge::clear_in_flight_streams();

    let outcome_label = match &outcome {
        Ok(ephpm_php::WorkerExit::Clean) if !had_unsent => "ok",
        Ok(ephpm_php::WorkerExit::Clean) => "no_response",
        Ok(ephpm_php::WorkerExit::ScriptExit) => "script_exit",
        Ok(ephpm_php::WorkerExit::ScriptFatal) => "script_fatal",
        Ok(ephpm_php::WorkerExit::Fatal) => "bailout",
        Err(_) => "run_failed",
    };
    counter!("ephpm_xdebug_debug_requests_total", "outcome" => outcome_label).increment(1);
    if outcome_label == "ok" {
        tracing::info!(
            debug_worker = id,
            total_secs = start.elapsed().as_secs_f64(),
            "debug worker served its request — retiring (ends the DBGp session)"
        );
    } else {
        tracing::warn!(
            debug_worker = id,
            ?outcome,
            had_unsent,
            aborted_stream,
            "debug worker ended abnormally — retiring"
        );
    }

    // php_request_shutdown → Xdebug RSHUTDOWN → DBGp `stopping`; then the
    // TSRM slot is freed. Runs here, as ordinary code on this thread, so the
    // pre-seeded strings in the bridge's thread-local are still alive.
    PhpRuntime::worker_thread_shutdown();
    tracing::debug!(debug_worker = id, "debug worker retired");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(value: &str) -> Vec<(String, String)> {
        vec![("Cookie".to_string(), value.to_string())]
    }

    #[test]
    fn trigger_in_query_string() {
        assert!(xdebug_triggered("XDEBUG_TRIGGER=1", &[]));
        assert!(xdebug_triggered("a=1&XDEBUG_TRIGGER=phpstorm&b=2", &[]));
        assert!(xdebug_triggered("XDEBUG_SESSION_START=1", &[]));
        assert!(xdebug_triggered("XDEBUG_TRIGGER", &[]), "bare key, no value");
        assert!(!xdebug_triggered("", &[]));
        assert!(!xdebug_triggered("xdebug_trigger=1", &[]), "Xdebug's check is case-sensitive");
        assert!(!xdebug_triggered("XDEBUG_SESSION_STOP=1", &[]), "a stop is not a start");
        assert!(!xdebug_triggered("NOT_XDEBUG_TRIGGER=1", &[]));
    }

    #[test]
    fn trigger_in_cookie() {
        assert!(xdebug_triggered("", &cookie("XDEBUG_SESSION=PHPSTORM")));
        assert!(xdebug_triggered("", &cookie("a=b; XDEBUG_SESSION=1; c=d")));
        assert!(xdebug_triggered("", &cookie("XDEBUG_TRIGGER=1")));
        assert!(xdebug_triggered("", &[("cookie".to_string(), "XDEBUG_SESSION=1".to_string())]));
        assert!(!xdebug_triggered("", &cookie("XDEBUG_SESSION_STOP=1")));
        assert!(!xdebug_triggered("", &cookie("PHPSESSID=XDEBUG_SESSION")), "value, not name");
        assert!(!xdebug_triggered("", &[("X-Other".to_string(), "XDEBUG_SESSION=1".to_string())]));
    }

    /// In stub mode the lane spawns a thread whose pre-seeded init is a no-op
    /// and whose `run_worker` refuses (PHP not linked), so the job is never
    /// pulled and the oneshot sender drops: the receiver must observe that
    /// (→ 500 at the router) rather than hang, and the permit must come back.
    #[tokio::test]
    async fn stub_mode_dispatch_resolves_and_releases_slot() {
        let lane =
            DebugLane::new(PathBuf::from("/nonexistent/worker.php"), 1, Duration::from_secs(1));
        let req = WorkerRequestOwned {
            method: "GET".into(),
            uri: "/?XDEBUG_TRIGGER=1".into(),
            query_string: "XDEBUG_TRIGGER=1".into(),
            cookie_data: String::new(),
            content_type: None,
            body: ephpm_php::worker_bridge::WorkerBody::Buffered(Vec::new()),
            server_vars: Vec::new(),
            headers: Vec::new(),
        };
        let rx = lane.dispatch(req).await.expect("thread spawns");
        let got = tokio::time::timeout(Duration::from_secs(10), rx).await.expect("resolves");
        assert!(got.is_err(), "stub lane cannot produce a response; sender must drop");
        // The slot is released once the thread retires.
        let permit =
            tokio::time::timeout(Duration::from_secs(10), lane.permits.clone().acquire_owned())
                .await
                .expect("slot released after the debug thread retired");
        assert!(permit.is_ok());
    }
}

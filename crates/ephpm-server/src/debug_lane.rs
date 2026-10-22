//! Worker-mode **debug lane**: per-request Xdebug step debugging for
//! persistent workers.
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
//! Make the constraint the mechanism. A request that Xdebug would start a
//! session for ([`crate::xdebug_trigger`] mirrors its decision) is routed
//! here instead of the warm pool, and the lane spawns a dedicated OS thread
//! that:
//!
//! 1. registers with TSRM and starts its long-lived request with
//!    `SG(request_info)` **pre-seeded** from the triggering HTTP request
//!    ([`PhpRuntime::worker_thread_init_preseeded`]; a buffered urlencoded
//!    POST body is parsed into `$_POST` from a private copy), so Xdebug's
//!    RINIT sees the trigger natively and opens the DBGp session *before any
//!    PHP runs*;
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
//! who asked for it. When Xdebug is not loaded the lane does not exist and
//! routing is untouched (one `Option` check per worker-mode request).
//!
//! ## Capacity
//!
//! Two bounds, both derived from `[php.worker] debug_workers` (N), both
//! outside `[php] concurrency` so a developer paused on a breakpoint never
//! consumes a serving slot:
//!
//! - **N debug slots** — requests *in flight* on the lane. A triggered
//!   request beyond N **waits** (FIFO) for a slot rather than failing: with a
//!   browser helper extension every PHP request from the developer's tab
//!   carries the trigger cookie, and failing the page's AJAX calls with 503
//!   while one breakpoint is held would break the page being debugged. The
//!   wait is bounded by `[server.timeouts] request` and ends if the client
//!   goes away; and at most [`MAX_WAITERS_PER_SLOT`]·N requests may wait at
//!   once — beyond that a triggered request gets a 503 immediately, so even
//!   with `request = 0` (the recommended debugging setup) a client cannot
//!   pile up unbounded waiting requests. A slot is held by the HTTP request, not the thread: it is
//!   released the moment the response arrives **or the request is
//!   abandoned** (request timeout → 504, or the client disconnects).
//! - **2·N live debug threads**. An abandoned request's thread cannot be
//!   killed — it is parked inside Xdebug waiting for the IDE — so it keeps
//!   running until the IDE resumes or stops the session, then retires on its
//!   own; its response is discarded. The thread cap bounds how many such
//!   orphans (plus threads finishing their DBGp `stopping` handshake) can
//!   pile up: when it is reached, a triggered request waits briefly for one
//!   to retire and otherwise gets a 503, with a log line saying why.
//!
//! On shutdown [`DebugLane::drain`] closes the slots (waiting requests get
//! 503) and the server waits, bounded by `[server.timeouts] shutdown`, for
//! live debug threads to retire; a session still paused in an IDE keeps its
//! thread's PHP context alive, so PHP module shutdown is skipped (the same
//! safe path a wedged pool worker takes).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ephpm_php::PhpRuntime;
use ephpm_php::worker_bridge::{
    DebugSeed, WorkerBody, WorkerJob, WorkerRequestOwned, WorkerResponse,
};
use metrics::{counter, gauge};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

use crate::worker_pool::{DispatchClosed, WorkerPool};
use crate::xdebug_trigger::{TriggerInput, TriggerPolicy};

/// How long a triggered request that found the live-thread cap reached waits
/// for a debug thread to retire before it is refused with 503. Threads that
/// just served their request retire within milliseconds once the IDE
/// acknowledges `stopping`; only abandoned sessions parked in an IDE outlast
/// this.
const RETIRE_GRACE: Duration = Duration::from_secs(2);

/// Waiting triggered requests allowed per debug slot. Generous for a page
/// whose AJAX calls all carry the trigger cookie; small enough that waiting
/// requests (each holding a connection and a body of up to
/// `[php.worker] stream_threshold`) stay bounded when `[server.timeouts]
/// request = 0`.
pub const MAX_WAITERS_PER_SLOT: usize = 16;

/// Body of one debug thread. Production: [`debug_worker_main`]; tests inject
/// a fake so slot/thread accounting can be exercised without PHP.
pub(crate) type Runner =
    Arc<dyn Fn(usize, DebugSeed, &async_channel::Receiver<WorkerJob>) + Send + Sync + 'static>;

/// Where a worker-mode request is executed: the warm pool, or a fresh
/// debug-lane thread for a request that will start an Xdebug session.
pub enum WorkerTarget<'a> {
    /// The persistent warm pool (every untriggered request).
    Pool(&'a Arc<WorkerPool>),
    /// A one-request debug thread (triggered requests only).
    Debug(&'a Arc<DebugLane>),
}

/// A dispatched worker-mode request: the response receiver, plus — on the
/// debug lane — the debug slot the HTTP request holds until it is answered
/// or abandoned.
pub struct Dispatched {
    /// Resolves with the worker's response; `Err` if the worker dropped it.
    pub rx: oneshot::Receiver<WorkerResponse>,
    slot: Option<DebugSlot>,
}

impl Dispatched {
    /// Record that the worker answered (with a response or by dropping the
    /// sender), so releasing the slot is not reported as an abandonment.
    pub fn mark_answered(&mut self) {
        if let Some(slot) = self.slot.as_mut() {
            slot.answered = true;
        }
    }

    /// Split into the response receiver (for a reply wait that takes
    /// ownership of it) and the guard that holds the debug slot.
    #[must_use]
    pub fn into_parts(self) -> (oneshot::Receiver<WorkerResponse>, SlotGuard) {
        (self.rx, SlotGuard(self.slot))
    }
}

/// The debug slot of a [`Dispatched`] request after [`Dispatched::into_parts`]
/// (empty for the warm pool). Hold it until the request is done; dropping it
/// without [`SlotGuard::mark_answered`] reports the request abandoned.
pub struct SlotGuard(Option<DebugSlot>);

impl SlotGuard {
    /// See [`Dispatched::mark_answered`].
    pub fn mark_answered(&mut self) {
        if let Some(slot) = self.0.as_mut() {
            slot.answered = true;
        }
    }
}

/// One held debug slot. Dropping it frees the slot for the next triggered
/// request; dropping it unanswered means the request was abandoned (timeout
/// or client disconnect) while its thread is still running.
struct DebugSlot {
    _permit: OwnedSemaphorePermit,
    id: usize,
    answered: bool,
}

impl Drop for DebugSlot {
    fn drop(&mut self) {
        if !self.answered {
            counter!("ephpm_xdebug_debug_requests_total", "outcome" => "abandoned").increment(1);
            tracing::warn!(
                debug_worker = self.id,
                "debug request abandoned (request timeout or client disconnect) — its slot is \
                 released now; the thread stays paused in the debugger until the IDE resumes or \
                 stops the session, then retires and its response is discarded"
            );
        }
    }
}

impl WorkerTarget<'_> {
    /// Dispatch to whichever lane this is.
    ///
    /// # Errors
    ///
    /// [`DispatchClosed`] when the lane cannot accept the request (pool or
    /// lane draining, debug thread cap reached, or the debug thread could
    /// not be spawned). The caller answers 503.
    pub async fn dispatch(
        &self,
        request: WorkerRequestOwned,
    ) -> Result<Dispatched, DispatchClosed> {
        match self {
            Self::Pool(pool) => {
                pool.dispatch(request).await.map(|rx| Dispatched { rx, slot: None })
            }
            Self::Debug(lane) => lane.dispatch_tracked(request, None).await,
        }
    }

    /// [`WorkerTarget::dispatch`], plus a flag the worker raises when it
    /// pulls the job — how the reply wait tells a request that was *running*
    /// at its deadline from one still queued (see [`crate::php_wait`]).
    ///
    /// # Errors
    ///
    /// As [`WorkerTarget::dispatch`].
    pub(crate) async fn dispatch_tracked(
        &self,
        request: WorkerRequestOwned,
        started: &crate::php_wait::StartFlag,
    ) -> Result<Dispatched, DispatchClosed> {
        let started = Some(Arc::clone(started));
        match self {
            Self::Pool(pool) => pool
                .dispatch_tracked(request, started)
                .await
                .map(|rx| Dispatched { rx, slot: None }),
            Self::Debug(lane) => lane.dispatch_tracked(request, started).await,
        }
    }
}

/// A started debug-lane request missed its deadline. There is nothing to
/// replace — the thread was never part of the serving set, and its slot is
/// released when the request's [`SlotGuard`] drops — so this only explains
/// the 504.
impl crate::php_wait::HungTracker for DebugLane {
    fn hung(_lane: &Arc<Self>) {
        tracing::warn!(
            "debug-lane request hit [server.timeouts] request (still paused in the debugger?) — \
             set it to 0 while step debugging"
        );
    }
    fn recovered(_lane: &Arc<Self>) {}
}

/// Decrements the live-thread count when a debug thread ends (normally or by
/// unwinding) and wakes requests waiting on the thread cap.
struct LiveGuard {
    live: Arc<AtomicUsize>,
    retired: Arc<Notify>,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
        gauge!("ephpm_xdebug_debug_workers").decrement(1.0);
        self.retired.notify_waiters();
    }
}

/// Counts one request waiting for a debug slot; released when the wait ends
/// for any reason (slot acquired, lane closed, or the request's future
/// dropped by a timeout / client disconnect).
struct WaitGuard<'a>(&'a AtomicUsize);

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The debug lane: a capped supply of one-request debug threads.
pub struct DebugLane {
    /// Which requests Xdebug would start a session for.
    policy: TriggerPolicy,
    /// One permit per in-flight debug request (`debug_workers`).
    slots: Arc<Semaphore>,
    /// Requests currently waiting for a slot.
    waiting: AtomicUsize,
    /// Cap on `waiting` (`MAX_WAITERS_PER_SLOT × debug_workers`).
    max_waiting: usize,
    /// Debug threads currently alive (in flight, retiring, or orphaned).
    live: Arc<AtomicUsize>,
    /// Cap on `live` (`2 × debug_workers`).
    max_live: usize,
    /// Notified whenever a debug thread retires.
    retired: Arc<Notify>,
    /// Set by [`DebugLane::drain`].
    draining: AtomicBool,
    /// Monotonic id source for thread names / log fields.
    next_id: AtomicUsize,
    /// Configured `debug_workers`.
    max_workers: usize,
    /// Thread body.
    runner: Runner,
}

impl DebugLane {
    /// Create the lane. `max_workers` (≥ 1) is `[php.worker] debug_workers`.
    /// `script` is the worker entrypoint the pool boots;
    /// `stream_send_timeout` is the pool's streaming-response send timeout.
    #[must_use]
    pub fn new(
        script: PathBuf,
        max_workers: usize,
        policy: TriggerPolicy,
        stream_send_timeout: Duration,
    ) -> Arc<Self> {
        let runner: Runner = Arc::new(move |id, seed, rx| {
            debug_worker_main(id, seed, rx, &script, stream_send_timeout);
        });
        Self::with_runner(max_workers, policy, runner)
    }

    pub(crate) fn with_runner(
        max_workers: usize,
        policy: TriggerPolicy,
        runner: Runner,
    ) -> Arc<Self> {
        let max_workers = max_workers.max(1);
        // Registers the gauge at 0 so dashboards see the lane exists.
        gauge!("ephpm_xdebug_debug_workers").increment(0.0);
        Arc::new(Self {
            policy,
            slots: Arc::new(Semaphore::new(max_workers)),
            waiting: AtomicUsize::new(0),
            max_waiting: max_workers.saturating_mul(MAX_WAITERS_PER_SLOT),
            live: Arc::new(AtomicUsize::new(0)),
            max_live: max_workers.saturating_mul(2),
            retired: Arc::new(Notify::new()),
            draining: AtomicBool::new(false),
            next_id: AtomicUsize::new(0),
            max_workers,
            runner,
        })
    }

    /// Configured cap on concurrent debug requests.
    #[must_use]
    pub fn max_workers(&self) -> usize {
        self.max_workers
    }

    /// The trigger policy the lane routes by.
    #[must_use]
    pub fn policy(&self) -> &TriggerPolicy {
        &self.policy
    }

    /// Debug threads currently alive.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// Whether `req` should run on this lane: Xdebug would open a step
    /// debugging session for it at request startup.
    #[must_use]
    pub fn routes(&self, req: &WorkerRequestOwned) -> bool {
        let buffered_body = match &req.body {
            WorkerBody::Buffered(bytes) => Some(bytes.as_slice()),
            WorkerBody::Streaming { .. } => None,
        };
        self.policy.routes(&TriggerInput {
            method: &req.method,
            query_string: &req.query_string,
            cookie_data: &req.cookie_data,
            content_type: req.content_type.as_deref(),
            buffered_body,
        })
    }

    /// Stop accepting debug requests: waiting and future dispatches fail
    /// with [`DispatchClosed`] (→ 503). Live threads finish on their own.
    pub fn drain(&self) {
        if !self.draining.swap(true, Ordering::AcqRel) {
            self.slots.close();
            let live = self.live_count();
            if live > 0 {
                tracing::info!(live, "debug lane draining — waiting for debug threads to retire");
            }
        }
    }

    /// Atomically increment the live-thread count unless it is at the cap.
    #[allow(
        deprecated,
        reason = "`try_update` (the `fetch_update` rename) postdates our MSRV 1.88; switch when \
                  MSRV allows"
    )]
    fn try_take_live(&self) -> bool {
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max_live).then_some(n + 1)
            })
            .is_ok()
    }

    /// Reserve one live-thread place, waiting up to [`RETIRE_GRACE`] for a
    /// thread to retire if the cap is reached.
    async fn reserve_thread(&self) -> Option<LiveGuard> {
        let deadline = tokio::time::Instant::now() + RETIRE_GRACE;
        loop {
            // Register for the wake-up BEFORE checking, so a retirement
            // between the check and the wait is not missed.
            let retired = self.retired.notified();
            tokio::pin!(retired);
            retired.as_mut().enable();
            if self.try_take_live() {
                gauge!("ephpm_xdebug_debug_workers").increment(1.0);
                return Some(LiveGuard {
                    live: Arc::clone(&self.live),
                    retired: Arc::clone(&self.retired),
                });
            }
            if tokio::time::timeout_at(deadline, retired).await.is_err() {
                return None;
            }
        }
    }

    /// Take a debug slot, waiting (FIFO) if all are in use — unless
    /// [`MAX_WAITERS_PER_SLOT`] × `debug_workers` requests are already
    /// waiting. `Err(true)` = waiting room full, `Err(false)` = lane closed.
    async fn acquire_slot(&self) -> Result<OwnedSemaphorePermit, bool> {
        match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => return Ok(permit),
            Err(tokio::sync::TryAcquireError::Closed) => return Err(false),
            Err(tokio::sync::TryAcquireError::NoPermits) => {}
        }
        if self.waiting.fetch_add(1, Ordering::AcqRel) >= self.max_waiting {
            self.waiting.fetch_sub(1, Ordering::AcqRel);
            return Err(true);
        }
        let _waiting = WaitGuard(&self.waiting);
        Arc::clone(&self.slots).acquire_owned().await.map_err(|_| false)
    }

    /// Run `request` on a fresh debug thread. Waits (FIFO) for a free debug
    /// slot first.
    ///
    /// # Errors
    ///
    /// [`DispatchClosed`] if the lane is draining, too many requests are
    /// already waiting for a slot, the live-thread cap stays reached
    /// (abandoned sessions still paused in an IDE), or the OS thread could
    /// not be spawned. The request never ran; the caller 503s.
    pub async fn dispatch(
        self: &Arc<Self>,
        request: WorkerRequestOwned,
    ) -> Result<Dispatched, DispatchClosed> {
        self.dispatch_tracked(request, None).await
    }

    /// [`DebugLane::dispatch`] with the job's start flag (see
    /// [`WorkerTarget::dispatch_tracked`]).
    async fn dispatch_tracked(
        self: &Arc<Self>,
        request: WorkerRequestOwned,
        started: Option<Arc<AtomicBool>>,
    ) -> Result<Dispatched, DispatchClosed> {
        let permit = match self.acquire_slot().await {
            Ok(permit) => permit,
            Err(waiting_room_full) => {
                if waiting_room_full {
                    counter!("ephpm_xdebug_debug_requests_total", "outcome" => "rejected")
                        .increment(1);
                    tracing::warn!(
                        max_waiting = self.max_waiting,
                        "debug lane: too many triggered requests already waiting for a debug \
                         slot; answering 503"
                    );
                }
                return Err(DispatchClosed);
            }
        };
        let Some(live_guard) = self.reserve_thread().await else {
            counter!("ephpm_xdebug_debug_requests_total", "outcome" => "rejected").increment(1);
            tracing::warn!(
                live_threads = self.live_count(),
                max_threads = self.max_live,
                "debug lane full: abandoned debug sessions are still paused in an IDE — resume or \
                 stop them (or raise [php.worker] debug_workers); answering 503"
            );
            return Err(DispatchClosed);
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        // The seed is copied out BEFORE the request moves into the job: it is
        // what php_request_startup() sees; the job is what take_request()
        // hands to the framework. Same request, two consumers.
        let seed = DebugSeed::from_request(&request);
        let (respond_to, rx) = oneshot::channel();
        let (job_tx, job_rx) = async_channel::bounded(1);
        let job = WorkerJob { request, respond_to, admission: None, started };
        // Capacity 1 on a fresh channel whose receiver is held here: neither
        // Full nor Closed is possible.
        if job_tx.try_send(job).is_err() {
            return Err(DispatchClosed);
        }
        // Nothing else will ever be queued: after this one job the worker's
        // take_request() sees a closed channel (and max_requests = 1), ends
        // its loop, and the thread retires — which is what ends the session.
        job_tx.close();

        let runner = Arc::clone(&self.runner);
        let spawned = std::thread::Builder::new()
            .name(format!("ephpm-debug-worker-{id}"))
            // Same stack as pool workers: PHP's C-stack guard is sized from it.
            .stack_size(ephpm_php::PHP_THREAD_STACK)
            .spawn(move || {
                // Dropped last, after the thread body (and its PHP shutdown)
                // has completed — or while unwinding.
                let _live = live_guard;
                runner(id, seed, &job_rx);
            });
        match spawned {
            Ok(_) => {
                counter!("ephpm_xdebug_debug_requests_total", "outcome" => "dispatched")
                    .increment(1);
                Ok(Dispatched {
                    rx,
                    slot: Some(DebugSlot { _permit: permit, id, answered: false }),
                })
            }
            Err(e) => {
                // The closure (job, live guard) was dropped by the failed
                // spawn, releasing the thread place.
                tracing::error!(debug_worker = id, %e, "failed to spawn debug worker thread");
                counter!("ephpm_xdebug_debug_requests_total", "outcome" => "spawn_failed")
                    .increment(1);
                Err(DispatchClosed)
            }
        }
    }
}

/// Decide at startup whether to arm the debug lane, logging the decision.
///
/// Returns `None` — routing untouched — unless `debug_workers > 0`, the
/// xdebug extension is loaded (`xdebug` is its startup snapshot), and its
/// settings let a step-debugging session start at request startup. When it
/// arms, it warns: a loaded Xdebug is a development-only configuration, and
/// any client that can reach the server can send a trigger.
#[must_use]
pub fn arm(
    script: PathBuf,
    debug_workers: usize,
    xdebug: Option<ephpm_php::xdebug::XdebugIni>,
    request_timeout_secs: u64,
    stream_send_timeout: Duration,
) -> Option<Arc<DebugLane>> {
    let Some(ini) = xdebug else {
        // The production case. Only an explicitly non-default knob earns a
        // line (the no-silent-no-op rule) — at the default it is noise.
        if debug_workers != ephpm_config::WorkerConfig::default().debug_workers {
            tracing::info!(
                debug_workers,
                "[php.worker] debug_workers is set but the xdebug extension is not loaded — \
                 the worker-mode debug lane is inactive"
            );
        }
        return None;
    };
    if debug_workers == 0 {
        tracing::info!(
            "xdebug is loaded but [php.worker] debug_workers = 0 — the worker-mode debug lane is \
             disabled; triggered requests stay on the warm pool, where Xdebug cannot start a \
             per-request session"
        );
        return None;
    }
    let policy = match TriggerPolicy::from_ini(&ini) {
        Ok(policy) => policy,
        Err(reason) => {
            tracing::info!("xdebug is loaded but the worker-mode debug lane is inactive: {reason}");
            return None;
        }
    };

    tracing::warn!(
        debug_workers,
        start_with_request = ?policy.start_with_request(),
        shared_secret = policy.has_shared_secret(),
        "Xdebug step debugging is ON in worker mode: a request carrying an Xdebug trigger runs \
         on a fresh one-request worker (framework boot included) with its own debug session; \
         other requests stay on the warm pool. Xdebug is a development tool — never load it on \
         a production or internet-facing server: any client that can send a trigger can make \
         this server boot a debug worker and open a debugger connection"
    );
    if !policy.has_shared_secret() {
        tracing::warn!(
            "xdebug.trigger_value is empty — any value of XDEBUG_TRIGGER / XDEBUG_SESSION starts \
             a session; set xdebug.trigger_value to a secret to require it"
        );
    }
    if ini_bool(&ini.discover_client_host) {
        tracing::warn!(
            "xdebug.discover_client_host is ON — Xdebug connects back to whichever client sent \
             the trigger, so anyone who can reach this server can attach a debugger (and run \
             code through it). Only use it on a trusted, private network"
        );
    }
    if request_timeout_secs > 0 {
        tracing::warn!(
            request_timeout_secs,
            "[server.timeouts] request is non-zero: a debug request paused on a breakpoint longer \
             than this is answered 504 and abandoned — set it to 0 while step debugging"
        );
    }
    Some(DebugLane::new(script, debug_workers, policy, stream_send_timeout))
}

/// PHP's ini boolean parsing (`zend_ini_parse_bool`): `on`/`yes`/`true`
/// (any case) or a non-zero leading integer.
fn ini_bool(value: &str) -> bool {
    let v = value.trim();
    if ["on", "yes", "true"].iter().any(|t| v.eq_ignore_ascii_case(t)) {
        return true;
    }
    let (sign, rest) = match v.as_bytes().first() {
        Some(b'-' | b'+') => v.split_at(1),
        _ => ("", v),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    format!("{sign}{digits}").parse::<i64>().is_ok_and(|n| n != 0)
}

/// Body of one debug thread: pre-seeded TSRM init, framework boot, serve the
/// one queued request, retire. Mirrors `worker_pool::worker_main` minus the
/// supervisor bookkeeping (no readiness, no respawn — this thread is not
/// part of the serving set).
fn debug_worker_main(
    id: usize,
    seed: DebugSeed,
    rx: &async_channel::Receiver<WorkerJob>,
    script: &Path,
    stream_send_timeout: Duration,
) {
    ephpm_php::worker_bridge::set_dispatch_receiver(rx.clone());
    ephpm_php::worker_bridge::set_max_requests(1);
    ephpm_php::worker_bridge::set_stream_send_timeout(stream_send_timeout);

    let start = Instant::now();
    ephpm_php::worker_bridge::set_boot_notifier(Box::new(move || {
        let boot_secs = start.elapsed().as_secs_f64();
        tracing::info!(
            debug_worker = id,
            boot_secs,
            "debug worker booted inside the Xdebug session — serving the triggered request"
        );
    }));

    // Path only: the query string can carry the xdebug.trigger_value secret.
    tracing::info!(
        debug_worker = id,
        method = %seed.method,
        path = %seed.path(),
        post_body = seed.post_body.is_some(),
        "debug worker starting: pre-seeded request startup, then framework boot"
    );

    // TSRM register + php_request_startup with the HTTP request pre-seeded,
    // so Xdebug sees the trigger before the framework boots.
    if let Err(e) = PhpRuntime::worker_thread_init_preseeded(seed, script) {
        tracing::error!(debug_worker = id, ?e, "debug worker request startup failed");
        counter!("ephpm_xdebug_debug_requests_total", "outcome" => "startup_failed").increment(1);
        // Registered even on failure: tear the partial request down and free
        // the TSRM entry. The unpulled job then drops with the channel when
        // the lane's closure returns → oneshot sender dropped → 500.
        PhpRuntime::worker_thread_shutdown();
        return;
    }

    let outcome = PhpRuntime::run_worker(script);

    // Crash seam, same as the pool: a response never sent becomes a 500, an
    // unfinished streamed body is aborted rather than ended cleanly.
    let unsent = ephpm_php::worker_bridge::take_pending_sender();
    let had_unsent = unsent.is_some();
    if let Some(sender) = unsent {
        let _ = sender.send(WorkerResponse::internal_error());
    }
    let aborted_stream = ephpm_php::worker_bridge::clear_in_flight_streams();
    // The script returned without ever calling take_request() (an exit at
    // boot): the job is still queued and will drop with the channel (→ 500).
    let never_taken = !rx.is_empty();

    let outcome_label = match &outcome {
        Ok(ephpm_php::WorkerExit::Clean) if !had_unsent && !never_taken => "ok",
        Ok(ephpm_php::WorkerExit::Clean) => "no_response",
        Ok(ephpm_php::WorkerExit::ScriptExit) => "script_exit",
        Ok(ephpm_php::WorkerExit::ScriptFatal) => "script_fatal",
        Ok(ephpm_php::WorkerExit::Fatal) => "bailout",
        Ok(ephpm_php::WorkerExit::Contained) => "contained",
        Err(_) => "run_failed",
    };
    counter!("ephpm_xdebug_debug_requests_total", "outcome" => outcome_label).increment(1);
    if outcome_label == "ok" {
        tracing::info!(
            debug_worker = id,
            total_secs = start.elapsed().as_secs_f64(),
            "debug worker served its request — retiring (ends the Xdebug session)"
        );
    } else {
        tracing::warn!(
            debug_worker = id,
            ?outcome,
            had_unsent,
            never_taken,
            aborted_stream,
            "debug worker ended abnormally — retiring"
        );
    }

    // php_request_shutdown → Xdebug RSHUTDOWN → DBGp `stopping`; then the
    // TSRM slot is freed. Runs here, as ordinary code on this thread, so the
    // pre-seeded strings in the bridge's thread-local are still alive. After a
    // contained hard-memory-limit exit the thread is poisoned and this skips
    // the PHP teardown (the IDE then sees its connection drop, not `stopping`).
    PhpRuntime::worker_thread_shutdown();
    tracing::debug!(debug_worker = id, "debug worker retired");
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use ephpm_php::xdebug::XdebugIni;

    use super::*;

    fn policy() -> TriggerPolicy {
        TriggerPolicy::from_ini(&XdebugIni {
            mode: "debug".into(),
            start_with_request: "trigger".into(),
            ..XdebugIni::default()
        })
        .expect("debug mode arms the lane")
    }

    fn request(query: &str) -> WorkerRequestOwned {
        WorkerRequestOwned {
            method: "GET".into(),
            uri: format!("/?{query}"),
            query_string: query.into(),
            cookie_data: String::new(),
            content_type: None,
            body: WorkerBody::Buffered(Vec::new()),
            server_vars: Vec::new(),
            headers: Vec::new(),
        }
    }

    async fn free_slots(lane: &DebugLane) -> usize {
        // Give a just-released permit a moment to land.
        tokio::task::yield_now().await;
        lane.slots.available_permits()
    }

    /// A runner standing in for a debug thread parked on a breakpoint: it
    /// pulls its job and holds it until the test releases it, then answers.
    fn parked_runner(release: mpsc::Receiver<()>) -> Runner {
        let release = std::sync::Mutex::new(release);
        Arc::new(move |_id, _seed, rx| {
            let job = rx.recv_blocking().expect("job queued");
            let _ = release.lock().unwrap().recv();
            let _ = job.respond_to.send(WorkerResponse::internal_error());
        })
    }

    #[test]
    fn routes_only_what_xdebug_would_debug() {
        let lane = DebugLane::with_runner(1, policy(), Arc::new(|_, _, _| {}));
        assert!(lane.routes(&request("XDEBUG_TRIGGER=1")));
        assert!(!lane.routes(&request("page=2")));

        let mut post = request("");
        post.method = "POST".into();
        post.content_type = Some("application/x-www-form-urlencoded".into());
        post.body = WorkerBody::Buffered(b"a=1&XDEBUG_TRIGGER=1".to_vec());
        assert!(lane.routes(&post), "urlencoded POST body trigger");

        // A streaming body is never read for routing.
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        post.body = WorkerBody::Streaming { rx, declared_len: 20 };
        assert!(!lane.routes(&post));
    }

    /// The seed copies a buffered urlencoded POST body (so PHP can build
    /// `$_POST` at startup) and nothing else.
    #[test]
    fn seed_copies_only_buffered_form_bodies() {
        let mut req = request("");
        req.method = "POST".into();
        req.content_type = Some("Application/X-WWW-Form-Urlencoded; charset=UTF-8".into());
        req.body = WorkerBody::Buffered(b"XDEBUG_TRIGGER=1".to_vec());
        assert_eq!(
            DebugSeed::from_request(&req).post_body.as_deref(),
            Some(&b"XDEBUG_TRIGGER=1"[..])
        );

        req.content_type = Some("multipart/form-data; boundary=x".into());
        assert!(DebugSeed::from_request(&req).post_body.is_none());

        req.content_type = Some("application/x-www-form-urlencoded".into());
        req.method = "PUT".into();
        assert!(DebugSeed::from_request(&req).post_body.is_none(), "PHP only parses POST");

        req.method = "POST".into();
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        req.body = WorkerBody::Streaming { rx, declared_len: 16 };
        assert!(DebugSeed::from_request(&req).post_body.is_none(), "never consume a stream");
    }

    /// In stub mode the real runner's pre-seeded init is a no-op and
    /// `run_worker` refuses (PHP not linked), so the job is never pulled and
    /// the oneshot sender drops: the receiver must observe that (→ 500 at
    /// the router) rather than hang, and both the slot and the thread place
    /// must come back.
    #[tokio::test]
    async fn stub_mode_dispatch_resolves_and_releases_everything() {
        let lane = DebugLane::new(
            PathBuf::from("/nonexistent/worker.php"),
            1,
            policy(),
            Duration::from_secs(1),
        );
        let mut dispatched = lane.dispatch(request("XDEBUG_TRIGGER=1")).await.expect("spawns");
        let got = tokio::time::timeout(Duration::from_secs(10), &mut dispatched.rx)
            .await
            .expect("resolves");
        assert!(got.is_err(), "stub lane cannot produce a response; sender must drop");
        dispatched.mark_answered();
        drop(dispatched);
        assert_eq!(free_slots(&lane).await, 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while lane.live_count() != 0 {
            assert!(Instant::now() < deadline, "debug thread never retired");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The cap: with one slot taken by a request paused in the debugger, a
    /// second triggered request WAITS for it (FIFO) instead of failing.
    #[tokio::test]
    async fn a_full_lane_queues_the_next_request() {
        let (release_tx, release_rx) = mpsc::channel();
        let lane = DebugLane::with_runner(1, policy(), parked_runner(release_rx));

        let mut first = lane.dispatch(request("XDEBUG_TRIGGER=1")).await.unwrap();
        let lane2 = Arc::clone(&lane);
        let second = tokio::spawn(async move { lane2.dispatch(request("XDEBUG_TRIGGER=2")).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!second.is_finished(), "second request must wait for the slot");

        release_tx.send(()).unwrap();
        assert!((&mut first.rx).await.is_ok());
        first.mark_answered();
        drop(first);

        let mut second = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("slot handed over")
            .unwrap()
            .expect("dispatched");
        release_tx.send(()).unwrap();
        assert!((&mut second.rx).await.is_ok());
    }

    /// The timeout guarantee: abandoning a request whose thread is parked in
    /// the debugger releases its slot immediately (the next debug request
    /// runs), and the orphan cap — 2 × debug_workers live threads — turns
    /// further requests into a prompt refusal instead of unbounded threads.
    #[tokio::test]
    async fn abandoned_request_releases_its_slot_and_orphans_are_capped() {
        let (release_tx, release_rx) = mpsc::channel();
        let lane = DebugLane::with_runner(1, policy(), parked_runner(release_rx));

        // Request 1 parks, then is abandoned (what a 504 / disconnect does).
        let first = lane.dispatch(request("XDEBUG_TRIGGER=1")).await.unwrap();
        drop(first);
        assert_eq!(free_slots(&lane).await, 1, "slot released on abandonment");
        assert_eq!(lane.live_count(), 1, "the orphan thread is still parked");

        // Request 2 gets the slot at once and parks too: 2 live = the cap.
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            lane.dispatch(request("XDEBUG_TRIGGER=2")),
        )
        .await
        .expect("no waiting behind an abandoned request")
        .unwrap();
        drop(second);
        assert_eq!(lane.live_count(), 2);

        // Request 3: slot free, but the thread cap is reached and nothing
        // retires within the grace period → refused (503 at the router).
        let started = Instant::now();
        assert!(lane.dispatch(request("XDEBUG_TRIGGER=3")).await.is_err());
        assert!(started.elapsed() >= RETIRE_GRACE);
        assert_eq!(free_slots(&lane).await, 1, "a refusal does not leak the slot");

        // The IDE resumes both sessions; the orphans retire and the lane
        // is usable again.
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while lane.live_count() != 0 {
            assert!(Instant::now() < deadline, "orphans never retired");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let fourth = lane.dispatch(request("XDEBUG_TRIGGER=4")).await.unwrap();
        release_tx.send(()).unwrap();
        let mut fourth = fourth;
        assert!((&mut fourth.rx).await.is_ok());
    }

    /// The waiting room is bounded even with no request timeout: once
    /// `MAX_WAITERS_PER_SLOT × debug_workers` requests wait behind a paused
    /// session, the next one is refused at once instead of queueing.
    #[tokio::test]
    async fn the_waiting_room_is_bounded() {
        let (release_tx, release_rx) = mpsc::channel();
        let lane = DebugLane::with_runner(1, policy(), parked_runner(release_rx));
        let first = lane.dispatch(request("XDEBUG_TRIGGER=1")).await.unwrap();

        let mut waiters = Vec::new();
        for i in 0..MAX_WAITERS_PER_SLOT {
            let lane = Arc::clone(&lane);
            waiters.push(tokio::spawn(async move {
                lane.dispatch(request(&format!("XDEBUG_TRIGGER={i}"))).await.is_ok()
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while lane.waiting.load(Ordering::Acquire) < MAX_WAITERS_PER_SLOT {
            assert!(Instant::now() < deadline, "waiters never queued");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let started = Instant::now();
        assert!(lane.dispatch(request("XDEBUG_TRIGGER=over")).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(1), "refused at once, not queued");

        // Cancelled waiters give their place back.
        for w in &waiters {
            w.abort();
        }
        for w in waiters {
            let _ = w.await;
        }
        assert_eq!(lane.waiting.load(Ordering::Acquire), 0);
        release_tx.send(()).unwrap();
        drop(first);
    }

    /// Drain closes the lane: queued and new triggered requests are refused.
    #[tokio::test]
    async fn drain_refuses_waiting_and_new_requests() {
        let (release_tx, release_rx) = mpsc::channel();
        let lane = DebugLane::with_runner(1, policy(), parked_runner(release_rx));
        let first = lane.dispatch(request("XDEBUG_TRIGGER=1")).await.unwrap();
        let lane2 = Arc::clone(&lane);
        let waiting =
            tokio::spawn(async move { lane2.dispatch(request("XDEBUG_TRIGGER=2")).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        lane.drain();
        assert!(waiting.await.unwrap().is_err(), "a queued request is refused on drain");
        assert!(lane.dispatch(request("XDEBUG_TRIGGER=3")).await.is_err());

        release_tx.send(()).unwrap();
        drop(first);
    }
}

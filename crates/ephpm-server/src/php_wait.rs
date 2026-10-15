//! Waiting on a PHP thread's reply: the per-request deadline, hung-thread
//! detection, and the accounting that lets a replaced thread's pool shrink
//! back once the "hung" thread comes back.
//!
//! # One deadline, not two timers
//!
//! `[server.timeouts] request` used to be enforced twice with the same
//! duration: once around the whole request in [`Router::handle`] and once
//! around the PHP reply wait (fpm pool / worker pool). The inner timer started
//! later (after routing, middleware and dispatch), so the outer always expired
//! first and dropped the inner future — the inner `504` arm, which is the only
//! place a wedged PHP thread is noted as hung and replaced, never ran. Hung
//! threads were never replaced, and `ephpm_http_timeouts_total{stage="worker"}`
//! / `{stage="fpm_pool"}` and `*_recycles_total{reason="hung"}` never moved.
//!
//! Now [`Router::handle`] computes one absolute deadline per request and
//! publishes it through the [`REQUEST_DEADLINE`] task-local. The PHP reply
//! wait uses that same instant, and the outer guard fires [`OUTER_GRACE`]
//! later — so a request blocked on PHP is always answered by the PHP-aware
//! inner arm, and the outer arm is left for everything else (routing,
//! middleware, a starved dispatch queue).
//!
//! # Started vs. queued
//!
//! A request can reach its deadline while its job still sits in the dispatch
//! queue: every thread is busy, nothing is wrong with any of them. Replacing a
//! thread then would be wrong — it would grow the pool under plain overload.
//! The job therefore carries a [`StartFlag`] that the pool thread sets when it
//! begins the request; only a timed-out request whose job had *started* marks
//! a thread hung.
//!
//! # Replace, then shrink back
//!
//! A hung thread cannot be killed (its Zend context would be corrupted), so
//! the pool spawns a replacement and abandons it. Most "hung" threads are only
//! slow and do come back — and without accounting each one would leave the
//! pool permanently one thread larger, a full ZTS context per timeout. The
//! [`PendingReply`] keeps the receiver of a hung request alive and reports the
//! thread's return through [`HungTracker::recovered`]; the pool then lets one
//! surplus thread retire at its next idle point.
//!
//! [`Router::handle`]: crate::router::Router::handle

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;

/// How much later than the request deadline the outer, whole-request timeout
/// fires. Gives the PHP reply wait — which uses the deadline itself — a
/// deterministic head start, so a request blocked on PHP is always answered
/// (and its thread judged) by the PHP-aware path. Also bounds how late a
/// request blocked somewhere else (middleware, a starved dispatch queue) gets
/// its `504`.
pub(crate) const OUTER_GRACE: Duration = Duration::from_millis(50);

tokio::task_local! {
    /// The absolute deadline of the HTTP request being served on this task,
    /// set by `Router::handle`. `None` when `[server.timeouts] request = 0`.
    pub(crate) static REQUEST_DEADLINE: Option<Instant>;
}

/// The deadline the current request's PHP reply wait must honour.
///
/// Inside `Router::handle` this is the request's own deadline. Outside it (a
/// code path that dispatches PHP without going through `handle`) it falls back
/// to `now + timeout`, the old per-wait behaviour. `None` = no deadline
/// (`[server.timeouts] request = 0`).
pub(crate) fn current_deadline(timeout: Duration) -> Option<Instant> {
    REQUEST_DEADLINE
        .try_with(|deadline| *deadline)
        .unwrap_or_else(|_| (!timeout.is_zero()).then(|| Instant::now() + timeout))
}

/// Set by the pool thread when it begins running a job. Distinguishes a
/// request that was *running* at its deadline (the thread may be hung) from
/// one still queued behind busy threads (it is not).
pub(crate) type StartFlag = Arc<AtomicBool>;

/// Wrap an fpm task so it raises `started` the moment a pool thread runs it.
pub(crate) fn track_start(
    task: crate::fpm_pool::FpmTask,
    started: &StartFlag,
) -> crate::fpm_pool::FpmTask {
    let started = Arc::clone(started);
    Box::new(move || {
        started.store(true, Ordering::Release);
        task()
    })
}

/// A pool whose threads can be marked hung and later reported recovered.
pub(crate) trait HungTracker: Send + Sync + 'static {
    /// A thread failed to answer its started request by the deadline: count
    /// it, abandon it and spawn a replacement.
    fn hung(pool: &Arc<Self>);
    /// A thread previously passed to [`HungTracker::hung`] answered (or
    /// dropped) its request at last, so the pool may shed the surplus thread.
    fn recovered(pool: &Arc<Self>);
}

impl HungTracker for crate::fpm_pool::FpmPool {
    fn hung(pool: &Arc<Self>) {
        pool.note_hung();
    }
    fn recovered(pool: &Arc<Self>) {
        pool.note_recovered();
    }
}

impl HungTracker for crate::worker_pool::WorkerPool {
    fn hung(pool: &Arc<Self>) {
        pool.note_hung();
    }
    fn recovered(pool: &Arc<Self>) {
        pool.note_recovered();
    }
}

/// What [`PendingReply::wait`] resolved to.
pub(crate) enum ReplyOutcome<T> {
    /// The thread answered (`Ok`), or dropped its sender without answering
    /// (`Err` — a bailout / panic the pool already turned into a 500 path).
    Reply(Result<T, oneshot::error::RecvError>),
    /// The deadline passed first. `hung` is true when the job had started, in
    /// which case its thread has been marked hung and replaced.
    TimedOut {
        /// Whether a thread was marked hung for this timeout.
        hung: bool,
    },
}

/// The wait for one dispatched PHP job's reply, bounded by the request
/// deadline. Owns the receiver so that a timeout — or the request future being
/// dropped mid-wait (client disconnect) — still accounts for the thread.
pub(crate) struct PendingReply<T: Send + 'static, P: HungTracker> {
    /// `Some` until [`PendingReply::wait`] resolves; a `Some` seen in `Drop`
    /// means the wait was cancelled.
    rx: Option<oneshot::Receiver<T>>,
    started: StartFlag,
    deadline: Option<Instant>,
    pool: Arc<P>,
}

impl<T: Send + 'static, P: HungTracker> PendingReply<T, P> {
    /// Wrap a dispatched job's receiver.
    pub(crate) fn new(
        rx: oneshot::Receiver<T>,
        started: StartFlag,
        deadline: Option<Instant>,
        pool: Arc<P>,
    ) -> Self {
        Self { rx: Some(rx), started, deadline, pool }
    }

    /// Wait for the reply until the deadline.
    pub(crate) async fn wait(mut self) -> ReplyOutcome<T> {
        let Some(rx) = self.rx.as_mut() else {
            // Unreachable: `rx` is only taken below, after which `self` is
            // consumed. Report it as a timeout rather than panic.
            return ReplyOutcome::TimedOut { hung: false };
        };
        let waited = match self.deadline {
            None => Ok(rx.await),
            Some(deadline) => tokio::time::timeout_at(deadline, rx).await,
        };
        match waited {
            Ok(reply) => {
                self.rx = None;
                ReplyOutcome::Reply(reply)
            }
            Err(_elapsed) => {
                let rx = self.rx.take();
                let hung = self.started.load(Ordering::Acquire);
                if hung {
                    P::hung(&self.pool);
                    if let Some(rx) = rx {
                        spawn_recovery_watch(rx, Arc::clone(&self.pool));
                    }
                }
                // Not started: the job is still queued behind busy threads.
                // Nothing is hung; dropping the receiver leaves the job to run
                // later with nobody waiting, exactly as before.
                ReplyOutcome::TimedOut { hung }
            }
        }
    }
}

impl<T: Send + 'static, P: HungTracker> Drop for PendingReply<T, P> {
    /// The request future was dropped mid-wait (client disconnect, or the
    /// outer guard). Nobody gets a 504, but the thread may still be wedged:
    /// keep watching it until the request deadline and account for it then.
    fn drop(&mut self) {
        let Some(rx) = self.rx.take() else { return };
        // No deadline (`request = 0`): there is no point at which a thread
        // counts as hung.
        let Some(deadline) = self.deadline else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let started = Arc::clone(&self.started);
        let pool = Arc::clone(&self.pool);
        handle.spawn(async move {
            let mut rx = rx;
            if tokio::time::timeout_at(deadline, &mut rx).await.is_ok() {
                return;
            }
            if !started.load(Ordering::Acquire) {
                return;
            }
            P::hung(&pool);
            let _ = rx.await;
            P::recovered(&pool);
        });
    }
}

/// Decrement a pool's hung-thread count, never below zero.
pub(crate) fn release_hung(hung: &AtomicUsize) {
    let mut current = hung.load(Ordering::Acquire);
    while current > 0 {
        match hung.compare_exchange_weak(current, current - 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

/// Claim one surplus retirement for a pool with `target` threads.
///
/// `live` counts every thread, `hung` the ones marked hung that have not come
/// back (live but not serving), `retiring` the ones that already claimed. A
/// claim succeeds — and bumps `retiring` — only while serving threads still
/// exceed `target`, so concurrent claimers cannot overshoot: each surplus slot
/// is claimed once. The claimer decrements `retiring` after it has left
/// `live`.
pub(crate) fn claim_surplus(
    live: &AtomicUsize,
    hung: &AtomicUsize,
    retiring: &AtomicUsize,
    target: usize,
) -> bool {
    let mut claimed = retiring.load(Ordering::Acquire);
    loop {
        let serving = live
            .load(Ordering::Acquire)
            .saturating_sub(hung.load(Ordering::Acquire))
            .saturating_sub(claimed);
        if serving <= target {
            return false;
        }
        match retiring.compare_exchange_weak(
            claimed,
            claimed + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(current) => claimed = current,
        }
    }
}

/// Hold a hung job's receiver until its thread finally answers (or drops the
/// sender), then report the thread recovered so the pool can shed the
/// replacement's surplus. A thread that never returns keeps this task — one
/// parked future — alive for as long as the thread itself is lost.
fn spawn_recovery_watch<T: Send + 'static, P: HungTracker>(rx: oneshot::Receiver<T>, pool: Arc<P>) {
    tokio::spawn(async move {
        let _ = rx.await;
        P::recovered(&pool);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Surplus is measured against serving threads: a hung thread's
    /// replacement is not surplus, and each surplus slot is claimed once.
    #[test]
    fn claim_surplus_discounts_hung_threads_and_claims_each_slot_once() {
        let (live, hung, retiring) =
            (AtomicUsize::new(3), AtomicUsize::new(1), AtomicUsize::new(0));
        // Target 2: one thread stuck, its replacement live — nothing to shed.
        assert!(!claim_surplus(&live, &hung, &retiring, 2));
        // The stuck thread comes back: exactly one claim succeeds.
        hung.store(0, Ordering::SeqCst);
        assert!(claim_surplus(&live, &hung, &retiring, 2));
        assert!(!claim_surplus(&live, &hung, &retiring, 2), "the slot is already claimed");
        assert_eq!(retiring.load(Ordering::SeqCst), 1);
        // The claimer leaves: back at target, nothing more to claim.
        live.fetch_sub(1, Ordering::SeqCst);
        retiring.fetch_sub(1, Ordering::SeqCst);
        assert!(!claim_surplus(&live, &hung, &retiring, 2));
    }

    /// Records `hung` / `recovered` calls.
    #[derive(Default)]
    struct Counts {
        hung: AtomicUsize,
        recovered: AtomicUsize,
    }

    impl HungTracker for Counts {
        fn hung(pool: &Arc<Self>) {
            pool.hung.fetch_add(1, Ordering::SeqCst);
        }
        fn recovered(pool: &Arc<Self>) {
            pool.recovered.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn deadline_in(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    /// Let spawned watcher tasks run (paused clock: sleeping auto-advances).
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_before_the_deadline_marks_nothing() {
        let counts = Arc::new(Counts::default());
        let (tx, rx) = oneshot::channel::<u32>();
        let started = StartFlag::default();
        started.store(true, Ordering::SeqCst);
        tx.send(7).unwrap();
        let outcome = PendingReply::new(rx, started, Some(deadline_in(1000)), Arc::clone(&counts))
            .wait()
            .await;
        assert!(matches!(outcome, ReplyOutcome::Reply(Ok(7))));
        settle().await;
        assert_eq!(counts.hung.load(Ordering::SeqCst), 0);
        assert_eq!(counts.recovered.load(Ordering::SeqCst), 0);
    }

    /// A started job that misses the deadline marks its thread hung, and its
    /// eventual answer reports the thread recovered.
    #[tokio::test(start_paused = true)]
    async fn a_started_job_past_the_deadline_is_hung_then_recovers() {
        let counts = Arc::new(Counts::default());
        let (tx, rx) = oneshot::channel::<u32>();
        let started = StartFlag::default();
        started.store(true, Ordering::SeqCst);
        let outcome = PendingReply::new(rx, started, Some(deadline_in(100)), Arc::clone(&counts))
            .wait()
            .await;
        assert!(matches!(outcome, ReplyOutcome::TimedOut { hung: true }));
        assert_eq!(counts.hung.load(Ordering::SeqCst), 1);
        assert_eq!(counts.recovered.load(Ordering::SeqCst), 0, "the thread has not come back");

        // The slow thread finally answers: nobody wants the reply any more,
        // but the pool must learn the thread is serving again.
        tx.send(1).unwrap();
        settle().await;
        assert_eq!(counts.recovered.load(Ordering::SeqCst), 1);
    }

    /// A job still queued at the deadline is overload, not a hung thread.
    #[tokio::test(start_paused = true)]
    async fn a_queued_job_past_the_deadline_is_not_hung() {
        let counts = Arc::new(Counts::default());
        let (_tx, rx) = oneshot::channel::<u32>();
        let outcome = PendingReply::new(
            rx,
            StartFlag::default(),
            Some(deadline_in(100)),
            Arc::clone(&counts),
        )
        .wait()
        .await;
        assert!(matches!(outcome, ReplyOutcome::TimedOut { hung: false }));
        settle().await;
        assert_eq!(counts.hung.load(Ordering::SeqCst), 0);
    }

    /// Cancelling the wait (client disconnect) before the deadline still
    /// catches a thread that then misses it.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wait_still_detects_a_hung_thread() {
        let counts = Arc::new(Counts::default());
        let (tx, rx) = oneshot::channel::<u32>();
        let started = StartFlag::default();
        started.store(true, Ordering::SeqCst);
        let wait =
            PendingReply::new(rx, started, Some(deadline_in(100)), Arc::clone(&counts)).wait();
        // Poll briefly, then drop the future mid-wait.
        assert!(tokio::time::timeout(Duration::from_millis(10), wait).await.is_err());
        assert_eq!(counts.hung.load(Ordering::SeqCst), 0, "not hung before the deadline");

        tokio::time::sleep(Duration::from_millis(200)).await;
        settle().await;
        assert_eq!(counts.hung.load(Ordering::SeqCst), 1, "hung once the deadline passed");
        drop(tx);
        settle().await;
        assert_eq!(counts.recovered.load(Ordering::SeqCst), 1);
    }

    /// A cancelled wait whose thread answers before the deadline is not hung.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wait_answered_in_time_is_not_hung() {
        let counts = Arc::new(Counts::default());
        let (tx, rx) = oneshot::channel::<u32>();
        let started = StartFlag::default();
        started.store(true, Ordering::SeqCst);
        let wait =
            PendingReply::new(rx, started, Some(deadline_in(100)), Arc::clone(&counts)).wait();
        assert!(tokio::time::timeout(Duration::from_millis(10), wait).await.is_err());
        tx.send(3).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        settle().await;
        assert_eq!(counts.hung.load(Ordering::SeqCst), 0);
        assert_eq!(counts.recovered.load(Ordering::SeqCst), 0);
    }

    /// The PHP wait's deadline is the request's own deadline when one is in
    /// scope, and `now + timeout` otherwise.
    #[tokio::test(start_paused = true)]
    async fn current_deadline_prefers_the_request_scope() {
        let request_deadline = Instant::now() + Duration::from_secs(5);
        let inside = REQUEST_DEADLINE
            .scope(Some(request_deadline), async { current_deadline(Duration::from_secs(30)) })
            .await;
        assert_eq!(inside, Some(request_deadline));

        let disabled =
            REQUEST_DEADLINE.scope(None, async { current_deadline(Duration::from_secs(30)) }).await;
        assert_eq!(disabled, None, "request = 0 in scope means no deadline");

        let outside = current_deadline(Duration::from_secs(30));
        assert_eq!(outside, Some(Instant::now() + Duration::from_secs(30)));
        assert_eq!(current_deadline(Duration::ZERO), None);
    }
}

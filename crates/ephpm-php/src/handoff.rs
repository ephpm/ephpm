//! Per-worker job handoff for ePHPm's dedicated PHP thread pools.
//!
//! Both pools (`worker_pool` for `[php] mode = "worker"`, `fpm_pool` for
//! per-request mode) hand one job at a time to a fixed set of OS threads. They
//! used to share one bounded MPMC channel (`async_channel`): every idle thread
//! parked on the channel's waiter list, and every enqueue and every park went
//! through that list's lock. Profiling a saturated 20-worker pool put ~22% of
//! all PHP-thread context switches on that one contended lock.
//!
//! [`Dispatcher`] replaces it with a direct handoff:
//!
//! - Each pool thread owns a single-entry mailbox (a [`Worker`] handle). While
//!   the thread waits for work, its mailbox sits on the dispatcher's **idle
//!   stack**.
//! - [`Dispatcher::try_push`] pops an idle thread and drops the job straight into
//!   its mailbox. Only when **no** thread is idle does the job go to the shared
//!   **overflow queue** (FIFO, bounded by the pool's `queue_depth`).
//! - A thread that finishes a job takes the oldest overflow job before it ever
//!   goes idle.
//!
//! ## Invariants
//!
//! The idle stack and the overflow queue live under one mutex and are only
//! changed together, which gives the property the pools rely on:
//!
//! > **A thread is idle only while the overflow queue is empty.**
//!
//! A thread re-checks the queue under the lock before it registers as idle, and
//! a dispatcher pushes to the queue only after it found no idle thread under
//! the same lock. Consequences:
//!
//! - **No head-of-line blocking.** A job is only ever handed to a thread that is
//!   idle *right now*; nothing is queued behind a busy thread. When every thread
//!   is busy, jobs wait in one shared FIFO and the next thread to finish takes
//!   the oldest — a slow request never holds up a fast one that another thread
//!   could serve.
//! - **Arrival order.** Overflow is FIFO, and a newly arriving job cannot
//!   overtake a queued one (if anything is queued, nothing is idle).
//! - **Bounded memory.** The overflow queue never grows past `capacity`
//!   ([`TryPushError::Full`]); mailboxes hold at most one job each, and only for
//!   the instant between handoff and pickup.
//!
//! The lock is held for a push or a pop only — the wake-up (`unpark`) of a
//! parked thread happens after it is released. The idle stack is LIFO on
//! purpose: the most recently idled thread is usually still in its short
//! spin-poll (see [`Worker::recv`]) and picks the job up without a futex wake,
//! while threads at the bottom of the stack stay parked.
//!
//! ## Spinning before parking
//!
//! How long an idle thread polls its own mailbox before it parks is a
//! [`SpinPolicy`]: a budget, how many of the polls are pure `spin_loop` hints
//! before they become `sched_yield`s, and whether the budget adapts per thread.
//! A thread that is polling marks its mailbox `SPINNING`, and a delivery into
//! a mailbox that is not `PARKED` costs no syscall at all — the poller sees the
//! job on its next read. Once any thread has spun, a dispatcher prefers a
//! `SPINNING` mailbox near the top of the idle stack over a parked one above
//! it, which matters once adaptive budgets let threads park at different times.
//!
//! With an **adaptive** policy, each delivery is time-stamped by the
//! dispatcher and the thread compares that with when it went idle. A gap the
//! full budget would have covered restores the full budget; a longer one
//! halves the thread's current budget, down to zero. A thread that keeps
//! waiting longer than the budget stops spinning, so a server at low request
//! rates burns no CPU on polls that never pay off, and the first short gap
//! turns spinning back on. The gap is measured from the dispatcher's side, so
//! the scheduler latency of waking a parked thread does not count against
//! spinning.
//!
//! ## Retirement
//!
//! A thread is on the idle stack only inside [`Worker::recv`], so a thread that
//! retires (recycle, crash, hang replacement) between requests holds no job.
//! As a defence in depth, dropping a [`Worker`] removes it from the idle stack
//! and re-routes any job still sitting in its mailbox to another idle thread or
//! the front of the overflow queue — a job is never lost with its thread.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// Why [`Dispatcher::try_push`] did not accept a job. The job is handed back.
#[derive(Debug)]
pub enum TryPushError<T> {
    /// No thread is idle and the overflow queue is at capacity.
    Full(T),
    /// [`Dispatcher::close`] was called.
    Closed(T),
}

impl<T> TryPushError<T> {
    /// Recover the job that was not accepted.
    #[must_use]
    pub fn into_inner(self) -> T {
        match self {
            Self::Full(job) | Self::Closed(job) => job,
        }
    }

    /// `true` for [`TryPushError::Closed`].
    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }
}

/// [`Worker::recv`] found the dispatcher closed and the overflow queue empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvClosed;

/// Pure `spin_loop` polls before [`Worker::recv`] switches to `sched_yield`
/// polls, unless a [`SpinPolicy`] says otherwise.
pub const DEFAULT_PURE_SPINS: u32 = 32;

/// An adaptive budget that halves below this collapses to zero (park at once).
const ADAPTIVE_FLOOR: Duration = Duration::from_micros(2);

/// How far down the idle stack a dispatcher looks for a spinning mailbox.
const STEER_SCAN: usize = 8;

/// How an idle pool thread waits for its next job (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpinPolicy {
    /// Longest time to poll the mailbox before parking. `Duration::ZERO`
    /// parks at once. With `adaptive`, the ceiling of each thread's budget.
    pub budget: Duration,
    /// Polls that are pure `spin_loop` hints before each further poll yields
    /// the CPU (`sched_yield`). `u32::MAX` never yields; `0` always yields.
    pub pure_spins: u32,
    /// Adapt each thread's budget to the idle gaps it actually sees.
    pub adaptive: bool,
}

impl SpinPolicy {
    /// Park as soon as no job is waiting.
    pub const PARK: Self = Self::fixed(Duration::ZERO);

    /// Poll for up to `budget` ([`DEFAULT_PURE_SPINS`] pure spins, then
    /// yields), never adapting.
    #[must_use]
    pub const fn fixed(budget: Duration) -> Self {
        Self { budget, pure_spins: DEFAULT_PURE_SPINS, adaptive: false }
    }

    /// The worker-mode pool's policy: an adaptive budget capped at
    /// `default_budget` unless overridden by `EPHPM_DISPATCH_SPIN_US`,
    /// `EPHPM_DISPATCH_SPIN_PURE` and `EPHPM_DISPATCH_SPIN_ADAPTIVE`. These
    /// are experiment knobs, not configuration: read once at startup, and an
    /// unparseable value is warned about and ignored.
    ///
    /// Adaptive is the default because it matched a fixed budget at
    /// saturation while spending 18-19% less CPU per request at 1k-5k req/s.
    #[must_use]
    pub fn worker_from_env(default_budget: Duration) -> Self {
        Self::from_vars(
            "worker",
            Self { adaptive: true, ..Self::fixed(default_budget) },
            std::env::var("EPHPM_DISPATCH_SPIN_US").ok().as_deref(),
            std::env::var("EPHPM_DISPATCH_SPIN_PURE").ok().as_deref(),
            std::env::var("EPHPM_DISPATCH_SPIN_ADAPTIVE").ok().as_deref(),
        )
    }

    /// The per-request pool's policy: `default_budget` unless overridden by
    /// `EPHPM_FPM_DISPATCH_SPIN_US`. The pure-spin and adaptive variables are
    /// shared with [`SpinPolicy::worker_from_env`].
    #[must_use]
    pub fn fpm_from_env(default_budget: Duration) -> Self {
        Self::from_vars(
            "per_request",
            Self::fixed(default_budget),
            std::env::var("EPHPM_FPM_DISPATCH_SPIN_US").ok().as_deref(),
            std::env::var("EPHPM_DISPATCH_SPIN_PURE").ok().as_deref(),
            std::env::var("EPHPM_DISPATCH_SPIN_ADAPTIVE").ok().as_deref(),
        )
    }

    /// Override `default` with the raw variable values (`None` = unset).
    fn from_vars(
        pool: &str,
        default: Self,
        budget_us: Option<&str>,
        pure_spins: Option<&str>,
        adaptive: Option<&str>,
    ) -> Self {
        let mut policy = default;
        let overridden = budget_us.is_some() || pure_spins.is_some() || adaptive.is_some();
        if let Some(raw) = budget_us {
            match raw.trim().parse::<u64>() {
                Ok(us) => policy.budget = Duration::from_micros(us),
                Err(_) => tracing::warn!(
                    pool,
                    value = raw,
                    "ignoring unparseable dispatch spin budget (want microseconds)"
                ),
            }
        }
        if let Some(raw) = pure_spins {
            match raw.trim() {
                "max" => policy.pure_spins = u32::MAX,
                v => match v.parse::<u32>() {
                    Ok(n) => policy.pure_spins = n,
                    Err(_) => tracing::warn!(
                        pool,
                        value = raw,
                        "ignoring unparseable dispatch pure-spin count (want a number or \"max\")"
                    ),
                },
            }
        }
        if let Some(raw) = adaptive {
            match raw.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => policy.adaptive = true,
                "0" | "false" | "no" | "off" | "" => policy.adaptive = false,
                _ => tracing::warn!(
                    pool,
                    value = raw,
                    "ignoring unparseable dispatch spin adaptive flag (want 0 or 1)"
                ),
            }
        }
        if overridden {
            tracing::info!(
                pool,
                budget_us = policy.budget.as_micros(),
                pure_spins = policy.pure_spins,
                adaptive = policy.adaptive,
                "dispatch spin policy overridden from the environment"
            );
        }
        policy
    }
}

/// Mailbox states. The worker moves `WAITING` or `SPINNING` to `PARKED` before
/// it sleeps; a dispatcher (or [`Dispatcher::close`]) moves any of them to
/// `READY` and unparks only when it saw `PARKED`. `SPINNING` is `WAITING` plus
/// a hint to dispatchers that this thread is polling and needs no wake-up.
const WAITING: u8 = 0;
const PARKED: u8 = 1;
const READY: u8 = 2;
const SPINNING: u8 = 3;

/// A mailbox's content: the job, and when it was delivered (only when the
/// owner asked for a time stamp — see [`Slot::stamp`]).
struct Delivery<T> {
    job: Option<T>,
    at: Option<Instant>,
}

/// One pool thread's single-entry mailbox.
struct Slot<T> {
    job: Mutex<Delivery<T>>,
    state: AtomicU8,
    /// The owner wants deliveries time-stamped (adaptive spinning). Written by
    /// the owner before it pushes the slot onto the idle stack and read by the
    /// dispatcher that pops it, both under the dispatcher lock.
    stamp: AtomicBool,
    thread: Thread,
}

impl<T> Slot<T> {
    /// Put `job` (or, with `None`, a bare wake-up) in the mailbox and wake the
    /// owner if it is parked. Called by exactly one party per idle period: the
    /// one that popped this slot off the idle stack.
    fn deliver(&self, job: Option<T>) {
        let at = self.stamp.load(Ordering::Relaxed).then(Instant::now);
        *lock(&self.job) = Delivery { job, at };
        if self.state.swap(READY, Ordering::AcqRel) == PARKED {
            self.thread.unpark();
        }
    }

    fn take(&self) -> (Option<T>, Option<Instant>) {
        let mut d = lock(&self.job);
        (d.job.take(), d.at.take())
    }
}

/// Take a thread off the idle stack for a delivery: the top one, unless
/// `steer` is set and the top one is not spinning — then the nearest spinning
/// one among the top [`STEER_SCAN`], so the job needs no wake-up. The state
/// read is only a hint: a thread that parks right after it is still woken by
/// [`Slot::deliver`].
fn take_idle<T>(idle: &mut Vec<Arc<Slot<T>>>, steer: bool) -> Option<Arc<Slot<T>>> {
    if steer {
        let n = idle.len();
        for i in (n.saturating_sub(STEER_SCAN)..n).rev() {
            if idle[i].state.load(Ordering::Relaxed) == SPINNING {
                return Some(idle.remove(i));
            }
        }
    }
    idle.pop()
}

struct State<T> {
    /// Threads waiting for work, most recently idled last (LIFO).
    idle: Vec<Arc<Slot<T>>>,
    /// Jobs waiting for a thread, oldest first. Non-empty only while `idle`
    /// is empty.
    queue: VecDeque<T>,
    closed: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    /// Bound on `queue.len()`.
    capacity: usize,
    /// Mirror of `queue.len()` for lock-free reads (gauges, tests).
    queued: AtomicUsize,
    /// Mirror of `state.closed` for lock-free reads.
    closed: AtomicBool,
    /// Signalled when a pop frees overflow space while a [`Dispatcher::push`]
    /// caller is waiting for it.
    space: tokio::sync::Notify,
    /// [`Dispatcher::push`] callers currently waiting for space. Lets the hot
    /// pop path skip the `Notify` entirely in the common case (FIFO admission
    /// never waits here).
    push_waiters: AtomicUsize,
    /// Set (once, never cleared) the first time a thread spins. Until then a
    /// delivery takes the top of the idle stack without looking further, so a
    /// pool that never spins hands out jobs exactly as it did before spinning
    /// existed.
    steer: AtomicBool,
}

/// Decrements [`Shared::push_waiters`] when a `push` ends, however it ends
/// (including cancellation).
struct WaiterGuard<'a>(&'a AtomicUsize);

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn lock<V>(m: &Mutex<V>) -> MutexGuard<'_, V> {
    // Nothing panics while holding these locks (plain container ops), but a
    // poisoned lock must never wedge the pool either.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The producer side, shared by every request handler. Cheap to clone.
pub struct Dispatcher<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Dispatcher<T> {
    fn clone(&self) -> Self {
        Self { shared: Arc::clone(&self.shared) }
    }
}

impl<T> std::fmt::Debug for Dispatcher<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("capacity", &self.shared.capacity)
            .field("queued", &self.queued())
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl<T> Dispatcher<T> {
    /// A dispatcher whose overflow queue holds at most `capacity` jobs (at
    /// least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    idle: Vec::new(),
                    queue: VecDeque::with_capacity(capacity),
                    closed: false,
                }),
                capacity,
                queued: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                space: tokio::sync::Notify::new(),
                push_waiters: AtomicUsize::new(0),
                steer: AtomicBool::new(false),
            }),
        }
    }

    /// Register the **calling thread** as a pool thread. The returned handle
    /// receives jobs through [`Worker::recv`] and must stay on this thread.
    #[must_use]
    pub fn register_worker(&self) -> Worker<T> {
        Worker {
            shared: Arc::clone(&self.shared),
            slot: Arc::new(Slot {
                job: Mutex::new(Delivery { job: None, at: None }),
                state: AtomicU8::new(READY),
                stamp: AtomicBool::new(false),
                thread: std::thread::current(),
            }),
            budget: Cell::new(Duration::MAX),
            _not_send: std::marker::PhantomData,
        }
    }

    /// Hand `job` to an idle thread, or queue it if every thread is busy.
    /// Never blocks.
    ///
    /// # Errors
    ///
    /// [`TryPushError::Full`] when no thread is idle and the overflow queue is
    /// at capacity; [`TryPushError::Closed`] after [`Dispatcher::close`].
    pub fn try_push(&self, job: T) -> Result<(), TryPushError<T>> {
        self.try_push_inner(job).map(|_space_left| ())
    }

    /// [`Dispatcher::try_push`], also reporting whether room for another job
    /// (an idle thread or a free queue slot) is left afterwards.
    fn try_push_inner(&self, job: T) -> Result<bool, TryPushError<T>> {
        let mut st = lock(&self.shared.state);
        if st.closed {
            return Err(TryPushError::Closed(job));
        }
        let steer = self.shared.steer.load(Ordering::Relaxed);
        if let Some(slot) = take_idle(&mut st.idle, steer) {
            // Popping the slot under the lock makes this caller its only
            // deliverer; the delivery itself (and any unpark) runs unlocked.
            let space_left = !st.idle.is_empty() || st.queue.len() < self.shared.capacity;
            drop(st);
            slot.deliver(Some(job));
            return Ok(space_left);
        }
        if st.queue.len() >= self.shared.capacity {
            return Err(TryPushError::Full(job));
        }
        st.queue.push_back(job);
        self.shared.queued.store(st.queue.len(), Ordering::Relaxed);
        Ok(st.queue.len() < self.shared.capacity)
    }

    /// Like [`Dispatcher::try_push`], but wait for space while the overflow
    /// queue is full.
    ///
    /// Admission here is a race, not a queue: a fresh caller tries first, and
    /// a woken waiter that loses the race goes back to waiting. That is the
    /// `[php] admission = "barge"` discipline; the default FIFO discipline
    /// orders callers with a semaphore *before* `try_push` instead.
    /// Cancelling the future drops the job unqueued.
    ///
    /// # Errors
    ///
    /// Gives the job back if the dispatcher is (or becomes) closed.
    pub async fn push(&self, mut job: T) -> Result<(), T> {
        // Fast path: nothing to wait for.
        job = match self.try_push(job) {
            Ok(()) => return Ok(()),
            Err(TryPushError::Closed(j)) => return Err(j),
            Err(TryPushError::Full(j)) => j,
        };
        // Counted before the re-check below takes the lock, so any pop that
        // the re-check misses sees the count and notifies.
        self.shared.push_waiters.fetch_add(1, Ordering::AcqRel);
        let _guard = WaiterGuard(&self.shared.push_waiters);
        loop {
            let notified = self.shared.space.notified();
            tokio::pin!(notified);
            // Register interest before re-checking, so a pop that frees space
            // between the check and the await cannot be missed.
            notified.as_mut().enable();
            match self.try_push_inner(job) {
                Ok(space_left) => {
                    // A wake-up is one unit of room, but room can also open
                    // without a pop (a thread going idle) or be more than
                    // this waiter used (its job went to an idle thread,
                    // leaving the freed queue slot). Pass the baton on so no
                    // other waiter sleeps while there is room.
                    if space_left && self.shared.push_waiters.load(Ordering::Acquire) > 1 {
                        self.shared.space.notify_one();
                    }
                    return Ok(());
                }
                Err(TryPushError::Closed(j)) => return Err(j),
                Err(TryPushError::Full(j)) => job = j,
            }
            notified.await;
        }
    }

    /// Stop accepting jobs and wake every idle thread. Jobs already queued are
    /// still handed out; once the queue is empty, [`Worker::recv`] returns
    /// [`RecvClosed`]. Idempotent.
    pub fn close(&self) {
        let idle = {
            let mut st = lock(&self.shared.state);
            st.closed = true;
            self.shared.closed.store(true, Ordering::Release);
            std::mem::take(&mut st.idle)
        };
        for slot in idle {
            slot.deliver(None);
        }
        // Wake every `push` waiter so it observes the close.
        self.shared.space.notify_waiters();
    }

    /// Whether [`Dispatcher::close`] has been called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Jobs waiting in the overflow queue (handed-off jobs are not counted).
    #[must_use]
    pub fn queued(&self) -> usize {
        self.shared.queued.load(Ordering::Relaxed)
    }

    /// Threads currently waiting for work.
    #[must_use]
    pub fn idle_count(&self) -> usize {
        lock(&self.shared.state).idle.len()
    }

    /// Take the oldest overflow job without being a pool thread. For tests
    /// that play the worker against a pool with no threads.
    #[doc(hidden)]
    #[must_use]
    pub fn try_pop_queued(&self) -> Option<T> {
        pop_queued(&self.shared, &mut lock(&self.shared.state))
    }
}

/// Pop the oldest overflow job under the caller's lock, keeping the mirror
/// counter and the `push` waiters in step.
fn pop_queued<T>(shared: &Shared<T>, st: &mut State<T>) -> Option<T> {
    let job = st.queue.pop_front()?;
    shared.queued.store(st.queue.len(), Ordering::Relaxed);
    if shared.push_waiters.load(Ordering::Acquire) > 0 {
        // Space just opened: let one `push` waiter retry. One notify per pop,
        // so N pops admit N waiters.
        shared.space.notify_one();
    }
    Some(job)
}

/// A pool thread's receiving end. Owned by exactly one thread (the one that
/// called [`Dispatcher::register_worker`]).
pub struct Worker<T> {
    shared: Arc<Shared<T>>,
    slot: Arc<Slot<T>>,
    /// This thread's current spin budget under an adaptive [`SpinPolicy`]
    /// (clamped to the policy's budget on use, so it starts at the ceiling).
    budget: Cell<Duration>,
    /// `!Send`: the mailbox wakes the thread that registered it.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl<T> std::fmt::Debug for Worker<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl<T> Worker<T> {
    /// Wait for the next job, polling for up to `spin` before parking —
    /// [`Worker::recv_with`] with [`SpinPolicy::fixed`].
    ///
    /// # Errors
    ///
    /// [`RecvClosed`] once the dispatcher is closed and its queue drained.
    pub fn recv(&self, spin: Duration) -> Result<T, RecvClosed> {
        self.recv_with(SpinPolicy::fixed(spin))
    }

    /// [`recv`](Self::recv), running `before_park` on this thread each time it
    /// is about to park — [`Worker::recv_with_hook`] with [`SpinPolicy::fixed`].
    ///
    /// # Errors
    ///
    /// [`RecvClosed`] once the dispatcher is closed and its queue drained.
    pub fn recv_with_park_hook(
        &self,
        spin: Duration,
        before_park: impl FnMut(),
    ) -> Result<T, RecvClosed> {
        self.recv_with_hook(SpinPolicy::fixed(spin), before_park)
    }

    /// Wait for the next job — [`Worker::recv_with_hook`] with no park hook.
    ///
    /// # Errors
    ///
    /// [`RecvClosed`] once the dispatcher is closed and its queue drained.
    pub fn recv_with(&self, policy: SpinPolicy) -> Result<T, RecvClosed> {
        self.recv_with_hook(policy, || {})
    }

    /// Wait for the next job.
    ///
    /// Takes the oldest overflow job if there is one; otherwise registers this
    /// thread as idle and waits on its own mailbox — polling it for up to the
    /// policy's budget (pure spins, then `sched_yield` polls) before parking.
    /// The poll touches only this thread's mailbox, so idle threads never
    /// contend with each other or with the dispatcher. A zero budget parks at
    /// once.
    ///
    /// `before_park` runs on this thread each time it is about to park — after
    /// the spin found nothing, never when a queued job or a delivery during the
    /// spin ends the wait. A worker uses it to release per-request resources
    /// that must not be held across an unbounded idle wait (the PHP execution
    /// timer) without paying for that on every request when it never goes
    /// idle. A delivery that lands while the hook runs is not lost: the park is
    /// then skipped.
    ///
    /// # Errors
    ///
    /// [`RecvClosed`] once the dispatcher is closed and its queue drained.
    pub fn recv_with_hook(
        &self,
        policy: SpinPolicy,
        mut before_park: impl FnMut(),
    ) -> Result<T, RecvClosed> {
        loop {
            let spin =
                if policy.adaptive { self.budget.get().min(policy.budget) } else { policy.budget };
            {
                let mut st = lock(&self.shared.state);
                if let Some(job) = pop_queued(&self.shared, &mut st) {
                    return Ok(job);
                }
                if st.closed {
                    return Err(RecvClosed);
                }
                // Reset the mailbox before it becomes visible to dispatchers:
                // from the push below until a dispatcher pops it, only that
                // dispatcher (or `close`) may touch it.
                self.slot.stamp.store(policy.adaptive, Ordering::Relaxed);
                if spin.is_zero() {
                    self.slot.state.store(WAITING, Ordering::Release);
                } else {
                    self.slot.state.store(SPINNING, Ordering::Release);
                    if !self.shared.steer.load(Ordering::Relaxed) {
                        self.shared.steer.store(true, Ordering::Relaxed);
                    }
                }
                st.idle.push(Arc::clone(&self.slot));
                // An idle thread is room as well: a `push` waiter parked on a
                // full queue must hear about it, or it can sleep while
                // threads sit idle (otherwise only pops would wake it).
                if self.shared.push_waiters.load(Ordering::Acquire) > 0 {
                    self.shared.space.notify_one();
                }
            }
            // Taken after the lock is released, so a delivery can be stamped
            // before it; the gap then saturates to zero (a hit), as it should.
            let idle_since = policy.adaptive.then(Instant::now);
            self.wait_ready(spin, policy.pure_spins, &mut before_park);
            let (job, delivered_at) = self.slot.take();
            if let (Some(since), Some(at)) = (idle_since, delivered_at) {
                self.adapt(policy.budget, at.saturating_duration_since(since));
            }
            if let Some(job) = job {
                return Ok(job);
            }
            // A bare wake-up: `close()` emptied the idle stack. Loop to drain
            // whatever is still queued, then report the close.
        }
    }

    /// Adaptive budget update after an idle period that lasted `gap` (from
    /// going idle to the dispatcher's delivery): a gap the full budget `max`
    /// would have covered restores it, a longer one halves the current budget.
    fn adapt(&self, max: Duration, gap: Duration) {
        let next = if gap <= max {
            max
        } else {
            let half = self.budget.get().min(max) / 2;
            if half < ADAPTIVE_FLOOR { Duration::ZERO } else { half }
        };
        self.budget.set(next);
    }

    /// Block until the mailbox is `READY`: poll for up to `spin`, then run
    /// `before_park` and park.
    fn wait_ready(&self, spin: Duration, pure_spins: u32, before_park: &mut impl FnMut()) {
        let state = &self.slot.state;
        let announced = if spin.is_zero() { WAITING } else { SPINNING };
        if !spin.is_zero() {
            let mut started: Option<Instant> = None;
            let mut polls: u32 = 0;
            loop {
                if state.load(Ordering::Acquire) == READY {
                    return;
                }
                let start = *started.get_or_insert_with(Instant::now);
                if start.elapsed() >= spin {
                    break;
                }
                if polls < pure_spins {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
                polls = polls.saturating_add(1);
            }
        }
        // About to park (unless a delivery lands first — checked by the CAS
        // below, so one arriving while the hook runs is never missed).
        before_park();
        // Announce the park. If a delivery won the race the CAS fails and the
        // mailbox is already READY. Only this thread moves the mailbox out of
        // `announced`, and every other party only ever stores READY, so the
        // CAS fails exactly when a delivery has landed — the job is never
        // stranded in a mailbox whose owner sleeps.
        if state.compare_exchange(announced, PARKED, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        // `park` may wake spuriously; only READY ends the wait. An unpark
        // that lands before the park is not lost (the thread's park token).
        while state.load(Ordering::Acquire) != READY {
            std::thread::park();
        }
    }

    /// Put this thread on the idle stack without waiting, as `recv` does just
    /// before it blocks. Lets tests stage a delivery into a mailbox whose
    /// owner then retires.
    #[cfg(test)]
    fn register_idle_for_test(&self) {
        let mut st = lock(&self.shared.state);
        self.slot.state.store(WAITING, Ordering::Release);
        st.idle.push(Arc::clone(&self.slot));
    }

    /// This thread's current adaptive budget, as [`Worker::recv_with`] would
    /// use it under `policy`.
    #[cfg(test)]
    fn current_budget(&self, policy: SpinPolicy) -> Duration {
        self.budget.get().min(policy.budget)
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        // Defence in depth — `recv` never returns with this thread still idle
        // or with a job in its mailbox. If a thread retires anyway in that
        // window, nothing it was handed may be lost.
        let was_idle = {
            let mut st = lock(&self.shared.state);
            let before = st.idle.len();
            st.idle.retain(|s| !Arc::ptr_eq(s, &self.slot));
            st.idle.len() != before
        };
        if !was_idle {
            // Not on the idle stack: either this thread is between jobs
            // (mailbox READY and empty), or a dispatcher popped it and is about
            // to deliver (mailbox not READY yet). Wait out the latter — the
            // delivery is a store and a swap, and needs no lock.
            while self.slot.state.load(Ordering::Acquire) != READY {
                std::thread::yield_now();
            }
        }
        let (Some(job), _) = self.slot.take() else {
            return;
        };
        let mut st = lock(&self.shared.state);
        let steer = self.shared.steer.load(Ordering::Relaxed);
        if let Some(other) = take_idle(&mut st.idle, steer) {
            drop(st);
            other.deliver(Some(job));
        } else {
            // Front: it was dispatched before anything still queued. Under
            // FIFO admission the job still holds its permit, so the bound
            // holds; under barge admission each re-routed job can exceed
            // `capacity` by one (a window `recv` never leaves open).
            st.queue.push_front(job);
            self.shared.queued.store(st.queue.len(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    const LONG: Duration = Duration::from_secs(10);

    /// Spawn a pool thread that registers itself and forwards every job it
    /// receives (tagged with its index) until the dispatcher closes.
    fn spawn_forwarder(
        d: &Dispatcher<u32>,
        idx: usize,
        spin: Duration,
        tx: mpsc::Sender<(usize, u32)>,
    ) -> std::thread::JoinHandle<()> {
        spawn_forwarder_with(d, idx, SpinPolicy::fixed(spin), tx)
    }

    fn spawn_forwarder_with(
        d: &Dispatcher<u32>,
        idx: usize,
        policy: SpinPolicy,
        tx: mpsc::Sender<(usize, u32)>,
    ) -> std::thread::JoinHandle<()> {
        let d = d.clone();
        std::thread::spawn(move || {
            let w = d.register_worker();
            while let Ok(job) = w.recv_with(policy) {
                if tx.send((idx, job)).is_err() {
                    break;
                }
            }
        })
    }

    const fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    /// The policies every stress test runs under: today's two (park, fixed
    /// 200 µs), tiny budgets that put most deliveries right at the
    /// spin/park boundary, pure spinning, always-yield, and adaptive.
    const STRESS_POLICIES: [SpinPolicy; 7] = [
        SpinPolicy::PARK,
        SpinPolicy::fixed(us(200)),
        SpinPolicy::fixed(us(2)),
        SpinPolicy { budget: us(5), pure_spins: 0, adaptive: false },
        SpinPolicy { budget: us(50), pure_spins: u32::MAX, adaptive: false },
        SpinPolicy { budget: us(200), pure_spins: DEFAULT_PURE_SPINS, adaptive: true },
        SpinPolicy { budget: us(3), pure_spins: 0, adaptive: true },
    ];

    /// How many times the stress tests repeat each policy
    /// (`EPHPM_TEST_SPIN_REPEAT`, default 1).
    fn stress_repeat() -> usize {
        std::env::var("EPHPM_TEST_SPIN_REPEAT").ok().and_then(|v| v.parse().ok()).unwrap_or(1)
    }

    fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + LONG;
        while !cond() {
            assert!(Instant::now() < deadline, "condition not reached in time");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn queues_until_a_worker_arrives_then_hands_out_in_order() {
        let d = Dispatcher::new(4);
        for j in 1..=3 {
            d.try_push(j).unwrap();
        }
        assert_eq!(d.queued(), 3);
        let (tx, rx) = mpsc::channel();
        let h = spawn_forwarder(&d, 0, Duration::ZERO, tx);
        let got: Vec<u32> = (0..3).map(|_| rx.recv_timeout(LONG).unwrap().1).collect();
        assert_eq!(got, vec![1, 2, 3], "overflow is FIFO");
        assert_eq!(d.queued(), 0);
        d.close();
        h.join().unwrap();
    }

    #[test]
    fn full_queue_rejects_and_gives_the_job_back() {
        let d = Dispatcher::new(2);
        d.try_push(1).unwrap();
        d.try_push(2).unwrap();
        match d.try_push(3) {
            Err(TryPushError::Full(3)) => {}
            other => panic!("expected Full(3), got {other:?}"),
        }
        assert_eq!(d.queued(), 2, "a rejected job must not be counted");
    }

    #[test]
    fn idle_worker_gets_the_job_directly_spinning_or_parked() {
        for spin in [Duration::ZERO, Duration::from_millis(50), LONG] {
            let d = Dispatcher::new(1);
            let (tx, rx) = mpsc::channel();
            let h = spawn_forwarder(&d, 0, spin, tx);
            wait_until(|| d.idle_count() == 1);
            // Let the worker park when the spin is short.
            std::thread::sleep(Duration::from_millis(80));
            d.try_push(7).unwrap();
            assert_eq!(d.queued(), 0, "an idle worker is fed directly, not via the queue");
            assert_eq!(rx.recv_timeout(LONG).unwrap(), (0, 7));
            d.close();
            h.join().unwrap();
        }
    }

    /// The park hook (the worker's PHP execution-timer disarm) runs only when
    /// the worker really parks: not for a queued job, not for a delivery that
    /// lands during the spin — and it does run before a park.
    #[test]
    fn park_hook_runs_only_when_the_worker_parks() {
        use std::sync::atomic::AtomicUsize;

        // Queued job: taken without waiting, hook never runs.
        let d = Dispatcher::new(4);
        d.try_push(1).unwrap();
        let w = d.register_worker();
        let mut hooks = 0;
        assert_eq!(w.recv_with_park_hook(LONG, || hooks += 1), Ok(1));
        assert_eq!(hooks, 0, "a queued job must not run the park hook");

        // Delivered during a long spin: hook never runs.
        let d = Dispatcher::new(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let h = {
            let (d, calls) = (d.clone(), Arc::clone(&calls));
            std::thread::spawn(move || {
                let w = d.register_worker();
                w.recv_with_park_hook(LONG, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                })
            })
        };
        wait_until(|| d.idle_count() == 1);
        d.try_push(2).unwrap();
        assert_eq!(h.join().unwrap(), Ok(2));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "a delivery during the spin must not park");

        // No spin: the hook runs before the park, and the delivery still lands.
        let d = Dispatcher::new(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let h = {
            let (d, calls) = (d.clone(), Arc::clone(&calls));
            std::thread::spawn(move || {
                let w = d.register_worker();
                w.recv_with_park_hook(Duration::ZERO, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                })
            })
        };
        wait_until(|| calls.load(Ordering::SeqCst) == 1);
        d.try_push(3).unwrap();
        assert_eq!(h.join().unwrap(), Ok(3));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn close_wakes_parked_and_spinning_workers() {
        let d = Dispatcher::new(1);
        let (tx, _rx) = mpsc::channel();
        let parked = spawn_forwarder(&d, 0, Duration::ZERO, tx.clone());
        let spinning = spawn_forwarder(&d, 1, LONG, tx);
        wait_until(|| d.idle_count() == 2);
        d.close();
        parked.join().unwrap();
        spinning.join().unwrap();
        assert!(d.try_push(1).unwrap_err().is_closed());
    }

    #[test]
    fn queued_jobs_still_drain_after_close() {
        let d = Dispatcher::new(4);
        d.try_push(1).unwrap();
        d.try_push(2).unwrap();
        d.close();
        let w = d.register_worker();
        assert_eq!(w.recv(Duration::ZERO), Ok(1));
        assert_eq!(w.recv(Duration::ZERO), Ok(2));
        assert_eq!(w.recv(Duration::ZERO), Err(RecvClosed));
    }

    /// Head-of-line: with one worker stuck on a long job and another idle,
    /// every new job goes to the idle one and none waits behind the busy one.
    #[test]
    fn no_job_waits_behind_a_busy_worker_while_another_is_idle() {
        let d: Dispatcher<u32> = Dispatcher::new(8);
        let (tx, rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        // Worker 0 blocks inside job 0 until released.
        let slow = {
            let d = d.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let w = d.register_worker();
                while let Ok(job) = w.recv(Duration::ZERO) {
                    tx.send((0, job)).unwrap();
                    if job == 0 {
                        release_rx.recv().unwrap();
                    }
                }
            })
        };
        wait_until(|| d.idle_count() == 1);
        d.try_push(0).unwrap();
        assert_eq!(rx.recv_timeout(LONG).unwrap(), (0, 0));
        let fast = spawn_forwarder(&d, 1, Duration::ZERO, tx);
        for j in 1..=20 {
            wait_until(|| d.idle_count() == 1);
            d.try_push(j).unwrap();
            assert_eq!(
                rx.recv_timeout(LONG).unwrap(),
                (1, j),
                "job {j} must go to the idle worker, not queue behind the busy one"
            );
        }
        release_tx.send(()).unwrap();
        d.close();
        slow.join().unwrap();
        fast.join().unwrap();
    }

    /// A job sitting in the mailbox of a thread that retires before picking it
    /// up is re-routed: to another idle thread if there is one, else to the
    /// front of the queue.
    #[test]
    fn a_retiring_worker_reroutes_its_undelivered_job() {
        // To the front of the queue.
        let d = Dispatcher::new(4);
        let w = d.register_worker();
        w.register_idle_for_test();
        d.try_push(1).unwrap(); // lands in w's mailbox
        assert_eq!(d.queued(), 0);
        drop(w);
        assert_eq!(d.queued(), 1, "the orphaned job must be queued, not dropped");
        assert_eq!(d.try_pop_queued(), Some(1));

        // To another idle thread.
        let d = Dispatcher::new(4);
        let (tx, rx) = mpsc::channel();
        let survivor = spawn_forwarder(&d, 9, Duration::ZERO, tx);
        wait_until(|| d.idle_count() == 1);
        let w = d.register_worker();
        w.register_idle_for_test(); // top of the LIFO stack
        d.try_push(2).unwrap(); // goes to w
        drop(w);
        assert_eq!(rx.recv_timeout(LONG).unwrap(), (9, 2));
        d.close();
        survivor.join().unwrap();

        // A retiring idle thread is removed from the idle stack, so later jobs
        // are not delivered into a dead mailbox.
        let d = Dispatcher::new(4);
        let w = d.register_worker();
        w.register_idle_for_test();
        drop(w);
        assert_eq!(d.idle_count(), 0);
        d.try_push(3).unwrap();
        assert_eq!(d.queued(), 1);
    }

    #[test]
    fn many_producers_many_workers_lose_nothing() {
        for _ in 0..stress_repeat() {
            for policy in STRESS_POLICIES {
                // Half the threads park at once, so spinning and parked
                // mailboxes mix on the idle stack and steering is exercised.
                many_producers_many_workers_lose_nothing_with(policy);
            }
        }
    }

    fn many_producers_many_workers_lose_nothing_with(policy: SpinPolicy) {
        const WORKERS: usize = 8;
        const PRODUCERS: u32 = 4;
        const PER: u32 = 5_000;
        let d = Dispatcher::new(16);
        let (tx, rx) = mpsc::channel();
        let workers: Vec<_> = (0..WORKERS)
            .map(|i| {
                let p = if i % 2 == 0 { SpinPolicy::PARK } else { policy };
                spawn_forwarder_with(&d, i, p, tx.clone())
            })
            .collect();
        drop(tx);
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|p| {
                let d = d.clone();
                std::thread::spawn(move || {
                    for i in 0..PER {
                        let mut job = p * PER + i;
                        loop {
                            match d.try_push(job) {
                                Ok(()) => break,
                                Err(TryPushError::Full(j)) => {
                                    job = j;
                                    std::thread::yield_now();
                                }
                                Err(TryPushError::Closed(_)) => panic!("closed early"),
                            }
                        }
                    }
                })
            })
            .collect();
        for p in producers {
            p.join().unwrap();
        }
        let mut seen = vec![false; (PRODUCERS * PER) as usize];
        for _ in 0..PRODUCERS * PER {
            let (_, job) =
                rx.recv_timeout(LONG).unwrap_or_else(|_| panic!("a job went missing ({policy:?})"));
            assert!(!seen[job as usize], "job {job} delivered twice");
            seen[job as usize] = true;
        }
        d.close();
        for w in workers {
            w.join().unwrap();
        }
        assert!(seen.iter().all(|s| *s));
    }

    /// Worker recycling under load (`[php.worker] max_requests`): every
    /// thread retires after a few jobs and a fresh thread replaces it, while
    /// producers keep pushing. Nothing may be lost, duplicated or stranded in
    /// a retired thread's mailbox.
    #[test]
    fn recycling_workers_under_load_lose_nothing() {
        for _ in 0..stress_repeat() {
            for policy in STRESS_POLICIES {
                recycling_workers_under_load_lose_nothing_with(policy);
            }
        }
    }

    fn recycling_workers_under_load_lose_nothing_with(policy: SpinPolicy) {
        const POOL: usize = 6;
        const MAX_REQUESTS: usize = 7;
        const TOTAL: u32 = 6_000;

        fn spawn_recycling(
            d: Dispatcher<u32>,
            tx: mpsc::Sender<u32>,
            live: Arc<AtomicUsize>,
            spawned: Arc<AtomicUsize>,
            policy: SpinPolicy,
        ) {
            spawned.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let w = d.register_worker();
                let mut served = 0;
                while served < MAX_REQUESTS {
                    let Ok(job) = w.recv_with(policy) else {
                        live.fetch_sub(1, Ordering::SeqCst);
                        return;
                    };
                    tx.send(job).unwrap();
                    served += 1;
                }
                // Retire like a recycled worker: drop the mailbox, then the
                // supervisor spawns the replacement.
                drop(w);
                if d.is_closed() {
                    live.fetch_sub(1, Ordering::SeqCst);
                } else {
                    spawn_recycling(d, tx, live, spawned, policy);
                }
            });
        }

        let d = Dispatcher::new(8);
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(AtomicUsize::new(POOL));
        let spawned = Arc::new(AtomicUsize::new(0));
        for _ in 0..POOL {
            spawn_recycling(d.clone(), tx.clone(), Arc::clone(&live), Arc::clone(&spawned), policy);
        }
        drop(tx);
        let producer = {
            let d = d.clone();
            std::thread::spawn(move || {
                for mut job in 0..TOTAL {
                    loop {
                        match d.try_push(job) {
                            Ok(()) => break,
                            Err(TryPushError::Full(j)) => {
                                job = j;
                                std::thread::yield_now();
                            }
                            Err(TryPushError::Closed(_)) => panic!("closed early"),
                        }
                    }
                }
            })
        };
        let mut seen = vec![false; TOTAL as usize];
        for _ in 0..TOTAL {
            let job = rx
                .recv_timeout(LONG)
                .unwrap_or_else(|_| panic!("a job went missing or was stranded ({policy:?})"));
            assert!(!seen[job as usize], "job {job} delivered twice");
            seen[job as usize] = true;
        }
        producer.join().unwrap();
        assert!(spawned.load(Ordering::SeqCst) > POOL * 10, "workers must actually have recycled");
        d.close();
        wait_until(|| live.load(Ordering::SeqCst) == 0);
    }

    #[tokio::test]
    async fn push_waits_for_space_and_is_barge_ordered() {
        let d = Dispatcher::new(1);
        d.try_push(1).unwrap();
        let waiter = {
            let d = d.clone();
            tokio::spawn(async move { d.push(2).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the queue is full: push must wait");
        // Free the slot; a newcomer's try_push steals it before the woken
        // waiter runs (barge), and the waiter re-waits.
        assert_eq!(d.try_pop_queued(), Some(1));
        d.try_push(3).unwrap();
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the lapped waiter must still be waiting");
        assert_eq!(d.try_pop_queued(), Some(3));
        waiter.await.unwrap().unwrap();
        assert_eq!(d.try_pop_queued(), Some(2));
    }

    /// Barge waiters must be woken by any new room, not just by queue pops:
    /// here the first waiter's job goes to a thread that just went idle,
    /// leaving the freed queue slot for the second waiter.
    #[tokio::test]
    async fn barge_waiter_not_stranded_when_space_frees_via_idle_handoff() {
        let d: Dispatcher<u32> = Dispatcher::new(1);
        d.try_push(1).unwrap();
        let w1 = {
            let d = d.clone();
            tokio::spawn(async move { d.push(2).await })
        };
        let w2 = {
            let d = d.clone();
            tokio::spawn(async move { d.push(3).await })
        };
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(d.try_pop_queued(), Some(1)); // wakes one waiter
        let worker = d.register_worker();
        worker.register_idle_for_test(); // the woken job bypasses the queue
        assert!(tokio::time::timeout(Duration::from_secs(2), w1).await.is_ok());
        assert!(
            tokio::time::timeout(Duration::from_millis(500), w2).await.is_ok(),
            "w2 stranded: the queue has a free slot but nobody woke it"
        );
    }

    /// Barge `push` against real threads that keep going idle: every waiter
    /// must get in, round after round.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn barge_push_under_churn_never_strands() {
        for (round, policy) in
            (0..50 * stress_repeat()).flat_map(|r| STRESS_POLICIES.into_iter().map(move |p| (r, p)))
        {
            let d: Dispatcher<u32> = Dispatcher::new(1);
            let done = Arc::new(AtomicUsize::new(0));
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let d = d.clone();
                    let done = Arc::clone(&done);
                    std::thread::spawn(move || {
                        let w = d.register_worker();
                        while w.recv_with(policy).is_ok() {
                            std::thread::sleep(Duration::from_micros(300));
                            done.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();
            let tasks: Vec<_> = (0..16u32)
                .map(|i| {
                    let d = d.clone();
                    tokio::spawn(async move { d.push(i).await })
                })
                .collect();
            for t in tasks {
                assert!(
                    tokio::time::timeout(Duration::from_secs(3), t).await.is_ok(),
                    "round {round} ({policy:?}): stranded (queued={}, idle={})",
                    d.queued(),
                    d.idle_count()
                );
            }
            d.close();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(done.load(Ordering::SeqCst), 16);
        }
    }

    /// A cancelled `push` (the request timeout) releases its waiter count.
    #[tokio::test]
    async fn cancelled_push_releases_its_waiter_slot() {
        let d: Dispatcher<u32> = Dispatcher::new(1);
        d.try_push(1).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), d.push(2)).await.is_err());
        assert_eq!(d.shared.push_waiters.load(Ordering::SeqCst), 0);
        assert_eq!(d.queued(), 1, "the cancelled job was never queued");
    }

    #[tokio::test]
    async fn push_fails_when_closed_while_waiting() {
        let d = Dispatcher::new(1);
        d.try_push(1).unwrap();
        let waiter = {
            let d = d.clone();
            tokio::spawn(async move { d.push(2).await })
        };
        tokio::task::yield_now().await;
        d.close();
        assert_eq!(waiter.await.unwrap(), Err(2));
    }

    /// Busy-wait `d` (no sleep: the point is landing deliveries at the
    /// microsecond the receiver stops spinning).
    fn busy_wait(d: Duration) {
        let start = Instant::now();
        while start.elapsed() < d {
            std::hint::spin_loop();
        }
    }

    /// Lost wake-up hunt: one job at a time, each pushed after a delay that
    /// sweeps across the spin budget, so many deliveries land just before,
    /// at, or just after the moment the receiver gives up spinning and parks.
    /// A lost wake-up shows up as a job that is never answered.
    #[test]
    fn deliveries_at_the_spin_boundary_are_never_lost() {
        const ROUNDS: u32 = 4_000;
        let policies = [
            SpinPolicy::fixed(us(1)),
            SpinPolicy::fixed(us(4)),
            SpinPolicy { budget: us(8), pure_spins: u32::MAX, adaptive: false },
            SpinPolicy { budget: us(6), pure_spins: 0, adaptive: true },
        ];
        for _ in 0..stress_repeat() {
            for policy in policies {
                let d = Dispatcher::new(4);
                let (tx, rx) = mpsc::channel();
                let h = spawn_forwarder_with(&d, 0, policy, tx);
                for i in 0..ROUNDS {
                    // Spin (not sleep) until the receiver is idle, then wait
                    // 0..=16 µs: the sweep straddles every budget above.
                    while d.idle_count() == 0 {
                        std::hint::spin_loop();
                    }
                    busy_wait(us(u64::from(i % 17)));
                    d.try_push(i).unwrap();
                    let got = rx.recv_timeout(LONG).unwrap_or_else(|_| {
                        panic!("job {i} lost at the spin boundary ({policy:?})")
                    });
                    assert_eq!(got, (0, i));
                }
                d.close();
                h.join().unwrap();
            }
        }
    }

    /// The same hunt with several receivers and producers racing, so a
    /// delivery can also land while the target is being steered past.
    #[test]
    fn concurrent_boundary_deliveries_are_never_lost() {
        const WORKERS: usize = 4;
        const PRODUCERS: u32 = 3;
        const PER: u32 = 3_000;
        for _ in 0..stress_repeat() {
            for policy in [
                SpinPolicy::fixed(us(3)),
                SpinPolicy { adaptive: true, ..SpinPolicy::fixed(us(5)) },
            ] {
                let d = Dispatcher::new(64);
                let (tx, rx) = mpsc::channel();
                let workers: Vec<_> =
                    (0..WORKERS).map(|i| spawn_forwarder_with(&d, i, policy, tx.clone())).collect();
                drop(tx);
                let producers: Vec<_> = (0..PRODUCERS)
                    .map(|p| {
                        let d = d.clone();
                        std::thread::spawn(move || {
                            for i in 0..PER {
                                busy_wait(us(u64::from((i * 7 + p) % 11)));
                                let mut job = p * PER + i;
                                loop {
                                    match d.try_push(job) {
                                        Ok(()) => break,
                                        Err(TryPushError::Full(j)) => {
                                            job = j;
                                            std::thread::yield_now();
                                        }
                                        Err(TryPushError::Closed(_)) => panic!("closed early"),
                                    }
                                }
                            }
                        })
                    })
                    .collect();
                for p in producers {
                    p.join().unwrap();
                }
                let mut seen = vec![false; (PRODUCERS * PER) as usize];
                for _ in 0..PRODUCERS * PER {
                    let (_, job) = rx
                        .recv_timeout(LONG)
                        .unwrap_or_else(|_| panic!("a job went missing ({policy:?})"));
                    assert!(!seen[job as usize], "job {job} delivered twice");
                    seen[job as usize] = true;
                }
                d.close();
                for w in workers {
                    w.join().unwrap();
                }
            }
        }
    }

    /// With a parked thread on top of the idle stack and a spinning one below
    /// it, a delivery goes to the spinning one (no wake-up needed).
    #[test]
    fn delivery_prefers_a_spinning_thread_over_a_parked_one() {
        let d = Dispatcher::new(4);
        let (tx, rx) = mpsc::channel();
        let spinner = spawn_forwarder(&d, 1, LONG, tx.clone());
        wait_until(|| d.idle_count() == 1);
        let parker = spawn_forwarder(&d, 2, Duration::ZERO, tx);
        wait_until(|| d.idle_count() == 2);
        // The parker is on top; give it time to actually park.
        std::thread::sleep(Duration::from_millis(50));
        d.try_push(5).unwrap();
        assert_eq!(rx.recv_timeout(LONG).unwrap(), (1, 5), "the spinning thread must get the job");
        d.close();
        spinner.join().unwrap();
        parker.join().unwrap();
    }

    /// Without any spinning thread, delivery is plain LIFO, exactly as before
    /// spin policies existed.
    #[test]
    fn without_spinning_delivery_is_plain_lifo() {
        let d = Dispatcher::new(4);
        let (tx, rx) = mpsc::channel();
        let first = spawn_forwarder(&d, 1, Duration::ZERO, tx.clone());
        wait_until(|| d.idle_count() == 1);
        let second = spawn_forwarder(&d, 2, Duration::ZERO, tx);
        wait_until(|| d.idle_count() == 2);
        d.try_push(5).unwrap();
        assert_eq!(rx.recv_timeout(LONG).unwrap(), (2, 5));
        assert!(!d.shared.steer.load(Ordering::Relaxed), "nothing spun, so nothing steers");
        d.close();
        first.join().unwrap();
        second.join().unwrap();
    }

    #[test]
    fn adaptive_budget_arithmetic() {
        let d: Dispatcher<u32> = Dispatcher::new(1);
        let w = d.register_worker();
        let p = SpinPolicy { adaptive: true, ..SpinPolicy::fixed(us(200)) };
        assert_eq!(w.current_budget(p), us(200), "starts at the ceiling");
        let mut expect = us(200);
        for _ in 0..6 {
            w.adapt(p.budget, Duration::from_millis(5));
            expect /= 2;
            assert_eq!(w.current_budget(p), expect);
        }
        // 3.125 µs halves below the 2 µs floor: collapse to zero.
        w.adapt(p.budget, Duration::from_millis(5));
        assert_eq!(w.current_budget(p), Duration::ZERO);
        w.adapt(p.budget, Duration::from_millis(5));
        assert_eq!(w.current_budget(p), Duration::ZERO, "zero stays zero on long gaps");
        // A gap the full budget would have covered restores it at once.
        w.adapt(p.budget, us(150));
        assert_eq!(w.current_budget(p), us(200));
        // A smaller ceiling clamps the stored budget.
        let small = SpinPolicy { budget: us(10), ..p };
        assert_eq!(w.current_budget(small), us(10));
    }

    /// End to end: long idle gaps drive a thread's adaptive budget to zero; a
    /// short one turns spinning back on.
    #[test]
    fn adaptive_budget_decays_when_idle_and_recovers_under_load() {
        let d: Dispatcher<u32> = Dispatcher::new(4);
        let w = d.register_worker();
        let p = SpinPolicy { adaptive: true, ..SpinPolicy::fixed(Duration::from_millis(1)) };
        let pusher = |delay: Duration, job: u32| {
            let d = d.clone();
            std::thread::spawn(move || {
                while d.idle_count() == 0 {
                    std::thread::yield_now();
                }
                std::thread::sleep(delay);
                d.try_push(job).unwrap();
            })
        };
        // 1 ms halves to below 2 µs in 10 steps.
        for job in 0..12 {
            let h = pusher(Duration::from_millis(15), job);
            assert_eq!(w.recv_with(p), Ok(job));
            h.join().unwrap();
        }
        assert_eq!(w.current_budget(p), Duration::ZERO, "long gaps must switch spinning off");
        // A prompt delivery (well inside 1 ms) restores the budget. Allow a
        // few tries in case the host deschedules the pusher.
        let mut restored = false;
        for job in 100..105 {
            let h = pusher(Duration::ZERO, job);
            assert_eq!(w.recv_with(p), Ok(job));
            h.join().unwrap();
            if w.current_budget(p) == p.budget {
                restored = true;
                break;
            }
        }
        assert!(restored, "a short gap must turn spinning back on");
    }

    #[test]
    fn spin_policy_from_vars() {
        let def = SpinPolicy::fixed(us(200));
        assert_eq!(SpinPolicy::from_vars("t", def, None, None, None), def);
        assert_eq!(
            SpinPolicy::from_vars("t", def, Some("0"), None, None),
            SpinPolicy::PARK,
            "budget 0 parks at once"
        );
        assert_eq!(
            SpinPolicy::from_vars("t", def, Some(" 50 "), Some("max"), Some("1")),
            SpinPolicy { budget: us(50), pure_spins: u32::MAX, adaptive: true }
        );
        assert_eq!(
            SpinPolicy::from_vars("t", def, Some("7"), Some("0"), Some("off")),
            SpinPolicy { budget: us(7), pure_spins: 0, adaptive: false }
        );
        // Garbage is ignored, keeping the default.
        assert_eq!(SpinPolicy::from_vars("t", def, Some("50us"), Some("-1"), Some("maybe")), def);
        // An adaptive default (the worker pool's) can be switched off.
        let adaptive = SpinPolicy { adaptive: true, ..def };
        assert_eq!(SpinPolicy::from_vars("t", adaptive, None, None, None), adaptive);
        assert_eq!(SpinPolicy::from_vars("t", adaptive, None, None, Some("0")), def);
    }

    /// On-CPU time of the calling thread, in nanoseconds (Linux schedstat).
    #[cfg(target_os = "linux")]
    fn thread_cpu_ns() -> u64 {
        std::fs::read_to_string("/proc/thread-self/schedstat")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
            .unwrap_or(0)
    }

    /// Total on-CPU time of `WORKERS` pool threads over a 500 ms window in
    /// which `traffic` jobs arrive evenly spaced. Each thread reports its own
    /// schedstat on a sentinel job, then holds at a barrier so each thread
    /// takes exactly one sentinel.
    #[cfg(target_os = "linux")]
    fn window_cpu_ms(policy: SpinPolicy, traffic: u32) -> f64 {
        const WORKERS: usize = 8;
        const REPORT: u32 = u32::MAX;
        let d: Dispatcher<u32> = Dispatcher::new(64);
        let (tx, rx) = mpsc::channel::<u64>();
        let barrier = Arc::new(std::sync::Barrier::new(WORKERS + 1));
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                let d = d.clone();
                let tx = tx.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let w = d.register_worker();
                    while let Ok(job) = w.recv_with(policy) {
                        if job == REPORT {
                            tx.send(thread_cpu_ns()).unwrap();
                            barrier.wait();
                        }
                    }
                })
            })
            .collect();
        let report = || -> u64 {
            wait_until(|| d.idle_count() == WORKERS);
            for _ in 0..WORKERS {
                d.try_push(REPORT).unwrap();
            }
            let total = (0..WORKERS).map(|_| rx.recv_timeout(LONG).unwrap()).sum();
            barrier.wait();
            total
        };
        // Let every thread finish its first idle period before measuring.
        std::thread::sleep(Duration::from_millis(100));
        let before = report();
        let window = Duration::from_millis(500);
        let start = Instant::now();
        if traffic == 0 {
            std::thread::sleep(window);
        } else {
            let step = window / traffic;
            for i in 0..traffic {
                let at = start + step * i;
                if let Some(wait) = at.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
                // Offered load, not a capacity check: on a slow or shared host
                // (the arm64 nightly node, three legs at once) the 8 workers
                // can fall behind at 5000 jobs/s and the 64-slot queue fills.
                // Back off and retry instead of failing the measurement.
                let mut job = i;
                loop {
                    match d.try_push(job) {
                        Ok(()) => break,
                        Err(TryPushError::Full(j)) => {
                            job = j;
                            std::thread::yield_now();
                        }
                        Err(TryPushError::Closed(_)) => panic!("dispatcher closed mid-window"),
                    }
                }
            }
            std::thread::sleep(window.saturating_sub(start.elapsed()));
        }
        let after = report();
        d.close();
        for w in workers {
            w.join().unwrap();
        }
        #[allow(clippy::cast_precision_loss)]
        let ms = after.saturating_sub(before) as f64 / 1e6;
        ms
    }

    /// With no traffic at all, idle threads cost ~nothing whatever the
    /// policy: they spin once, then park.
    #[cfg(target_os = "linux")]
    #[test]
    fn idle_pool_burns_no_cpu_with_spinning_on() {
        for policy in [
            SpinPolicy::fixed(us(200)),
            SpinPolicy { budget: us(200), pure_spins: u32::MAX, adaptive: false },
            SpinPolicy { adaptive: true, ..SpinPolicy::fixed(us(200)) },
        ] {
            // The window includes the threads' one post-report spin (8 x
            // 200 µs = 1.6 ms) and the report round itself.
            let ms = window_cpu_ms(policy, 0);
            assert!(ms < 10.0, "{policy:?}: {ms:.2} ms of CPU in a 500 ms idle window");
        }
    }

    /// Measurement, not a check: CPU of an 8-thread pool over 500 ms, idle
    /// and at low request rates, per policy. Run with `--ignored
    /// --nocapture` on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "measurement; prints a table"]
    fn measure_idle_and_low_rate_cpu() {
        let policies = [
            ("park", SpinPolicy::PARK),
            ("fixed 50us", SpinPolicy::fixed(us(50))),
            ("fixed 200us", SpinPolicy::fixed(us(200))),
            ("fixed 200us pure", SpinPolicy { pure_spins: u32::MAX, ..SpinPolicy::fixed(us(200)) }),
            ("adaptive 200us", SpinPolicy { adaptive: true, ..SpinPolicy::fixed(us(200)) }),
        ];
        println!("policy               | jobs/s | pool CPU ms per 500 ms | % of one core");
        for traffic in [0u32, 50, 500, 2_500] {
            for (name, policy) in policies {
                let ms = window_cpu_ms(policy, traffic);
                println!("{name:<20} | {:>6} | {ms:>22.2} | {:>5.1}", traffic * 2, ms / 5.0);
            }
        }
    }
}

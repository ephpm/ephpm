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
//! ## Retirement
//!
//! A thread is on the idle stack only inside [`Worker::recv`], so a thread that
//! retires (recycle, crash, hang replacement) between requests holds no job.
//! As a defence in depth, dropping a [`Worker`] removes it from the idle stack
//! and re-routes any job still sitting in its mailbox to another idle thread or
//! the front of the overflow queue — a job is never lost with its thread.

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

/// Mailbox states. The worker moves `WAITING -> PARKED` before it sleeps; a
/// dispatcher (or [`Dispatcher::close`]) moves either to `READY` and unparks
/// only when it saw `PARKED`.
const WAITING: u8 = 0;
const PARKED: u8 = 1;
const READY: u8 = 2;

/// One pool thread's single-entry mailbox.
struct Slot<T> {
    job: Mutex<Option<T>>,
    state: AtomicU8,
    thread: Thread,
}

impl<T> Slot<T> {
    /// Put `job` (or, with `None`, a bare wake-up) in the mailbox and wake the
    /// owner if it is parked. Called by exactly one party per idle period: the
    /// one that popped this slot off the idle stack.
    fn deliver(&self, job: Option<T>) {
        *lock(&self.job) = job;
        if self.state.swap(READY, Ordering::AcqRel) == PARKED {
            self.thread.unpark();
        }
    }

    fn take(&self) -> Option<T> {
        lock(&self.job).take()
    }
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
                job: Mutex::new(None),
                state: AtomicU8::new(READY),
                thread: std::thread::current(),
            }),
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
        let mut st = lock(&self.shared.state);
        if st.closed {
            return Err(TryPushError::Closed(job));
        }
        if let Some(slot) = st.idle.pop() {
            // Popping the slot under the lock makes this caller its only
            // deliverer; the delivery itself (and any unpark) runs unlocked.
            drop(st);
            slot.deliver(Some(job));
            return Ok(());
        }
        if st.queue.len() >= self.shared.capacity {
            return Err(TryPushError::Full(job));
        }
        st.queue.push_back(job);
        self.shared.queued.store(st.queue.len(), Ordering::Relaxed);
        Ok(())
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
            match self.try_push(job) {
                Ok(()) => return Ok(()),
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
}

impl<T> std::fmt::Debug for Worker<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl<T> Worker<T> {
    /// Wait for the next job.
    ///
    /// Takes the oldest overflow job if there is one; otherwise registers this
    /// thread as idle and waits on its own mailbox — polling it for up to
    /// `spin` (32 pure spins, then `sched_yield` polls) before parking. The
    /// poll touches only this thread's mailbox, so idle threads never contend
    /// with each other or with the dispatcher. `Duration::ZERO` parks at once.
    ///
    /// # Errors
    ///
    /// [`RecvClosed`] once the dispatcher is closed and its queue drained.
    pub fn recv(&self, spin: Duration) -> Result<T, RecvClosed> {
        loop {
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
                self.slot.state.store(WAITING, Ordering::Release);
                st.idle.push(Arc::clone(&self.slot));
            }
            self.wait_ready(spin);
            if let Some(job) = self.slot.take() {
                return Ok(job);
            }
            // A bare wake-up: `close()` emptied the idle stack. Loop to drain
            // whatever is still queued, then report the close.
        }
    }

    /// Block until the mailbox is `READY`: poll for up to `spin`, then park.
    fn wait_ready(&self, spin: Duration) {
        const PURE_SPINS: u32 = 32;
        let state = &self.slot.state;
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
                if polls < PURE_SPINS {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
                polls = polls.saturating_add(1);
            }
        }
        // Announce the park. If a delivery won the race the CAS fails and the
        // mailbox is already READY.
        if state.compare_exchange(WAITING, PARKED, Ordering::AcqRel, Ordering::Acquire).is_err() {
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
    #[doc(hidden)]
    pub fn register_idle_for_test(&self) {
        let mut st = lock(&self.shared.state);
        self.slot.state.store(WAITING, Ordering::Release);
        st.idle.push(Arc::clone(&self.slot));
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
        let Some(job) = self.slot.take() else {
            return;
        };
        let mut st = lock(&self.shared.state);
        if let Some(other) = st.idle.pop() {
            drop(st);
            other.deliver(Some(job));
        } else {
            // Front: it was dispatched before anything still queued. This
            // can exceed `capacity` by at most the one rerouted job.
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
        let d = d.clone();
        std::thread::spawn(move || {
            let w = d.register_worker();
            while let Ok(job) = w.recv(spin) {
                if tx.send((idx, job)).is_err() {
                    break;
                }
            }
        })
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
        const WORKERS: usize = 8;
        const PRODUCERS: u32 = 4;
        const PER: u32 = 5_000;
        let d = Dispatcher::new(16);
        let (tx, rx) = mpsc::channel();
        let workers: Vec<_> = (0..WORKERS)
            .map(|i| {
                let spin = if i % 2 == 0 { Duration::ZERO } else { Duration::from_micros(200) };
                spawn_forwarder(&d, i, spin, tx.clone())
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
            let (_, job) = rx.recv_timeout(LONG).expect("a job went missing");
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
        const POOL: usize = 6;
        const MAX_REQUESTS: usize = 7;
        const TOTAL: u32 = 6_000;

        fn spawn_recycling(
            d: Dispatcher<u32>,
            tx: mpsc::Sender<u32>,
            live: Arc<AtomicUsize>,
            spawned: Arc<AtomicUsize>,
        ) {
            spawned.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let w = d.register_worker();
                let mut served = 0;
                while served < MAX_REQUESTS {
                    let Ok(job) = w.recv(Duration::from_micros(50)) else {
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
                    spawn_recycling(d, tx, live, spawned);
                }
            });
        }

        let d = Dispatcher::new(8);
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(AtomicUsize::new(POOL));
        let spawned = Arc::new(AtomicUsize::new(0));
        for _ in 0..POOL {
            spawn_recycling(d.clone(), tx.clone(), Arc::clone(&live), Arc::clone(&spawned));
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
            let job = rx.recv_timeout(LONG).expect("a job went missing or was stranded");
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
}

//! Versioned key watches — the blocking-wait primitive behind
//! `ephpm_kv_wait()`.
//!
//! # Design
//!
//! Each watched key owns a [`WatchSlot`]: a monotonically increasing
//! per-key version guarded by a `Mutex`, plus a `Condvar` that wakes
//! blocked waiters. Slots are created **lazily on the first wait** for a
//! key and live for the rest of the process — they are intentionally
//! never reclaimed (see *Slot lifecycle* below).
//!
//! # The zero-cost invariant (why per-key slots, not a global sequence)
//!
//! The KV write path is one of ePHPm's hottest code paths (sessions,
//! rate-limit counters, cache writes). This feature must cost nothing
//! for deployments that never call `ephpm_kv_wait()`:
//!
//! - Writes check a single store-level atomic counter of live slots
//!   ([`Store::notify_write`](super::Store)) — one `Acquire` load. While
//!   the counter is zero (no key has ever been waited on), that load is
//!   the *entire* added cost: no map lookup, no lock, no allocation.
//! - Only once a slot exists does a write pay a `DashMap` lookup in the
//!   (tiny) watch registry, and only a write to a *watched* key pays the
//!   version bump + `notify_all`.
//!
//! A store-global write sequence (`AtomicU64` bumped on every write) was
//! rejected: it would put an unconditional `fetch_add` — a contended
//! cache-line write under multi-core load — on every write forever, to
//! serve a feature most deployments don't use.
//!
//! # Version semantics
//!
//! A slot is created with version `1`, which represents "the state of
//! the key at the moment the watch began". Every subsequent write to the
//! key (set / setnx-insert / del / incr / decr / append / lazy-expiry
//! reap / flush) increments the version. Versions therefore only advance
//! for writes made **after** the first wait on that key — which is
//! exactly what the wait protocol needs:
//!
//! 1. `wait(key, 0, _)` — since the fresh slot's version (`1`) already
//!    exceeds `0`, this returns immediately with the current value and
//!    version. This is the race-free "register + snapshot" step: any
//!    write that lands after this call finds the slot and bumps it.
//! 2. `wait(key, v, timeout)` — blocks until the version exceeds `v`
//!    (returning the new value + version) or the timeout expires.
//!
//! Watches observe **string keys only**. Hash-field writes (`hset` /
//! `hdel`) do not bump versions; deleting a hash key via `remove` does
//! (the delete wakes waiters, whose value read then returns "absent").
//! TTL-only changes (`expire` / `persist`) do not bump versions — the
//! value did not change; the eventual expiry reap does.
//!
//! # Slot lifecycle
//!
//! Slots are never removed. Reclaiming a slot would reset its version to
//! `1` on re-creation, and a client still holding a higher version from
//! the previous slot incarnation would block past real writes — a lost
//! wakeup. Keeping slots alive makes versions monotonic for the process
//! lifetime. The expected cardinality is small (one slot per pub/sub-ish
//! topic, e.g. one per SSE channel); each slot costs ~the key string
//! plus two words. An app waiting on unbounded random keys would grow
//! the registry without bound — documented as an anti-pattern in the KV
//! guide.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Per-key watch state: a version counter and the condvar that wakes
/// waiters when it advances.
#[derive(Debug)]
pub(super) struct WatchSlot {
    /// Monotonic per-key version. Starts at 1 ("state at watch start");
    /// incremented once per observed write. Guarded by the mutex so a
    /// bump-then-notify can never slip between a waiter's version check
    /// and its `Condvar` sleep (the classic lost-wakeup race).
    version: Mutex<u64>,
    /// Wakes all waiters after a version bump.
    cond: Condvar,
}

impl WatchSlot {
    /// New slot at version 1 — "current state at watch start".
    pub(super) fn new() -> Self {
        Self { version: Mutex::new(1), cond: Condvar::new() }
    }

    /// Increment the version and wake every waiter.
    pub(super) fn bump(&self) {
        // Poison recovery: a panic in some other waiter's closure can't
        // corrupt a plain u64 — take the guard and proceed.
        let mut v = self.version.lock().unwrap_or_else(PoisonError::into_inner);
        *v = v.saturating_add(1);
        drop(v);
        self.cond.notify_all();
    }

    /// Current version.
    #[cfg(test)]
    pub(super) fn version(&self) -> u64 {
        *self.version.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Block until the version exceeds `last_version` or `timeout`
    /// elapses. Returns `Some(new_version)` on change, `None` on timeout.
    ///
    /// Designed to be called from dedicated worker OS threads or the
    /// tokio `spawn_blocking` pool — never from an async task.
    pub(super) fn wait_past(&self, last_version: u64, timeout: Duration) -> Option<u64> {
        let guard = self.version.lock().unwrap_or_else(PoisonError::into_inner);
        if *guard > last_version {
            return Some(*guard);
        }
        let (guard, _timed_out) = self
            .cond
            .wait_timeout_while(guard, timeout, |v| *v <= last_version)
            .unwrap_or_else(PoisonError::into_inner);
        // Re-check the predicate rather than trusting the timeout flag:
        // a bump can land between the timeout firing and the lock being
        // reacquired, and reporting it is strictly better.
        if *guard > last_version { Some(*guard) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use super::*;

    #[test]
    fn new_slot_starts_at_one() {
        let slot = WatchSlot::new();
        assert_eq!(slot.version(), 1);
    }

    #[test]
    fn wait_past_zero_returns_immediately() {
        let slot = WatchSlot::new();
        let start = Instant::now();
        // Version 1 > 0 — must not block even with a long timeout.
        assert_eq!(slot.wait_past(0, Duration::from_secs(30)), Some(1));
        assert!(start.elapsed() < Duration::from_secs(1), "must not have blocked");
    }

    #[test]
    fn wait_past_current_times_out() {
        let slot = WatchSlot::new();
        let start = Instant::now();
        assert_eq!(slot.wait_past(1, Duration::from_millis(50)), None);
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn bump_wakes_waiter() {
        let slot = Arc::new(WatchSlot::new());
        let waiter = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.wait_past(1, Duration::from_secs(10)))
        };
        // Give the waiter a moment to actually block, then bump.
        thread::sleep(Duration::from_millis(50));
        slot.bump();
        assert_eq!(waiter.join().unwrap(), Some(2));
    }

    #[test]
    fn bump_wakes_all_waiters() {
        let slot = Arc::new(WatchSlot::new());
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let slot = Arc::clone(&slot);
                thread::spawn(move || slot.wait_past(1, Duration::from_secs(10)))
            })
            .collect();
        thread::sleep(Duration::from_millis(50));
        slot.bump();
        for w in waiters {
            assert_eq!(w.join().unwrap(), Some(2));
        }
    }
}

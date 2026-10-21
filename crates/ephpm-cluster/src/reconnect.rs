//! Jittered exponential backoff for cluster-channel reconnect loops.
//!
//! # Why this exists
//!
//! Every replica-side CDC driver (one per replicated site in per-site
//! clustered mode) used to retry a failed stream after a **fixed** 2 s. A
//! transient owner stall fails a whole cohort of streams in the same instant,
//! so the cohort re-dialed in lockstep two seconds later — a few hundred
//! simultaneous handshakes against an owner whose listener runs at most 64 at
//! a time and *drops* the rest. Every dropped dial was itself a stream error,
//! which scheduled the next lockstep retry: a self-sustaining reconnect storm
//! that persisted after the triggering load had stopped (stress report F1:
//! 5,432 dropped handshakes, 1,934 of them in one wall-second).
//!
//! Two properties break that loop, and both are needed:
//!
//! * **Jitter** spreads a cohort that failed together over a window instead of
//!   a single instant, so the owner sees a trickle of handshakes it can absorb.
//! * **Exponential growth** on *consecutive* failures turns "the owner is
//!   refusing me" into backpressure: a stream that keeps failing backs off
//!   towards the cap rather than hammering at a fixed rate.
//!
//! A stream that stayed up for a while is a success, not another failure in a
//! run: [`ReconnectBackoff::record_attempt`] resets the growth once an attempt
//! lasted [`ReconnectBackoff::HEALTHY_AFTER`], so a long-lived subscription
//! that finally breaks retries promptly.
//!
//! The constants are deliberately not configurable: they are internal pacing,
//! not an operator decision.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Exponential reconnect backoff with ±50 % jitter.
///
/// The nominal delay doubles per consecutive failure from `base` up to `cap`;
/// the delay actually returned is the nominal one scaled by a uniform factor in
/// `[0.5, 1.5)`, so the longest possible wait is `1.5 × cap`.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    base: Duration,
    cap: Duration,
    /// Consecutive failures since the last reset.
    failures: u32,
}

impl ReconnectBackoff {
    /// Nominal first retry delay for a replica's CDC/snapshot stream (the
    /// fixed delay the drivers used before backoff existed).
    pub const REPLICA_BASE: Duration = Duration::from_secs(2);

    /// Nominal ceiling for a replica stream's retry delay. With the jitter the
    /// longest single wait is 30 s — still well inside the gossip
    /// failure-detection window a role change would interrupt it with anyway.
    pub const REPLICA_CAP: Duration = Duration::from_secs(20);

    /// An attempt that lasted at least this long counts as a success: the
    /// next failure starts again from `base` instead of continuing the run.
    pub const HEALTHY_AFTER: Duration = Duration::from_secs(30);

    /// A backoff doubling from `base` to at most `cap`.
    #[must_use]
    pub const fn new(base: Duration, cap: Duration) -> Self {
        Self { base, cap, failures: 0 }
    }

    /// The pacing every replica-side reconnect loop uses.
    #[must_use]
    pub const fn replica_stream() -> Self {
        Self::new(Self::REPLICA_BASE, Self::REPLICA_CAP)
    }

    /// The nominal (un-jittered) delay for the next retry.
    #[must_use]
    pub fn nominal(&self) -> Duration {
        // 2^16 × base is far past any sane cap; clamping the exponent keeps
        // the multiplication from overflowing on a very long failure run.
        let factor = 1u32 << self.failures.min(16);
        self.base.saturating_mul(factor).min(self.cap)
    }

    /// Record one more consecutive failure and return how long to wait before
    /// the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let nominal = self.nominal();
        self.failures = self.failures.saturating_add(1);
        jittered(nominal)
    }

    /// Forget the failure run: the next delay starts again from `base`.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// Account for an attempt that ran for `ran_for` before it ended: an
    /// attempt that stayed up for [`Self::HEALTHY_AFTER`] resets the run.
    pub fn record_attempt(&mut self, ran_for: Duration) {
        if ran_for >= Self::HEALTHY_AFTER {
            self.reset();
        }
    }

    /// Consecutive failures recorded since the last reset.
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

/// `nominal` scaled by a uniform random factor in `[0.5, 1.5)`.
///
/// For retry loops whose *count* is bounded (a fixed attempt budget) and must
/// keep its total duration, but should still not fire in lockstep with every
/// other loop that failed at the same moment.
#[must_use]
pub fn jittered(nominal: Duration) -> Duration {
    nominal.mul_f64(0.5 + unit_random())
}

/// A uniform value in `[0, 1)`.
///
/// Jitter only has to decorrelate retry loops inside one process and across
/// nodes; it is not security-relevant, so std's per-process randomly keyed
/// SipHash over a counter is plenty and needs no RNG dependency.
fn unit_random() -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let high = u32::try_from(hasher.finish() >> 32).unwrap_or(0);
    f64::from(high) / 4_294_967_296.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn within(d: Duration, nominal: Duration) -> bool {
        d >= nominal.mul_f64(0.5) && d < nominal.mul_f64(1.5)
    }

    #[test]
    fn delay_doubles_per_failure_up_to_the_cap() {
        let mut b = ReconnectBackoff::new(Duration::from_secs(2), Duration::from_secs(20));
        let expected = [2u64, 4, 8, 16, 20, 20, 20];
        for secs in expected {
            let nominal = Duration::from_secs(secs);
            assert_eq!(b.nominal(), nominal);
            let d = b.next_delay();
            assert!(within(d, nominal), "{d:?} outside [0.5, 1.5) x {nominal:?}");
        }
        assert_eq!(b.failures(), 7);
    }

    #[test]
    fn a_long_failure_run_never_overflows() {
        let mut b = ReconnectBackoff::replica_stream();
        for _ in 0..10_000 {
            let d = b.next_delay();
            assert!(d < ReconnectBackoff::REPLICA_CAP.mul_f64(1.5));
        }
    }

    #[test]
    fn reset_and_healthy_attempts_restart_from_base() {
        let mut b = ReconnectBackoff::replica_stream();
        for _ in 0..5 {
            b.next_delay();
        }
        b.record_attempt(Duration::from_secs(1));
        assert_eq!(b.nominal(), ReconnectBackoff::REPLICA_CAP, "a short attempt keeps the run");
        b.record_attempt(ReconnectBackoff::HEALTHY_AFTER);
        assert_eq!(b.nominal(), ReconnectBackoff::REPLICA_BASE, "a healthy attempt resets it");
        b.next_delay();
        b.reset();
        assert_eq!(b.failures(), 0);
    }

    /// The property that breaks the F1 storm: a cohort of streams that failed
    /// in the same instant must not all retry in the same instant. Four
    /// hundred drivers (200 sites x 2 replicas, the stress run's shape) are
    /// spread over most of the jitter window.
    #[test]
    fn a_cohort_that_failed_together_does_not_retry_together() {
        let delays: Vec<Duration> =
            (0..400).map(|_| ReconnectBackoff::replica_stream().next_delay()).collect();
        let min = delays.iter().min().copied().unwrap();
        let max = delays.iter().max().copied().unwrap();
        assert!(
            max - min > Duration::from_millis(1_500),
            "first retries must spread over the [1 s, 3 s) window, got {min:?}..{max:?}"
        );
        // And no single 100 ms bucket holds a large fraction of the cohort
        // (uniform: ~20 per bucket over 20 buckets).
        let mut buckets = [0usize; 20];
        for d in &delays {
            let ms = d.as_millis().saturating_sub(1_000) / 100;
            buckets[usize::try_from(ms).unwrap_or(19).min(19)] += 1;
        }
        assert!(
            buckets.iter().all(|&n| n < 80),
            "a 100 ms window held too many simultaneous retries: {buckets:?}"
        );
    }

    #[test]
    fn jittered_stays_in_range() {
        for _ in 0..1_000 {
            let d = jittered(Duration::from_secs(2));
            assert!(within(d, Duration::from_secs(2)), "{d:?}");
        }
    }
}

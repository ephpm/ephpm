//! OPcache JIT buffer observability.
//!
//! Exports the JIT code-buffer size and free space as Prometheus gauges so an
//! operator can see buffer exhaustion — the JIT's only failure mode that is
//! otherwise *completely silent*: when `opcache.jit_buffer_size` runs out the
//! engine simply stops compiling new traces, with no error, no log line, and
//! no status flag. This matters most in multi-tenant mode with the JIT forced
//! on, because per-vhost `opcache_invalidate` never returns JIT buffer to the
//! free pool (measured: `buffer_free` is untouched by invalidation; only a
//! full `opcache_reset` reclaims it).
//!
//! # Sampling design
//!
//! `opcache_get_status()` is a userland-facing call that needs a
//! TSRM-registered thread with an active PHP request context — there is no
//! background thread that can legally make it. Instead of dedicating (and
//! TSRM-registering) a thread just for a gauge, sampling **piggybacks on the
//! request path**: the router's PHP dispatch closure and the worker-mode
//! `take_request` loop call [`maybe_sample`], which is a single relaxed atomic
//! load on the hot path and performs the actual FFI status call at most once
//! per [`SAMPLE_INTERVAL_MS`] process-wide. Consequences, deliberately
//! accepted:
//!
//! - With zero PHP traffic the gauges go stale (they keep the last sampled
//!   value) — but with zero traffic the JIT state cannot change either.
//! - The gauges appear in `/metrics` only after the first successful sample,
//!   i.e. only when PHP is linked, initialized, and OPcache is actually
//!   loaded. Stub builds and `opcache.enable=0` runs never record them — no
//!   phantom metrics.
//!
//! The gauges are recorded whether the JIT is on or off (`buffer_size` is `0`
//! when no buffer is configured), so "JIT off" is visible as an honest zero
//! rather than an absent series once PHP traffic has flowed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Gauge: `opcache.jit_buffer_size` in bytes (0 = no JIT buffer configured).
pub const METRIC_JIT_BUFFER_SIZE: &str = "ephpm_opcache_jit_buffer_size_bytes";
/// Gauge: free bytes remaining in the JIT code buffer. A value trending to 0
/// means the JIT is about to silently stop compiling new code.
pub const METRIC_JIT_BUFFER_FREE: &str = "ephpm_opcache_jit_buffer_free_bytes";

/// Minimum interval between two FFI status calls, milliseconds. 10s keeps the
/// gauge fresh on Prometheus scrape timescales while costing at most one
/// `opcache_get_status(false)` (no per-script table — cheap) per interval.
pub const SAMPLE_INTERVAL_MS: u64 = 10_000;

/// Epoch-milliseconds of the last *started* sample, process-wide. `0` = never.
static LAST_SAMPLE_MS: AtomicU64 = AtomicU64::new(0);

/// Milliseconds since the Unix epoch (0 on a pre-1970 clock).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Sample the JIT buffer gauges if the sampling interval has elapsed.
///
/// Must be called where [`crate::PhpRuntime::opcache_jit_stats`] is legal: on
/// a TSRM-registered thread with an active request context (the router's PHP
/// dispatch closure; a worker thread inside its long-lived request). The hot
/// path is one relaxed atomic load; the winner of the compare-exchange makes
/// the FFI call, so concurrent request threads never sample twice.
pub fn maybe_sample() {
    let now = now_ms();
    let last = LAST_SAMPLE_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SAMPLE_INTERVAL_MS {
        return;
    }
    // One winner per interval; losers skip (a sample is already in flight or
    // just done). The timestamp advances even if the FFI call then fails, so
    // an OPcache-less build retries at most once per interval, not per request.
    if LAST_SAMPLE_MS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    if let Some((size, free)) = crate::PhpRuntime::opcache_jit_stats() {
        record(size, free);
    }
}

/// Record the two gauges. Split out so the metric-emission contract is
/// testable without libphp.
fn record(buffer_size: u64, buffer_free: u64) {
    // Prometheus gauges are f64; JIT buffers are ≤ a few hundred MB, far
    // inside f64's exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    {
        metrics::gauge!(METRIC_JIT_BUFFER_SIZE).set(buffer_size as f64);
        metrics::gauge!(METRIC_JIT_BUFFER_FREE).set(buffer_free as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_gate_admits_at_most_one_sampler_per_window() {
        // Reset the process-wide gate for this test.
        LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
        let now = now_ms();
        assert!(now > 0);

        // First caller wins the exchange…
        let last = LAST_SAMPLE_MS.load(Ordering::Relaxed);
        assert!(now.saturating_sub(last) >= SAMPLE_INTERVAL_MS);
        assert!(
            LAST_SAMPLE_MS
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        );
        // …and an immediate second attempt is inside the window.
        let last = LAST_SAMPLE_MS.load(Ordering::Relaxed);
        assert!(now_ms().saturating_sub(last) < SAMPLE_INTERVAL_MS);
    }

    #[test]
    fn maybe_sample_is_a_noop_in_stub_mode() {
        // In stub mode `opcache_jit_stats` is `None`, so this must record
        // nothing (no phantom metrics) and must not panic without a recorder.
        LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
        maybe_sample();
    }

    #[test]
    fn record_sets_both_gauges() {
        // With the default no-op recorder installed this only proves the call
        // does not panic; the metric NAMES are pinned by the constants, which
        // the reference docs quote.
        record(64 * 1024 * 1024, 12 * 1024 * 1024);
        assert_eq!(METRIC_JIT_BUFFER_SIZE, "ephpm_opcache_jit_buffer_size_bytes");
        assert_eq!(METRIC_JIT_BUFFER_FREE, "ephpm_opcache_jit_buffer_free_bytes");
    }
}

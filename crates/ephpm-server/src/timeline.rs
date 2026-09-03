//! Per-request timeline ring buffer (`/_ephpm/requests`).
//!
//! Dev-mode waterfall diagnostics: the router records one [`TimelineEntry`]
//! per completed request into a fixed-size ring buffer, and the
//! `/_ephpm/requests` endpoint serves the buffer as JSON, newest first.
//!
//! The entries reuse the measurements the router already takes for its
//! Prometheus histograms — the total-request `Instant` in `Router::handle`,
//! the worker-queue wait, and the PHP execution timer. Nothing here measures
//! time itself; the dispatch paths hand their existing values over via a
//! [`PhpTimings`] response extension.
//!
//! Concurrency: a plain `Mutex<VecDeque>` — the buffer is only enabled in
//! dev mode (or explicitly via `[server.diagnostics] request_log`), where
//! request rates are trivially low, and the critical section is a push/pop
//! pair.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Number of entries the request timeline retains.
pub(crate) const REQUEST_LOG_CAPACITY: usize = 256;

/// PHP-path timings carried from the dispatch paths to `Router::handle` via
/// response extensions, so the ring buffer consumes the same measurement
/// points as the existing histograms instead of re-measuring.
///
/// `queue_wait` holds the dispatch-queue wait on both the per-request pool
/// and the worker pool; it is `None` only for requests that never reached a
/// dispatch queue (e.g. the defensive no-pool arm).
/// `execute` is `None` when the request never produced a completed PHP
/// execution measurement (worker bailout, worker timeout, dispatch failure).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PhpTimings {
    /// Dispatch-queue wait (`ephpm_worker_request_wait_seconds` in worker
    /// mode; measured at the pool dispatch site in per-request mode).
    pub queue_wait: Option<Duration>,
    /// PHP execution timer (`ephpm_php_execution_duration_seconds`). In
    /// worker mode this timer starts at dispatch, so it includes the queue
    /// wait — identical to the histogram's semantics.
    pub execute: Option<Duration>,
}

/// One completed request, as served by `/_ephpm/requests`.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct TimelineEntry {
    /// Completion time, milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Request method as sent by the client.
    pub method: String,
    /// Percent-encoded request path as sent by the client (no query string).
    pub path: String,
    /// Response status code.
    pub status: u16,
    /// Total request duration in milliseconds (the same measurement as
    /// `ephpm_http_request_duration_seconds`).
    pub total_ms: f64,
    /// Worker-mode dispatch-queue wait in milliseconds. `null` for requests
    /// that never waited in a worker queue (static files, fpm-mode PHP).
    pub queue_wait_ms: Option<f64>,
    /// PHP execution time in milliseconds (in worker mode this includes the
    /// queue wait, matching `ephpm_php_execution_duration_seconds`). `null`
    /// for non-PHP requests and for PHP requests that never completed
    /// execution (bailout, timeout).
    pub php_ms: Option<f64>,
    /// Response body size in bytes, when known up front. `null` for
    /// streaming responses whose size is unknown at header time.
    pub response_bytes: Option<u64>,
}

/// Fixed-size ring buffer of the most recent [`TimelineEntry`] values.
pub(crate) struct RequestLog {
    capacity: usize,
    entries: Mutex<VecDeque<TimelineEntry>>,
}

impl RequestLog {
    /// Create a ring buffer retaining at most `capacity` entries.
    pub(crate) fn new(capacity: usize) -> Self {
        Self { capacity, entries: Mutex::new(VecDeque::with_capacity(capacity)) }
    }

    /// Record a completed request, evicting the oldest entry at capacity.
    pub(crate) fn record(&self, entry: TimelineEntry) {
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.len() == self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Serialize the buffer as a JSON array, newest entry first.
    pub(crate) fn to_json(&self) -> String {
        let snapshot: Vec<TimelineEntry> = {
            let entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.iter().rev().cloned().collect()
        };
        // TimelineEntry contains no map keys or non-string keys, so
        // serialization cannot fail; fall back to an empty array anyway
        // rather than panicking on a diagnostics path.
        serde_json::to_string(&snapshot).unwrap_or_else(|_| "[]".to_string())
    }
}

/// Milliseconds since the Unix epoch, saturating at zero for a pre-epoch
/// clock (never happens on a sane system, but must not panic).
pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> TimelineEntry {
        TimelineEntry {
            timestamp_ms: unix_now_ms(),
            method: "GET".to_string(),
            path: path.to_string(),
            status: 200,
            total_ms: 1.0,
            queue_wait_ms: None,
            php_ms: None,
            response_bytes: Some(1),
        }
    }

    /// At capacity the oldest entry is evicted — the buffer never grows past
    /// its capacity and always retains the most recent entries.
    #[test]
    fn ring_buffer_evicts_oldest_at_capacity() {
        let log = RequestLog::new(3);
        for i in 0..5 {
            log.record(entry(&format!("/{i}")));
        }
        let json = log.to_json();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 3, "capacity must bound the buffer: {json}");
        let paths: Vec<&str> = parsed.iter().map(|e| e["path"].as_str().unwrap()).collect();
        assert_eq!(paths, vec!["/4", "/3", "/2"], "newest first, oldest evicted");
    }

    /// Absent measurements serialize as `null`, not zero — a request with no
    /// worker queue must be distinguishable from one that waited 0ms.
    #[test]
    fn absent_timings_serialize_as_null() {
        let log = RequestLog::new(4);
        log.record(entry("/static.txt"));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&log.to_json()).unwrap();
        assert!(parsed[0]["queue_wait_ms"].is_null());
        assert!(parsed[0]["php_ms"].is_null());
        assert_eq!(parsed[0]["response_bytes"], 1);
    }
}

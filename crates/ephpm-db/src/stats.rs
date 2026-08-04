//! Query-stats recording for the wire proxies.
//!
//! The DB proxy forwards raw wire-protocol bytes between PHP and a real
//! MySQL/PostgreSQL server. It never materialises a `litewire` backend, so
//! it cannot reuse `ephpm-server`'s `TrackedBackend` decorator — the only
//! place a statement is visible is the point where the proxy already parses
//! the client's command in order to classify it for read/write splitting.
//! This module holds the small amount of shared machinery those tap points
//! need, and documents how the numbers they produce differ from the ones
//! `TrackedBackend` produces.
//!
//! # Timing semantics vs `TrackedBackend`
//!
//! `TrackedBackend` wraps an **in-process** litewire backend: the duration it
//! records is the time spent inside SQLite (plus litewire's translation),
//! with no network in the measurement at all.
//!
//! The proxy records a **wire round trip**: from the moment the client's
//! command has been written to the pooled backend socket, to the moment the
//! last byte of that command's response has been read back from it. That
//! includes the network path to the database server, the server's own
//! execution time, and result-set transfer — but *not* time the client spends
//! thinking between statements, and *not* the time the proxy spends waiting
//! for a pooled connection (the checkout happens before the clock starts).
//!
//! Both feed the same [`QueryStats`] instance and therefore the same
//! Prometheus series. A deployment that runs the proxy and the embedded
//! SQLite path simultaneously will see the two mixed under one metric name;
//! that is deliberate (one metrics surface, no new label dimension), and is
//! called out in the metrics reference.
//!
//! # What is *not* recorded
//!
//! Recording only happens on code paths where the proxy already frames the
//! protocol. Anything it forwards as an opaque byte splice is invisible to
//! it, and is left out rather than estimated:
//!
//! - MySQL `COM_STMT_EXECUTE` on the single-backend (non R/W-split) path.
//!   That path never parses the backend→client direction, so it never sees
//!   the statement ID that `COM_STMT_PREPARE` returned and cannot map an
//!   execute back to SQL text.
//! - MySQL `COM_STMT_PREPARE` itself, on every path. Preparing is a metadata
//!   operation, not an execution; recording its round trip under the same
//!   digest as the statement would report parse latency as query latency.
//!   This matches `TrackedBackend`, which deliberately passes
//!   `describe_columns` through unrecorded for the same reason.
//! - PostgreSQL extended-query-protocol traffic (`Parse`/`Bind`/`Execute`)
//!   and both copy sub-protocols. Those pin the session to one backend and
//!   splice it verbatim, because message boundaries stop lining up with turn
//!   boundaries.
//! - Every statement on a session using `reset_strategy = "never"` or
//!   `"always"` *without* R/W splitting, which is spliced end to end.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ephpm_query_stats::QueryStats;

/// What the proxy managed to observe about one statement's response.
///
/// Fields are only ever populated from bytes the proxy actually parsed; a
/// field the wire path cannot attribute stays at its neutral value rather
/// than being guessed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResponseOutcome {
    /// `false` when the response carried a protocol-level error packet.
    pub ok: bool,
    /// Rows returned, or rows affected for a mutation. `0` when the path
    /// cannot count them.
    pub rows: u64,
}

impl ResponseOutcome {
    /// A successful response whose row count could not be attributed.
    pub(crate) const fn ok_unknown_rows() -> Self {
        Self { ok: true, rows: 0 }
    }
}

/// Cross-task observation point for the MySQL single-backend splice path.
///
/// That path runs two concurrent halves: a packet-framed client→backend
/// loop (which knows the SQL and when it was forwarded) and a bulk
/// backend→client copy (which knows when response bytes arrived, but not
/// what they mean). This type is the only thing they share.
///
/// The completion rule exploits the MySQL client/server protocol being
/// strictly synchronous: a client may not send command *N+1* until it has
/// received the whole response to command *N*. So the arrival of the next
/// client command proves the previous response is complete, and the most
/// recent backend read timestamp at that moment is the timestamp of its
/// last byte. Statements are therefore recorded one command late, and the
/// final statement of a session is recorded when the client disconnects.
///
/// A client that pipelines commands (nothing in the PHP ecosystem does —
/// neither `mysqlnd` nor `libmysqlclient` will) would have its timings
/// mis-attributed by this rule; it cannot desynchronise the proxy itself,
/// since no forwarding decision depends on any of this state.
pub(crate) struct SpliceWatch {
    /// Reference point for the nanosecond stamps below.
    epoch: Instant,
    /// Nanoseconds-since-`epoch` of the most recent non-empty backend read.
    last_backend_ns: AtomicU64,
    /// Set when a statement has been forwarded and its first response chunk
    /// has not been inspected yet.
    awaiting_first_chunk: AtomicBool,
    /// Set when the first response chunk began with a MySQL `ERR_Packet`.
    response_error: AtomicBool,
}

/// A statement forwarded to the backend whose response has not been
/// attributed yet.
pub(crate) struct PendingStatement {
    /// The SQL text as sent by the client.
    pub sql: String,
    /// Nanoseconds-since-epoch at which the command finished being written
    /// to the backend socket.
    pub sent_ns: u64,
}

impl SpliceWatch {
    /// Create a watch anchored at the current instant.
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_backend_ns: AtomicU64::new(0),
            awaiting_first_chunk: AtomicBool::new(false),
            response_error: AtomicBool::new(false),
        }
    }

    /// Nanoseconds elapsed since this watch was created.
    pub(crate) fn now_ns(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Arm the watch for the response to a statement just forwarded.
    pub(crate) fn arm(&self) {
        self.response_error.store(false, Ordering::Relaxed);
        self.awaiting_first_chunk.store(true, Ordering::Relaxed);
    }

    /// Record that `chunk` bytes just arrived from the backend.
    ///
    /// The first chunk after [`Self::arm`] starts at a packet boundary (the
    /// previous response ended on one), so byte 4 is the first payload byte
    /// of the response's first packet — `0xFF` marks a MySQL `ERR_Packet`.
    /// A first chunk shorter than five bytes leaves the status at "ok"; a
    /// real server writes the whole packet in one `write`, so this is a
    /// theoretical rather than an observed case.
    pub(crate) fn note_backend_chunk(&self, chunk: &[u8]) {
        self.last_backend_ns.store(self.now_ns(), Ordering::Relaxed);
        if self.awaiting_first_chunk.swap(false, Ordering::Relaxed)
            && chunk.len() >= 5
            && chunk[4] == 0xFF
        {
            self.response_error.store(true, Ordering::Relaxed);
        }
    }

    /// Resolve a pending statement against the bytes seen since it was sent.
    ///
    /// Returns `None` when no backend bytes have arrived since the statement
    /// was forwarded — the response cannot be attributed, so nothing is
    /// recorded rather than recording a fabricated duration.
    pub(crate) fn settle(&self, pending: &PendingStatement) -> Option<(Duration, bool)> {
        let last = self.last_backend_ns.load(Ordering::Relaxed);
        if last <= pending.sent_ns {
            return None;
        }
        let elapsed = Duration::from_nanos(last - pending.sent_ns);
        Some((elapsed, !self.response_error.load(Ordering::Relaxed)))
    }

    /// Settle `pending` and record it into `stats` if it can be attributed.
    pub(crate) fn flush(&self, stats: &QueryStats, pending: &PendingStatement) {
        if let Some((elapsed, ok)) = self.settle(pending) {
            // Row counts are unavailable on this path — see the module docs.
            stats.record(&pending.sql, elapsed, ok, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use ephpm_query_stats::StatsConfig;

    use super::*;

    fn stats() -> QueryStats {
        QueryStats::new(StatsConfig::default())
    }

    #[test]
    fn settle_returns_none_without_backend_bytes() {
        let watch = SpliceWatch::new();
        watch.arm();
        let pending = PendingStatement { sql: "SELECT 1".into(), sent_ns: watch.now_ns() };
        assert!(
            watch.settle(&pending).is_none(),
            "a statement with no observed response must not be recorded"
        );
    }

    #[test]
    fn settle_measures_from_send_to_last_backend_byte() {
        let watch = SpliceWatch::new();
        watch.arm();
        let pending = PendingStatement { sql: "SELECT 1".into(), sent_ns: 0 };
        watch.note_backend_chunk(&[0, 0, 0, 1, 0x00]);
        let (elapsed, ok) = watch.settle(&pending).expect("response observed");
        assert!(ok, "an OK first packet must not be counted as an error");
        assert!(elapsed > Duration::ZERO);
    }

    #[test]
    fn err_packet_in_first_chunk_marks_failure() {
        let watch = SpliceWatch::new();
        watch.arm();
        let pending = PendingStatement { sql: "SELECT bad".into(), sent_ns: 0 };
        // [len:3][seq:1][0xFF ...] — a MySQL ERR_Packet.
        watch.note_backend_chunk(&[0x10, 0, 0, 1, 0xFF, 0x48, 0x04]);
        let (_, ok) = watch.settle(&pending).expect("response observed");
        assert!(!ok, "an ERR_Packet must be recorded as an error");
    }

    #[test]
    fn only_the_first_chunk_is_inspected_for_errors() {
        // A row packet whose payload happens to start with 0xFF must not be
        // mistaken for an error once the first chunk has been consumed.
        let watch = SpliceWatch::new();
        watch.arm();
        let pending = PendingStatement { sql: "SELECT 1".into(), sent_ns: 0 };
        watch.note_backend_chunk(&[0x01, 0, 0, 1, 0x01]);
        watch.note_backend_chunk(&[0x02, 0, 0, 2, 0xFF, 0xFF]);
        let (_, ok) = watch.settle(&pending).expect("response observed");
        assert!(ok, "later chunks must not flip the status");
    }

    #[test]
    fn flush_records_into_stats() {
        let stats = stats();
        let watch = SpliceWatch::new();
        watch.arm();
        let pending =
            PendingStatement { sql: "SELECT * FROM users WHERE id = 7".into(), sent_ns: 0 };
        watch.note_backend_chunk(&[0x01, 0, 0, 1, 0x01]);
        watch.flush(&stats, &pending);

        assert_eq!(stats.digest_count(), 1);
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert!(top[0].digest_text.contains('?'), "literals must be normalized away");
        assert_eq!(top[0].total_rows, 0, "the splice path must not invent a row count");
    }

    #[test]
    fn flush_is_a_no_op_when_the_response_was_never_seen() {
        let stats = stats();
        let watch = SpliceWatch::new();
        watch.arm();
        let pending = PendingStatement { sql: "SELECT 1".into(), sent_ns: watch.now_ns() };
        watch.flush(&stats, &pending);
        assert_eq!(stats.digest_count(), 0);
    }

    #[test]
    fn disabled_stats_record_nothing_through_flush() {
        let stats = QueryStats::new(StatsConfig { enabled: false, ..Default::default() });
        let watch = SpliceWatch::new();
        watch.arm();
        let pending = PendingStatement { sql: "SELECT 1".into(), sent_ns: 0 };
        watch.note_backend_chunk(&[0x01, 0, 0, 1, 0x01]);
        watch.flush(&stats, &pending);
        assert_eq!(stats.digest_count(), 0, "the toggle must reach the proxy tap point");
    }

    #[test]
    fn ok_unknown_rows_is_neutral() {
        let outcome = ResponseOutcome::ok_unknown_rows();
        assert!(outcome.ok);
        assert_eq!(outcome.rows, 0);
    }
}

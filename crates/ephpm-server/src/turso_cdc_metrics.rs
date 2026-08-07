//! Prometheus instrumentation for the experimental CDC-native Turso
//! replication path ([`crate::turso_cdc`]).
//!
//! Every series here is emitted **only** when
//! `[db.sqlite.replication] cdc_experimental = true` actually starts the
//! CDC path — [`init`] is called from `start_clustered_turso_cdc` and
//! nothing else registers these names. A deployment on sqld or
//! single-node SQLite has no `ephpm_cdc_*` series at all.
//!
//! # The lag metric, precisely
//!
//! [`METRIC_LAG_CHANGES`] is the headline number and it is a **row
//! count, not a duration**. It counts `turso_cdc` change-log rows on the
//! primary that have not yet been shipped to the slowest attached
//! subscriber:
//!
//! ```text
//! ephpm_cdc_replication_lag_changes
//!   = ephpm_cdc_primary_head_change_id   (MAX(turso_cdc.change_id) here)
//!   - ephpm_cdc_shipped_change_id        (min cursor over subscribers)
//! ```
//!
//! Both inputs are `change_id` values, which turso allocates one per
//! captured row change. So "lag = 500" means *five hundred row changes*
//! behind, and says nothing about seconds — 500 changes is
//! sub-millisecond on an idle cluster and minutes behind a bulk import.
//! Do not graph it with a seconds unit.
//!
//! ## Why there is no time-based lag
//!
//! A seconds-valued lag needs a commit timestamp travelling with the
//! change so the consumer can subtract it from its own clock. The
//! `turso_cdc` table does carry one (`change_time`, column 2 of the v2
//! schema), but litewire's `CdcRow` does not expose it — the struct is
//! `(change_id, change_txn_id, change_type, table_name, id, before,
//! after, updates)`. Surfacing a time-based lag therefore needs **two**
//! upstream changes, not an ephpm-side calculation:
//!
//! 1. a litewire change adding `change_time` to `CdcRow` (plus a pin
//!    bump here), and
//! 2. a wire change adding the field to `turso_cdc`'s `WireCdcRow`.
//!
//! Until both land, any "seconds behind" figure this crate published
//! would be invented. The row-count lag above is what the data actually
//! supports, so it is what ships.
//!
//! ## What the lag does and does not cover
//!
//! It is measured at the **ship** boundary — the moment a batch is
//! written into the subscriber's yamux stream — not at the apply
//! boundary on the replica. It therefore excludes network flight time
//! and the replica's `apply_batch` cost. For true end-to-end lag,
//! subtract across nodes in PromQL:
//!
//! ```promql
//! max(ephpm_cdc_primary_head_change_id) - min(ephpm_cdc_applied_change_id)
//! ```
//!
//! ## Behaviour with no subscribers
//!
//! [`METRIC_SUBSCRIBERS`] going to `0` is the unambiguous "replication
//! is not happening" signal and is what to alert on. The lag gauge stays
//! *live* in that state rather than freezing: the shipped cursor retains
//! the last position any subscriber reached, so a primary that keeps
//! taking writes after its replica dies shows a genuinely growing lag.
//! Before the first subscriber ever attaches there is no cursor to
//! subtract from, and the lag and shipped gauges are simply not emitted.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use dashmap::DashMap;
use metrics::{counter, gauge, histogram};

// ---------------------------------------------------------------------------
// Metric names. Exported so integration tests assert against the same
// strings the recorder emits, and so `site/content/reference/metrics.md`
// has one authoritative source to be checked against.
// ---------------------------------------------------------------------------

/// Gauge: CDC subscriber streams currently attached to this node.
pub const METRIC_SUBSCRIBERS: &str = "ephpm_cdc_subscribers";
/// Counter: committed transaction batches written to subscriber streams.
pub const METRIC_BATCHES_SHIPPED: &str = "ephpm_cdc_batches_shipped_total";
/// Counter: CDC rows contained in those shipped batches.
pub const METRIC_ROWS_SHIPPED: &str = "ephpm_cdc_rows_shipped_total";
/// Gauge: lowest `change_id` shipped across all attached subscribers.
pub const METRIC_SHIPPED_CHANGE_ID: &str = "ephpm_cdc_shipped_change_id";
/// Gauge: `MAX(turso_cdc.change_id)` observed on this node.
pub const METRIC_HEAD_CHANGE_ID: &str = "ephpm_cdc_primary_head_change_id";
/// Gauge: head minus shipped cursor, in change-log rows. See the module
/// docs — this is **not** seconds.
pub const METRIC_LAG_CHANGES: &str = "ephpm_cdc_replication_lag_changes";
/// Counter: `turso_cdc` tail-poll failures on the primary.
pub const METRIC_TAIL_POLL_ERRORS: &str = "ephpm_cdc_tail_poll_errors_total";
/// Counter: inbound streams refused because this node is not the primary.
/// Label: `stream` (`cdc` / `snapshot`).
pub const METRIC_STREAMS_REFUSED: &str = "ephpm_cdc_streams_refused_total";
/// Counter: snapshot bootstraps served. Label: `status` (`ok` / `error`).
pub const METRIC_SNAPSHOTS_SERVED: &str = "ephpm_cdc_snapshots_served_total";
/// Counter: snapshot dump bytes written to dialing replicas.
pub const METRIC_SNAPSHOT_BYTES_SERVED: &str = "ephpm_cdc_snapshot_bytes_served_total";
/// Histogram: snapshot transfer duration. Label: `role` (`serve` / `fetch`).
pub const METRIC_SNAPSHOT_DURATION: &str = "ephpm_cdc_snapshot_duration_seconds";

/// Counter: CDC batches applied to the local database.
pub const METRIC_BATCHES_APPLIED: &str = "ephpm_cdc_batches_applied_total";
/// Counter: CDC rows applied to the local database.
pub const METRIC_ROWS_APPLIED: &str = "ephpm_cdc_rows_applied_total";
/// Gauge: this replica's applied watermark (`change_id`).
pub const METRIC_APPLIED_CHANGE_ID: &str = "ephpm_cdc_applied_change_id";
/// Counter: `apply_batch` failures on the replica.
pub const METRIC_APPLY_ERRORS: &str = "ephpm_cdc_apply_errors_total";
/// Histogram: per-batch `apply_batch` duration on the replica.
pub const METRIC_APPLY_DURATION: &str = "ephpm_cdc_apply_duration_seconds";
/// Counter: replica subscribe attempts by terminal outcome. Label:
/// `outcome` (`closed` / `dial_error` / `stream_error` / `watermark_error`).
pub const METRIC_REPLICA_CONNECTS: &str = "ephpm_cdc_replica_connects_total";
/// Counter: cold-start snapshot bootstrap outcomes. Label: `outcome`
/// (`ok` / `skipped` / `failed`).
pub const METRIC_BOOTSTRAP: &str = "ephpm_cdc_bootstrap_total";
/// Counter: snapshot dump bytes received during bootstrap.
pub const METRIC_SNAPSHOT_BYTES_RECEIVED: &str = "ephpm_cdc_snapshot_bytes_received_total";

// ---------------------------------------------------------------------------
// Process-global subscriber registry.
//
// A process runs at most one CDC role at a time, and every gauge above is
// unlabelled and process-wide, so the bookkeeping behind them is global
// too. Making it global keeps `serve_subscriber`'s signature free of a
// registry parameter that only exists to be threaded through.
// ---------------------------------------------------------------------------

static REGISTRY: LazyLock<Arc<SubscriberRegistry>> =
    LazyLock::new(|| Arc::new(SubscriberRegistry::new()));

/// Primary-side bookkeeping behind the shipped/head/lag gauges.
///
/// `Relaxed` ordering throughout: every field feeds a gauge that is
/// sampled by a scrape seconds later, so a reader that observes a value
/// a few microseconds stale around a concurrent update publishes a
/// number that was true a moment earlier. Nothing here guards memory
/// another thread must see, so stronger ordering would only cost
/// fences.
struct SubscriberRegistry {
    /// Live subscribers: opaque id → the `change_id` shipped to it.
    cursors: DashMap<u64, i64>,
    /// Source of the opaque ids above.
    next_id: AtomicU64,
    /// Highest `change_id` this node has seen in its own CDC log.
    /// Monotonic (`fetch_max`) because `change_id` only grows.
    head: AtomicI64,
    /// Slowest live subscriber's cursor, retained after the last
    /// subscriber detaches so lag keeps growing against a dead replica
    /// instead of freezing.
    shipped: AtomicI64,
    /// Whether `shipped` has ever been set. Before the first subscriber
    /// there is no meaningful cursor and the lag gauge is not emitted.
    has_shipped: AtomicBool,
}

impl SubscriberRegistry {
    fn new() -> Self {
        Self {
            cursors: DashMap::new(),
            next_id: AtomicU64::new(0),
            head: AtomicI64::new(0),
            shipped: AtomicI64::new(0),
            has_shipped: AtomicBool::new(false),
        }
    }

    /// Recompute the derived gauges from current state.
    ///
    /// Called on every attach, detach, shipped batch, and head sample —
    /// i.e. once per replicated transaction at worst, which is orders of
    /// magnitude cheaper than the database write that produced it.
    fn refresh(&self) {
        let head = self.head.load(Ordering::Relaxed);
        gauge!(METRIC_HEAD_CHANGE_ID).set(head as f64);

        // Min over live subscribers is the slowest replica. When the map
        // is empty we keep the previous value: a replica that died at
        // change_id 900 while the primary is at 1500 is 600 behind, not
        // caught up.
        if let Some(min_live) = self.cursors.iter().map(|e| *e.value()).min() {
            self.shipped.store(min_live, Ordering::Relaxed);
            self.has_shipped.store(true, Ordering::Relaxed);
        }

        if self.has_shipped.load(Ordering::Relaxed) {
            let shipped = self.shipped.load(Ordering::Relaxed);
            gauge!(METRIC_SHIPPED_CHANGE_ID).set(shipped as f64);
            // Clamp at zero: `head` and `shipped` are sampled without a
            // common lock, so a subscriber can publish a cursor from a
            // batch whose change_id has not yet reached `head`.
            gauge!(METRIC_LAG_CHANGES).set((head - shipped).max(0) as f64);
        }
    }
}

/// A live CDC subscriber's slot in the registry.
///
/// Dropping the guard detaches the subscriber and refreshes the gauges,
/// so a stream that ends for *any* reason — clean close, poll error,
/// task abort, panic — leaves the subscriber count correct. That is why
/// this is RAII rather than a matching `detach()` call: the CDC serve
/// loop has several exit paths and `?` short-circuits most of them.
pub struct SubscriberGuard {
    registry: Arc<SubscriberRegistry>,
    id: u64,
}

impl SubscriberGuard {
    /// Record one committed batch written into this subscriber's stream.
    ///
    /// `commit_change_id` becomes this subscriber's cursor, and also
    /// advances the node's head — the primary demonstrably has at least
    /// that much log, so lag never reads negative just because the head
    /// sampler has not ticked yet.
    pub fn record_batch_shipped(&self, commit_change_id: i64, rows: usize) {
        counter!(METRIC_BATCHES_SHIPPED).increment(1);
        counter!(METRIC_ROWS_SHIPPED).increment(rows as u64);
        self.registry.cursors.insert(self.id, commit_change_id);
        self.registry.head.fetch_max(commit_change_id, Ordering::Relaxed);
        self.registry.refresh();
    }
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        self.registry.cursors.remove(&self.id);
        gauge!(METRIC_SUBSCRIBERS).set(self.registry.cursors.len() as f64);
        self.registry.refresh();
    }
}

/// Attach a subscriber that has announced `from_change_id` as its
/// applied watermark, returning the guard that keeps it counted.
///
/// Called after the `Subscribe` frame is read, so a stream that connects
/// and never subscribes is never counted as a replica.
#[must_use]
pub fn attach_subscriber(from_change_id: i64) -> SubscriberGuard {
    let registry = Arc::clone(&REGISTRY);
    let id = registry.next_id.fetch_add(1, Ordering::Relaxed);
    registry.cursors.insert(id, from_change_id);
    gauge!(METRIC_SUBSCRIBERS).set(registry.cursors.len() as f64);
    registry.refresh();
    SubscriberGuard { registry, id }
}

/// Record a directly observed `MAX(turso_cdc.change_id)` for this node.
///
/// Fed by the primary's head sampler and by the idle path of a
/// subscriber loop (a tailer that polls no batch has, by definition,
/// reached the head).
pub fn observe_head(head: i64) {
    REGISTRY.head.fetch_max(head, Ordering::Relaxed);
    REGISTRY.refresh();
}

// ---------------------------------------------------------------------------
// Flat recorders. Each wraps exactly one call site in `turso_cdc` so the
// metric name lives here rather than being spelled out inline.
// ---------------------------------------------------------------------------

/// Record a `turso_cdc` tail-poll failure. The stream is dropped and the
/// replica resumes from the same watermark, so this is a retry signal,
/// not a data-loss signal.
pub fn record_tail_poll_error() {
    counter!(METRIC_TAIL_POLL_ERRORS).increment(1);
}

/// Record an inbound stream refused because this node is not the primary.
/// `stream` is `cdc` or `snapshot`.
pub fn record_stream_refused(stream: &'static str) {
    counter!(METRIC_STREAMS_REFUSED, "stream" => stream).increment(1);
}

/// Record a served snapshot bootstrap. `bytes` and `elapsed` are only
/// meaningful on the `ok` path; an error is counted without them.
pub fn record_snapshot_served(status: &'static str, bytes: u64, elapsed: Duration) {
    counter!(METRIC_SNAPSHOTS_SERVED, "status" => status).increment(1);
    if status == "ok" {
        counter!(METRIC_SNAPSHOT_BYTES_SERVED).increment(bytes);
        histogram!(METRIC_SNAPSHOT_DURATION, "role" => "serve").record(elapsed.as_secs_f64());
    }
}

/// Record a received snapshot bootstrap body on the replica.
pub fn record_snapshot_received(bytes: u64, elapsed: Duration) {
    counter!(METRIC_SNAPSHOT_BYTES_RECEIVED).increment(bytes);
    histogram!(METRIC_SNAPSHOT_DURATION, "role" => "fetch").record(elapsed.as_secs_f64());
}

/// Record the outcome of the cold-start bootstrap decision. `outcome` is
/// `ok`, `skipped` (local DB already populated) or `failed` (retry budget
/// exhausted — startup aborts).
pub fn record_bootstrap(outcome: &'static str) {
    counter!(METRIC_BOOTSTRAP, "outcome" => outcome).increment(1);
}

/// Record one applied CDC batch on the replica, advancing the watermark
/// gauge to the batch's commit `change_id`.
pub fn record_batch_applied(commit_change_id: i64, rows: usize, elapsed: Duration) {
    counter!(METRIC_BATCHES_APPLIED).increment(1);
    counter!(METRIC_ROWS_APPLIED).increment(rows as u64);
    histogram!(METRIC_APPLY_DURATION).record(elapsed.as_secs_f64());
    record_applied_watermark(commit_change_id);
}

/// Publish the replica's applied watermark. Also called on subscribe and
/// after a snapshot bootstrap, so the gauge reflects durable local state
/// rather than only what this process happened to apply.
pub fn record_applied_watermark(change_id: i64) {
    gauge!(METRIC_APPLIED_CHANGE_ID).set(change_id as f64);
}

/// Record an `apply_batch` failure. The stream is failed and the
/// watermark deliberately does not advance.
pub fn record_apply_error() {
    counter!(METRIC_APPLY_ERRORS).increment(1);
}

/// Record a replica subscribe attempt by terminal outcome, so each
/// attempt increments exactly one series. `outcome` is `closed`,
/// `dial_error`, `stream_error` or `watermark_error`; reconnect rate is
/// the sum across all four.
pub fn record_connect_outcome(outcome: &'static str) {
    counter!(METRIC_REPLICA_CONNECTS, "outcome" => outcome).increment(1);
}

/// Register the zero-valued series at CDC startup.
///
/// Without this, a healthy cluster's scrape omits every error counter and
/// an operator cannot tell "no apply errors" from "this build has no CDC
/// instrumentation" — the same reason `ephpm_db_proxy_*` seeds its gauges
/// at boot. Only counters and the subscriber gauge are seeded: the
/// head/shipped/lag gauges are deliberately left absent until a
/// subscriber attaches, because a lag of `0` published by a node that has
/// never replicated anything would be a lie.
pub fn init() {
    gauge!(METRIC_SUBSCRIBERS).set(0.0);
    for name in [
        METRIC_BATCHES_SHIPPED,
        METRIC_ROWS_SHIPPED,
        METRIC_TAIL_POLL_ERRORS,
        METRIC_SNAPSHOT_BYTES_SERVED,
        METRIC_BATCHES_APPLIED,
        METRIC_ROWS_APPLIED,
        METRIC_APPLY_ERRORS,
        METRIC_SNAPSHOT_BYTES_RECEIVED,
    ] {
        counter!(name).increment(0);
    }
    for stream in ["cdc", "snapshot"] {
        counter!(METRIC_STREAMS_REFUSED, "stream" => stream).increment(0);
    }
    for status in ["ok", "error"] {
        counter!(METRIC_SNAPSHOTS_SERVED, "status" => status).increment(0);
    }
    for outcome in ["closed", "dial_error", "stream_error", "watermark_error"] {
        counter!(METRIC_REPLICA_CONNECTS, "outcome" => outcome).increment(0);
    }
    for outcome in ["ok", "skipped", "failed"] {
        counter!(METRIC_BOOTSTRAP, "outcome" => outcome).increment(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global, so these tests must not run
    /// concurrently with each other. They assert on the *state* behind
    /// the gauges (which is deterministic) rather than on recorded metric
    /// values (which need an installed recorder — that coverage lives in
    /// `tests/turso_cdc_metrics_e2e.rs`, where a real scrape is parsed).
    fn reset() {
        REGISTRY.cursors.clear();
        REGISTRY.head.store(0, Ordering::Relaxed);
        REGISTRY.shipped.store(0, Ordering::Relaxed);
        REGISTRY.has_shipped.store(false, Ordering::Relaxed);
    }

    /// Lag is head minus the *slowest* subscriber, not the fastest: one
    /// caught-up replica must not mask another that is far behind.
    #[test]
    fn lag_tracks_the_slowest_subscriber() {
        let _s = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reset();

        let fast = attach_subscriber(0);
        let slow = attach_subscriber(0);

        fast.record_batch_shipped(100, 10);
        slow.record_batch_shipped(40, 4);

        assert_eq!(REGISTRY.head.load(Ordering::Relaxed), 100);
        assert_eq!(REGISTRY.shipped.load(Ordering::Relaxed), 40, "min cursor, not max");
        assert_eq!(REGISTRY.cursors.len(), 2);

        // The slow one catching up moves the lag, the fast one cannot.
        slow.record_batch_shipped(100, 60);
        assert_eq!(REGISTRY.shipped.load(Ordering::Relaxed), 100);
    }

    /// Dropping a subscriber decrements the count on every exit path,
    /// and — the property that makes the lag gauge trustworthy — the
    /// shipped cursor is *retained* so lag keeps growing against a dead
    /// replica rather than freezing or resetting to zero.
    #[test]
    fn detached_subscriber_retains_cursor_so_lag_keeps_growing() {
        let _s = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reset();

        {
            let sub = attach_subscriber(0);
            sub.record_batch_shipped(900, 9);
            assert_eq!(REGISTRY.cursors.len(), 1);
        } // replica dies here

        assert_eq!(REGISTRY.cursors.len(), 0, "guard drop must detach");
        assert!(REGISTRY.has_shipped.load(Ordering::Relaxed));
        assert_eq!(REGISTRY.shipped.load(Ordering::Relaxed), 900, "cursor must survive detach");

        // Primary keeps taking writes with nobody attached.
        observe_head(1500);
        let lag = REGISTRY.head.load(Ordering::Relaxed) - REGISTRY.shipped.load(Ordering::Relaxed);
        assert_eq!(lag, 600, "a dead replica must show a growing lag, not a frozen one");
    }

    /// Head is monotonic: a stale or cold `MAX(change_id)` observation
    /// (the sampler reads `0` when `turso_cdc` does not exist yet) must
    /// never walk the head backwards and invent negative lag.
    #[test]
    fn head_is_monotonic_and_lag_never_negative() {
        let _s = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reset();

        let sub = attach_subscriber(0);
        sub.record_batch_shipped(50, 5);
        observe_head(0);
        assert_eq!(REGISTRY.head.load(Ordering::Relaxed), 50, "a 0 sample must not lower head");

        // A cursor ahead of the last sampled head clamps rather than
        // going negative.
        sub.record_batch_shipped(80, 3);
        let lag = REGISTRY.head.load(Ordering::Relaxed) - REGISTRY.shipped.load(Ordering::Relaxed);
        assert!(lag >= 0, "lag went negative: {lag}");
    }

    /// A fresh replica joining at a low watermark must *lower* the
    /// shipped cursor — its backlog is real, and reporting the caught-up
    /// replica's position instead would hide it.
    #[test]
    fn a_new_cold_subscriber_lowers_the_shipped_cursor() {
        let _s = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reset();

        let warm = attach_subscriber(0);
        warm.record_batch_shipped(1000, 10);
        assert_eq!(REGISTRY.shipped.load(Ordering::Relaxed), 1000);

        let cold = attach_subscriber(0);
        assert_eq!(REGISTRY.shipped.load(Ordering::Relaxed), 0, "cold join must show its backlog");
        drop(cold);

        // ...and once it leaves, the remaining replica's position stands.
        assert_eq!(REGISTRY.shipped.load(Ordering::Relaxed), 1000);
    }

    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

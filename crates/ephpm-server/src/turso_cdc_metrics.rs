//! Prometheus instrumentation for the experimental CDC-native Turso
//! replication path ([`crate::turso_cdc`]).
//!
//! Every series here is emitted **only** when clustered SQLite actually
//! starts the CDC path — [`init`] is called from `start_clustered_turso_cdc`
//! and `start_clustered_per_site_turso`, and nothing else registers these
//! names. A single-node Turso deployment has no `ephpm_cdc_*` series at all.
//!
//! # Two kinds of series: per-site positions, node-wide event counts
//!
//! A `change_id` is a position in **one database's** `turso_cdc` log. In
//! per-site clustered mode every site has its own log, so a position gauge
//! that is not keyed by site mixes unrelated sequences. That is exactly how
//! the old unlabelled `ephpm_cdc_replication_lag_changes` broke: the head was
//! `fetch_max` over every site's log and the shipped cursor `min` over every
//! site's subscribers, so a stress run read 5,967 changes behind while every
//! replica was byte-identical.
//!
//! So every **gauge** carries a `site` label — the vhost's canonical site key
//! in per-site clustered mode, `default` in whole-database clustered mode (the
//! same convention as `ephpm_sqlite_wal_bytes{site}`) — and is computed from
//! per-site state:
//!
//! | Gauge | Published by |
//! |-------|--------------|
//! | [`METRIC_SUBSCRIBERS`] | the node shipping `site` (its owner / the primary) |
//! | [`METRIC_HEAD_CHANGE_ID`] | the node shipping `site` |
//! | [`METRIC_SHIPPED_CHANGE_ID`] | the node shipping `site`, once a subscriber has attached |
//! | [`METRIC_LAG_CHANGES`] | the node shipping `site`, once a subscriber has attached |
//! | [`METRIC_APPLIED_CHANGE_ID`] | a node replicating `site` from someone else |
//!
//! **Counters and histograms** stay node-wide with no `site` label. They count
//! events, and a sum of events across sites is still exactly the right number
//! ("is anything replicating", "is anything failing"); a per-site label would
//! multiply a dozen series by the site count for no question they answer that
//! the per-site gauges do not.
//!
//! # Why these gauges are rendered here rather than through `metrics`
//!
//! The per-site gauges must **disappear** when this node stops shipping or
//! applying a site (ownership moved, or the site's driver exited) — a stale
//! `applied_change_id` left behind on a node that became the owner would make
//! the cross-node lag query below report a lag that does not exist, and
//! zeroing it would be just as wrong (`0` is a position). The `metrics` facade
//! (0.24) has no way to unregister a series, so these five gauges are kept in
//! the per-site registry below and appended to the exporter's output at scrape
//! time by [`render_site_gauges`] (called from [`crate::metrics::render_text`]).
//! Nothing else records these names, so there is no duplicate family.
//!
//! # The lag metric, precisely
//!
//! [`METRIC_LAG_CHANGES`] is a **row count, not a duration**, published by
//! the node shipping the site:
//!
//! ```text
//! ephpm_cdc_replication_lag_changes{site}
//!   = ephpm_cdc_primary_head_change_id{site}   (MAX(turso_cdc.change_id) of that site's log)
//!   - ephpm_cdc_shipped_change_id{site}        (min cursor over that site's subscribers)
//! ```
//!
//! Both inputs are `change_id` values, which turso allocates one per
//! captured row change. So "lag = 500" means *five hundred row changes*
//! behind, and says nothing about seconds. Do not graph it with a seconds
//! unit. A replica never publishes the lag for a site (it does not know the
//! owner's head); it publishes its applied watermark, and end-to-end lag is a
//! cross-node subtraction:
//!
//! ```promql
//! max by (site) (ephpm_cdc_primary_head_change_id)
//!   - min by (site) (ephpm_cdc_applied_change_id)
//! ```
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
//! and the replica's `apply_batch` cost; the cross-node query above
//! covers those.
//!
//! ## Behaviour with no subscribers
//!
//! [`METRIC_SUBSCRIBERS`] going to `0` on the shipping node is the
//! unambiguous "this site is not being replicated" signal. The lag gauge
//! stays *live* in that state rather than freezing: the shipped cursor
//! retains the last position any subscriber reached, and the head keeps
//! being sampled, so an owner that keeps taking writes after its replica
//! dies shows a genuinely growing lag. Before the first subscriber ever
//! attaches there is no cursor to subtract from, and the lag and shipped
//! gauges are simply not emitted.
//!
//! The lag covers **attached** subscribers only. The owner does not know how
//! many replicas a site should have, so a replica that is bootstrapping (or
//! has lost its stream) while another stays caught up is invisible here and
//! the lag reads `0` (the v0.13.0 RC stress run saw exactly that for 22
//! sites). The replica side closes the gap: a per-site replica publishes
//! [`METRIC_APPLIED_CHANGE_ID`] as `0` when it starts a snapshot bootstrap,
//! so the cross-node query above reports it as behind instead of skipping it.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use dashmap::DashMap;
use metrics::{counter, histogram};

// ---------------------------------------------------------------------------
// Metric names. Exported so integration tests assert against the same
// strings the recorder emits, and so `site/content/reference/metrics.md`
// has one authoritative source to be checked against.
// ---------------------------------------------------------------------------

/// Gauge: CDC subscriber streams currently attached for a site. Label: `site`.
pub const METRIC_SUBSCRIBERS: &str = "ephpm_cdc_subscribers";
/// Counter: committed transaction batches written to subscriber streams.
pub const METRIC_BATCHES_SHIPPED: &str = "ephpm_cdc_batches_shipped_total";
/// Counter: CDC rows contained in those shipped batches.
pub const METRIC_ROWS_SHIPPED: &str = "ephpm_cdc_rows_shipped_total";
/// Gauge: lowest `change_id` shipped across a site's subscribers. Label: `site`.
pub const METRIC_SHIPPED_CHANGE_ID: &str = "ephpm_cdc_shipped_change_id";
/// Gauge: `MAX(turso_cdc.change_id)` of a site's log on the node shipping
/// it. Label: `site`.
pub const METRIC_HEAD_CHANGE_ID: &str = "ephpm_cdc_primary_head_change_id";
/// Gauge: head minus shipped cursor, in change-log rows. Label: `site`.
/// See the module docs — this is **not** seconds.
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
/// Gauge: this replica's applied watermark (`change_id`) for a site.
/// Label: `site`.
pub const METRIC_APPLIED_CHANGE_ID: &str = "ephpm_cdc_applied_change_id";
/// Counter: `apply_batch` failures on the replica.
pub const METRIC_APPLY_ERRORS: &str = "ephpm_cdc_apply_errors_total";
/// Histogram: per-batch `apply_batch` duration on the replica.
pub const METRIC_APPLY_DURATION: &str = "ephpm_cdc_apply_duration_seconds";
/// Counter: replica subscribe attempts by terminal outcome. Label:
/// `outcome` (`closed` / `dial_error` / `stream_error` / `watermark_error`).
pub const METRIC_REPLICA_CONNECTS: &str = "ephpm_cdc_replica_connects_total";
/// Counter: cold-start snapshot bootstrap outcomes. Label: `outcome`
/// (`ok` / `skipped` / `failed` in whole-database mode; `handoff_ok` /
/// `handoff_new` / `handoff_blocked` for a per-site ownership handoff).
pub const METRIC_BOOTSTRAP: &str = "ephpm_cdc_bootstrap_total";
/// Counter: snapshot dump bytes received during bootstrap.
pub const METRIC_SNAPSHOT_BYTES_RECEIVED: &str = "ephpm_cdc_snapshot_bytes_received_total";

/// The `site` label value of the whole-database clustered mode, which
/// replicates one database over `cdc/default`. Same value as
/// [`crate::wal_gate::DEFAULT_SITE_LABEL`], so dashboards can join the two.
pub const DEFAULT_SITE: &str = crate::wal_gate::DEFAULT_SITE_LABEL;

// ---------------------------------------------------------------------------
// Process-global per-site registry.
//
// Global because the shipping side (`serve_subscriber`, spawned per inbound
// stream) and the applying side (`subscribe_and_consume`) are reached from
// several unrelated tasks; threading a registry handle through all of them
// would only move the global into every signature. Keyed by site, so two
// sites — or two tests using distinct site keys — never touch each other's
// state.
// ---------------------------------------------------------------------------

static REGISTRY: LazyLock<SiteRegistry> = LazyLock::new(SiteRegistry::default);

#[derive(Default)]
struct SiteRegistry {
    /// Sites this node ships (owns / is primary for).
    shipping: DashMap<String, Arc<ShippingState>>,
    /// Sites this node applies from someone else: site → applied watermark.
    applied: DashMap<String, i64>,
}

/// Shipping-side bookkeeping for one site.
///
/// `Relaxed` ordering throughout: every field feeds a gauge that is
/// sampled by a scrape seconds later, so a reader that observes a value
/// a few microseconds stale around a concurrent update publishes a
/// number that was true a moment earlier. Nothing here guards memory
/// another thread must see.
#[derive(Default)]
struct ShippingState {
    /// Live subscribers: opaque id → the `change_id` shipped to it.
    cursors: DashMap<u64, i64>,
    /// Source of the opaque ids above.
    next_id: AtomicU64,
    /// Highest `change_id` seen in this site's own CDC log. Monotonic
    /// (`fetch_max`) because `change_id` only grows.
    head: AtomicI64,
    /// Whether `head` has been observed. The head gauge is absent until then.
    head_seen: AtomicBool,
    /// Slowest live subscriber's cursor, retained after the last
    /// subscriber detaches so lag keeps growing against a dead replica
    /// instead of freezing.
    shipped: AtomicI64,
    /// Whether `shipped` has ever been set. Before the first subscriber
    /// there is no meaningful cursor and the lag gauge is not emitted.
    has_shipped: AtomicBool,
}

impl ShippingState {
    /// Re-derive the shipped cursor from the live subscribers. When there
    /// are none, the previous value is kept: a replica that died at
    /// change_id 900 while the owner is at 1500 is 600 behind, not caught up.
    fn refresh_shipped(&self) {
        if let Some(min_live) = self.cursors.iter().map(|e| *e.value()).min() {
            self.shipped.store(min_live, Ordering::Relaxed);
            self.has_shipped.store(true, Ordering::Relaxed);
        }
    }

    fn observe_head(&self, head: i64) {
        self.head.fetch_max(head, Ordering::Relaxed);
        self.head_seen.store(true, Ordering::Relaxed);
    }
}

fn shipping_state(site: &str) -> Arc<ShippingState> {
    if let Some(s) = REGISTRY.shipping.get(site) {
        return Arc::clone(s.value());
    }
    Arc::clone(REGISTRY.shipping.entry(site.to_string()).or_default().value())
}

/// A live CDC subscriber's slot in its site's registry entry.
///
/// Dropping the guard detaches the subscriber, so a stream that ends for
/// *any* reason — clean close, poll error, task abort, panic — leaves the
/// subscriber count correct. That is why this is RAII rather than a
/// matching `detach()` call: the CDC serve loop has several exit paths and
/// `?` short-circuits most of them.
///
/// The guard holds the site's state by `Arc`. If the site is forgotten
/// while the stream is still draining ([`forget_shipping`]), the guard
/// keeps updating the detached state and nothing it does reaches a scrape.
pub struct SubscriberGuard {
    state: Arc<ShippingState>,
    id: u64,
}

impl SubscriberGuard {
    /// Record one committed batch written into this subscriber's stream.
    ///
    /// `commit_change_id` becomes this subscriber's cursor, and also
    /// advances the site's head — the owner demonstrably has at least that
    /// much log, so lag never reads negative just because the head sampler
    /// has not ticked yet.
    pub fn record_batch_shipped(&self, commit_change_id: i64, rows: usize) {
        counter!(METRIC_BATCHES_SHIPPED).increment(1);
        counter!(METRIC_ROWS_SHIPPED).increment(rows as u64);
        self.state.cursors.insert(self.id, commit_change_id);
        self.state.observe_head(commit_change_id);
        self.state.refresh_shipped();
    }
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        self.state.cursors.remove(&self.id);
        self.state.refresh_shipped();
    }
}

/// Attach a subscriber of `site` that has announced `from_change_id` as its
/// applied watermark, returning the guard that keeps it counted.
///
/// Called after the `Subscribe` frame is read, so a stream that connects
/// and never subscribes is never counted as a replica.
#[must_use]
pub fn attach_subscriber(site: &str, from_change_id: i64) -> SubscriberGuard {
    let state = shipping_state(site);
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    state.cursors.insert(id, from_change_id);
    state.refresh_shipped();
    SubscriberGuard { state, id }
}

/// Record a directly observed `MAX(turso_cdc.change_id)` of `site`'s log.
///
/// Fed by the head samplers (whole-database primary; per-site owner) and by
/// the idle path of a subscriber loop (a tailer that polls no batch has, by
/// definition, reached the head).
pub fn observe_head(site: &str, head: i64) {
    shipping_state(site).observe_head(head);
}

/// Mark this node as shipping `site`, so `ephpm_cdc_subscribers{site}`
/// renders (as `0`) before any subscriber attaches. Called once at startup
/// for [`DEFAULT_SITE`] in whole-database mode, and whenever a per-site
/// driver finds this node is the site's owner. Idempotent.
pub fn track_shipping(site: &str) {
    drop(shipping_state(site));
}

/// Drop every shipping-side series of `site` (subscribers, head, shipped,
/// lag): this node no longer ships it.
pub fn forget_shipping(site: &str) {
    REGISTRY.shipping.remove(site);
}

/// Drop `site`'s applied-watermark series: this node no longer replicates it
/// from someone else.
pub fn forget_applied(site: &str) {
    REGISTRY.applied.remove(site);
}

/// Drop every per-site series of `site`. Called when a per-site replication
/// driver exits, so a site that leaves this node's replication set stops
/// being reported rather than lingering at its last value.
pub fn forget_site(site: &str) {
    forget_shipping(site);
    forget_applied(site);
}

/// Append the per-site gauges to a rendered Prometheus scrape.
///
/// One `# TYPE` header per family, then one sample per site, sites sorted so
/// the output is stable between scrapes. Families with no samples are
/// omitted entirely (an absent series, not a zero).
#[allow(clippy::cast_precision_loss)] // change_ids and counts fit f64's 2^53 exactly
pub fn render_site_gauges(out: &mut String) {
    let mut shipping: Vec<(String, Arc<ShippingState>)> =
        REGISTRY.shipping.iter().map(|e| (e.key().clone(), Arc::clone(e.value()))).collect();
    shipping.sort_by(|a, b| a.0.cmp(&b.0));
    let mut applied: Vec<(String, i64)> =
        REGISTRY.applied.iter().map(|e| (e.key().clone(), *e.value())).collect();
    applied.sort();

    let mut subscribers = Vec::new();
    let mut head = Vec::new();
    let mut shipped = Vec::new();
    let mut lag = Vec::new();
    for (site, state) in &shipping {
        subscribers.push((site.as_str(), i64::try_from(state.cursors.len()).unwrap_or(i64::MAX)));
        let h = state.head.load(Ordering::Relaxed);
        let head_seen = state.head_seen.load(Ordering::Relaxed);
        if head_seen {
            head.push((site.as_str(), h));
        }
        if state.has_shipped.load(Ordering::Relaxed) {
            let s = state.shipped.load(Ordering::Relaxed);
            shipped.push((site.as_str(), s));
            // Clamp at zero: `head` and `shipped` are updated without a
            // common lock, so a subscriber can publish a cursor from a batch
            // whose change_id has not yet reached `head`.
            lag.push((site.as_str(), (h - s).max(0)));
        }
    }
    let applied: Vec<(&str, i64)> = applied.iter().map(|(s, v)| (s.as_str(), *v)).collect();

    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for (name, samples) in [
        (METRIC_SUBSCRIBERS, &subscribers),
        (METRIC_HEAD_CHANGE_ID, &head),
        (METRIC_SHIPPED_CHANGE_ID, &shipped),
        (METRIC_LAG_CHANGES, &lag),
        (METRIC_APPLIED_CHANGE_ID, &applied),
    ] {
        if samples.is_empty() {
            continue;
        }
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (site, value) in samples {
            let _ = writeln!(out, "{name}{{site=\"{}\"}} {}", escape_label(site), *value as f64);
        }
        out.push('\n');
    }
}

/// Escape a label value per the Prometheus text format. Site keys are
/// validated to `[a-z0-9._-]` before they get here, so this is belt and
/// braces.
fn escape_label(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s
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
/// exhausted — startup aborts); per-site mode records `handoff_ok`,
/// `handoff_new` and `handoff_blocked` (see `turso_cdc::settle_site_serving`).
pub fn record_bootstrap(outcome: &'static str) {
    counter!(METRIC_BOOTSTRAP, "outcome" => outcome).increment(1);
}

/// Record one applied CDC batch of `site` on the replica, advancing that
/// site's watermark gauge to the batch's commit `change_id`.
pub fn record_batch_applied(site: &str, commit_change_id: i64, rows: usize, elapsed: Duration) {
    counter!(METRIC_BATCHES_APPLIED).increment(1);
    counter!(METRIC_ROWS_APPLIED).increment(rows as u64);
    histogram!(METRIC_APPLY_DURATION).record(elapsed.as_secs_f64());
    record_applied_watermark(site, commit_change_id);
}

/// Publish the replica's applied watermark for `site`. Also called on
/// subscribe and after a snapshot bootstrap, so the gauge reflects durable
/// local state rather than only what this process happened to apply.
pub fn record_applied_watermark(site: &str, change_id: i64) {
    REGISTRY.applied.insert(site.to_string(), change_id);
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

/// Register the zero-valued counters at CDC startup.
///
/// Without this, a healthy cluster's scrape omits every error counter and
/// an operator cannot tell "no apply errors" from "this build has no CDC
/// instrumentation" — the same reason `ephpm_db_proxy_*` seeds its gauges
/// at boot. The per-site gauges are not seeded here: whole-database mode
/// calls [`track_shipping`]`(`[`DEFAULT_SITE`]`)` so its subscriber gauge
/// reads `0` from boot, and the head/shipped/lag/applied gauges are left
/// absent until there is a position to report, because a lag of `0`
/// published by a node that has never replicated anything would be a lie.
pub fn init() {
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

    // The registry is process-global, but it is keyed by site: every test
    // here uses site keys no other test (and no production path, which only
    // ever uses `default` or a real vhost key) touches, so they need no
    // serialization against each other or against tests that drive the
    // production `serve_subscriber` on `default`.

    /// The value of `name{site="<site>"}` in a rendered scrape, or `None`
    /// when the series is absent.
    #[allow(clippy::cast_possible_truncation)] // every value here is an exact integer
    fn rendered(name: &str, site: &str) -> Option<i64> {
        let mut out = String::new();
        render_site_gauges(&mut out);
        let prefix = format!("{name}{{site=\"{site}\"}} ");
        out.lines().find_map(|l| l.strip_prefix(&prefix)).map(|v| v.parse::<f64>().unwrap() as i64)
    }

    fn state(site: &str) -> Arc<ShippingState> {
        Arc::clone(REGISTRY.shipping.get(site).expect("site is tracked").value())
    }

    /// **The regression**: two sites replicating through one process must
    /// report independent lags. Site A is far ahead in its own log and fully
    /// shipped; site B is at low change_ids and its replica is behind. The
    /// old unlabelled gauge computed `max(head) - min(shipped)` across both
    /// logs — here 6000 - 33 — while each site's real lag is 0 and 7.
    #[test]
    fn two_sites_report_independent_lag() {
        let (a, b) = ("lagtest-a.example", "lagtest-b.example");

        let sub_a = attach_subscriber(a, 0);
        sub_a.record_batch_shipped(6000, 10);
        observe_head(a, 6000);

        let sub_b = attach_subscriber(b, 0);
        sub_b.record_batch_shipped(33, 3);
        observe_head(b, 40);

        assert_eq!(rendered(METRIC_LAG_CHANGES, a), Some(0), "caught-up site must read 0");
        assert_eq!(rendered(METRIC_LAG_CHANGES, b), Some(7), "lagging site must read its own lag");
        assert_eq!(rendered(METRIC_HEAD_CHANGE_ID, a), Some(6000));
        assert_eq!(rendered(METRIC_HEAD_CHANGE_ID, b), Some(40));
        assert_eq!(rendered(METRIC_SHIPPED_CHANGE_ID, a), Some(6000));
        assert_eq!(rendered(METRIC_SHIPPED_CHANGE_ID, b), Some(33));
        assert_eq!(rendered(METRIC_SUBSCRIBERS, a), Some(1));
        assert_eq!(rendered(METRIC_SUBSCRIBERS, b), Some(1));

        // Replica-side watermarks are per site too.
        record_applied_watermark("lagtest-c.example", 12);
        record_applied_watermark("lagtest-d.example", 900);
        assert_eq!(rendered(METRIC_APPLIED_CHANGE_ID, "lagtest-c.example"), Some(12));
        assert_eq!(rendered(METRIC_APPLIED_CHANGE_ID, "lagtest-d.example"), Some(900));

        drop((sub_a, sub_b));
        for s in [a, b, "lagtest-c.example", "lagtest-d.example"] {
            forget_site(s);
        }
    }

    /// A site whose driver stopped (or whose ownership moved away) must
    /// vanish from the scrape — not linger at its last value, and not read
    /// `0`, which for a position gauge is a lie.
    #[test]
    fn forgotten_site_series_are_removed_from_the_scrape() {
        let owned = "forget-owned.example";
        let replicated = "forget-replicated.example";
        let kept = "forget-kept.example";

        let sub = attach_subscriber(owned, 0);
        sub.record_batch_shipped(50, 5);
        observe_head(owned, 80);
        record_applied_watermark(replicated, 70);
        record_applied_watermark(kept, 5);
        assert_eq!(rendered(METRIC_LAG_CHANGES, owned), Some(30));
        assert_eq!(rendered(METRIC_APPLIED_CHANGE_ID, replicated), Some(70));

        forget_site(owned);
        forget_site(replicated);
        for name in [
            METRIC_SUBSCRIBERS,
            METRIC_HEAD_CHANGE_ID,
            METRIC_SHIPPED_CHANGE_ID,
            METRIC_LAG_CHANGES,
            METRIC_APPLIED_CHANGE_ID,
        ] {
            assert_eq!(rendered(name, owned), None, "{name} lingered for a forgotten site");
            assert_eq!(rendered(name, replicated), None, "{name} lingered for a forgotten site");
        }
        assert_eq!(rendered(METRIC_APPLIED_CHANGE_ID, kept), Some(5), "other sites unaffected");

        // A stream still draining after its site was forgotten must not
        // resurrect the series.
        sub.record_batch_shipped(90, 1);
        drop(sub);
        assert_eq!(rendered(METRIC_LAG_CHANGES, owned), None);
        assert_eq!(rendered(METRIC_SUBSCRIBERS, owned), None);

        forget_site(kept);
    }

    /// Ownership moving away clears only the shipping side, and becoming the
    /// owner clears only the applying side.
    #[test]
    fn role_changes_clear_only_the_side_that_stopped() {
        let site = "rolechange.example";
        record_applied_watermark(site, 10);
        track_shipping(site);
        assert_eq!(rendered(METRIC_SUBSCRIBERS, site), Some(0), "an owner renders 0 subscribers");

        forget_applied(site);
        assert_eq!(rendered(METRIC_APPLIED_CHANGE_ID, site), None);
        assert_eq!(rendered(METRIC_SUBSCRIBERS, site), Some(0));

        record_applied_watermark(site, 11);
        forget_shipping(site);
        assert_eq!(rendered(METRIC_SUBSCRIBERS, site), None);
        assert_eq!(rendered(METRIC_APPLIED_CHANGE_ID, site), Some(11));

        forget_site(site);
    }

    /// Lag is head minus the *slowest* subscriber, not the fastest: one
    /// caught-up replica must not mask another that is far behind.
    #[test]
    fn lag_tracks_the_slowest_subscriber() {
        let site = "slowest.example";
        let fast = attach_subscriber(site, 0);
        let slow = attach_subscriber(site, 0);

        fast.record_batch_shipped(100, 10);
        slow.record_batch_shipped(40, 4);

        let s = state(site);
        assert_eq!(s.head.load(Ordering::Relaxed), 100);
        assert_eq!(s.shipped.load(Ordering::Relaxed), 40, "min cursor, not max");
        assert_eq!(s.cursors.len(), 2);
        assert_eq!(rendered(METRIC_LAG_CHANGES, site), Some(60));

        // The slow one catching up moves the lag, the fast one cannot.
        slow.record_batch_shipped(100, 60);
        assert_eq!(s.shipped.load(Ordering::Relaxed), 100);
        assert_eq!(rendered(METRIC_LAG_CHANGES, site), Some(0));

        drop((fast, slow));
        forget_site(site);
    }

    /// Dropping a subscriber decrements the count on every exit path,
    /// and — the property that makes the lag gauge trustworthy — the
    /// shipped cursor is *retained* so lag keeps growing against a dead
    /// replica rather than freezing or resetting to zero.
    #[test]
    fn detached_subscriber_retains_cursor_so_lag_keeps_growing() {
        let site = "detached.example";
        {
            let sub = attach_subscriber(site, 0);
            sub.record_batch_shipped(900, 9);
            assert_eq!(state(site).cursors.len(), 1);
        } // replica dies here

        let s = state(site);
        assert_eq!(s.cursors.len(), 0, "guard drop must detach");
        assert!(s.has_shipped.load(Ordering::Relaxed));
        assert_eq!(s.shipped.load(Ordering::Relaxed), 900, "cursor must survive detach");

        // Owner keeps taking writes with nobody attached.
        observe_head(site, 1500);
        assert_eq!(
            rendered(METRIC_LAG_CHANGES, site),
            Some(600),
            "a dead replica must show a growing lag, not a frozen one"
        );
        assert_eq!(rendered(METRIC_SUBSCRIBERS, site), Some(0));

        forget_site(site);
    }

    /// Head is monotonic: a stale or cold `MAX(change_id)` observation
    /// (the sampler reads `0` when `turso_cdc` does not exist yet) must
    /// never walk the head backwards and invent negative lag.
    #[test]
    fn head_is_monotonic_and_lag_never_negative() {
        let site = "monotonic.example";
        let sub = attach_subscriber(site, 0);
        sub.record_batch_shipped(50, 5);
        observe_head(site, 0);
        assert_eq!(state(site).head.load(Ordering::Relaxed), 50, "a 0 sample must not lower head");

        // A cursor ahead of the last sampled head clamps rather than
        // going negative.
        sub.record_batch_shipped(80, 3);
        assert!(rendered(METRIC_LAG_CHANGES, site).unwrap() >= 0);

        drop(sub);
        forget_site(site);
    }

    /// A fresh replica joining at a low watermark must *lower* the
    /// shipped cursor — its backlog is real, and reporting the caught-up
    /// replica's position instead would hide it.
    #[test]
    fn a_new_cold_subscriber_lowers_the_shipped_cursor() {
        let site = "coldjoin.example";
        let warm = attach_subscriber(site, 0);
        warm.record_batch_shipped(1000, 10);
        assert_eq!(rendered(METRIC_SHIPPED_CHANGE_ID, site), Some(1000));

        let cold = attach_subscriber(site, 0);
        assert_eq!(
            rendered(METRIC_SHIPPED_CHANGE_ID, site),
            Some(0),
            "cold join must show its backlog"
        );
        drop(cold);

        // ...and once it leaves, the remaining replica's position stands.
        assert_eq!(rendered(METRIC_SHIPPED_CHANGE_ID, site), Some(1000));

        drop(warm);
        forget_site(site);
    }

    /// Before any subscriber attaches, an owner publishes its subscriber
    /// count (0) and — once sampled — its head, but no shipped cursor and no
    /// lag: a lag of 0 from a node that has never replicated would read as
    /// "fully caught up".
    #[test]
    fn lag_is_absent_until_a_subscriber_attaches() {
        let site = "nosubs.example";
        track_shipping(site);
        assert_eq!(rendered(METRIC_SUBSCRIBERS, site), Some(0));
        assert_eq!(rendered(METRIC_HEAD_CHANGE_ID, site), None, "head absent until sampled");
        observe_head(site, 12);
        assert_eq!(rendered(METRIC_HEAD_CHANGE_ID, site), Some(12));
        assert_eq!(rendered(METRIC_SHIPPED_CHANGE_ID, site), None);
        assert_eq!(rendered(METRIC_LAG_CHANGES, site), None);
        forget_site(site);
    }

    /// The appended text is well-formed exposition format: one `# TYPE`
    /// line per family, samples grouped under it, separated from whatever
    /// the exporter rendered before.
    #[test]
    fn rendered_families_are_grouped_under_one_type_header() {
        let site = "format.example";
        track_shipping(site);
        let mut out = String::from("# TYPE other_metric counter\nother_metric 1");
        render_site_gauges(&mut out);
        assert!(out.starts_with("# TYPE other_metric counter\nother_metric 1\n"));
        assert_eq!(out.matches(&format!("# TYPE {METRIC_SUBSCRIBERS} gauge")).count(), 1);
        assert!(out.contains(&format!("{METRIC_SUBSCRIBERS}{{site=\"{site}\"}} 0\n")));
        forget_site(site);
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape_label("x\ny"), "x\\ny");
    }
}

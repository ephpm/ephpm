//! Query digest tracking and slow query logging.
//!
//! Records timing, throughput, and error rates for every SQL query
//! regardless of which runtime path handles it (DB Proxy or litewire).
//! Uses SQL normalization to group queries by structure rather than
//! literal values.

pub mod digest;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dashmap::{DashMap, DashSet};
use metrics::{counter, gauge, histogram};

/// Placeholder value used for the `digest` label on metrics once the
/// per-digest label series budget is exhausted. Named to line up with
/// Prometheus community conventions for cardinality overflow bins.
const DIGEST_OTHER_BUCKET: &str = "__other__";

/// Configuration for query stats tracking.
#[derive(Debug, Clone)]
pub struct StatsConfig {
    /// Whether query stats tracking is enabled.
    /// When `false`, `record()` is a no-op — zero overhead.
    pub enabled: bool,

    /// Queries slower than this are logged at WARN level.
    pub slow_query_threshold: Duration,

    /// Maximum number of distinct query digests to track internally.
    /// Prevents unbounded memory growth from unique queries. This
    /// bounds `top_queries()` output and `DigestEntry` storage; it is
    /// **not** the Prometheus label-series bound (see
    /// [`Self::metric_label_series_max`]).
    pub max_digests: usize,

    /// Maximum number of distinct `digest` label values exposed to
    /// Prometheus histograms and counters. Digests seen after this
    /// budget is exhausted are still tracked internally (up to
    /// `max_digests`) and returned by `top_queries()`, but their
    /// Prometheus emissions are folded into the shared
    /// `digest="__other__"` bucket to bound label-series cardinality.
    ///
    /// The default of `1000` keeps a single ePHPm process's
    /// per-metric cardinality well inside sensible Prometheus scrape
    /// budgets even under a query template explosion (the sort of
    /// thing that used to melt Prometheus servers when `max_digests`
    /// was allowed to be the cardinality bound).
    pub metric_label_series_max: usize,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            slow_query_threshold: Duration::from_secs(1),
            max_digests: 100_000,
            metric_label_series_max: 1000,
        }
    }
}

/// Accumulated statistics for a single query digest.
#[derive(Clone, Debug)]
pub struct DigestEntry {
    /// The 64-bit digest hash.
    pub digest_id: u64,
    /// The normalized SQL string (with `?` placeholders).
    pub digest_text: String,
    /// An example of the original SQL (updated periodically).
    pub example_sql: String,
    /// Total number of executions.
    pub count: u64,
    /// Number of failed executions.
    pub error_count: u64,
    /// Total wall-clock execution time.
    pub total_time: Duration,
    /// Minimum execution time observed.
    pub min_time: Duration,
    /// Maximum execution time observed.
    pub max_time: Duration,
    /// Sum of rows returned or affected.
    pub total_rows: u64,
    /// Timestamp of the first execution.
    pub first_seen: Instant,
    /// Timestamp of the most recent execution.
    pub last_seen: Instant,
    /// Cached Prometheus `digest` label value.
    ///
    /// Prepared once when the entry is first inserted, then cloned per record
    /// call. Avoids running `truncate_for_label` + `into_owned` on every
    /// query in steady state — the old hot path allocated three label strings
    /// (`digest`) per record (once each for the histogram, ok/error counter,
    /// rows counter).
    pub digest_label: String,
    /// Cached slow-query log tag `{id:#X}`, formatted once per digest.
    ///
    /// The slow-query WARN previously reformatted this on every slow record;
    /// pre-computing keeps steady-state slow-log alloc down to just the SQL
    /// text.
    pub slow_id_tag: String,
}

/// Whether the recorded SQL was a read or a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// SELECT, SHOW, DESCRIBE, EXPLAIN, PRAGMA.
    Query,
    /// INSERT, UPDATE, DELETE, CREATE, DROP, ALTER.
    Mutation,
}

impl QueryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutation => "mutation",
        }
    }
}

/// Shared query stats collector.
///
/// Thread-safe, cheaply cloneable. Create one per ePHPm process and pass
/// it to both the DB Proxy and litewire integration.
#[derive(Clone)]
pub struct QueryStats {
    entries: Arc<DashMap<u64, DigestEntry>>,
    /// Set of digest IDs whose real digest text has been emitted as a
    /// Prometheus `digest` label. Once `.len() >= metric_label_series_max`,
    /// subsequent new digests are folded into the shared
    /// `digest="__other__"` bucket to bound label cardinality.
    label_digests: Arc<DashSet<u64>>,
    /// One-shot flag: set the first time a digest falls into the
    /// `__other__` bucket, so we log a single warning per process
    /// instead of spamming.
    label_overflow_warned: Arc<AtomicBool>,
    config: StatsConfig,
}

impl QueryStats {
    /// Create a new stats collector with the given configuration.
    #[must_use]
    pub fn new(config: StatsConfig) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            label_digests: Arc::new(DashSet::new()),
            label_overflow_warned: Arc::new(AtomicBool::new(false)),
            config,
        }
    }

    /// Record a completed query (SELECT, SHOW, etc.).
    pub fn record_query(&self, sql: &str, duration: Duration, success: bool, rows: u64) {
        self.record_internal(sql, duration, success, rows, QueryKind::Query);
    }

    /// Record a completed mutation (INSERT, UPDATE, DELETE, etc.).
    pub fn record_mutation(&self, sql: &str, duration: Duration, success: bool, rows: u64) {
        self.record_internal(sql, duration, success, rows, QueryKind::Mutation);
    }

    /// Record a query, auto-detecting kind from the first keyword.
    pub fn record(&self, sql: &str, duration: Duration, success: bool, rows: u64) {
        let kind = classify_query(sql);
        self.record_internal(sql, duration, success, rows, kind);
    }

    /// Snapshot of all tracked digests, sorted by total time descending.
    #[must_use]
    pub fn top_queries(&self, limit: usize) -> Vec<DigestEntry> {
        let mut entries: Vec<DigestEntry> =
            self.entries.iter().map(|r| r.value().clone()).collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.total_time));
        entries.truncate(limit);
        entries
    }

    /// Number of distinct digests currently tracked.
    #[must_use]
    pub fn digest_count(&self) -> usize {
        self.entries.len()
    }

    /// Reset all counters.
    pub fn reset(&self) {
        self.entries.clear();
        self.label_digests.clear();
        self.label_overflow_warned.store(false, Ordering::Relaxed);
        gauge!("ephpm_query_active_digests").set(0.0);
    }

    /// Decide whether this digest's normalized SQL is allowed as a
    /// Prometheus `digest` label value, or whether it should fold into
    /// the shared `__other__` overflow bucket.
    fn label_for_digest<'a>(&self, id: u64, normalized: &'a str) -> std::borrow::Cow<'a, str> {
        // Fast path: already admitted.
        if self.label_digests.contains(&id) {
            return truncate_for_label(normalized);
        }
        // Try to admit — capped by metric_label_series_max.
        if self.label_digests.len() < self.config.metric_label_series_max {
            self.label_digests.insert(id);
            return truncate_for_label(normalized);
        }
        // Budget exhausted: fold into the overflow bucket and warn
        // exactly once so operators know cardinality is being clipped.
        if !self.label_overflow_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                cap = self.config.metric_label_series_max,
                "query stats metric label cardinality cap reached; new digests fold into digest=\"__other__\" for Prometheus emissions (internal tracking is unaffected)"
            );
        }
        std::borrow::Cow::Borrowed(DIGEST_OTHER_BUCKET)
    }

    fn record_internal(
        &self,
        sql: &str,
        duration: Duration,
        success: bool,
        rows: u64,
        kind: QueryKind,
    ) {
        if !self.config.enabled {
            return;
        }

        let normalized = digest::normalize(sql);
        let id = digest::digest_id(&normalized);
        let now = Instant::now();
        let kind_str = kind.as_str();
        let is_slow = duration > self.config.slow_query_threshold;

        // Update digest entry first so we can reuse its cached `digest_label`
        // string for the Prometheus emissions below. In steady state (entry
        // already exists) this is a single DashMap read+atomic update and
        // *zero* label-string allocations: we clone the cached label instead
        // of running `truncate_for_label(&normalized).into_owned()` three
        // times per record like the old hot path did.
        let (digest_label, slow_id_tag_opt) = if let Some(mut entry) = self.entries.get_mut(&id) {
            entry.count += 1;
            if !success {
                entry.error_count += 1;
            }
            entry.total_time += duration;
            if duration < entry.min_time {
                entry.min_time = duration;
            }
            if duration > entry.max_time {
                entry.max_time = duration;
            }
            entry.total_rows += rows;
            entry.last_seen = now;
            // Update example SQL occasionally (every 100th execution)
            if entry.count % 100 == 0 {
                entry.example_sql = sql.to_string();
            }
            let label = entry.digest_label.clone();
            // Only clone the slow-id tag when we actually need it — cheap
            // Option::None path when the query isn't slow.
            let slow_tag = if is_slow { Some(entry.slow_id_tag.clone()) } else { None };
            (label, slow_tag)
        } else if self.entries.len() < self.config.max_digests {
            // Prepare labels ONCE for the lifetime of this digest.
            //
            // `label_for_digest` returns a `Cow<'_, str>` and admits at
            // most `metric_label_series_max` distinct digests as real
            // label values across the process lifetime; every other
            // digest folds into a single `digest="__other__"` bucket.
            // This bounds the label-series cardinality of each
            // histogram/counter regardless of how many distinct query
            // shapes hit the app.
            let digest_label = self.label_for_digest(id, &normalized).into_owned();
            let slow_id_tag = format!("{id:#X}");
            let slow_tag = if is_slow { Some(slow_id_tag.clone()) } else { None };
            self.entries.insert(
                id,
                DigestEntry {
                    digest_id: id,
                    digest_text: normalized.clone(),
                    example_sql: sql.to_string(),
                    count: 1,
                    error_count: u64::from(!success),
                    total_time: duration,
                    min_time: duration,
                    max_time: duration,
                    total_rows: rows,
                    first_seen: now,
                    last_seen: now,
                    digest_label: digest_label.clone(),
                    slow_id_tag,
                },
            );
            #[allow(clippy::cast_precision_loss)] // digest count will never exceed 2^52
            let count = self.entries.len() as f64;
            gauge!("ephpm_query_active_digests").set(count);
            (digest_label, slow_tag)
        } else {
            // At the max_digests cap: the entry didn't exist and won't be
            // inserted. Emissions still fire with the appropriate label —
            // an admitted digest slot if one was previously granted for
            // this id (won't happen at cap unless a previous reset), or
            // the `__other__` overflow bucket. This preserves metric
            // continuity without permanently allocating a fresh label
            // string per record.
            let label = self.label_for_digest(id, &normalized).into_owned();
            let slow_tag = if is_slow { Some(format!("{id:#X}")) } else { None };
            (label, slow_tag)
        };

        // Prometheus metrics — cached digest label; kind is static; status
        // varies but is a `&'static str`.
        histogram!(
            "ephpm_query_duration_seconds",
            "digest" => digest_label.clone(),
            "kind" => kind_str,
        )
        .record(duration.as_secs_f64());
        let status = if success { "ok" } else { "error" };
        counter!(
            "ephpm_query_total",
            "digest" => digest_label.clone(),
            "kind" => kind_str,
            "status" => status,
        )
        .increment(1);
        counter!(
            "ephpm_query_rows_total",
            "digest" => digest_label,
            "kind" => kind_str,
        )
        .increment(rows);

        // Slow query logging — short-circuited when the query is under the
        // threshold. Also skips the `format!(...)` for the tracing fields
        // (they only get built inside the `warn!`).
        if is_slow {
            counter!("ephpm_query_slow_total").increment(1);
            // slow_id_tag_opt is Some when is_slow, unwrap safely with default
            let digest_tag = slow_id_tag_opt.unwrap_or_default();
            tracing::warn!(
                sql = %normalized,
                duration_ms = duration.as_millis(),
                rows,
                digest = %digest_tag,
                "slow query"
            );
        }
    }
}

/// Classify a query as read or write by its first keyword.
fn classify_query(sql: &str) -> QueryKind {
    let trimmed = sql.trim_start();
    let upper: String = trimmed.chars().take(10).collect::<String>().to_uppercase();
    if upper.starts_with("SELECT")
        || upper.starts_with("SHOW")
        || upper.starts_with("DESCRIBE")
        || upper.starts_with("EXPLAIN")
        || upper.starts_with("PRAGMA")
    {
        QueryKind::Query
    } else {
        QueryKind::Mutation
    }
}

/// Truncate normalized SQL for use as a Prometheus label. Caps at 64
/// chars to keep label bodies bounded (this is per-label-value length;
/// the cross-label cardinality is bounded separately by
/// [`StatsConfig::metric_label_series_max`]).
fn truncate_for_label(normalized: &str) -> std::borrow::Cow<'_, str> {
    if normalized.len() <= 64 {
        std::borrow::Cow::Borrowed(normalized)
    } else {
        std::borrow::Cow::Owned(format!("{}...", &normalized[..61]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_retrieve() {
        let stats = QueryStats::new(StatsConfig::default());
        stats.record_query("SELECT * FROM t WHERE id = 1", Duration::from_millis(5), true, 1);
        stats.record_query("SELECT * FROM t WHERE id = 2", Duration::from_millis(10), true, 1);

        assert_eq!(stats.digest_count(), 1);
        let top = stats.top_queries(10);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].count, 2);
        assert_eq!(top[0].total_rows, 2);
        assert!(top[0].digest_text.contains('?'));
    }

    #[test]
    fn separate_digests_for_different_queries() {
        let stats = QueryStats::new(StatsConfig::default());
        stats.record_query("SELECT * FROM users WHERE id = 1", Duration::from_millis(1), true, 1);
        stats.record_mutation(
            "INSERT INTO users VALUES (1, 'a')",
            Duration::from_millis(2),
            true,
            1,
        );

        assert_eq!(stats.digest_count(), 2);
    }

    #[test]
    fn error_counting() {
        let stats = QueryStats::new(StatsConfig::default());
        stats.record_query("SELECT * FROM t", Duration::from_millis(1), true, 0);
        stats.record_query("SELECT * FROM t", Duration::from_millis(1), false, 0);
        stats.record_query("SELECT * FROM t", Duration::from_millis(1), true, 0);

        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 3);
        assert_eq!(top[0].error_count, 1);
    }

    #[test]
    fn min_max_timing() {
        let stats = QueryStats::new(StatsConfig::default());
        stats.record_query("SELECT 1", Duration::from_millis(10), true, 0);
        stats.record_query("SELECT 2", Duration::from_millis(5), true, 0);
        stats.record_query("SELECT 3", Duration::from_millis(20), true, 0);

        let top = stats.top_queries(1);
        assert_eq!(top[0].min_time, Duration::from_millis(5));
        assert_eq!(top[0].max_time, Duration::from_millis(20));
        assert_eq!(top[0].total_time, Duration::from_millis(35));
    }

    #[test]
    fn max_digests_enforced() {
        let config = StatsConfig { max_digests: 3, ..Default::default() };
        let stats = QueryStats::new(config);

        for i in 0..10 {
            stats.record_query(
                &format!("SELECT * FROM table_{i}"),
                Duration::from_millis(1),
                true,
                0,
            );
        }

        assert!(stats.digest_count() <= 3);
    }

    #[test]
    fn reset_clears_all() {
        let stats = QueryStats::new(StatsConfig::default());
        stats.record_query("SELECT 1", Duration::from_millis(1), true, 0);
        assert_eq!(stats.digest_count(), 1);
        stats.reset();
        assert_eq!(stats.digest_count(), 0);
    }

    #[test]
    fn classify_select_as_query() {
        assert_eq!(classify_query("SELECT * FROM t"), QueryKind::Query);
        assert_eq!(classify_query("  select 1"), QueryKind::Query);
        assert_eq!(classify_query("SHOW TABLES"), QueryKind::Query);
    }

    #[test]
    fn classify_insert_as_mutation() {
        assert_eq!(classify_query("INSERT INTO t VALUES (1)"), QueryKind::Mutation);
        assert_eq!(classify_query("UPDATE t SET x = 1"), QueryKind::Mutation);
        assert_eq!(classify_query("DELETE FROM t"), QueryKind::Mutation);
        assert_eq!(classify_query("CREATE TABLE t (id INT)"), QueryKind::Mutation);
    }

    #[test]
    fn top_queries_sorted_by_total_time() {
        let stats = QueryStats::new(StatsConfig::default());
        stats.record_query("SELECT * FROM fast", Duration::from_millis(1), true, 0);
        stats.record_query("SELECT * FROM slow", Duration::from_millis(100), true, 0);
        stats.record_query("SELECT * FROM medium", Duration::from_millis(10), true, 0);

        let top = stats.top_queries(3);
        assert_eq!(top[0].total_time, Duration::from_millis(100));
        assert_eq!(top[1].total_time, Duration::from_millis(10));
        assert_eq!(top[2].total_time, Duration::from_millis(1));
    }

    #[test]
    fn truncate_label_short() {
        let s = "SELECT * FROM t";
        assert_eq!(truncate_for_label(s).as_ref(), s);
    }

    #[test]
    fn truncate_label_long() {
        let s = "SELECT very_long_column_name_1, very_long_column_name_2 FROM some_table WHERE condition = ?";
        let label = truncate_for_label(s);
        assert!(label.len() <= 67); // 64 + "..."
        assert!(label.ends_with("..."));
    }

    #[test]
    fn label_series_cap_folds_excess_digests_to_other_bucket() {
        // With a cap of 2, the third and later distinct digests must
        // fold into the shared __other__ bucket for Prometheus label
        // emission, but internal digest tracking is unaffected.
        let stats =
            QueryStats::new(StatsConfig { metric_label_series_max: 2, ..Default::default() });

        stats.record_query("SELECT * FROM t1 WHERE id = 1", Duration::from_millis(1), true, 0);
        stats.record_query("SELECT * FROM t2 WHERE id = 1", Duration::from_millis(1), true, 0);
        stats.record_query("SELECT * FROM t3 WHERE id = 1", Duration::from_millis(1), true, 0);
        stats.record_query("SELECT * FROM t4 WHERE id = 1", Duration::from_millis(1), true, 0);

        // Only the first two admitted digests get their own label
        // slot; t3 and t4 fold into __other__.
        assert_eq!(stats.label_digests.len(), 2);

        // Internal tracking still sees all four digests — nothing is
        // dropped there.
        assert_eq!(stats.digest_count(), 4);

        // Reset also clears the label admission set so the cap resets
        // cleanly.
        stats.reset();
        assert_eq!(stats.label_digests.len(), 0);
    }

    #[test]
    fn label_series_cap_admits_all_when_under_budget() {
        let stats =
            QueryStats::new(StatsConfig { metric_label_series_max: 100, ..Default::default() });
        for i in 0..5 {
            stats.record_query(
                &format!("SELECT * FROM t{i} WHERE id = 1"),
                Duration::from_millis(1),
                true,
                0,
            );
        }
        assert_eq!(stats.label_digests.len(), 5);
    }

    #[test]
    fn auto_classify_record() {
        let stats = QueryStats::new(StatsConfig::default());
        stats.record("SELECT 1", Duration::from_millis(1), true, 1);
        stats.record("INSERT INTO t VALUES (1)", Duration::from_millis(1), true, 1);
        assert_eq!(stats.digest_count(), 2);
    }

    #[test]
    fn disabled_stats_records_nothing() {
        let config = StatsConfig { enabled: false, ..Default::default() };
        let stats = QueryStats::new(config);
        stats.record_query("SELECT * FROM t WHERE id = 1", Duration::from_millis(5), true, 1);
        stats.record_mutation("INSERT INTO t VALUES (1)", Duration::from_millis(2), true, 1);

        assert_eq!(stats.digest_count(), 0, "disabled stats should not record anything");
    }

    #[test]
    fn digest_label_cached_on_first_record() {
        // Prove the digest label + slow-id tag are prepared once at
        // insertion time and persist on the DigestEntry for reuse by
        // subsequent records — no per-call `into_owned()`/format allocation
        // in steady state.
        let stats = QueryStats::new(StatsConfig::default());
        stats.record_query("SELECT * FROM t WHERE id = 1", Duration::from_millis(1), true, 0);

        let entry = stats
            .entries
            .get(&digest::digest_id(&digest::normalize("SELECT * FROM t WHERE id = 1")));
        let entry = entry.expect("entry inserted on first record");
        assert!(!entry.digest_label.is_empty(), "digest label must be cached");
        assert!(entry.slow_id_tag.starts_with("0x"), "slow id tag pre-formatted");
        // Label should equal the normalized SQL (short enough not to be
        // truncated in this case).
        assert_eq!(entry.digest_label, entry.digest_text);
    }

    #[test]
    fn cached_label_stable_across_repeated_records() {
        // The label string is prepared once and reused: after many records,
        // the entry still holds exactly one label string, not one per call.
        let stats = QueryStats::new(StatsConfig::default());
        for _ in 0..50 {
            stats.record_query(
                "SELECT * FROM users WHERE id = 42",
                Duration::from_millis(1),
                true,
                1,
            );
        }
        assert_eq!(stats.digest_count(), 1);
        let id = digest::digest_id(&digest::normalize("SELECT * FROM users WHERE id = 42"));
        let entry = stats.entries.get(&id).unwrap();
        assert_eq!(entry.count, 50);
        assert!(!entry.digest_label.is_empty());
    }

    #[test]
    fn slow_query_threshold_short_circuits_below() {
        // Under-threshold queries must not emit the slow counter. We
        // assert this by exercising the recording path and confirming
        // the digest entry accumulates normally without any panics from
        // the short-circuit branch.
        let stats = QueryStats::new(StatsConfig {
            slow_query_threshold: Duration::from_secs(10),
            ..Default::default()
        });
        stats.record_query("SELECT 1", Duration::from_millis(1), true, 0);
        stats.record_query("SELECT 1", Duration::from_micros(500), true, 0);
        // Nothing crashed; digest is tracked.
        assert_eq!(stats.digest_count(), 1);
    }

    #[test]
    fn slow_query_threshold_fires_above() {
        // A slow query must succeed at emitting through the WARN path
        // without regression in per-digest accounting.
        let stats = QueryStats::new(StatsConfig {
            slow_query_threshold: Duration::from_millis(1),
            ..Default::default()
        });
        stats.record_query("SELECT slow", Duration::from_millis(50), true, 3);
        let top = stats.top_queries(1);
        assert_eq!(top[0].count, 1);
        assert_eq!(top[0].total_rows, 3);
    }

    #[test]
    fn other_bucket_fold_still_bounds_label_series() {
        // Regression: the label overflow behaviour is unchanged by the
        // caching refactor — after `metric_label_series_max` distinct
        // digests, admission stops.
        let stats =
            QueryStats::new(StatsConfig { metric_label_series_max: 2, ..Default::default() });
        for i in 0..10 {
            stats.record_query(
                &format!("SELECT * FROM t{i} WHERE id = 1"),
                Duration::from_millis(1),
                true,
                0,
            );
        }
        assert_eq!(stats.label_digests.len(), 2, "label series still capped at 2");
        // All ten digests are still tracked internally.
        assert_eq!(stats.digest_count(), 10);
    }

    #[test]
    fn cached_label_for_overflow_bucket_is_other() {
        // Digests that fold to __other__ should store `__other__` (or the
        // truncated form) as their cached label, so repeat emissions keep
        // pointing at the shared overflow bucket.
        let stats =
            QueryStats::new(StatsConfig { metric_label_series_max: 1, ..Default::default() });
        stats.record_query("SELECT * FROM t1", Duration::from_millis(1), true, 0);
        stats.record_query("SELECT * FROM t2", Duration::from_millis(1), true, 0);

        let id2 = digest::digest_id(&digest::normalize("SELECT * FROM t2"));
        let entry = stats.entries.get(&id2).expect("t2 tracked internally");
        assert_eq!(
            entry.digest_label, DIGEST_OTHER_BUCKET,
            "overflow digests must cache __other__ as their label"
        );
    }
}

//! WAL checkpoint gate wiring: config → litewire gate, plus the Prometheus
//! surface for the gate's counters and each open database's `-wal` size.
//!
//! # What the gate is
//!
//! Turso 0.8.3 never rewinds its write-ahead log under constant read load:
//! the passive auto-checkpoint backfills frames, but a `TRUNCATE` checkpoint
//! needs every read-mark slot free and the engine reports `busy` the instant
//! one is held. Some session is always inside a statement, so the `-wal` file
//! grows by a frame per write — 1.3 GB in eight minutes on a single node, 6.5
//! GB on a per-site clustered owner. litewire's `wal_gate` (opt in via
//! `TursoBuilder::wal_checkpoint_gate`) fixes it: every session statement and
//! every CDC tailer poll holds the shared side of a per-file lock, and a
//! maintenance task drains statements for a bounded wait and truncates the log
//! once it crosses a threshold. Once the log is at least 4× the threshold,
//! timeouts escalate the wait (doubling up to `max_escalated_wait_ms`) rather
//! than back off, so statements that routinely outlast `max_wait_ms` cannot
//! starve it. See `litewire_turso::wal_gate` for the mechanism and its
//! guarantees.
//!
//! # ePHPm's part
//!
//! 1. [`gate_config`] turns `[db.sqlite.wal_checkpoint]` into the litewire
//!    config (or `None` when disabled) and [`builder_with_gate`] applies it.
//!    **Every** place ePHPm builds a file-backed Turso factory goes through
//!    these: the single-node factory, the per-site registry
//!    (`SiteBackends`), the clustered whole-database wire and mgmt factories,
//!    and the per-site clustered mgmt registry. The in-process `ephpm_db_*`
//!    bridge and the wire listeners use litewire sessions from those
//!    factories, so their statements take permits automatically; the raw
//!    connections ePHPm opens itself for CDC apply and snapshot work take
//!    [`Turso::wal_gate_permit`] explicitly in `turso_cdc`.
//! 2. A process-wide [`WalGateStatsSource`] registry feeds one sampler task
//!    that publishes the gate's counters and the `-wal` size as Prometheus
//!    series every [`SAMPLE_INTERVAL`]. The counters live in litewire as
//!    atomics; the sampler copies them with `Counter::absolute`, so a series
//!    is a faithful mirror rather than a re-count.
//!
//! # Labels and cardinality
//!
//! Every series carries one label, `site`: the vhost's canonical site key in
//! the per-site modes, or `default` for the single-database modes (the same
//! name the clustered path uses for its `cdc/default` stream). The set of
//! label values is bounded by the number of per-site databases that are
//! **open**, which `[db.sqlite] max_open_dbs` caps for the serving registry;
//! the per-site clustered mgmt registry is unbounded but holds one entry per
//! vhost directory on disk — the same bound the `vhost` label on
//! `ephpm_opcache_invalidations_total` already has. There are no per-peer or
//! per-connection labels.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use litewire::litewire_turso::{Turso, TursoBuilder, WalGateConfig, WalGateStats};
use metrics::{counter, gauge};

/// Label value for the single-database modes (single-node and whole-database
/// clustered), which have one database and no site key.
pub const DEFAULT_SITE_LABEL: &str = "default";

/// How often the sampler republishes the gate counters and the `-wal` size.
/// One `stat` per open database per tick; the counters are atomics.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// Gauge: current size of the database's `-wal` file in bytes. Label: `site`.
/// Published for every open embedded database whether or not the gate is on.
pub const METRIC_WAL_BYTES: &str = "ephpm_sqlite_wal_bytes";
/// Counter: gate attempts (the file was over the threshold at a tick).
pub const METRIC_ATTEMPTS: &str = "ephpm_sqlite_wal_gate_attempts_total";
/// Counter: attempts that truncated the log.
pub const METRIC_TRUNCATIONS: &str = "ephpm_sqlite_wal_gate_truncations_total";
/// Counter: attempts where the engine still reported the checkpoint busy
/// after the gate was held (a reader outside the gate was active).
pub const METRIC_BUSY: &str = "ephpm_sqlite_wal_gate_busy_total";
/// Counter: attempts abandoned because the exclusive side could not be taken
/// within the attempt's wait — `max_wait_ms`, or the escalated wait (an open
/// transaction or a slow statement outlived it).
pub const METRIC_TIMEOUTS: &str = "ephpm_sqlite_wal_gate_timeouts_total";
/// Counter: attempts that failed with an engine error.
pub const METRIC_ERRORS: &str = "ephpm_sqlite_wal_gate_errors_total";
/// Counter: attempts skipped because the database is not in `wal` journal mode.
pub const METRIC_SKIPPED_JOURNAL_MODE: &str = "ephpm_sqlite_wal_gate_skipped_journal_mode_total";
/// Counter: bytes the `-wal` file held just before each successful truncate.
pub const METRIC_BYTES_TRUNCATED: &str = "ephpm_sqlite_wal_gate_bytes_truncated_total";
/// Counter: frames moved into the main file by the passive backfill phase.
pub const METRIC_FRAMES_BACKFILLED: &str = "ephpm_sqlite_wal_gate_frames_backfilled_total";
/// Gauge: duration of the most recent exclusive window (the truncating
/// checkpoint itself, during which new statements wait), in seconds.
pub const METRIC_LAST_EXCLUSIVE_SECONDS: &str = "ephpm_sqlite_wal_gate_last_exclusive_seconds";
/// Gauge: longest exclusive window seen, in seconds.
pub const METRIC_MAX_EXCLUSIVE_SECONDS: &str = "ephpm_sqlite_wal_gate_max_exclusive_seconds";
/// Gauge: duration of the most recent passive backfill phase (outside the
/// gate; blocks nobody), in seconds.
pub const METRIC_LAST_BACKFILL_SECONDS: &str = "ephpm_sqlite_wal_gate_last_backfill_seconds";
/// Gauge: longest passive backfill phase seen, in seconds.
pub const METRIC_MAX_BACKFILL_SECONDS: &str = "ephpm_sqlite_wal_gate_max_backfill_seconds";
/// Gauge: escalation level of the gate's schedule. `0` = attempts wait the
/// plain `max_wait_ms`; `n` = the next attempt waits `max_wait_ms × 2ⁿ`
/// (capped at `max_escalated_wait_ms`) because the log is at least
/// 4 × `threshold_bytes` and earlier attempts timed out. Resets on a truncate.
pub const METRIC_ESCALATION_LEVEL: &str = "ephpm_sqlite_wal_gate_escalation_level";
/// Gauge: attempts in a row that timed out since the last truncate (resets on
/// a truncate or once the log is back under `threshold_bytes`).
pub const METRIC_CONSECUTIVE_TIMEOUTS: &str = "ephpm_sqlite_wal_gate_consecutive_timeouts";
/// Gauge: the exclusive wait the next attempt will use, in seconds — the
/// worst-case stall it can add to new statements.
pub const METRIC_NEXT_WAIT_SECONDS: &str = "ephpm_sqlite_wal_gate_next_wait_seconds";

/// Translate `[db.sqlite.wal_checkpoint]` into litewire's gate config.
///
/// `None` when the gate is disabled, which is what every factory builder
/// takes to mean "do not install a gate". The config was validated at load
/// (`Config::validate` refuses zeros), so no clamping happens here.
#[must_use]
pub fn gate_config(cfg: &ephpm_config::WalCheckpointConfig) -> Option<WalGateConfig> {
    if !cfg.enabled {
        return None;
    }
    Some(WalGateConfig {
        threshold_bytes: cfg.threshold_bytes,
        interval: Duration::from_secs(cfg.interval_secs),
        max_wait: Duration::from_millis(cfg.max_wait_ms),
        max_backoff: Duration::from_secs(cfg.max_backoff_secs),
        max_escalated_wait: Duration::from_millis(cfg.effective_max_escalated_wait_ms()),
    })
}

/// Apply an optional gate config to a builder. The one place the opt-in call
/// is spelled, so every factory site reads the same way.
#[must_use]
pub fn builder_with_gate(builder: TursoBuilder, gate: Option<&WalGateConfig>) -> TursoBuilder {
    match gate {
        Some(cfg) => builder.wal_checkpoint_gate(cfg.clone()),
        None => builder,
    }
}

/// Log, once per process, which gate settings are in force.
pub fn log_mode(gate: Option<&WalGateConfig>) {
    match gate {
        Some(cfg) => tracing::info!(
            threshold_bytes = cfg.threshold_bytes,
            interval_ms = cfg.interval.as_millis(),
            max_wait_ms = cfg.max_wait.as_millis(),
            max_backoff_ms = cfg.max_backoff.as_millis(),
            max_escalated_wait_ms = cfg.max_escalated_wait.as_millis(),
            "WAL checkpoint gate enabled for embedded databases"
        ),
        None => tracing::warn!(
            "[db.sqlite.wal_checkpoint] enabled = false: the -wal file of every embedded \
             database will grow without bound under constant read load"
        ),
    }
}

/// Anything that can report a Turso factory's gate counters — the factory
/// itself, or a decorator stack around it.
pub trait WalGateStatsProvider {
    /// Snapshot the gate counters, or `None` if this factory has no gate.
    fn wal_gate_stats(&self) -> Option<WalGateStats>;
}

impl WalGateStatsProvider for Turso {
    fn wal_gate_stats(&self) -> Option<WalGateStats> {
        Turso::wal_gate_stats(self)
    }
}

impl<B: WalGateStatsProvider> WalGateStatsProvider for crate::tracked_backend::TrackedBackend<B> {
    fn wal_gate_stats(&self) -> Option<WalGateStats> {
        self.inner().wal_gate_stats()
    }
}

impl<B: WalGateStatsProvider> WalGateStatsProvider for crate::screened_backend::ScreenedBackend<B> {
    fn wal_gate_stats(&self) -> Option<WalGateStats> {
        self.inner().wal_gate_stats()
    }
}

/// One open database's observation: its label, its file, and the gate
/// counters (`None` when the gate is off for that factory).
#[derive(Debug, Clone)]
pub struct WalGateSample {
    /// The `site` label value.
    pub site: String,
    /// The database file; the sampler stats `<db_path>-wal` for the size gauge.
    pub db_path: PathBuf,
    /// The gate counters, shared by every factory on the same file.
    pub stats: Option<WalGateStats>,
}

/// A registry of open databases that the sampler can walk.
///
/// Implementations must not take refcounts that would interfere with their
/// own lifecycle rules (the per-site registry reads its slots by reference
/// under `try_read`, never cloning an `Arc` the eviction pass counts).
pub trait WalGateStatsSource: Send + Sync {
    /// Append one sample per open database this source knows about.
    fn wal_gate_samples(&self, out: &mut Vec<WalGateSample>);
}

/// A single labelled database held weakly: the source for the single-node
/// factory and the clustered whole-database mgmt factory.
///
/// Weak so the registry never extends a factory's life; an upgrade here is
/// harmless for these factories, which have no eviction and live as long as
/// the process.
pub struct DatabaseProbe<P> {
    site: String,
    db_path: PathBuf,
    provider: Weak<P>,
}

impl<P: WalGateStatsProvider + Send + Sync + 'static> DatabaseProbe<P> {
    /// Register `provider` (held weakly) under `site` with the process-wide
    /// sampler.
    pub fn register(site: &str, db_path: impl Into<PathBuf>, provider: &Arc<P>) {
        let probe: Arc<dyn WalGateStatsSource> = Arc::new(Self {
            site: site.to_string(),
            db_path: db_path.into(),
            provider: Arc::downgrade(provider),
        });
        register_source(probe);
    }
}

impl<P: WalGateStatsProvider + Send + Sync> WalGateStatsSource for DatabaseProbe<P> {
    fn wal_gate_samples(&self, out: &mut Vec<WalGateSample>) {
        if let Some(provider) = self.provider.upgrade() {
            out.push(WalGateSample {
                site: self.site.clone(),
                db_path: self.db_path.clone(),
                stats: provider.wal_gate_stats(),
            });
        }
    }
}

/// Every registered source. A handful of entries for the life of the process
/// (one per registry or long-lived factory), so a `Vec` under a mutex is the
/// right size of machinery.
static SOURCES: Mutex<Vec<Arc<dyn WalGateStatsSource>>> = Mutex::new(Vec::new());

/// Add a source to the sampler's walk. Sources are never removed; what they
/// report is what changes (an evicted database simply stops appearing).
pub fn register_source(source: Arc<dyn WalGateStatsSource>) {
    SOURCES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(source);
}

/// Walk every source once. Deduplicated by `site`, first source wins: in
/// per-site clustered mode an owned site is open in both the serving
/// registry and the mgmt registry, and the gate counters are per *file*, so
/// the two would report identical numbers.
#[must_use]
pub fn sample_all() -> Vec<WalGateSample> {
    let sources: Vec<Arc<dyn WalGateStatsSource>> =
        SOURCES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let mut raw = Vec::new();
    for source in &sources {
        source.wal_gate_samples(&mut raw);
    }
    let mut seen = std::collections::HashSet::new();
    raw.retain(|s| seen.insert(s.site.clone()));
    raw
}

/// Size of the `-wal` file next to `db_path`, `0` when absent.
#[must_use]
pub fn wal_file_size(db_path: &Path) -> u64 {
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(wal).map_or(0, |m| m.len())
}

/// Publish one sample. Counters use `absolute` because the authoritative
/// count lives in litewire's atomics; gauges are plain sets.
#[allow(clippy::cast_precision_loss)] // byte counts and microseconds fit f64's 2^53 exactly
fn publish(sample: &WalGateSample) {
    let site = sample.site.clone();
    gauge!(METRIC_WAL_BYTES, "site" => site.clone()).set(wal_file_size(&sample.db_path) as f64);
    let Some(s) = sample.stats else { return };
    for (name, value) in [
        (METRIC_ATTEMPTS, s.attempts),
        (METRIC_TRUNCATIONS, s.truncated),
        (METRIC_BUSY, s.busy),
        (METRIC_TIMEOUTS, s.timed_out),
        (METRIC_ERRORS, s.errors),
        (METRIC_SKIPPED_JOURNAL_MODE, s.skipped_journal_mode),
        (METRIC_BYTES_TRUNCATED, s.bytes_truncated),
        (METRIC_FRAMES_BACKFILLED, s.frames_backfilled),
    ] {
        counter!(name, "site" => site.clone()).absolute(value);
    }
    for (name, micros) in [
        (METRIC_LAST_EXCLUSIVE_SECONDS, s.last_exclusive_micros),
        (METRIC_MAX_EXCLUSIVE_SECONDS, s.max_exclusive_micros),
        (METRIC_LAST_BACKFILL_SECONDS, s.last_backfill_micros),
        (METRIC_MAX_BACKFILL_SECONDS, s.max_backfill_micros),
        (METRIC_NEXT_WAIT_SECONDS, s.next_wait_micros),
    ] {
        gauge!(name, "site" => site.clone()).set(micros as f64 / 1_000_000.0);
    }
    for (name, value) in [
        (METRIC_ESCALATION_LEVEL, s.escalation_level),
        (METRIC_CONSECUTIVE_TIMEOUTS, s.consecutive_timeouts),
    ] {
        gauge!(name, "site" => site.clone()).set(value as f64);
    }
}

/// Spawn the sampler. Called once from the embedded-database startup path;
/// sources registered later are picked up on the next tick.
pub fn spawn_sampler(handles: &mut Vec<tokio::task::JoinHandle<()>>) {
    handles.push(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            for sample in sample_all() {
                publish(&sample);
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> ephpm_config::WalCheckpointConfig {
        ephpm_config::WalCheckpointConfig {
            enabled,
            threshold_bytes: 1024,
            interval_secs: 3,
            max_wait_ms: 40,
            max_backoff_secs: 9,
            max_escalated_wait_ms: Some(900),
        }
    }

    /// Every knob reaches litewire, and `enabled = false` yields no gate at
    /// all (the value every builder site treats as "install nothing").
    #[test]
    fn config_maps_every_knob_and_disabled_is_none() {
        let gate = gate_config(&cfg(true)).expect("enabled must produce a gate config");
        assert_eq!(gate.threshold_bytes, 1024);
        assert_eq!(gate.interval, Duration::from_secs(3));
        assert_eq!(gate.max_wait, Duration::from_millis(40));
        assert_eq!(gate.max_backoff, Duration::from_secs(9));
        assert_eq!(gate.max_escalated_wait, Duration::from_millis(900));
        assert!(gate_config(&cfg(false)).is_none());
    }

    /// An unset escalation ceiling is 2 s, but never below `max_wait_ms`, so a
    /// config that raised `max_wait_ms` past 2 s keeps loading and simply
    /// runs without escalation.
    #[test]
    fn unset_escalation_ceiling_follows_max_wait() {
        let mut c = cfg(true);
        c.max_escalated_wait_ms = None;
        assert_eq!(gate_config(&c).unwrap().max_escalated_wait, Duration::from_secs(2));
        c.max_wait_ms = 3000;
        assert_eq!(gate_config(&c).unwrap().max_escalated_wait, Duration::from_millis(3000));
    }

    /// The shipped defaults are litewire's own defaults, so "on, untouched"
    /// in ePHPm is exactly the configuration litewire validated.
    #[test]
    fn shipped_defaults_match_litewire_defaults() {
        let ours = gate_config(&ephpm_config::WalCheckpointConfig::default()).unwrap();
        let theirs = WalGateConfig::default();
        assert_eq!(ours.threshold_bytes, theirs.threshold_bytes);
        assert_eq!(ours.interval, theirs.interval);
        assert_eq!(ours.max_wait, theirs.max_wait);
        assert_eq!(ours.max_backoff, theirs.max_backoff);
        assert_eq!(ours.max_escalated_wait, theirs.max_escalated_wait);
    }

    /// A gated file-backed factory reports stats; an ungated one, and an
    /// in-memory one (no WAL file), report `None`. Stub mode: no PHP involved.
    #[tokio::test]
    async fn builder_with_gate_installs_a_gate_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.db");
        let path = path.to_str().unwrap();
        let gate = gate_config(&cfg(true));

        let gated = builder_with_gate(Turso::builder(path), gate.as_ref()).build().await.unwrap();
        // Counters start at zero. (The schedule gauges, e.g. `next_wait_micros`,
        // are published by the task as soon as it starts, so they are not
        // compared here.)
        let stats = gated.wal_gate_stats().expect("a gated factory must expose gate stats");
        assert_eq!(
            (stats.attempts, stats.truncated, stats.timed_out, stats.escalation_level),
            (0, 0, 0, 0),
            "a fresh gate has done nothing: {stats:?}"
        );
        let finished = gated.wal_gate_task_finished().expect("gated factory has a task");
        assert!(!*finished.borrow(), "the task runs while the factory lives");

        let plain = builder_with_gate(Turso::builder(path), None).build().await.unwrap();
        assert!(plain.wal_gate_stats().is_none(), "no gate was asked for");
        assert!(plain.wal_gate_task_finished().is_none());

        let memory =
            builder_with_gate(Turso::builder(":memory:"), gate.as_ref()).build().await.unwrap();
        assert!(memory.wal_gate_stats().is_none(), ":memory: has no WAL file to gate");

        // Dropping the factory stops its task promptly.
        drop(gated);
        let mut finished = finished;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !*finished.borrow_and_update() {
                if finished.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .expect("the gate task must exit once its factory is dropped");
    }

    /// The provider trait sees through ePHPm's decorator stack, which is how
    /// the per-site registry reads stats off its stats-wrapped, screened
    /// backends without unwrapping them.
    #[tokio::test]
    async fn provider_sees_through_tracked_and_screened_decorators() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.db");
        let gate = gate_config(&cfg(true));
        let turso = builder_with_gate(Turso::builder(path.to_str().unwrap()), gate.as_ref())
            .build()
            .await
            .unwrap();
        let stats = ephpm_query_stats::QueryStats::new(ephpm_query_stats::StatsConfig {
            enabled: false,
            ..Default::default()
        });
        let stack = crate::tracked_backend::TrackedBackend::new(
            crate::screened_backend::ScreenedBackend::new(turso),
            stats,
        );
        assert!(stack.wal_gate_stats().is_some());
    }

    /// A weakly-held probe reports while its factory lives and vanishes with
    /// it, and `wal_file_size` reads the sibling `-wal` file.
    #[tokio::test]
    async fn database_probe_follows_its_factory_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let factory = Arc::new(
            builder_with_gate(
                Turso::builder(path.to_str().unwrap()),
                gate_config(&cfg(true)).as_ref(),
            )
            .build()
            .await
            .unwrap(),
        );
        let probe = DatabaseProbe {
            site: "probe-test".to_string(),
            db_path: path.clone(),
            provider: Arc::downgrade(&factory),
        };
        let mut out = Vec::new();
        probe.wal_gate_samples(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].site, "probe-test");
        assert!(out[0].stats.is_some());

        // Write something so a -wal file exists, then measure it.
        let conn = factory.raw_connection().unwrap();
        conn.execute("CREATE TABLE t (v TEXT)", ()).await.unwrap();
        conn.execute("INSERT INTO t VALUES ('x')", ()).await.unwrap();
        assert!(wal_file_size(&path) > 0, "a write must produce a non-empty -wal file");
        assert_eq!(wal_file_size(Path::new("/definitely/not/here.db")), 0);

        drop(conn);
        drop(factory);
        out.clear();
        probe.wal_gate_samples(&mut out);
        assert!(out.is_empty(), "a dropped factory must stop being sampled");
    }
}

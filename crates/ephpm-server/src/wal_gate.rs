//! WAL checkpoint gate wiring: hardcoded settings → litewire gate, plus the
//! Prometheus surface for each open database's `-wal` size and the gate's
//! timeout count.
//!
//! # A temporary workaround — not a feature
//!
//! Turso 0.8.3 never rewinds its write-ahead log under constant read load:
//! the passive auto-checkpoint backfills frames, but a `TRUNCATE` checkpoint
//! needs every read-mark slot free and the engine reports `busy` the instant
//! one is held instead of waiting for it. Some session is always inside a
//! statement, so the `-wal` file grows by a frame per write — 1.3 GB in eight
//! minutes on a single node, 6.5 GB on a per-site clustered owner. litewire's
//! `wal_gate` (opt in via `TursoBuilder::wal_checkpoint_gate`) works around
//! it: every session statement and every CDC tailer poll holds the shared
//! side of a per-file lock, and a maintenance task drains statements for a
//! bounded wait and truncates the log once it crosses a threshold. Once the
//! log is at least 4× the threshold, timeouts escalate the wait (doubling up
//! to [`MAX_ESCALATED_WAIT`]) rather than back off, so statements that
//! routinely outlast [`MAX_WAIT`] cannot starve it. See
//! `litewire_turso::wal_gate` for the mechanism and its guarantees.
//!
//! The gate is **always on and deliberately not configurable**: its settings
//! are the constants below (litewire's own validated defaults). Once Turso's
//! `TRUNCATE` checkpoint can wait for readers, this whole module is replaced
//! by a periodic `PRAGMA wal_checkpoint(TRUNCATE)`, and an operator-facing knob
//! would only be something to deprecate.
//!
//! # ePHPm's part
//!
//! 1. [`builder_with_gate`] installs the gate. **Every** place ePHPm builds a
//!    file-backed Turso factory goes through it: the single-node factory, the
//!    per-site registry (`SiteBackends`), the clustered whole-database wire
//!    and mgmt factories, and the per-site clustered mgmt registry. The
//!    in-process `ephpm_db_*` bridge and the wire listeners use litewire
//!    sessions from those factories, so their statements take permits
//!    automatically; the raw connections ePHPm opens itself for CDC apply and
//!    snapshot work take [`Turso::wal_gate_permit`] explicitly in `turso_cdc`.
//! 2. A process-wide [`WalGateStatsSource`] registry feeds one sampler task
//!    that publishes the `-wal` size and the gate's timeout count every
//!    [`SAMPLE_INTERVAL`]. The counter lives in litewire as an atomic; the
//!    sampler copies it with `Counter::absolute`, so the series is a faithful
//!    mirror rather than a re-count. The gate's other internals are
//!    deliberately not exported — they would disappear with the workaround.
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
use std::sync::{Arc, Mutex, Once, Weak};
use std::time::Duration;

use litewire::litewire_turso::{Turso, TursoBuilder, WalGateConfig, WalGateStats};
use metrics::{counter, gauge};

/// Label value for the single-database modes (single-node and whole-database
/// clustered), which have one database and no site key.
pub const DEFAULT_SITE_LABEL: &str = "default";

/// How often the sampler republishes the `-wal` size and the timeout count.
/// One `stat` per open database per tick; the counter is an atomic.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// Truncate once the `-wal` file is at least this many bytes (64 MiB): large
/// enough that the passive checkpoint has long since backfilled everything
/// but the tail, so the exclusive window has almost nothing left to copy.
const THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;
/// Seconds between size samples; also the base of the retry backoff.
const INTERVAL: Duration = Duration::from_secs(2);
/// Longest a plain attempt waits for in-flight statements to drain. New
/// statements queue behind it, so this is the worst-case stall one ordinary
/// attempt adds to a request.
const MAX_WAIT: Duration = Duration::from_millis(250);
/// Ceiling for the escalated wait once the log is ≥ 4 × [`THRESHOLD_BYTES`]
/// and attempts keep timing out — the worst-case stall while escalated. Don't
/// lower it back to [`MAX_WAIT`] without re-running the mixed-workload stress
/// (litewire#50: slow reads starved the plain schedule, 1.2 GiB `-wal`).
const MAX_ESCALATED_WAIT: Duration = Duration::from_secs(2);
/// Cap on the exponential retry backoff after a failed attempt.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Gauge: current size of the database's `-wal` file in bytes. Label: `site`.
pub const METRIC_WAL_BYTES: &str = "ephpm_sqlite_wal_bytes";
/// Counter: gate attempts abandoned because the exclusive side could not be
/// taken within the attempt's wait (an open transaction or a slow statement
/// outlived it). Label: `site`.
pub const METRIC_TIMEOUTS: &str = "ephpm_sqlite_wal_gate_timeouts_total";

/// The gate settings every factory gets.
fn gate_config() -> WalGateConfig {
    WalGateConfig {
        threshold_bytes: THRESHOLD_BYTES,
        interval: INTERVAL,
        max_wait: MAX_WAIT,
        max_backoff: MAX_BACKOFF,
        max_escalated_wait: MAX_ESCALATED_WAIT,
    }
}

/// Install the WAL checkpoint gate on a builder. The one place the opt-in
/// call is spelled, so every factory site reads the same way. (`:memory:`
/// builders accept it and simply get no gate — there is no WAL file.)
#[must_use]
pub fn builder_with_gate(builder: TursoBuilder) -> TursoBuilder {
    builder.wal_checkpoint_gate(gate_config())
}

/// Log, once per process, that the gate is active and with which settings.
pub fn log_active() {
    static LOGGED: Once = Once::new();
    LOGGED.call_once(|| {
        tracing::info!(
            threshold_bytes = THRESHOLD_BYTES,
            max_wait_ms = MAX_WAIT.as_millis(),
            max_escalated_wait_ms = MAX_ESCALATED_WAIT.as_millis(),
            "WAL checkpoint gate active for embedded databases"
        );
    });
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

/// Publish one sample. The counter uses `absolute` because the authoritative
/// count lives in litewire's atomics; the gauge is a plain set.
#[allow(clippy::cast_precision_loss)] // byte counts fit f64's 2^53 exactly
fn publish(sample: &WalGateSample) {
    let site = sample.site.clone();
    gauge!(METRIC_WAL_BYTES, "site" => site.clone()).set(wal_file_size(&sample.db_path) as f64);
    if let Some(s) = sample.stats {
        counter!(METRIC_TIMEOUTS, "site" => site).absolute(s.timed_out);
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

    /// The hardcoded settings are litewire's own defaults, so the always-on
    /// gate runs exactly the configuration litewire validated (64 MiB, 2 s,
    /// 250 ms, 2 s escalation ceiling, 60 s backoff cap).
    #[test]
    fn hardcoded_settings_match_litewire_defaults() {
        let ours = gate_config();
        let theirs = WalGateConfig::default();
        assert_eq!(ours.threshold_bytes, theirs.threshold_bytes);
        assert_eq!(ours.interval, theirs.interval);
        assert_eq!(ours.max_wait, theirs.max_wait);
        assert_eq!(ours.max_backoff, theirs.max_backoff);
        assert_eq!(ours.max_escalated_wait, theirs.max_escalated_wait);
        assert_eq!(ours.threshold_bytes, 64 * 1024 * 1024);
        assert_eq!(ours.max_wait, Duration::from_millis(250));
        assert_eq!(ours.max_escalated_wait, Duration::from_secs(2));
    }

    /// A file-backed factory built through [`builder_with_gate`] is gated; a
    /// plain builder and an in-memory one (no WAL file) are not. Stub mode:
    /// no PHP involved.
    #[tokio::test]
    async fn builder_with_gate_installs_a_gate_on_file_backed_factories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.db");
        let path = path.to_str().unwrap();

        let gated = builder_with_gate(Turso::builder(path)).build().await.unwrap();
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

        let plain = Turso::builder(path).build().await.unwrap();
        assert!(plain.wal_gate_stats().is_none(), "no gate was asked for");
        assert!(plain.wal_gate_task_finished().is_none());

        let memory = builder_with_gate(Turso::builder(":memory:")).build().await.unwrap();
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
        let turso =
            builder_with_gate(Turso::builder(path.to_str().unwrap())).build().await.unwrap();
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
            builder_with_gate(Turso::builder(path.to_str().unwrap())).build().await.unwrap(),
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

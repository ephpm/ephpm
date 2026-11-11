//! Distributed per-site cron — a declarative scheduler modelled faithfully on
//! Kubernetes `CronJob`, running each job **exactly once while cluster
//! membership is stable and converged, and at-least-once across membership
//! changes, node crashes and partitions**. Write jobs to be idempotent.
//!
//! # Two layers
//!
//! 1. **Ownership gate** ([`gate`], the primary guarantee): a node only
//!    *considers* a job it owns — a job with a `site` on that site's HRW owner,
//!    a siteless job on the elected cluster leader. Single-node owns everything.
//!    Fail-closed: unconfirmed ownership never runs.
//! 2. **Per-tick lease** ([`lease`], the handoff backstop): before running, a
//!    node acquires `cron:lease:<job>:<tick_ts>` via the clustered KV's atomic
//!    conditional-set, so during a brief ownership-move overlap where two nodes
//!    both pass the gate, exactly one wins the tick. Fail-closed on an
//!    unreachable KV owner.
//!
//! # The guarantee, stated honestly
//!
//! The lease rides the **gossip KV**, an AP store: a `set_nx` is a single-arbiter
//! RPC to `hash(key) mod sorted-alive-nodes` **computed from the caller's own
//! membership view**. So the lease is exactly as strong as membership agreement,
//! and that bounds what cron can promise:
//!
//! * **Stable, converged membership** — one agreed arbiter per key, one agreed
//!   owner per job: exactly-once.
//! * **Partition** — once failure detection declares the far side dead (seconds),
//!   an isolated node becomes HRW owner *and* arbiter of its own view and fires
//!   everything it owns. This is **at-least-once, not at-most-once**: the
//!   isolated node does not skip.
//! * **Membership changes / crashes** — two seconds-wide double-fire windows
//!   remain even without a partition: (1) on a plain join/leave, nodes ticking
//!   within clock-skew of each other can briefly disagree on the arbiter and both
//!   acquire; (2) if the firing node is also the key's arbiter and crashes before
//!   its fire-and-forget cursor write propagates, a new owner re-fires that tick.
//!
//! A gossip KV cannot offer linearizable CAS, so closing these would need a
//! consensus lease store (rejected: "KV is not a broker"). Idempotent jobs are
//! the contract. A per-job `require_quorum` opt-in (planned) would close the
//! isolated-*minority* partition case only — not (1) or (2). The design doc
//! states the limit plainly.
//!
//! # Mechanism (Kubernetes `CronJob` port)
//!
//! Tick granularity is one minute. Each tick evaluates the half-open window
//! `(lastScheduleTime, now]` ([`decision`]): the current tick fires; with no
//! `starting_deadline` there is no catch-up; with one, the single most-recent
//! missed tick within the window fires; more than 100 missed trips the valve.
//! `lastScheduleTime` lives in the clustered KV so an ownership move resumes
//! where the old owner left off. `concurrency_policy`
//! (`forbid`/`allow`/`replace`) is a per-node guard; `max_concurrent` is a
//! node-wide cap.

pub mod decision;
pub mod directory;
pub mod exec;
pub mod gate;
pub mod lease;
pub mod metrics;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ::metrics::{counter, gauge, histogram};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use ephpm_cluster::{ClusterHandle, NodeInfo};
use ephpm_config::{ConcurrencyPolicy, Config, CronSchedule};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use self::decision::Decision;
use self::exec::{ExecRequest, JobExecutor, RunOutcome};
use self::gate::Responsibility;
use self::lease::{CursorStore, LeaseOutcome, LeaseStore};

/// The margin added to a job's lease-window estimate to size its lease TTL, so
/// the lease always outlives the window in which two nodes could both try the
/// same tick (a slow run, or a late catch-up).
const LEASE_MARGIN: Duration = Duration::from_secs(60);

/// Upper bound on a per-tick lease's TTL. A lease key is tick-scoped
/// (`cron:lease:<job>:<minute>`), so a long TTL only means stale keys linger;
/// this caps that so a pathologically large `starting_deadline` cannot pin them
/// for an unbounded time. 7 days comfortably covers a daily/weekly catch-up
/// window while keeping key lifetime finite.
const MAX_LEASE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// How long to wait for a pre-empted (`replace`) run to die before starting the
/// replacement.
const PREEMPT_WAIT: Duration = Duration::from_secs(15);

/// Which tier a job came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// An operator `[[cron.jobs]]` entry.
    Operator,
    /// A tenant `.ephpm/cron/` file.
    Directory,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Tier::Operator => "operator",
            Tier::Directory => "directory",
        }
    }
}

/// A fully-resolved job ready to evaluate — the common shape of an operator job
/// and a directory-tier job.
#[derive(Debug, Clone)]
pub struct ResolvedJob {
    /// Unique name (lease/metric/log key).
    pub name: String,
    /// The site, or `None` for a siteless job.
    pub site: Option<String>,
    /// The source tier.
    pub tier: Tier,
    /// The parsed schedule.
    pub schedule: CronSchedule,
    /// The resolved timezone.
    pub tz: Tz,
    /// The command argv (for a directory job, the single script path).
    pub command: Vec<String>,
    /// Whether the command runs PHP.
    pub is_php: bool,
    /// Wall-clock cap.
    pub timeout: Duration,
    /// Overlap policy.
    pub concurrency_policy: ConcurrencyPolicy,
    /// Catch-up deadline (`None` ⇒ no catch-up).
    pub starting_deadline: Option<Duration>,
    /// Suspended jobs never evaluate.
    pub suspend: bool,
}

impl ResolvedJob {
    fn site_label(&self) -> &str {
        self.site.as_deref().unwrap_or("-")
    }

    fn exec_request(
        &self,
        config_path: Option<PathBuf>,
        sites_dir: Option<&std::path::Path>,
    ) -> ExecRequest {
        // A per-site job runs with its site root as the working directory, so a
        // relative command (e.g. `bin/cake.php`) resolves there rather than
        // against the server's cwd — matching `ephpm exec`.
        let cwd = match (&self.site, sites_dir) {
            (Some(site), Some(dir)) => Some(dir.join(site)),
            _ => None,
        };
        ExecRequest {
            job: self.name.clone(),
            site: self.site.clone(),
            is_php: self.is_php,
            command: self.command.clone(),
            config_path,
            cwd,
            timeout: self.timeout,
        }
    }
}

/// The last-known state of a job, for the status endpoint.
#[derive(Debug, Clone, Default)]
struct JobStatus {
    last_tick: Option<DateTime<Utc>>,
    last_outcome: Option<&'static str>,
    last_started: Option<DateTime<Utc>>,
    last_duration_ms: Option<u64>,
    runs: u64,
    skips: u64,
}

/// Node-wide and per-job running state.
#[derive(Clone)]
struct RunningState {
    node_count: Arc<AtomicUsize>,
    jobs: Arc<Mutex<HashMap<String, RunHandle>>>,
    max_concurrent: usize,
}

struct RunHandle {
    id: u64,
    cancel: CancellationToken,
}

impl RunningState {
    fn new(max_concurrent: usize) -> Self {
        Self {
            node_count: Arc::new(AtomicUsize::new(0)),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent,
        }
    }

    async fn is_running(&self, name: &str) -> bool {
        self.jobs.lock().await.contains_key(name)
    }

    async fn cancel_of(&self, name: &str) -> Option<CancellationToken> {
        self.jobs.lock().await.get(name).map(|h| h.cancel.clone())
    }

    /// Register a new run; returns its unique id. Increments the node count.
    async fn register(&self, name: &str, cancel: CancellationToken) -> u64 {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
        self.node_count.fetch_add(1, Ordering::SeqCst);
        self.jobs.lock().await.insert(name.to_string(), RunHandle { id, cancel });
        id
    }

    /// Finish a run: decrement the node count, and remove the map entry only if
    /// it is still ours (a `replace` may have installed a newer run).
    async fn finish(&self, name: &str, id: u64) {
        self.node_count.fetch_sub(1, Ordering::SeqCst);
        let mut map = self.jobs.lock().await;
        if map.get(name).is_some_and(|h| h.id == id) {
            map.remove(name);
        }
    }

    fn at_cap(&self) -> bool {
        self.node_count.load(Ordering::SeqCst) >= self.max_concurrent
    }
}

/// The scheduler. Holds the resolved operator jobs and the handles it needs to
/// gate, lease, and run. The tick logic is injectable (lease/cursor/executor
/// traits, an explicit membership snapshot) so exactly-once is unit-testable.
pub struct CronScheduler {
    operator_jobs: Vec<ResolvedJob>,
    config_path: Option<PathBuf>,
    /// Sites with the directory tier enabled: `None` ⇒ all sites under
    /// `sites_dir`; `Some(set)` ⇒ only those keys.
    directory_sites: Option<Vec<String>>,
    sites_dir: Option<PathBuf>,
    cadence: directory::CadenceSchedules,
    directory_timeout: Duration,
    max_jobs_per_site: usize,
    default_tz: Tz,

    clustered: bool,
    cluster: Option<Arc<ClusterHandle>>,
    self_id: String,

    lease: Arc<dyn LeaseStore>,
    cursor: Arc<dyn CursorStore>,
    executor: Arc<dyn JobExecutor>,

    running: RunningState,
    // A plain `std` mutex: the status map is only ever locked briefly and never
    // across an `.await`, so the status endpoint (a sync dispatch) can read it
    // without an async lock.
    status: Arc<std::sync::Mutex<HashMap<String, JobStatus>>>,
}

impl CronScheduler {
    /// Build the scheduler from config, resolving every operator job (invalid
    /// ones are logged and skipped, though `Config::validate` already rejected
    /// them at startup). Returns `None` when `[cron] enabled = false`.
    #[must_use]
    pub fn from_config(
        config: &Config,
        cluster: Option<Arc<ClusterHandle>>,
        self_id: String,
        lease: Arc<dyn LeaseStore>,
        cursor: Arc<dyn CursorStore>,
        executor: Arc<dyn JobExecutor>,
    ) -> Option<Self> {
        if !config.cron.enabled {
            return None;
        }
        let cron = &config.cron;
        let default_tz = ephpm_config::parse_timezone(&cron.timezone).unwrap_or(chrono_tz::UTC);

        let mut operator_jobs = Vec::new();
        for job in &cron.jobs {
            match resolve_operator_job(job, &cron.timezone) {
                Ok(resolved) => operator_jobs.push(resolved),
                Err(e) => tracing::error!(job = job.name, error = %e, "cron: skipping invalid job"),
            }
        }

        let directory_sites = if cron.directory_cron {
            None
        } else if cron.directory_cron_sites.is_empty() {
            Some(Vec::new())
        } else {
            Some(cron.directory_cron_sites.clone())
        };

        let cadence = directory::CadenceSchedules::new(
            cron.directory_hourly.as_deref(),
            cron.directory_daily.as_deref(),
            cron.directory_weekly.as_deref(),
            cron.directory_monthly.as_deref(),
        )
        .unwrap_or_else(|_| default_cadence());

        let clustered = cluster.is_some();
        let max_concurrent = cron.max_concurrent.max(1) as usize;

        Some(Self {
            operator_jobs,
            config_path: config.source_path.clone(),
            directory_sites,
            sites_dir: config.server.sites_dir.clone(),
            cadence,
            directory_timeout: Duration::from_secs(3600),
            max_jobs_per_site: cron.max_jobs_per_site.max(1) as usize,
            default_tz,
            clustered,
            cluster,
            self_id,
            lease,
            cursor,
            executor,
            running: RunningState::new(max_concurrent),
            status: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// The jobs to evaluate this tick: the fixed operator jobs plus a fresh
    /// scan of each directory-enabled site this node owns (live discovery).
    fn jobs_for_tick(&self, nodes: &[NodeInfo]) -> Vec<ResolvedJob> {
        let mut jobs = self.operator_jobs.clone();

        let Some(sites_dir) = &self.sites_dir else {
            return jobs;
        };
        // Which sites have the directory tier on?
        let sites: Vec<String> = match &self.directory_sites {
            Some(list) if list.is_empty() => return jobs, // directory tier off
            Some(list) => list.clone(),
            None => enumerate_site_dirs(sites_dir),
        };

        for site in sites {
            // Only the site's owner scans + runs its directory jobs.
            if self.responsible_for_site(&site, nodes) != Responsibility::Yes {
                continue;
            }
            let root = sites_dir.join(&site);
            for dj in directory::scan_site(&site, &root, &self.cadence, self.max_jobs_per_site) {
                let tz = dj
                    .timezone
                    .as_deref()
                    .and_then(|n| ephpm_config::parse_timezone(n).ok())
                    .unwrap_or(self.default_tz);
                jobs.push(ResolvedJob {
                    name: dj.name,
                    site: Some(site.clone()),
                    tier: Tier::Directory,
                    schedule: dj.schedule,
                    tz,
                    command: vec![dj.script.to_string_lossy().into_owned()],
                    is_php: true,
                    timeout: self.directory_timeout,
                    concurrency_policy: ConcurrencyPolicy::Forbid,
                    starting_deadline: None,
                    suspend: false,
                });
            }
        }
        jobs
    }

    fn responsible(&self, job: &ResolvedJob, nodes: &[NodeInfo]) -> Responsibility {
        if !self.clustered {
            return Responsibility::Yes;
        }
        match &job.site {
            Some(site) => gate::responsible_for_site(nodes, &self.self_id, site),
            None => gate::responsible_as_leader(nodes, &self.self_id),
        }
    }

    fn responsible_for_site(&self, site: &str, nodes: &[NodeInfo]) -> Responsibility {
        if !self.clustered {
            return Responsibility::Yes;
        }
        gate::responsible_for_site(nodes, &self.self_id, site)
    }

    /// Evaluate and act on every job for one tick. Pure of wall-clock sleeps:
    /// `now` and the membership snapshot are passed in, so it is deterministic
    /// in tests.
    pub async fn tick(&self, now: DateTime<Utc>, nodes: &[NodeInfo]) {
        let jobs = self.jobs_for_tick(nodes);
        for job in &jobs {
            self.consider(job, now, nodes).await;
        }
    }

    async fn consider(&self, job: &ResolvedJob, now: DateTime<Utc>, nodes: &[NodeInfo]) {
        if job.suspend {
            return;
        }

        match self.responsible(job, nodes) {
            Responsibility::Yes => {}
            Responsibility::No => {
                self.record_skip(job, metrics::SKIP_NOT_OWNER).await;
                return;
            }
            Responsibility::Unknown => {
                tracing::warn!(
                    job = job.name,
                    site = job.site_label(),
                    "cron: ownership could not be confirmed; skipping (fail-closed)"
                );
                self.record_skip(job, metrics::SKIP_NOT_OWNER).await;
                return;
            }
        }

        let cursor = match self.cursor.get(&job.name) {
            Some(c) => c,
            None => {
                // First sight of this job (fresh deploy, a newly dropped
                // `.ephpm/cron/` file, or a cursor the clustered KV lost). Seed
                // the cursor to the start of the current minute (less a tick), so
                // the first window is `(floor(now) - 1s, now]` — the current
                // minute only. A tick whose instant *is* the current minute then
                // still fires on time (seeding plain `now` would drop it whenever
                // first sight lands a fraction of a second past the minute
                // boundary), while nothing older is replayed — the lost-cursor
                // fail-safe. Without any seed the window is forever `(now, now]`
                // and the job never fires.
                let start = decision::floor_minute(now) - chrono::Duration::seconds(1);
                self.cursor.set(&job.name, start);
                start
            }
        };
        match decision::evaluate(&job.schedule, job.tz, now, cursor, job.starting_deadline) {
            Decision::Idle => {}
            Decision::SkipDue { advance_to, valve } => {
                if valve {
                    tracing::error!(
                        job = job.name,
                        site = job.site_label(),
                        "cron: more than 100 missed schedules; abandoning catch-up and \
                         advancing the cursor"
                    );
                    self.record_skip(job, metrics::SKIP_MISSED_VALVE).await;
                }
                self.cursor.set(&job.name, advance_to);
            }
            Decision::Fire { tick_ts } => {
                self.try_fire(job, tick_ts).await;
            }
        }
    }

    /// Attempt to fire `job` for `tick_ts`, applying the concurrency policy, the
    /// node-wide cap and the per-tick lease. Returns what actually happened so a
    /// caller (`trigger_now`) can report it accurately; the scheduling tick
    /// loop ignores it (its outcome is already recorded as a metric/skip).
    async fn try_fire(&self, job: &ResolvedJob, tick_ts: DateTime<Utc>) -> FireOutcome {
        // Concurrency policy.
        match job.concurrency_policy {
            ConcurrencyPolicy::Forbid => {
                if self.running.is_running(&job.name).await {
                    self.record_skip(job, metrics::SKIP_OVERLAP).await;
                    self.cursor.set(&job.name, tick_ts);
                    return FireOutcome::SkippedOverlap;
                }
            }
            ConcurrencyPolicy::Allow => {}
            ConcurrencyPolicy::Replace => {
                if self.running.is_running(&job.name).await {
                    self.preempt(&job.name).await;
                }
            }
        }

        // Node-wide cap.
        if self.running.at_cap() {
            self.record_skip(job, metrics::SKIP_AT_CAP).await;
            self.cursor.set(&job.name, tick_ts);
            return FireOutcome::SkippedAtCap;
        }

        // Per-tick lease.
        let token = lease::mint_token(&self.self_id);
        let key = lease::lease_key(&job.name, tick_ts);
        // The lease must outlive the whole window in which two nodes could both
        // try this same tick: a run as slow as `timeout`, and — with catch-up
        // enabled — a peer firing it up to `starting_deadline` late after an
        // ownership move or restart. Sizing it to just `timeout` let a lease
        // expire inside a long catch-up window and the tick double-fire. Bounded
        // so a very large deadline cannot pin tick-scoped keys indefinitely.
        let window = job.timeout.max(job.starting_deadline.unwrap_or_default());
        let ttl = (window + LEASE_MARGIN).min(MAX_LEASE_TTL);
        match self.lease.acquire(&key, &token, ttl) {
            LeaseOutcome::Acquired => {
                self.start_run(job, tick_ts).await;
                FireOutcome::Started
            }
            LeaseOutcome::Held => {
                // Another node won this tick (handoff window): skip, do not
                // advance the cursor — the winner advances the shared cursor.
                self.record_skip(job, metrics::SKIP_LEASE_LOST).await;
                FireOutcome::LeaseHeld
            }
            LeaseOutcome::Unavailable => {
                tracing::warn!(
                    job = job.name,
                    "cron: lease owner unreachable; skipping this tick (fail-closed)"
                );
                self.record_skip(job, metrics::SKIP_LEASE_LOST).await;
                FireOutcome::LeaseUnavailable
            }
        }
    }

    async fn start_run(&self, job: &ResolvedJob, tick_ts: DateTime<Utc>) {
        // lastScheduleTime advances on successful start.
        self.cursor.set(&job.name, tick_ts);
        counter!(
            metrics::RUNS_STARTED,
            "job" => job.name.clone(),
            "site" => job.site_label().to_string(),
            "tier" => job.tier.label(),
        )
        .increment(1);
        gauge!(
            metrics::LAST_SCHEDULE_TIMESTAMP,
            "job" => job.name.clone(),
            "site" => job.site_label().to_string(),
        )
        .set(tick_ts.timestamp() as f64);

        {
            let mut status = self.status.lock().expect("status mutex");
            let s = status.entry(job.name.clone()).or_default();
            s.last_tick = Some(tick_ts);
            s.last_started = Some(Utc::now());
            s.runs += 1;
        }

        tracing::info!(
            job = job.name,
            site = job.site_label(),
            tier = job.tier.label(),
            tick_ts = %tick_ts,
            "cron: starting run"
        );

        let cancel = CancellationToken::new();
        let id = self.running.register(&job.name, cancel.clone()).await;
        let req = job.exec_request(self.config_path.clone(), self.sites_dir.as_deref());
        let executor = Arc::clone(&self.executor);
        let running = self.running.clone();
        let status = Arc::clone(&self.status);
        let name = job.name.clone();
        let site = job.site_label().to_string();
        let tier = job.tier.label();

        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let outcome = executor.run(req, cancel).await;
            let elapsed = started.elapsed();
            running.finish(&name, id).await;

            counter!(
                metrics::RUNS_TOTAL,
                "job" => name.clone(),
                "site" => site.clone(),
                "tier" => tier,
                "outcome" => outcome.label(),
            )
            .increment(1);
            histogram!(
                metrics::DURATION_SECONDS,
                "job" => name.clone(),
                "site" => site.clone(),
            )
            .record(elapsed.as_secs_f64());

            {
                let mut st = status.lock().expect("status mutex");
                let s = st.entry(name.clone()).or_default();
                s.last_outcome = Some(outcome.label());
                s.last_duration_ms = Some(elapsed.as_millis() as u64);
            }

            match outcome {
                RunOutcome::Ok => tracing::info!(job = name, ?elapsed, "cron: run finished ok"),
                other => tracing::warn!(
                    job = name,
                    outcome = other.label(),
                    ?elapsed,
                    "cron: run finished"
                ),
            }
        });
    }

    async fn preempt(&self, name: &str) {
        if let Some(cancel) = self.running.cancel_of(name).await {
            cancel.cancel();
            let deadline = std::time::Instant::now() + PREEMPT_WAIT + Duration::from_secs(2);
            while self.running.is_running(name).await && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    async fn record_skip(&self, job: &ResolvedJob, reason: &'static str) {
        counter!(
            metrics::SKIPPED_TOTAL,
            "job" => job.name.clone(),
            "site" => job.site_label().to_string(),
            "reason" => reason,
        )
        .increment(1);
        if reason != metrics::SKIP_NOT_OWNER {
            // not_owner is the common, quiet case; others are noteworthy.
            tracing::debug!(job = job.name, reason, "cron: tick skipped");
        }
        let mut status = self.status.lock().expect("status mutex");
        status.entry(job.name.clone()).or_default().skips += 1;
    }

    /// The current status as a JSON string, for `GET /_ephpm/cron`.
    #[must_use]
    pub fn status_json(&self) -> String {
        let status = self.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut jobs = Vec::new();
        for job in &self.operator_jobs {
            let s = status.get(&job.name).cloned().unwrap_or_default();
            jobs.push(job_status_json(job, &s, self.cursor.get(&job.name)));
        }
        serde_json::json!({
            "node": self.self_id,
            "clustered": self.clustered,
            "running": self.running.node_count.load(Ordering::SeqCst),
            "max_concurrent": self.running.max_concurrent,
            "jobs": jobs,
        })
        .to_string()
    }

    /// Fire one job now (honouring the per-tick lease unless `force`). Used by
    /// `POST /_ephpm/cron/run/<name>`.
    ///
    /// Returns `(fired, message)`: `fired` is whether the job actually started
    /// (so a caller can set a non-zero exit code on a no-op), and `message` is a
    /// short human line describing exactly what happened. A plain run-now shares
    /// the scheduled tick's per-minute lease, so it is a no-op if that tick
    /// already ran this minute — the message says so rather than claiming a run.
    pub async fn trigger_now(&self, name: &str, force: bool, nodes: &[NodeInfo]) -> (bool, String) {
        let jobs = self.jobs_for_tick(nodes);
        let Some(job) = jobs.iter().find(|j| j.name == name) else {
            return (false, format!("no such job {name:?}"));
        };
        // Floor to the minute so the ad-hoc lease shares the scheduled tick's
        // lease key (`cron:lease:<job>:<minute_epoch>`); otherwise run-now and
        // the scheduled run take different keys and both fire.
        let tick_ts = decision::floor_minute(Utc::now());
        if force {
            // `--force` bypasses the per-minute lease (and the overlap/cap
            // gates) and starts a run unconditionally.
            self.start_run(job, tick_ts).await;
            return (true, format!("ran {name:?} now (forced; bypassed this minute's lease)"));
        }
        let outcome = self.try_fire(job, tick_ts).await;
        let message = match outcome {
            FireOutcome::Started => format!("ran {name:?} now"),
            FireOutcome::SkippedOverlap => format!(
                "skipped {name:?}: already running (concurrencyPolicy=Forbid) — \
                 use --force to run anyway"
            ),
            FireOutcome::SkippedAtCap => format!(
                "skipped {name:?}: this node is at its concurrent-run cap — \
                 use --force to run anyway"
            ),
            FireOutcome::LeaseHeld => format!(
                "skipped {name:?}: this minute's tick already holds the lease — \
                 use --force to run it again"
            ),
            FireOutcome::LeaseUnavailable => format!(
                "skipped {name:?}: lease owner unreachable (fail-closed) — \
                 use --force to run anyway"
            ),
        };
        (outcome == FireOutcome::Started, message)
    }
}

/// What [`CronScheduler::try_fire`] did with a tick — reported by
/// [`CronScheduler::trigger_now`] and ignored by the scheduling loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FireOutcome {
    /// The run was started (lease acquired, or forced).
    Started,
    /// Skipped: a previous run is still going and `concurrencyPolicy=Forbid`.
    SkippedOverlap,
    /// Skipped: this node is already at its concurrent-run cap.
    SkippedAtCap,
    /// Skipped: another node holds this minute's lease.
    LeaseHeld,
    /// Skipped: the lease owner was unreachable (fail-closed).
    LeaseUnavailable,
}

/// Resolve one operator `CronJob` into a `ResolvedJob`.
fn resolve_operator_job(
    job: &ephpm_config::CronJob,
    default_tz: &str,
) -> Result<ResolvedJob, String> {
    let schedule = job.schedule_parsed().map_err(|e| e.to_string())?;
    let tz = job.timezone_resolved(default_tz).map_err(|e| e.to_string())?;
    let timeout = job.timeout_duration().map_err(|e| e.to_string())?;
    let starting_deadline = job.starting_deadline_duration().map_err(|e| e.to_string())?;
    let concurrency_policy = job.concurrency_policy_parsed().map_err(|e| e.to_string())?;
    Ok(ResolvedJob {
        name: job.name.clone(),
        site: job.site.clone(),
        tier: Tier::Operator,
        schedule,
        tz,
        command: job.command.clone(),
        is_php: job.is_php_command(),
        timeout,
        concurrency_policy,
        starting_deadline,
        suspend: job.suspend,
    })
}

/// List vhost directory names directly under `sites_dir` (the canonical site
/// keys), sorted.
fn enumerate_site_dirs(sites_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sites_dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| !name.starts_with('.'))
        .collect();
    out.sort();
    out
}

fn job_status_json(
    job: &ResolvedJob,
    s: &JobStatus,
    cursor: Option<DateTime<Utc>>,
) -> serde_json::Value {
    let next = job.schedule.next_after(Utc::now(), job.tz);
    serde_json::json!({
        "name": job.name,
        "site": job.site,
        "tier": job.tier.label(),
        "schedule": job.schedule.source(),
        "timezone": job.tz.name(),
        "suspended": job.suspend,
        "next_fire": next.map(|t| t.to_rfc3339()),
        "last_schedule_time": cursor.map(|t| t.to_rfc3339()),
        "last_tick": s.last_tick.map(|t| t.to_rfc3339()),
        "last_started": s.last_started.map(|t| t.to_rfc3339()),
        "last_outcome": s.last_outcome,
        "last_duration_ms": s.last_duration_ms,
        "runs": s.runs,
        "skips": s.skips,
    })
}

/// Spawn the scheduler's tick loop (aligned to the minute boundary) and return
/// the server-held `Arc<CronScheduler>` (for the status endpoint) and the loop
/// `JoinHandle` (aborted at drain, like `db_backup`). Returns `None` when cron
/// is disabled.
#[must_use]
pub fn spawn(
    config: &Config,
    cluster: Option<Arc<ClusterHandle>>,
    self_id: String,
    kv_store: Arc<ephpm_kv::store::Store>,
) -> Option<(Arc<CronScheduler>, tokio::task::JoinHandle<()>)> {
    let lease: Arc<dyn LeaseStore> = Arc::new(lease::KvStore::new(Arc::clone(&kv_store)));
    let cursor: Arc<dyn CursorStore> = Arc::new(lease::KvStore::new(kv_store));
    let executor: Arc<dyn JobExecutor> = match exec::ChildExecutor::new() {
        Ok(e) => Arc::new(e),
        Err(e) => {
            tracing::error!(error = %e, "cron: cannot resolve the ephpm binary; scheduler disabled");
            return None;
        }
    };

    let scheduler =
        Arc::new(CronScheduler::from_config(config, cluster, self_id, lease, cursor, executor)?);

    tracing::info!(
        jobs = scheduler.operator_jobs.len(),
        clustered = scheduler.clustered,
        "cron: scheduler enabled"
    );

    let loop_sched = Arc::clone(&scheduler);
    let handle = tokio::spawn(async move { loop_sched.run_loop().await });
    Some((scheduler, handle))
}

impl CronScheduler {
    /// The minute-aligned tick loop.
    async fn run_loop(self: Arc<Self>) {
        loop {
            sleep_to_next_minute().await;
            let now = Utc::now();
            let nodes = match &self.cluster {
                Some(c) => c.nodes().await,
                None => Vec::new(),
            };
            self.tick(now, &nodes).await;
        }
    }

    /// Snapshot the current membership (for the trigger endpoint).
    pub async fn membership(&self) -> Vec<NodeInfo> {
        match &self.cluster {
            Some(c) => c.nodes().await,
            None => Vec::new(),
        }
    }
}

/// The built-in directory cadence schedules (used when overrides are unset or,
/// defensively, fail to parse — though config validation already rejects a bad
/// override at startup).
fn default_cadence() -> directory::CadenceSchedules {
    directory::CadenceSchedules::new(None, None, None, None)
        .expect("built-in cadence defaults parse")
}

/// Sleep until the start of the next wall-clock minute.
async fn sleep_to_next_minute() {
    use chrono::Timelike as _;
    let now = Utc::now();
    let next_minute = (now + chrono::Duration::minutes(1))
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    let remaining = (next_minute - now).to_std().unwrap_or(Duration::from_secs(1));
    tokio::time::sleep(remaining.max(Duration::from_millis(1))).await;
}

#[cfg(test)]
mod tests;

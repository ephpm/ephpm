//! Deterministic tests for the scheduler's exactly-once behaviour.
//!
//! The lease/cursor/executor are in-memory fakes and the clock + membership are
//! injected (`tick(now, nodes)`), so every proof is reproducible with no
//! wall-clock sleeps on the decision path. Covered: single-node firing, cluster
//! exactly-once under stable membership (gate), exactly-once during an
//! ownership-move overlap (lease backstop), a clean ownership move, abrupt node
//! death (absent / `Dead` — not draining), fail-closed on unconfirmable
//! ownership and on an unreachable lease store, cursor survival across an
//! ownership move, the catch-up rules at the scheduler level, the
//! concurrency/cap guards, the directory tier's opt-in + gate + lease, the
//! trigger endpoint, and the metric `reason` labels every skip path emits.
//!
//! Metric assertions use a **thread-local** recorder
//! (`::metrics::with_local_recorder` + `metrics_util::debugging`) on a
//! current-thread runtime, so parallel tests never share a global recorder and
//! the spawned run tasks (which record the finished-run metrics) poll on the
//! same thread inside `block_on`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ::metrics::{SharedString, Unit};
use chrono::{DateTime, TimeZone, Timelike, Utc};
use ephpm_cluster::{NodeInfo, NodeState};
use ephpm_config::CronSchedule;
use metrics_util::CompositeKey;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

use self::exec::{ExecRequest, RunOutcome};
use self::lease::{CursorStore, LeaseOutcome, LeaseStore};
use super::*;

// ── fakes ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct InMemLease {
    held: Mutex<HashMap<String, Vec<u8>>>,
    unreachable: bool,
}

impl InMemLease {
    /// The token holding `key`, if any.
    fn holder(&self, key: &str) -> Option<Vec<u8>> {
        self.held.lock().unwrap().get(key).cloned()
    }
}

impl LeaseStore for InMemLease {
    fn acquire(&self, key: &str, token: &[u8], _ttl: std::time::Duration) -> LeaseOutcome {
        if self.unreachable {
            return LeaseOutcome::Unavailable;
        }
        let mut held = self.held.lock().unwrap();
        if held.contains_key(key) {
            LeaseOutcome::Held
        } else {
            held.insert(key.to_string(), token.to_vec());
            LeaseOutcome::Acquired
        }
    }
    fn release(&self, key: &str, token: &[u8]) {
        let mut held = self.held.lock().unwrap();
        if held.get(key).is_some_and(|t| t == token) {
            held.remove(key);
        }
    }
}

#[derive(Default)]
struct InMemCursor {
    map: Mutex<HashMap<String, DateTime<Utc>>>,
}

impl CursorStore for InMemCursor {
    fn get(&self, job: &str) -> Option<DateTime<Utc>> {
        self.map.lock().unwrap().get(job).copied()
    }
    fn set(&self, job: &str, ts: DateTime<Utc>) {
        self.map.lock().unwrap().insert(job.to_string(), ts);
    }
}

#[derive(Default)]
struct RecordingExec {
    /// Job names, in execution order (the observable side effect).
    runs: Arc<Mutex<Vec<String>>>,
    /// The full requests, for asserting *how* a job was run (site, PHP, argv).
    reqs: Arc<Mutex<Vec<ExecRequest>>>,
    sleep_ms: u64,
}

impl JobExecutor for RecordingExec {
    fn run<'a>(
        &'a self,
        req: ExecRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RunOutcome> + Send + 'a>> {
        let runs = Arc::clone(&self.runs);
        let reqs = Arc::clone(&self.reqs);
        let sleep_ms = self.sleep_ms;
        Box::pin(async move {
            runs.lock().unwrap().push(req.job.clone());
            reqs.lock().unwrap().push(req);
            if sleep_ms > 0 {
                // Honour cancellation so a `replace` pre-emption resolves fast.
                tokio::select! {
                    () = tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)) => {}
                    () = cancel.cancelled() => return RunOutcome::Killed,
                }
            }
            RunOutcome::Ok
        })
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

fn at_s(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
}

fn floor_minute(ts: DateTime<Utc>) -> DateTime<Utc> {
    ts.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap()
}

fn alive(id: &str) -> NodeInfo {
    NodeInfo::new(id, format!("{id}:7000"), NodeState::Alive)
}

/// A node the failure detector has declared dead but not yet pruned from the
/// snapshot — the other shape an abrupt death takes (besides plain absence).
fn dead(id: &str) -> NodeInfo {
    let mut n = alive(id);
    n.state = NodeState::Dead;
    n
}

/// Which of `ids` owns `site` under `nodes`.
fn owner_of<'a>(nodes: &[NodeInfo], ids: &[&'a str], site: &str) -> &'a str {
    ids.iter()
        .copied()
        .find(|id| gate::responsible_for_site(nodes, id, site) == Responsibility::Yes)
        .expect("exactly one owner")
}

fn every_minute_job(name: &str, site: Option<&str>) -> ResolvedJob {
    ResolvedJob {
        name: name.to_string(),
        site: site.map(str::to_string),
        tier: Tier::Operator,
        schedule: CronSchedule::parse("* * * * *").unwrap(),
        tz: chrono_tz::UTC,
        command: vec!["true".to_string()],
        is_php: false,
        timeout: std::time::Duration::from_secs(60),
        concurrency_policy: ConcurrencyPolicy::Forbid,
        starting_deadline: None,
        suspend: false,
    }
}

fn daily_3am_job(name: &str, site: Option<&str>) -> ResolvedJob {
    let mut job = every_minute_job(name, site);
    job.schedule = CronSchedule::parse("0 3 * * *").unwrap();
    job
}

/// Everything a test scheduler can vary; the plain `scheduler()` wrapper
/// covers the operator-only cases.
struct Opts {
    jobs: Vec<ResolvedJob>,
    clustered: bool,
    self_id: String,
    lease: Arc<dyn LeaseStore>,
    cursor: Arc<dyn CursorStore>,
    executor: Arc<dyn JobExecutor>,
    max_concurrent: usize,
    sites_dir: Option<PathBuf>,
    directory_sites: Option<Vec<String>>,
    max_jobs_per_site: usize,
}

fn build(o: Opts) -> CronScheduler {
    CronScheduler {
        operator_jobs: o.jobs,
        config_path: None,
        directory_sites: o.directory_sites,
        sites_dir: o.sites_dir,
        cadence: directory::CadenceSchedules::new(None, None, None, None).unwrap(),
        directory_timeout: std::time::Duration::from_secs(3600),
        max_jobs_per_site: o.max_jobs_per_site,
        default_tz: chrono_tz::UTC,
        clustered: o.clustered,
        cluster: None,
        self_id: o.self_id,
        lease: o.lease,
        cursor: o.cursor,
        executor: o.executor,
        running: RunningState::new(o.max_concurrent),
        status: Arc::new(std::sync::Mutex::new(HashMap::new())),
    }
}

#[allow(clippy::too_many_arguments)]
fn scheduler(
    jobs: Vec<ResolvedJob>,
    clustered: bool,
    self_id: &str,
    lease: Arc<dyn LeaseStore>,
    cursor: Arc<dyn CursorStore>,
    executor: Arc<dyn JobExecutor>,
    max_concurrent: usize,
) -> CronScheduler {
    build(Opts {
        jobs,
        clustered,
        self_id: self_id.to_string(),
        lease,
        cursor,
        executor,
        max_concurrent,
        sites_dir: None,
        directory_sites: Some(Vec::new()),
        max_jobs_per_site: 20,
    })
}

async fn fire_count(s: &CronScheduler, job: &str) -> u64 {
    s.status.lock().unwrap().get(job).map_or(0, |st| st.runs)
}

async fn skip_count(s: &CronScheduler, job: &str) -> u64 {
    s.status.lock().unwrap().get(job).map_or(0, |st| st.skips)
}

/// Let spawned run tasks finish (the fake executor with `sleep_ms = 0`
/// completes in one poll; this is a scheduler yield, not a decision-path
/// sleep).
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
}

// ── metrics harness ─────────────────────────────────────────────────────────

type Snap = Vec<(CompositeKey, Option<Unit>, Option<SharedString>, DebugValue)>;

/// Run `f()` on a current-thread runtime with a thread-local metrics recorder
/// installed; returns the future's output and the recorded snapshot.
fn with_metrics<T, Fut>(f: impl FnOnce() -> Fut) -> (T, Snap)
where
    Fut: Future<Output = T>,
{
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let out = ::metrics::with_local_recorder(&recorder, || rt.block_on(f()));
    (out, snapshotter.snapshot().into_vec())
}

fn key_matches(k: &CompositeKey, name: &str, labels: &[(&str, &str)]) -> bool {
    k.key().name() == name
        && labels
            .iter()
            .all(|(ln, lv)| k.key().labels().any(|l| l.key() == *ln && l.value() == *lv))
}

/// Sum of every counter named `name` whose labels include all of `labels`.
fn counter(snap: &Snap, name: &str, labels: &[(&str, &str)]) -> u64 {
    snap.iter()
        .filter(|(k, ..)| key_matches(k, name, labels))
        .map(|(.., v)| match v {
            DebugValue::Counter(c) => *c,
            _ => 0,
        })
        .sum()
}

fn skips(snap: &Snap, job: &str, reason: &str) -> u64 {
    counter(snap, metrics::SKIPPED_TOTAL, &[("job", job), ("reason", reason)])
}

/// Every `reason` label value recorded for `job`, in label order.
fn skip_reasons(snap: &Snap, job: &str) -> Vec<String> {
    let mut out: Vec<String> = snap
        .iter()
        .filter(|(k, ..)| key_matches(k, metrics::SKIPPED_TOTAL, &[("job", job)]))
        .filter_map(|(k, ..)| {
            k.key().labels().find(|l| l.key() == "reason").map(|l| l.value().to_string())
        })
        .collect();
    out.sort();
    out
}

fn gauge(snap: &Snap, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    snap.iter().find(|(k, ..)| key_matches(k, name, labels)).and_then(|(.., v)| match v {
        DebugValue::Gauge(g) => Some(g.0),
        _ => None,
    })
}

fn histogram_samples(snap: &Snap, name: &str, labels: &[(&str, &str)]) -> usize {
    snap.iter()
        .filter(|(k, ..)| key_matches(k, name, labels))
        .map(|(.., v)| match v {
            DebugValue::Histogram(h) => h.len(),
            _ => 0,
        })
        .sum()
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn single_node_fires_and_executes_command() {
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let runs = Arc::new(Mutex::new(Vec::new()));
    let executor: Arc<dyn JobExecutor> =
        Arc::new(RecordingExec { runs: Arc::clone(&runs), ..Default::default() });

    let s = scheduler(
        vec![every_minute_job("cleanup", None)],
        false,
        "solo",
        lease,
        cursor.clone(),
        executor,
        4,
    );
    // Seed the cursor two minutes back so the current minute is due.
    let now = at(2026, 1, 1, 0, 2);
    cursor.set("cleanup", at(2026, 1, 1, 0, 0));

    s.tick(now, &[]).await;

    assert_eq!(fire_count(&s, "cleanup").await, 1, "the job fires once");
    // Let the spawned run task record the execution (observable side effect).
    settle().await;
    assert_eq!(&*runs.lock().unwrap(), &["cleanup".to_string()], "the command ran");
    // The cursor advanced to the fired tick.
    assert_eq!(cursor.get("cleanup"), Some(at(2026, 1, 1, 0, 2)));
}

#[tokio::test]
async fn first_tick_after_start_fires_current_minute_without_catch_up() {
    // With no cursor, it is seeded to `floor(now) - 1s`, so the first window is
    // the current minute: an every-minute job first seen at 00:00:00 fires its
    // on-time 00:00 tick exactly once — and nothing from before boot is replayed
    // (no catch-up).
    let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
    let s = scheduler(
        vec![every_minute_job("j", None)],
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        Arc::new(RecordingExec::default()),
        4,
    );
    s.tick(at(2026, 1, 1, 0, 0), &[]).await;
    settle().await;
    assert_eq!(fire_count(&s, "j").await, 1, "the current-minute tick fires on time");
    assert_eq!(cursor.get("j"), Some(at(2026, 1, 1, 0, 0)), "cursor advanced to the fired tick");
}

/// D10 — a lost `lastScheduleTime` fails safe: no backlog is replayed (not even
/// with a generous `starting_deadline`), while a tick whose instant is the
/// **current minute** still fires on time. The first-sight seed is
/// `floor(now) - 1s`, so the first window is exactly the current minute. Three
/// jobs: strict daily, daily with a 7-day catch-up deadline, and every-minute.
///
/// This is also the shape of every job on a fresh install (no `cron:state:*`
/// key yet) and of every newly dropped directory-tier file.
#[tokio::test]
async fn lost_cursor_fails_safe_to_window_starts_now() {
    let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
    let exec = Arc::new(RecordingExec::default());
    let mut deadline = daily_3am_job("daily-deadline", None);
    deadline.starting_deadline = Some(std::time::Duration::from_secs(7 * 86_400));
    let s = scheduler(
        vec![daily_3am_job("daily", None), deadline, every_minute_job("minutely", None)],
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        exec.clone(),
        4,
    );

    // 09:00:05 on day 1, six hours past the 03:00 slot, cursor unknown. The
    // daily jobs have no tick in the *current* minute, so nothing is replayed —
    // the 7-day deadline does not drag in the long-past 03:00 slot. The
    // every-minute job's 09:00 tick is the current minute, so it fires on time.
    s.tick(at_s(2026, 1, 1, 9, 0, 5), &[]).await;
    settle().await;
    assert_eq!(fire_count(&s, "daily").await, 0, "no backlog replay (strict)");
    assert_eq!(fire_count(&s, "daily-deadline").await, 0, "no backlog replay (deadline)");
    assert_eq!(skip_count(&s, "daily").await + skip_count(&s, "daily-deadline").await, 0);
    assert_eq!(fire_count(&s, "minutely").await, 1, "the current-minute tick fires at first sight");

    // The next minute: the every-minute job is on time again.
    s.tick(at_s(2026, 1, 1, 9, 1, 2), &[]).await;
    settle().await;
    assert_eq!(fire_count(&s, "minutely").await, 2, "the next on-time minute fires too");

    // Day 2 at 03:00: both daily jobs are on time and fire exactly once — the
    // long-past day-1 03:00 slot is never replayed.
    s.tick(at_s(2026, 1, 2, 3, 0, 3), &[]).await;
    settle().await;
    assert_eq!(fire_count(&s, "daily").await, 1, "the next on-time daily tick fires");
    assert_eq!(fire_count(&s, "daily-deadline").await, 1);
    assert_eq!(cursor.get("daily"), Some(at(2026, 1, 2, 3, 0)));
    let runs = exec.runs.lock().unwrap();
    assert_eq!(runs.iter().filter(|r| *r == "daily").count(), 1, "never twice: {runs:?}");
}

#[tokio::test]
async fn clustered_exactly_once_stable_membership() {
    // Two nodes, shared lease + shared cursor, a per-site job. Exactly one
    // node (the HRW owner) fires; the other records not_owner.
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    cursor.set("shop-job", at(2026, 1, 1, 0, 0));
    let runs = Arc::new(Mutex::new(Vec::new()));

    let mk = |id: &str| {
        let exec: Arc<dyn JobExecutor> =
            Arc::new(RecordingExec { runs: Arc::clone(&runs), ..Default::default() });
        scheduler(
            vec![every_minute_job("shop-job", Some("shop"))],
            true,
            id,
            Arc::clone(&lease),
            Arc::clone(&cursor),
            exec,
            4,
        )
    };
    let n0 = mk("ephpm-0");
    let n1 = mk("ephpm-1");
    let nodes = [alive("ephpm-0"), alive("ephpm-1")];
    let now = at(2026, 1, 1, 0, 2);

    n0.tick(now, &nodes).await;
    n1.tick(now, &nodes).await;

    let fires = fire_count(&n0, "shop-job").await + fire_count(&n1, "shop-job").await;
    assert_eq!(fires, 1, "exactly one node fires the per-site job");
    // One node owns (fires), the other recorded not_owner.
    let skips = skip_count(&n0, "shop-job").await + skip_count(&n1, "shop-job").await;
    assert_eq!(skips, 1, "the non-owner skipped once (not_owner)");
}

/// D3 (siteless half) — a job with no `site` runs only on the elected leader
/// (lowest-ordinal alive, non-draining); every other node records `not_owner`.
#[test]
fn siteless_job_fires_only_on_the_leader() {
    let ((fires, skips_total), snap) = with_metrics(|| async {
        let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
        let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
        cursor.set("global", at(2026, 1, 1, 0, 0));
        let exec = Arc::new(RecordingExec::default());
        let mk = |id: &str| {
            scheduler(
                vec![every_minute_job("global", None)],
                true,
                id,
                Arc::clone(&lease),
                Arc::clone(&cursor),
                exec.clone(),
                4,
            )
        };
        // Deliberately out of order: the leader is by id, not by position.
        let (n2, n0, n1) = (mk("ephpm-2"), mk("ephpm-0"), mk("ephpm-1"));
        let nodes = [alive("ephpm-2"), alive("ephpm-0"), alive("ephpm-1")];
        let now = at(2026, 1, 1, 0, 1);
        n2.tick(now, &nodes).await;
        n1.tick(now, &nodes).await;
        n0.tick(now, &nodes).await;
        settle().await;
        assert_eq!(fire_count(&n0, "global").await, 1, "the lowest ordinal is the leader");
        assert_eq!(fire_count(&n1, "global").await, 0);
        assert_eq!(fire_count(&n2, "global").await, 0);
        assert_eq!(exec.runs.lock().unwrap().len(), 1, "the command ran exactly once");
        (
            fire_count(&n0, "global").await,
            skip_count(&n1, "global").await + skip_count(&n2, "global").await,
        )
    });
    assert_eq!(fires, 1);
    assert_eq!(skips_total, 2);
    assert_eq!(skips(&snap, "global", metrics::SKIP_NOT_OWNER), 2, "both followers: not_owner");
    assert_eq!(skip_reasons(&snap, "global"), vec!["not_owner"], "no other skip reason");
}

/// D8 — both nodes believe they own the site (a stale/partitioned membership
/// view during a handoff): each sees only itself alive, so both pass the gate.
/// The shared per-tick lease is the backstop — exactly one fires, the loser
/// records `lease_lost` and does **not** advance its cursor (so it can still
/// take over the next tick if the winner dies).
#[test]
fn clustered_exactly_once_during_ownership_overlap() {
    let ((f0, f1, c0, c1), snap) = with_metrics(|| async {
        let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
        let cursor0: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        let cursor1: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        cursor0.set("shop-job", at(2026, 1, 1, 0, 0));
        cursor1.set("shop-job", at(2026, 1, 1, 0, 0));
        let exec = Arc::new(RecordingExec::default());

        let n0 = scheduler(
            vec![every_minute_job("shop-job", Some("shop"))],
            true,
            "ephpm-0",
            Arc::clone(&lease),
            cursor0.clone(),
            exec.clone(),
            4,
        );
        let n1 = scheduler(
            vec![every_minute_job("shop-job", Some("shop"))],
            true,
            "ephpm-1",
            Arc::clone(&lease),
            cursor1.clone(),
            exec.clone(),
            4,
        );
        let now = at(2026, 1, 1, 0, 2);
        // Each node's membership view contains only itself → both are "owner".
        n0.tick(now, &[alive("ephpm-0")]).await;
        n1.tick(now, &[alive("ephpm-1")]).await;
        settle().await;
        assert_eq!(exec.runs.lock().unwrap().len(), 1, "the command ran exactly once");
        (
            fire_count(&n0, "shop-job").await,
            fire_count(&n1, "shop-job").await,
            cursor0.get("shop-job"),
            cursor1.get("shop-job"),
        )
    });
    assert_eq!(f0 + f1, 1, "the lease prevents a double-fire during the overlap");
    assert_eq!(skips(&snap, "shop-job", metrics::SKIP_LEASE_LOST), 1, "the loser: lease_lost");
    // Winner's cursor advanced to the tick; loser's stayed put.
    let (winner_cursor, loser_cursor) = if f0 == 1 { (c0, c1) } else { (c1, c0) };
    assert_eq!(winner_cursor, Some(at(2026, 1, 1, 0, 2)));
    assert_eq!(
        loser_cursor,
        Some(at(2026, 1, 1, 0, 0)),
        "a lost lease does not advance the cursor"
    );
}

/// D8 against the **production** lease adapter: two schedulers share one real
/// `ephpm_kv::store::Store` through `lease::KvStore` (the atomic `SET NX EX`),
/// not the in-memory fake — exactly one fires.
#[tokio::test]
async fn overlap_double_fire_is_blocked_by_the_real_kv_setnx() {
    let store = ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default());
    let lease: Arc<dyn LeaseStore> = Arc::new(lease::KvStore::new(Arc::clone(&store)));
    let cursor: Arc<dyn CursorStore> = Arc::new(lease::KvStore::new(Arc::clone(&store)));
    cursor.set("shop-job", at(2026, 1, 1, 0, 0));
    let exec = Arc::new(RecordingExec::default());
    let mk = |id: &str| {
        scheduler(
            vec![every_minute_job("shop-job", Some("shop"))],
            true,
            id,
            Arc::clone(&lease),
            Arc::clone(&cursor),
            exec.clone(),
            4,
        )
    };
    let (n0, n1) = (mk("ephpm-0"), mk("ephpm-1"));
    let now = at(2026, 1, 1, 0, 2);
    n0.tick(now, &[alive("ephpm-0")]).await;
    n1.tick(now, &[alive("ephpm-1")]).await;
    settle().await;
    assert_eq!(fire_count(&n0, "shop-job").await + fire_count(&n1, "shop-job").await, 1);
    assert_eq!(exec.runs.lock().unwrap().len(), 1);
    // The real lease key is present and the cursor round-tripped through the KV.
    assert!(store.get(&lease::lease_key("shop-job", now)).is_some());
    assert_eq!(cursor.get("shop-job"), Some(now));
}

/// D4 — a clean ownership move (the owner drains): the job fires exactly once
/// per tick across the transition — on the old owner before, on the **new**
/// owner after — no double, no drop.
#[tokio::test]
async fn clean_ownership_move_fires_once_per_tick_across_the_transition() {
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let site = "catalog";
    cursor.set("cat-job", at(2026, 1, 1, 0, 0));

    let mk = |id: &str| {
        scheduler(
            vec![every_minute_job("cat-job", Some(site))],
            true,
            id,
            Arc::clone(&lease),
            Arc::clone(&cursor),
            Arc::new(RecordingExec::default()),
            4,
        )
    };
    let n0 = mk("ephpm-0");
    let n1 = mk("ephpm-1");

    let nodes = [alive("ephpm-0"), alive("ephpm-1")];
    let owner = owner_of(&nodes, &["ephpm-0", "ephpm-1"], site);
    let (owner_s, other_s) = if owner == "ephpm-0" { (&n0, &n1) } else { (&n1, &n0) };

    // Tick 1: the owner fires.
    let now1 = at(2026, 1, 1, 0, 2);
    n0.tick(now1, &nodes).await;
    n1.tick(now1, &nodes).await;
    assert_eq!(fire_count(owner_s, "cat-job").await, 1, "tick 1 fires on the owner");
    assert_eq!(fire_count(other_s, "cat-job").await, 0);

    // Drain the owner → ownership moves. Tick 2 fires once on the new owner.
    let mut drained_owner = alive(owner);
    drained_owner.draining = true;
    let other = if owner == "ephpm-0" { "ephpm-1" } else { "ephpm-0" };
    let moved = [drained_owner, alive(other)];
    let new_owner = gate::responsible_for_site(&moved, other, site);
    assert_eq!(new_owner, Responsibility::Yes, "ownership moved to the other node");

    let now2 = at(2026, 1, 1, 0, 3);
    n0.tick(now2, &moved).await;
    n1.tick(now2, &moved).await;
    assert_eq!(fire_count(other_s, "cat-job").await, 1, "tick 2 fires on the NEW owner");
    assert_eq!(fire_count(owner_s, "cat-job").await, 1, "the draining node did not fire again");
    assert_eq!(cursor.get("cat-job"), Some(now2));
}

/// D5 — the owner **dies abruptly** (absent from the snapshot, not draining)
/// while its last run may still be in flight. Worst case for the survivor: the
/// dead node's cursor write never reached it (per-node cursor views). Prove:
///  * the overlap tick (the dead node's last `tick_ts`) is **not** re-run by the
///    new owner — the lease refuses it (`lease_lost`);
///  * the **next** tick fires exactly once, on the new owner;
///  * a snapshot that still lists the old owner as `Dead` behaves the same.
#[test]
fn owner_dies_abruptly_next_tick_fires_once_on_the_new_site_owner() {
    let (survivor, snap) = with_metrics(|| async {
        let lease: Arc<InMemLease> = Arc::new(InMemLease::default());
        let cur0: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        let cur1: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        let t0 = at(2026, 1, 1, 0, 0);
        cur0.set("cat-job", t0);
        cur1.set("cat-job", t0);
        let exec = Arc::new(RecordingExec::default());
        let mk = |id: &str, cur: Arc<InMemCursor>| {
            scheduler(
                vec![every_minute_job("cat-job", Some("catalog"))],
                true,
                id,
                lease.clone(),
                cur,
                exec.clone(),
                4,
            )
        };
        let n0 = mk("ephpm-0", cur0);
        let n1 = mk("ephpm-1", cur1);
        let full = [alive("ephpm-0"), alive("ephpm-1")];
        let owner = owner_of(&full, &["ephpm-0", "ephpm-1"], "catalog");
        let survivor = if owner == "ephpm-0" { "ephpm-1" } else { "ephpm-0" };
        let (owner_s, survivor_s) = if owner == "ephpm-0" { (&n0, &n1) } else { (&n1, &n0) };

        // T1: the owner fires (and then dies with this run in flight).
        let t1 = at(2026, 1, 1, 0, 1);
        owner_s.tick(t1, &full).await;
        survivor_s.tick(t1, &full).await;
        assert_eq!(fire_count(owner_s, "cat-job").await, 1);
        assert_eq!(fire_count(survivor_s, "cat-job").await, 0);

        // The owner is simply gone from membership: no draining flag.
        let after = [alive(survivor)];
        assert_eq!(gate::responsible_for_site(&after, survivor, "catalog"), Responsibility::Yes);

        // The survivor's first tick as owner still sees T1 as due (its cursor
        // is stale) — the lease must refuse the re-run.
        survivor_s.tick(t1, &after).await;
        assert_eq!(fire_count(survivor_s, "cat-job").await, 0, "T1 is not re-run");
        assert!(
            lease.holder(&lease::lease_key("cat-job", t1)).unwrap().starts_with(owner.as_bytes()),
            "T1's lease still belongs to the dead owner"
        );

        // T2: fires exactly once, on the survivor.
        let t2 = at(2026, 1, 1, 0, 2);
        survivor_s.tick(t2, &after).await;
        assert_eq!(fire_count(survivor_s, "cat-job").await, 1, "T2 fires on the new owner");
        assert!(
            lease
                .holder(&lease::lease_key("cat-job", t2))
                .unwrap()
                .starts_with(survivor.as_bytes())
        );

        // Let T2's run finish (so `forbid` has nothing to guard), then the same
        // with the dead node still listed (failure detector view).
        settle().await;
        let t3 = at(2026, 1, 1, 0, 3);
        survivor_s.tick(t3, &[dead(owner), alive(survivor)]).await;
        assert_eq!(fire_count(survivor_s, "cat-job").await, 2, "T3 fires on the new owner");

        settle().await;
        let runs = exec.runs.lock().unwrap();
        assert_eq!(runs.len(), 3, "T1 (owner) + T2 + T3 (survivor): {runs:?}");
        survivor.to_string()
    });
    assert_eq!(
        skips(&snap, "cat-job", metrics::SKIP_LEASE_LOST),
        1,
        "the overlap tick: lease_lost"
    );
    assert_eq!(skips(&snap, "cat-job", metrics::SKIP_NOT_OWNER), 1, "T1 as non-owner: not_owner");
    assert_eq!(skip_reasons(&snap, "cat-job"), vec!["lease_lost", "not_owner"]);
    assert!(["ephpm-0", "ephpm-1"].contains(&survivor.as_str()), "survivor = {survivor}");
}

/// D5 (siteless) — the leader dies abruptly; the next-lowest ordinal becomes
/// leader and runs the next tick exactly once; the overlap tick is lease-guarded.
#[test]
fn leader_dies_abruptly_next_tick_fires_once_on_the_new_leader() {
    let ((f0, f1, f2), snap) = with_metrics(|| async {
        let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
        let t0 = at(2026, 1, 1, 0, 0);
        let exec = Arc::new(RecordingExec::default());
        let mk = |id: &str| {
            // Per-node cursor views (the dead leader's write may not have landed).
            let cur: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
            cur.set("global", t0);
            scheduler(
                vec![every_minute_job("global", None)],
                true,
                id,
                lease.clone(),
                cur,
                exec.clone(),
                4,
            )
        };
        let (n0, n1, n2) = (mk("ephpm-0"), mk("ephpm-1"), mk("ephpm-2"));
        let full = [alive("ephpm-0"), alive("ephpm-1"), alive("ephpm-2")];
        let t1 = at(2026, 1, 1, 0, 1);
        n0.tick(t1, &full).await;
        n1.tick(t1, &full).await;
        n2.tick(t1, &full).await;
        assert_eq!(fire_count(&n0, "global").await, 1, "the leader fires T1");

        // ephpm-0 vanishes. Overlap: the new leader still sees T1 due.
        let after = [alive("ephpm-1"), alive("ephpm-2")];
        n1.tick(t1, &after).await;
        n2.tick(t1, &after).await;
        assert_eq!(fire_count(&n1, "global").await, 0, "T1 is not re-run by the new leader");

        let t2 = at(2026, 1, 1, 0, 2);
        n1.tick(t2, &after).await;
        n2.tick(t2, &after).await;
        settle().await;
        assert_eq!(exec.runs.lock().unwrap().len(), 2, "T1 once, T2 once");
        (
            fire_count(&n0, "global").await,
            fire_count(&n1, "global").await,
            fire_count(&n2, "global").await,
        )
    });
    assert_eq!((f0, f1, f2), (1, 1, 0), "T1 on the old leader, T2 on the new one");
    assert_eq!(skips(&snap, "global", metrics::SKIP_LEASE_LOST), 1, "new leader's overlap tick");
    // not_owner: n1+n2 at T1 (2), n2 at the overlap T1 (1), n2 at T2 (1).
    assert_eq!(skips(&snap, "global", metrics::SKIP_NOT_OWNER), 4);
}

/// D9 — `lastScheduleTime` lives in the shared (clustered) KV, so after an
/// ownership move the new owner reads it and does not even *consider* a tick
/// the old owner already ran: no fire, no skip of any kind — and then fires
/// the next tick.
#[test]
fn cursor_survives_ownership_move_so_the_new_owner_does_not_rerun_a_tick() {
    let ((owner_fires, survivor_fires), snap) = with_metrics(|| async {
        let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
        let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
        cursor.set("cat-job", at(2026, 1, 1, 0, 0));
        let exec = Arc::new(RecordingExec::default());
        let mk = |id: &str| {
            scheduler(
                vec![every_minute_job("cat-job", Some("catalog"))],
                true,
                id,
                lease.clone(),
                cursor.clone(),
                exec.clone(),
                4,
            )
        };
        let (n0, n1) = (mk("ephpm-0"), mk("ephpm-1"));
        let full = [alive("ephpm-0"), alive("ephpm-1")];
        let owner = owner_of(&full, &["ephpm-0", "ephpm-1"], "catalog");
        let survivor = if owner == "ephpm-0" { "ephpm-1" } else { "ephpm-0" };
        let (owner_s, survivor_s) = if owner == "ephpm-0" { (&n0, &n1) } else { (&n1, &n0) };

        let t1 = at(2026, 1, 1, 0, 1);
        owner_s.tick(t1, &full).await;
        assert_eq!(cursor.get("cat-job"), Some(t1), "the owner recorded lastScheduleTime");

        // Owner gone; the survivor evaluates the same tick from the shared cursor.
        let after = [alive(survivor)];
        survivor_s.tick(t1, &after).await;
        assert_eq!(fire_count(survivor_s, "cat-job").await, 0, "T1 not re-run");
        assert_eq!(
            skip_count(survivor_s, "cat-job").await,
            0,
            "not even a skip: the window is empty"
        );

        survivor_s.tick(at(2026, 1, 1, 0, 2), &after).await;
        settle().await;
        assert_eq!(exec.runs.lock().unwrap().len(), 2);
        (fire_count(owner_s, "cat-job").await, fire_count(survivor_s, "cat-job").await)
    });
    assert_eq!((owner_fires, survivor_fires), (1, 1));
    assert!(skip_reasons(&snap, "cat-job").is_empty(), "no skip was recorded at all");
}

/// D6 — ownership that cannot be confirmed (an empty membership snapshot, or
/// one where every node is dead) **never runs**: skip as `not_owner`, executor
/// not invoked, and the cursor untouched so a node that *can* confirm
/// ownership still fires the tick.
#[test]
fn unconfirmable_ownership_fails_closed() {
    let ((site_fires, global_fires, cursor_site, runs), snap) = with_metrics(|| async {
        let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        let t0 = at(2026, 1, 1, 0, 0);
        cursor.set("site-job", t0);
        cursor.set("global", t0);
        let exec = Arc::new(RecordingExec::default());
        let s = scheduler(
            vec![every_minute_job("site-job", Some("shop")), every_minute_job("global", None)],
            true,
            "ephpm-0",
            Arc::new(InMemLease::default()),
            cursor.clone(),
            exec.clone(),
            4,
        );
        let t1 = at(2026, 1, 1, 0, 1);
        // Membership unreachable / empty.
        s.tick(t1, &[]).await;
        // Everyone (including us) marked dead by the failure detector.
        s.tick(t1, &[dead("ephpm-0"), dead("ephpm-1")]).await;
        settle().await;
        (
            fire_count(&s, "site-job").await,
            fire_count(&s, "global").await,
            cursor.get("site-job"),
            exec.runs.lock().unwrap().len(),
        )
    });
    assert_eq!((site_fires, global_fires), (0, 0), "never run on unconfirmed ownership");
    assert_eq!(runs, 0, "the executor was never invoked");
    assert_eq!(cursor_site, Some(at(2026, 1, 1, 0, 0)), "the tick is not consumed");
    assert_eq!(skips(&snap, "site-job", metrics::SKIP_NOT_OWNER), 2);
    assert_eq!(skips(&snap, "global", metrics::SKIP_NOT_OWNER), 2);
    assert_eq!(skip_reasons(&snap, "site-job"), vec!["not_owner"]);
}

/// D7 — the lease store is unreachable (the KV key's owner cannot be reached
/// or the store refused): skip as `lease_lost`, never run blind, executor not
/// invoked, cursor not advanced (a peer that can reach the KV may still own
/// the tick).
#[test]
fn lease_unreachable_fails_closed() {
    let ((fires, cursor_after, runs), snap) = with_metrics(|| async {
        let lease: Arc<dyn LeaseStore> =
            Arc::new(InMemLease { unreachable: true, ..Default::default() });
        let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        cursor.set("j", at(2026, 1, 1, 0, 0));
        let exec = Arc::new(RecordingExec::default());
        let s = scheduler(
            vec![every_minute_job("j", None)],
            false,
            "solo",
            lease,
            cursor.clone(),
            exec.clone(),
            4,
        );
        s.tick(at(2026, 1, 1, 0, 2), &[]).await;
        settle().await;
        (fire_count(&s, "j").await, cursor.get("j"), exec.runs.lock().unwrap().len())
    });
    assert_eq!(fires, 0, "an unreachable lease owner never runs");
    assert_eq!(runs, 0, "the executor was never invoked");
    assert_eq!(cursor_after, Some(at(2026, 1, 1, 0, 0)), "the cursor is not advanced");
    assert_eq!(
        skips(&snap, "j", metrics::SKIP_LEASE_LOST),
        1,
        "the skip is recorded as lease_lost"
    );
    assert_eq!(skip_reasons(&snap, "j"), vec!["lease_lost"]);
}

/// D7/D8 — the production `lease::KvStore` adapter maps the store's atomic
/// `SET NX` outcomes: `Inserted` → `Acquired`, `Exists` → `Held`, and an
/// over-memory refusal (the key state is uncertain) → `Unavailable`
/// (fail-closed). Release is compare-and-delete by token. The cursor adapter
/// round-trips and treats garbage as "lost".
#[test]
fn kv_lease_adapter_maps_store_outcomes_and_fails_closed() {
    use ephpm_kv::store::{EvictionPolicy, Store, StoreConfig};

    let store = Store::new(StoreConfig::default());
    let kv = lease::KvStore::new(Arc::clone(&store));
    let ttl = std::time::Duration::from_secs(120);
    let (a, b) = (lease::mint_token("ephpm-0"), lease::mint_token("ephpm-1"));
    let key = lease::lease_key("j", at(2026, 1, 1, 0, 1));

    assert_eq!(kv.acquire(&key, &a, ttl), LeaseOutcome::Acquired);
    assert_eq!(kv.acquire(&key, &b, ttl), LeaseOutcome::Held, "second node loses the tick");
    assert_eq!(kv.acquire(&key, &a, ttl), LeaseOutcome::Held, "not re-entrant for the holder");
    kv.release(&key, &b);
    assert_eq!(kv.acquire(&key, &b, ttl), LeaseOutcome::Held, "a wrong token cannot release");
    kv.release(&key, &a);
    assert_eq!(kv.acquire(&key, &b, ttl), LeaseOutcome::Acquired, "the holder's release frees it");

    // A store that refuses the write (no memory, no eviction): fail closed.
    let full = Store::new(StoreConfig {
        memory_limit: 1,
        eviction_policy: EvictionPolicy::NoEviction,
        ..StoreConfig::default()
    });
    let kv_full = lease::KvStore::new(full);
    assert_eq!(kv_full.acquire(&key, &a, ttl), LeaseOutcome::Unavailable);

    // Cursor round-trip, and a corrupt value reads as lost (→ window starts now).
    let cursor = lease::KvStore::new(Arc::clone(&store));
    assert_eq!(cursor.get("j"), None);
    cursor.set("j", at(2026, 1, 1, 0, 1));
    assert_eq!(cursor.get("j"), Some(at(2026, 1, 1, 0, 1)));
    store.set(lease::cursor_key("j"), b"not-a-timestamp".to_vec(), None);
    assert_eq!(cursor.get("j"), None, "garbage is a lost cursor, not a panic");
}

// ── catch-up (B) ────────────────────────────────────────────────────────────

/// B1 — default (`starting_deadline` unset) = **no catch-up**: a daily tick
/// missed while down is skipped (cursor advanced past it, nothing loud), the
/// executor is not invoked, and the next on-time tick fires once.
#[test]
fn default_no_catch_up_skips_the_missed_tick_and_fires_the_next_on_time_one() {
    let ((fires_after_miss, fires_after_on_time, cursor_mid, tick_lease), snap) =
        with_metrics(|| async {
            let lease: Arc<InMemLease> = Arc::new(InMemLease::default());
            let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
            cursor.set("nightly", at(2026, 1, 1, 3, 0));
            let exec = Arc::new(RecordingExec::default());
            let s = scheduler(
                vec![daily_3am_job("nightly", None)],
                false,
                "solo",
                lease.clone(),
                cursor.clone(),
                exec.clone(),
                4,
            );

            // Down through 03:00 on day 2; back at 09:00.
            s.tick(at_s(2026, 1, 2, 9, 0, 1), &[]).await;
            settle().await;
            let fires_after_miss = fire_count(&s, "nightly").await;
            let cursor_mid = cursor.get("nightly");
            assert!(
                lease.holder(&lease::lease_key("nightly", at(2026, 1, 2, 3, 0))).is_none(),
                "the missed tick was never leased"
            );

            // Day 3, 03:00 on time: fires.
            let t = at(2026, 1, 3, 3, 0);
            s.tick(at_s(2026, 1, 3, 3, 0, 4), &[]).await;
            settle().await;
            assert_eq!(exec.runs.lock().unwrap().len(), 1);
            (
                fires_after_miss,
                fire_count(&s, "nightly").await,
                cursor_mid,
                lease.holder(&lease::lease_key("nightly", t)).is_some(),
            )
        });
    assert_eq!(fires_after_miss, 0, "the missed slot does NOT fire");
    assert_eq!(cursor_mid, Some(at(2026, 1, 2, 3, 0)), "cursor advanced past the missed tick");
    assert_eq!(fires_after_on_time, 1, "the next on-time tick fires once");
    assert!(tick_lease, "and was leased at its canonical tick_ts");
    assert!(skip_reasons(&snap, "nightly").is_empty(), "a plain miss is quiet (no missed_valve)");
}

/// B2 — with `starting_deadline`, at most the **single most-recent** missed
/// schedule fires; older missed ticks are dropped (never leased, never run).
#[tokio::test]
async fn starting_deadline_catches_up_only_the_most_recent_missed_tick() {
    let lease: Arc<InMemLease> = Arc::new(InMemLease::default());
    let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
    cursor.set("nightly", at(2026, 1, 1, 3, 0));
    let exec = Arc::new(RecordingExec::default());
    let mut job = daily_3am_job("nightly", None);
    job.starting_deadline = Some(std::time::Duration::from_secs(86_400));
    let s = scheduler(vec![job], false, "solo", lease.clone(), cursor.clone(), exec.clone(), 4);

    // Three missed nights; back at 03:01 on day 4 (within the 24 h deadline).
    s.tick(at(2026, 1, 4, 3, 1), &[]).await;
    settle().await;
    assert_eq!(fire_count(&s, "nightly").await, 1, "exactly one catch-up run");
    assert_eq!(exec.runs.lock().unwrap().len(), 1);
    assert_eq!(cursor.get("nightly"), Some(at(2026, 1, 4, 3, 0)), "cursor = the most recent tick");
    assert!(lease.holder(&lease::lease_key("nightly", at(2026, 1, 4, 3, 0))).is_some());
    for day in 2..=3 {
        assert!(
            lease.holder(&lease::lease_key("nightly", at(2026, 1, day, 3, 0))).is_none(),
            "older missed tick (day {day}) was never leased"
        );
    }
    // Past the deadline instead (a 10-minute window, back 6 h late): skipped
    // quietly, cursor still advances past the missed tick.
    let mut tight = daily_3am_job("nightly", None);
    tight.starting_deadline = Some(std::time::Duration::from_secs(600));
    let s2 = scheduler(vec![tight], false, "solo", lease.clone(), cursor.clone(), exec.clone(), 4);
    s2.tick(at(2026, 1, 6, 9, 0), &[]).await;
    settle().await;
    assert_eq!(fire_count(&s2, "nightly").await, 0, "a miss beyond the deadline does not fire");
    assert_eq!(exec.runs.lock().unwrap().len(), 1, "still the single catch-up run");
    assert_eq!(cursor.get("nightly"), Some(at(2026, 1, 6, 3, 0)));
    assert!(lease.holder(&lease::lease_key("nightly", at(2026, 1, 6, 3, 0))).is_none());
}

/// B3 — more than 100 missed schedules: give up loudly (`missed_valve`),
/// advance the cursor to `now`, fire **nothing** (no stampede), executor not
/// invoked; the next on-time tick then fires once.
#[test]
fn missed_valve_abandons_catch_up_without_a_stampede() {
    let ((fires_at_valve, cursor_after, fires_next, runs), snap) = with_metrics(|| async {
        let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        cursor.set("minutely", at(2026, 1, 1, 0, 0));
        let exec = Arc::new(RecordingExec::default());
        let mut job = every_minute_job("minutely", None);
        job.starting_deadline = Some(std::time::Duration::from_secs(600));
        let s = scheduler(
            vec![job],
            false,
            "solo",
            Arc::new(InMemLease::default()),
            cursor.clone(),
            exec.clone(),
            4,
        );

        // Three hours down with an every-minute schedule: >100 missed.
        let now = at_s(2026, 1, 1, 3, 0, 30);
        s.tick(now, &[]).await;
        settle().await;
        let fires_at_valve = fire_count(&s, "minutely").await;
        let cursor_after = cursor.get("minutely");

        s.tick(at_s(2026, 1, 1, 3, 1, 2), &[]).await;
        settle().await;
        (
            fires_at_valve,
            cursor_after,
            fire_count(&s, "minutely").await,
            exec.runs.lock().unwrap().len(),
        )
    });
    assert_eq!(fires_at_valve, 0, "nothing fires when the valve trips");
    assert_eq!(cursor_after, Some(at_s(2026, 1, 1, 3, 0, 30)), "cursor jumps to now");
    assert_eq!(skips(&snap, "minutely", metrics::SKIP_MISSED_VALVE), 1);
    assert_eq!(fires_next, 1, "the next on-time tick fires once");
    assert_eq!(runs, 1, "one run total — no stampede");
}

// ── concurrency & caps (C) ──────────────────────────────────────────────────

#[tokio::test]
async fn forbid_policy_skips_an_overlapping_run() {
    // A slow executor keeps the first run registered; a second due tick with
    // concurrency_policy = forbid is skipped as overlap.
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let runs = Arc::new(Mutex::new(Vec::new()));
    let executor: Arc<dyn JobExecutor> =
        Arc::new(RecordingExec { runs: Arc::clone(&runs), sleep_ms: 400, ..Default::default() });
    let s = scheduler(
        vec![every_minute_job("slow", None)],
        false,
        "solo",
        lease,
        cursor.clone(),
        executor,
        4,
    );

    // First tick fires and the run stays active (sleeping).
    cursor.set("slow", at(2026, 1, 1, 0, 0));
    let job = s.operator_jobs[0].clone();
    s.try_fire(&job, at(2026, 1, 1, 0, 2)).await;
    // Second, later tick while the first is still running → overlap skip.
    s.try_fire(&job, at(2026, 1, 1, 0, 3)).await;

    assert_eq!(fire_count(&s, "slow").await, 1, "forbid runs the job only once");
    assert!(skip_count(&s, "slow").await >= 1, "the overlap was skipped");
}

#[tokio::test]
async fn allow_policy_runs_overlapping() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let executor: Arc<dyn JobExecutor> =
        Arc::new(RecordingExec { runs: Arc::clone(&runs), sleep_ms: 400, ..Default::default() });
    let mut job = every_minute_job("overlap", None);
    job.concurrency_policy = ConcurrencyPolicy::Allow;
    let s = scheduler(
        vec![job.clone()],
        false,
        "solo",
        Arc::new(InMemLease::default()),
        Arc::new(InMemCursor::default()),
        executor,
        4,
    );
    // Two distinct ticks while the first is still running → both run (allow).
    s.try_fire(&job, at(2026, 1, 1, 0, 2)).await;
    s.try_fire(&job, at(2026, 1, 1, 0, 3)).await;
    assert_eq!(fire_count(&s, "overlap").await, 2, "allow runs both overlapping ticks");
}

#[tokio::test]
async fn replace_policy_preempts_running() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let executor: Arc<dyn JobExecutor> =
        Arc::new(RecordingExec { runs: Arc::clone(&runs), sleep_ms: 5_000, ..Default::default() });
    let mut job = every_minute_job("repl", None);
    job.concurrency_policy = ConcurrencyPolicy::Replace;
    let s = scheduler(
        vec![job.clone()],
        false,
        "solo",
        Arc::new(InMemLease::default()),
        Arc::new(InMemCursor::default()),
        executor,
        4,
    );
    // First run occupies the slot (5 s). A second tick with replace pre-empts
    // it (the fake executor honours the cancel) and starts a fresh run.
    s.try_fire(&job, at(2026, 1, 1, 0, 2)).await;
    s.try_fire(&job, at(2026, 1, 1, 0, 3)).await;
    assert_eq!(fire_count(&s, "repl").await, 2, "replace started a second run");
    // Let the replacement settle; the first was killed, so exactly one remains.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(runs.lock().unwrap().len(), 2, "both runs were spawned (first killed)");
}

/// C4 — `max_concurrent = 1` with a slow run occupying the slot: the second
/// job is skipped as `at_cap` (not queued — its cursor advances past the tick).
#[test]
fn node_cap_skips_when_full() {
    let ((fires, cursor_b, started), snap) = with_metrics(|| async {
        let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        let executor: Arc<dyn JobExecutor> =
            Arc::new(RecordingExec { sleep_ms: 400, ..Default::default() });
        let s = scheduler(
            vec![every_minute_job("a", None), every_minute_job("b", None)],
            false,
            "solo",
            Arc::new(InMemLease::default()),
            cursor.clone(),
            executor,
            1,
        );
        cursor.set("a", at(2026, 1, 1, 0, 0));
        cursor.set("b", at(2026, 1, 1, 0, 0));
        s.tick(at(2026, 1, 1, 0, 2), &[]).await;
        let fires = fire_count(&s, "a").await + fire_count(&s, "b").await;
        (fires, cursor.get("b"), fire_count(&s, "a").await)
    });
    assert_eq!(fires, 1, "only one job runs with max_concurrent = 1");
    assert_eq!(started, 1, "jobs evaluate in order: `a` took the slot");
    assert_eq!(skips(&snap, "b", metrics::SKIP_AT_CAP), 1, "`b` skipped at_cap");
    assert_eq!(cursor_b, Some(at(2026, 1, 1, 0, 2)), "skipped, not queued: cursor advanced");
    assert_eq!(counter(&snap, metrics::RUNS_STARTED, &[]), 1);
}

/// C6 — `suspend = true`: never evaluated, never fired, no skip recorded, the
/// cursor untouched (resuming later starts a fresh window, not a backlog).
#[test]
fn suspended_job_never_fires() {
    let ((fires, skips_n, cursor_after, runs), snap) = with_metrics(|| async {
        let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        cursor.set("paused", at(2026, 1, 1, 0, 0));
        let exec = Arc::new(RecordingExec::default());
        let mut job = every_minute_job("paused", None);
        job.suspend = true;
        let s = scheduler(
            vec![job],
            false,
            "solo",
            Arc::new(InMemLease::default()),
            cursor.clone(),
            exec.clone(),
            4,
        );
        for minute in 1..=5 {
            s.tick(at(2026, 1, 1, 0, minute), &[]).await;
        }
        settle().await;
        (
            fire_count(&s, "paused").await,
            skip_count(&s, "paused").await,
            cursor.get("paused"),
            exec.runs.lock().unwrap().len(),
        )
    });
    assert_eq!((fires, skips_n, runs), (0, 0, 0));
    assert_eq!(cursor_after, Some(at(2026, 1, 1, 0, 0)));
    assert_eq!(skip_reasons(&snap, "paused"), Vec::<String>::new());
    assert_eq!(counter(&snap, metrics::RUNS_STARTED, &[("job", "paused")]), 0);
}

// ── directory tier (E) ──────────────────────────────────────────────────────

/// A `sites_dir` with the given vhost directories.
fn sites_fixture(sites: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for s in sites {
        std::fs::create_dir_all(dir.path().join(s)).unwrap();
    }
    dir
}

fn drop_php(sites_dir: &Path, site: &str, bucket: &str, file: &str) -> PathBuf {
    let dir = sites_dir.join(site).join(".ephpm").join("cron").join(bucket);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(file);
    std::fs::write(&path, "<?php\n").unwrap();
    path
}

fn dir_job(site: &str, bucket: &str, file: &str) -> String {
    format!("site:{site}:dir:{bucket}:{file}")
}

#[allow(clippy::too_many_arguments)]
fn dir_scheduler(
    sites_dir: &Path,
    directory_sites: Option<Vec<String>>,
    max_jobs_per_site: usize,
    clustered: bool,
    self_id: &str,
    lease: Arc<dyn LeaseStore>,
    cursor: Arc<dyn CursorStore>,
    executor: Arc<dyn JobExecutor>,
) -> CronScheduler {
    build(Opts {
        jobs: Vec::new(),
        clustered,
        self_id: self_id.to_string(),
        lease,
        cursor,
        executor,
        max_concurrent: 8,
        sites_dir: Some(sites_dir.to_path_buf()),
        directory_sites,
        max_jobs_per_site,
    })
}

/// E1 + E2 — `.ephpm/cron/` is scanned live each tick (a file dropped after
/// startup fires at the next cadence with no restart), and every directory
/// job runs as PHP for **its** site (`ephpm php --site <site> -f <script>`),
/// with the site root as its working directory.
#[tokio::test]
async fn directory_jobs_are_discovered_live_and_run_as_php_for_their_site() {
    let sites = sites_fixture(&["shop"]);
    let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
    let exec = Arc::new(RecordingExec::default());
    let s = dir_scheduler(
        sites.path(),
        None,
        20,
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        exec.clone(),
    );

    // 01:00 — no files yet: nothing to run.
    assert!(s.jobs_for_tick(&[]).is_empty());
    s.tick(at(2026, 1, 1, 1, 0), &[]).await;
    assert!(exec.runs.lock().unwrap().is_empty());

    // Drop a file (no restart). Its cursor is seeded to the last cadence so
    // the live-rescan path, not the cursor seed, is what this test proves.
    let script = drop_php(sites.path(), "shop", "hourly", "report.php");
    let name = dir_job("shop", "hourly", "report.php");
    cursor.set(&name, at(2026, 1, 1, 1, 0));

    s.tick(at(2026, 1, 1, 2, 0), &[]).await;
    settle().await;
    // Scoped so the guard is released before the next `.await`.
    let req = {
        let reqs = exec.reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1, "the dropped file fired at the next cadence");
        reqs[0].clone()
    };
    assert_eq!(req.job, name);
    assert_eq!(req.site.as_deref(), Some("shop"), "runs as its own site");
    assert!(req.is_php, "directory jobs are PHP-only");
    assert_eq!(req.command, vec![script.to_string_lossy().into_owned()]);
    // The child runs with the site root as cwd, so a relative script resolves
    // there rather than against the server's working directory.
    assert_eq!(req.cwd, Some(sites.path().join("shop")), "cwd is the site root");
    let (prog, args) = exec::build_argv(&req, Path::new("/usr/bin/ephpm"));
    assert_eq!(prog, PathBuf::from("/usr/bin/ephpm"));
    assert_eq!(args[0], "php");
    assert_eq!(args[1..3], ["--site".to_string(), "shop".to_string()]);
    // `-f <script>`, never a leading `--` (which would make php read the
    // program from stdin and the job silently no-op). See the regression test
    // `exec::tests::argv_per_site_php_names_script_with_dash_f`.
    assert_eq!(args[3], "-f");
    assert_eq!(args[4], script.to_string_lossy());
    // Removing the file stops it at the next scan — no restart either way.
    std::fs::remove_file(&script).unwrap();
    s.tick(at(2026, 1, 1, 3, 0), &[]).await;
    settle().await;
    assert_eq!(exec.runs.lock().unwrap().len(), 1, "a removed file no longer fires");
}

/// E3 — the cadence clock: `hourly` :00, `daily` 00:00, `weekly` Sunday 00:00,
/// `monthly` 1st 00:00 (UTC default). 2026-02-01 is a Sunday **and** the 1st,
/// so all four coincide there; the following Monday 00:00 is hourly + daily;
/// 01:00 is hourly only.
#[tokio::test]
async fn directory_cadence_clock_matches_the_fixed_schedules() {
    let sites = sites_fixture(&["shop"]);
    for (bucket, file) in
        [("hourly", "h.php"), ("daily", "d.php"), ("weekly", "w.php"), ("monthly", "m.php")]
    {
        drop_php(sites.path(), "shop", bucket, file);
    }
    let names: Vec<String> =
        [("hourly", "h.php"), ("daily", "d.php"), ("weekly", "w.php"), ("monthly", "m.php")]
            .iter()
            .map(|(b, f)| dir_job("shop", b, f))
            .collect();
    let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
    let exec = Arc::new(RecordingExec::default());
    let s = dir_scheduler(
        sites.path(),
        None,
        20,
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        exec.clone(),
    );
    let seed = |ts: DateTime<Utc>| {
        for n in &names {
            cursor.set(n, ts);
        }
    };
    let fired = || -> Vec<String> {
        let mut v = exec.runs.lock().unwrap().drain(..).collect::<Vec<_>>();
        v.sort();
        v
    };

    seed(at(2026, 1, 31, 23, 59));
    s.tick(at(2026, 2, 1, 0, 0), &[]).await; // Sunday the 1st, midnight
    settle().await;
    assert_eq!(fired(), {
        let mut all = names.clone();
        all.sort();
        all
    });

    seed(at(2026, 2, 1, 23, 59));
    s.tick(at(2026, 2, 2, 0, 0), &[]).await; // Monday the 2nd, midnight
    settle().await;
    assert_eq!(
        fired(),
        vec![dir_job("shop", "daily", "d.php"), dir_job("shop", "hourly", "h.php")]
    );

    seed(at(2026, 2, 2, 0, 59));
    s.tick(at(2026, 2, 2, 1, 0), &[]).await; // 01:00
    settle().await;
    assert_eq!(fired(), vec![dir_job("shop", "hourly", "h.php")]);

    seed(at(2026, 2, 2, 1, 0));
    s.tick(at(2026, 2, 2, 1, 30), &[]).await; // off the clock
    settle().await;
    assert_eq!(fired(), Vec::<String>::new());
}

/// E5 — the directory tier is an **operator** opt-in, per site. OFF ⇒ a
/// tenant's files are ignored; ON for `shop` only ⇒ `blog`'s files are ignored;
/// a tenant can tighten (fewer files) but never loosen: the operator's
/// `max_jobs_per_site` cap, site binding, `forbid` policy, no catch-up and the
/// directory timeout are fixed on every resolved directory job.
#[tokio::test]
async fn directory_tier_is_an_operator_opt_in_per_site_that_tenants_cannot_loosen() {
    let sites = sites_fixture(&["shop", "blog"]);
    drop_php(sites.path(), "shop", "hourly", "a.php");
    drop_php(sites.path(), "blog", "hourly", "b.php");
    let cursor: Arc<InMemCursor> = Arc::new(InMemCursor::default());
    for (site, file) in [("shop", "a.php"), ("blog", "b.php")] {
        cursor.set(&dir_job(site, "hourly", file), at(2026, 1, 1, 0, 59));
    }
    let tick = at(2026, 1, 1, 1, 0);

    // OFF: the tenant dropped a file but the operator never enabled the tier.
    let exec = Arc::new(RecordingExec::default());
    let off = dir_scheduler(
        sites.path(),
        Some(Vec::new()),
        20,
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        exec.clone(),
    );
    assert!(off.jobs_for_tick(&[]).is_empty(), "tier off: no directory jobs are even considered");
    off.tick(tick, &[]).await;
    settle().await;
    assert!(exec.runs.lock().unwrap().is_empty(), "tier off: a tenant cannot self-enable");

    // ON for shop only.
    let exec = Arc::new(RecordingExec::default());
    let shop_only = dir_scheduler(
        sites.path(),
        Some(vec!["shop".to_string()]),
        20,
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        exec.clone(),
    );
    shop_only.tick(tick, &[]).await;
    settle().await;
    assert_eq!(
        *exec.runs.lock().unwrap(),
        vec![dir_job("shop", "hourly", "a.php")],
        "only the opted-in site fires"
    );
    assert_eq!(
        cursor.get(&dir_job("blog", "hourly", "b.php")),
        Some(at(2026, 1, 1, 0, 59)),
        "blog untouched"
    );

    // The operator's limits are fixed on every resolved directory job.
    let jobs = shop_only.jobs_for_tick(&[]);
    assert_eq!(jobs.len(), 1);
    let j = &jobs[0];
    assert_eq!(j.tier, Tier::Directory);
    assert_eq!(j.site.as_deref(), Some("shop"));
    assert!(j.is_php);
    assert_eq!(j.concurrency_policy, ConcurrencyPolicy::Forbid);
    assert_eq!(j.starting_deadline, None, "no catch-up for directory jobs");
    assert!(!j.suspend);
    assert_eq!(j.timeout, std::time::Duration::from_secs(3600));

    // Cap: three files, max_jobs_per_site = 1 → one job (a tenant cannot loosen).
    drop_php(sites.path(), "shop", "hourly", "c.php");
    drop_php(sites.path(), "shop", "hourly", "d.php");
    let capped = dir_scheduler(
        sites.path(),
        None,
        1,
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        Arc::new(RecordingExec::default()),
    );
    let shop_jobs: Vec<_> = capped
        .jobs_for_tick(&[])
        .into_iter()
        .filter(|j| j.site.as_deref() == Some("shop"))
        .collect();
    assert_eq!(shop_jobs.len(), 1, "capped at the operator's max_jobs_per_site");
}

/// E6 — directory jobs run through the **same** ownership gate and per-tick
/// lease as operator jobs: under stable membership only the site's HRW owner
/// scans + fires (the other node records nothing for the job — it never even
/// scans a site it does not own); during an ownership overlap the shared lease
/// yields exactly one fire. Per-node cursor views (as in the operator overlap
/// test) so the lease, not a shared cursor, is what decides the overlap.
#[test]
fn directory_jobs_pass_the_same_gate_and_lease() {
    let (total_runs, snap) = with_metrics(|| async {
        let sites = sites_fixture(&["shop"]);
        drop_php(sites.path(), "shop", "hourly", "a.php");
        let name = dir_job("shop", "hourly", "a.php");
        let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
        let cur0: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        let cur1: Arc<InMemCursor> = Arc::new(InMemCursor::default());
        cur0.set(&name, at(2026, 1, 1, 0, 59));
        cur1.set(&name, at(2026, 1, 1, 0, 59));
        let exec = Arc::new(RecordingExec::default());
        let mk = |id: &str, cur: Arc<InMemCursor>| {
            dir_scheduler(sites.path(), None, 20, true, id, lease.clone(), cur, exec.clone())
        };
        let (n0, n1) = (mk("ephpm-0", cur0.clone()), mk("ephpm-1", cur1.clone()));
        let full = [alive("ephpm-0"), alive("ephpm-1")];
        let owner = owner_of(&full, &["ephpm-0", "ephpm-1"], "shop");
        let (owner_s, other_s) = if owner == "ephpm-0" { (&n0, &n1) } else { (&n1, &n0) };

        // Stable membership: the non-owner does not even scan the site.
        assert_eq!(other_s.jobs_for_tick(&full).len(), 0, "a non-owner scans nothing for the site");
        assert_eq!(owner_s.jobs_for_tick(&full).len(), 1);
        n0.tick(at(2026, 1, 1, 1, 0), &full).await;
        n1.tick(at(2026, 1, 1, 1, 0), &full).await;
        settle().await;
        assert_eq!(fire_count(owner_s, &name).await, 1);
        assert_eq!(fire_count(other_s, &name).await, 0);

        // Overlap (each sees only itself): both scan, both pass the gate, the
        // lease picks exactly one.
        cur0.set(&name, at(2026, 1, 1, 1, 0));
        cur1.set(&name, at(2026, 1, 1, 1, 0));
        n0.tick(at(2026, 1, 1, 2, 0), &[alive("ephpm-0")]).await;
        n1.tick(at(2026, 1, 1, 2, 0), &[alive("ephpm-1")]).await;
        settle().await;
        assert_eq!(
            fire_count(&n0, &name).await + fire_count(&n1, &name).await,
            2,
            "1 + 1 across both ticks"
        );
        exec.runs.lock().unwrap().len()
    });
    assert_eq!(total_runs, 2, "one run per hour across two nodes");
    let name = dir_job("shop", "hourly", "a.php");
    assert_eq!(skips(&snap, &name, metrics::SKIP_LEASE_LOST), 1, "the overlap loser: lease_lost");
    assert_eq!(
        counter(&snap, metrics::RUNS_STARTED, &[("tier", "directory"), ("site", "shop")]),
        2
    );
}

// ── trigger endpoint (F) ────────────────────────────────────────────────────

/// F1 — `run-now` (no `--force`) **takes the scheduled tick's lease**, so it
/// cannot double-fire with a scheduled run of the same tick: after the
/// scheduler fired the current minute, a `run-now` in that minute is refused
/// (`lease_lost`).
///
/// `trigger_now` reads the wall clock, so the test pins the scheduled fire to
/// `floor_minute(Utc::now())` and retries if the minute rolls over in between.
///
/// `trigger_now` floors `Utc::now()` to the minute (`mod.rs`), so its ad-hoc
/// lease shares the scheduled tick's key `cron:lease:<job>:<minute_epoch>` and
/// the second run of the same minute is refused rather than double-firing.
#[test]
fn run_now_takes_the_scheduled_ticks_lease() {
    for _attempt in 0..3 {
        let ((fires, msg, conclusive), snap) = with_metrics(|| async {
            let lease: Arc<InMemLease> = Arc::new(InMemLease::default());
            let exec = Arc::new(RecordingExec::default());
            let s = scheduler(
                vec![every_minute_job("j", None)],
                false,
                "solo",
                lease.clone(),
                Arc::new(InMemCursor::default()),
                exec.clone(),
                4,
            );
            let tick_ts = floor_minute(Utc::now());
            let job = s.operator_jobs[0].clone();
            s.try_fire(&job, tick_ts).await; // the scheduled run of this minute
            settle().await; // ...which has finished (no `forbid` overlap in play)
            assert!(lease.holder(&lease::lease_key("j", tick_ts)).is_some());
            let (_fired, msg) = s.trigger_now("j", false, &[]).await;
            settle().await;
            let conclusive = floor_minute(Utc::now()) == tick_ts;
            (fire_count(&s, "j").await, msg, conclusive)
        });
        if !conclusive {
            continue; // the minute rolled over between the two calls; retry
        }
        assert_eq!(
            fires, 1,
            "run-now in the same minute as the scheduled run must not double-fire ({msg})"
        );
        assert_eq!(skips(&snap, "j", metrics::SKIP_LEASE_LOST), 1, "refused by the tick lease");
        return;
    }
    panic!("the minute boundary was crossed on three consecutive attempts");
}

/// F2 — `--force` bypasses the lease for a true ad-hoc run; a plain `run-now`
/// still honours it (here: an unreachable lease store refuses, fail-closed).
/// Unknown names are reported, not fired.
#[test]
fn run_now_force_bypasses_the_lease_and_plain_run_now_honours_it() {
    let (
        (
            plain_ran,
            plain_fires,
            plain_msg,
            forced_ran,
            forced_fires,
            forced_msg,
            unknown_ran,
            unknown_msg,
            runs,
        ),
        snap,
    ) = with_metrics(|| async {
        let lease: Arc<dyn LeaseStore> =
            Arc::new(InMemLease { unreachable: true, ..Default::default() });
        let exec = Arc::new(RecordingExec::default());
        let s = scheduler(
            vec![every_minute_job("j", None)],
            false,
            "solo",
            lease,
            Arc::new(InMemCursor::default()),
            exec.clone(),
            4,
        );
        let (plain_ran, plain_msg) = s.trigger_now("j", false, &[]).await;
        settle().await;
        let plain_fires = fire_count(&s, "j").await;
        let (forced_ran, forced_msg) = s.trigger_now("j", true, &[]).await;
        settle().await;
        let forced_fires = fire_count(&s, "j").await;
        let (unknown_ran, unknown_msg) = s.trigger_now("nope", true, &[]).await;
        settle().await;
        (
            plain_ran,
            plain_fires,
            plain_msg,
            forced_ran,
            forced_fires,
            forced_msg,
            unknown_ran,
            unknown_msg,
            exec.runs.lock().unwrap().len(),
        )
    });
    assert!(!plain_ran, "plain run-now did not fire: {plain_msg}");
    assert_eq!(plain_fires, 0, "plain run-now honours the (unreachable) lease: {plain_msg}");
    // The message now names the actual outcome (unreachable lease, fail-closed)
    // instead of the old blanket "gated + leased" that fired-or-not alike.
    assert!(plain_msg.contains("lease owner unreachable"), "{plain_msg}");
    assert_eq!(skips(&snap, "j", metrics::SKIP_LEASE_LOST), 1);
    assert!(forced_ran, "--force reports a real run: {forced_msg}");
    assert_eq!(forced_fires, 1, "--force bypasses the lease: {forced_msg}");
    assert!(forced_msg.contains("forced"), "{forced_msg}");
    assert!(!unknown_ran, "an unknown job did not fire: {unknown_msg}");
    assert!(unknown_msg.contains("no such job"), "{unknown_msg}");
    assert_eq!(runs, 1, "exactly the forced run executed");
}

/// F3 — the status JSON behind `ephpm cron status` surfaces schedule, timezone,
/// next fire, last schedule time, last outcome and run counts.
#[tokio::test]
async fn status_json_surfaces_schedule_timezone_next_and_last_run() {
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let mut job = daily_3am_job("nightly", Some("shop"));
    job.tz = chrono_tz::America::Chicago;
    let s = scheduler(
        vec![job],
        false,
        "solo",
        Arc::new(InMemLease::default()),
        cursor.clone(),
        Arc::new(RecordingExec::default()),
        4,
    );
    let before: serde_json::Value = serde_json::from_str(&s.status_json()).unwrap();
    assert_eq!(before["node"], "solo");
    assert_eq!(before["clustered"], false);
    assert_eq!(before["max_concurrent"], 4);
    let j = &before["jobs"][0];
    assert_eq!(j["name"], "nightly");
    assert_eq!(j["site"], "shop");
    assert_eq!(j["tier"], "operator");
    assert_eq!(j["schedule"], "0 3 * * *");
    assert_eq!(j["timezone"], "America/Chicago");
    assert_eq!(j["suspended"], false);
    assert!(j["next_fire"].is_string(), "next fire is computed from the schedule");
    assert!(j["last_schedule_time"].is_null());
    assert_eq!(j["runs"], 0);

    // 03:00 Chicago (CST, UTC-6) = 09:00 UTC.
    cursor.set("nightly", at(2026, 1, 1, 9, 0));
    let tick = at(2026, 1, 2, 9, 0);
    s.tick(tick, &[]).await;
    settle().await;
    let after: serde_json::Value = serde_json::from_str(&s.status_json()).unwrap();
    let j = &after["jobs"][0];
    assert_eq!(j["runs"], 1);
    assert_eq!(j["last_schedule_time"], tick.to_rfc3339());
    assert_eq!(j["last_tick"], tick.to_rfc3339());
    assert_eq!(j["last_outcome"], "ok");
    assert!(j["last_started"].is_string());
    assert!(j["last_duration_ms"].is_number());
}

// ── resolution (A5/A6) ──────────────────────────────────────────────────────

/// A5/A6 — a job that omits `timezone` resolves to the `[cron]` default; a
/// per-job `timezone` overrides it; the defaults for the other knobs are the
/// documented ones.
#[test]
fn resolve_operator_job_applies_default_timezone_and_per_job_override() {
    let base = ephpm_config::CronJob {
        name: "j".into(),
        site: None,
        schedule: "0 3 * * *".into(),
        timezone: None,
        command: vec!["maintenance".into()],
        timeout: "1h".into(),
        concurrency_policy: "forbid".into(),
        starting_deadline: None,
        suspend: false,
    };
    let r = resolve_operator_job(&base, "Europe/Berlin").unwrap();
    assert_eq!(r.tz, chrono_tz::Europe::Berlin, "omitted timezone → the [cron] default");
    assert_eq!(r.timeout, std::time::Duration::from_secs(3600));
    assert_eq!(r.concurrency_policy, ConcurrencyPolicy::Forbid);
    assert_eq!(r.starting_deadline, None);
    assert!(!r.is_php);
    assert_eq!(r.tier, Tier::Operator);

    let overridden = ephpm_config::CronJob {
        timezone: Some("America/Chicago".into()),
        starting_deadline: Some("10m".into()),
        ..base.clone()
    };
    let r = resolve_operator_job(&overridden, "Europe/Berlin").unwrap();
    assert_eq!(r.tz, chrono_tz::America::Chicago, "per-job timezone wins");
    assert_eq!(r.starting_deadline, Some(std::time::Duration::from_secs(600)));

    let utc_default = resolve_operator_job(&base, "UTC").unwrap();
    assert_eq!(utc_default.tz, chrono_tz::UTC);
}

// ── metrics presence ────────────────────────────────────────────────────────

/// Every `reason` label is emitted by a real path at least once — `overlap`,
/// `lease_lost`, `not_owner`, `missed_valve`, `at_cap` — plus runs started,
/// runs finished by outcome, the duration histogram and the last-schedule
/// gauge. No phantom metrics: everything asserted here is what the scheduler
/// recorded.
#[test]
fn every_skip_reason_and_run_metric_is_emitted_by_a_real_path() {
    let ((), snap) = with_metrics(|| async {
        let t0 = at(2026, 1, 1, 0, 0);
        let t2 = at(2026, 1, 1, 0, 2);

        // not_owner: a clustered non-leader.
        let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
        cursor.set("global", t0);
        let follower = scheduler(
            vec![every_minute_job("global", None)],
            true,
            "ephpm-1",
            Arc::new(InMemLease::default()),
            cursor,
            Arc::new(RecordingExec::default()),
            4,
        );
        follower.tick(t2, &[alive("ephpm-0"), alive("ephpm-1")]).await;

        // lease_lost: unreachable lease store.
        let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
        cursor.set("leased", t0);
        let unreachable = scheduler(
            vec![every_minute_job("leased", None)],
            false,
            "solo",
            Arc::new(InMemLease { unreachable: true, ..Default::default() }),
            cursor,
            Arc::new(RecordingExec::default()),
            4,
        );
        unreachable.tick(t2, &[]).await;

        // overlap: forbid with a run still active.
        let slow = scheduler(
            vec![every_minute_job("slow", None)],
            false,
            "solo",
            Arc::new(InMemLease::default()),
            Arc::new(InMemCursor::default()),
            Arc::new(RecordingExec { sleep_ms: 400, ..Default::default() }),
            4,
        );
        let job = slow.operator_jobs[0].clone();
        slow.try_fire(&job, t2).await;
        slow.try_fire(&job, at(2026, 1, 1, 0, 3)).await;

        // at_cap: max_concurrent = 1 with the slot taken.
        let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
        cursor.set("a", t0);
        cursor.set("b", t0);
        let capped = scheduler(
            vec![every_minute_job("a", None), every_minute_job("b", None)],
            false,
            "solo",
            Arc::new(InMemLease::default()),
            cursor,
            Arc::new(RecordingExec { sleep_ms: 400, ..Default::default() }),
            1,
        );
        capped.tick(t2, &[]).await;

        // missed_valve: >100 missed with catch-up on.
        let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
        cursor.set("valve", t0);
        let mut job = every_minute_job("valve", None);
        job.starting_deadline = Some(std::time::Duration::from_secs(600));
        let valve = scheduler(
            vec![job],
            false,
            "solo",
            Arc::new(InMemLease::default()),
            cursor,
            Arc::new(RecordingExec::default()),
            4,
        );
        valve.tick(at(2026, 1, 1, 3, 0), &[]).await;

        // A clean run, start to finish (ok).
        let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
        cursor.set("ok", t0);
        let fine = scheduler(
            vec![every_minute_job("ok", Some("shop"))],
            false,
            "solo",
            Arc::new(InMemLease::default()),
            cursor,
            Arc::new(RecordingExec::default()),
            4,
        );
        fine.tick(t2, &[]).await;
        settle().await;
    });

    let skipped = |job: &str, reason: &str| {
        counter(&snap, metrics::SKIPPED_TOTAL, &[("job", job), ("reason", reason)])
    };
    assert_eq!(skipped("global", metrics::SKIP_NOT_OWNER), 1);
    assert_eq!(skipped("leased", metrics::SKIP_LEASE_LOST), 1);
    assert_eq!(skipped("slow", metrics::SKIP_OVERLAP), 1);
    assert_eq!(skipped("b", metrics::SKIP_AT_CAP), 1);
    assert_eq!(skipped("valve", metrics::SKIP_MISSED_VALVE), 1);
    // The reason set is exactly the five documented values.
    let mut reasons: Vec<String> = snap
        .iter()
        .filter(|(k, ..)| k.key().name() == metrics::SKIPPED_TOTAL)
        .filter_map(|(k, ..)| {
            k.key().labels().find(|l| l.key() == "reason").map(|l| l.value().to_string())
        })
        .collect();
    reasons.sort();
    reasons.dedup();
    assert_eq!(reasons, vec!["at_cap", "lease_lost", "missed_valve", "not_owner", "overlap"]);

    // Runs: started + finished-by-outcome + duration + last-schedule gauge.
    assert_eq!(
        counter(
            &snap,
            metrics::RUNS_STARTED,
            &[("job", "ok"), ("site", "shop"), ("tier", "operator")]
        ),
        1
    );
    assert_eq!(counter(&snap, metrics::RUNS_TOTAL, &[("job", "ok"), ("outcome", "ok")]), 1);
    assert_eq!(
        histogram_samples(&snap, metrics::DURATION_SECONDS, &[("job", "ok"), ("site", "shop")]),
        1
    );
    let g = gauge(&snap, metrics::LAST_SCHEDULE_TIMESTAMP, &[("job", "ok"), ("site", "shop")])
        .expect("gauge set");
    assert!(
        (g - 1_767_225_720.0).abs() < f64::EPSILON,
        "2026-01-01T00:02:00Z as unix seconds, got {g}"
    );
    // Siteless jobs carry the `-` site label.
    assert_eq!(counter(&snap, metrics::SKIPPED_TOTAL, &[("job", "global"), ("site", "-")]), 1);
}

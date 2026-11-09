//! Deterministic tests for the scheduler's exactly-once behaviour.
//!
//! The lease/cursor/executor are in-memory fakes and the clock + membership are
//! injected (`tick(now, nodes)`), so every proof is reproducible with no
//! wall-clock sleeps on the decision path. Covered: single-node firing, cluster
//! exactly-once under stable membership (gate), exactly-once during an
//! ownership-move overlap (lease backstop), a clean ownership move, and the
//! concurrency/cap guards.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use ephpm_cluster::{NodeInfo, NodeState};
use ephpm_config::CronSchedule;

use super::*;
use self::exec::{ExecRequest, RunOutcome};
use self::lease::{CursorStore, LeaseOutcome, LeaseStore};

// ── fakes ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct InMemLease {
    held: Mutex<HashMap<String, Vec<u8>>>,
    unreachable: bool,
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
    runs: Arc<Mutex<Vec<String>>>,
    sleep_ms: u64,
}

impl JobExecutor for RecordingExec {
    fn run<'a>(
        &'a self,
        req: ExecRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RunOutcome> + Send + 'a>> {
        let runs = Arc::clone(&self.runs);
        let sleep_ms = self.sleep_ms;
        Box::pin(async move {
            runs.lock().unwrap().push(req.job);
            if sleep_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
            }
            RunOutcome::Ok
        })
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}

fn alive(id: &str) -> NodeInfo {
    NodeInfo::new(id, format!("{id}:7000"), NodeState::Alive)
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
    CronScheduler {
        operator_jobs: jobs,
        config_path: None,
        directory_sites: Some(Vec::new()),
        sites_dir: None,
        cadence: directory::CadenceSchedules::new(None, None, None, None).unwrap(),
        directory_timeout: std::time::Duration::from_secs(3600),
        max_jobs_per_site: 20,
        default_tz: chrono_tz::UTC,
        clustered,
        cluster: None,
        self_id: self_id.to_string(),
        lease,
        cursor,
        executor,
        running: RunningState::new(max_concurrent),
        status: Arc::new(std::sync::Mutex::new(HashMap::new())),
    }
}

async fn fire_count(s: &CronScheduler, job: &str) -> u64 {
    s.status.lock().unwrap().get(job).map_or(0, |st| st.runs)
}

async fn skip_count(s: &CronScheduler, job: &str) -> u64 {
    s.status.lock().unwrap().get(job).map_or(0, |st| st.skips)
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn single_node_fires_and_executes_command() {
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let runs = Arc::new(Mutex::new(Vec::new()));
    let executor: Arc<dyn JobExecutor> =
        Arc::new(RecordingExec { runs: Arc::clone(&runs), sleep_ms: 0 });

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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(&*runs.lock().unwrap(), &["cleanup".to_string()], "the command ran");
    // The cursor advanced to the fired tick.
    assert_eq!(cursor.get("cleanup"), Some(at(2026, 1, 1, 0, 2)));
}

#[tokio::test]
async fn first_tick_after_start_does_not_catch_up() {
    // With no cursor, it is seeded to `now`; the half-open window is empty, so
    // nothing fires until the next matching tick (no catch-up on boot).
    let s = scheduler(
        vec![every_minute_job("j", None)],
        false,
        "solo",
        Arc::new(InMemLease::default()),
        Arc::new(InMemCursor::default()),
        Arc::new(RecordingExec::default()),
        4,
    );
    s.tick(at(2026, 1, 1, 0, 0), &[]).await;
    assert_eq!(fire_count(&s, "j").await, 0);
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
            Arc::new(RecordingExec { runs: Arc::clone(&runs), sleep_ms: 0 });
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

#[tokio::test]
async fn clustered_exactly_once_during_ownership_overlap() {
    // Both nodes believe they own the site (a stale/partitioned membership view
    // during a handoff): each sees only itself alive, so both pass the gate.
    // The shared per-tick lease is the backstop — exactly one fires.
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor0: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let cursor1: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    cursor0.set("shop-job", at(2026, 1, 1, 0, 0));
    cursor1.set("shop-job", at(2026, 1, 1, 0, 0));

    let n0 = scheduler(
        vec![every_minute_job("shop-job", Some("shop"))],
        true,
        "ephpm-0",
        Arc::clone(&lease),
        cursor0,
        Arc::new(RecordingExec::default()),
        4,
    );
    let n1 = scheduler(
        vec![every_minute_job("shop-job", Some("shop"))],
        true,
        "ephpm-1",
        Arc::clone(&lease),
        cursor1,
        Arc::new(RecordingExec::default()),
        4,
    );
    let now = at(2026, 1, 1, 0, 2);
    // Each node's membership view contains only itself → both are "owner".
    n0.tick(now, &[alive("ephpm-0")]).await;
    n1.tick(now, &[alive("ephpm-1")]).await;

    let fires = fire_count(&n0, "shop-job").await + fire_count(&n1, "shop-job").await;
    assert_eq!(fires, 1, "the lease prevents a double-fire during the overlap");
}

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
    // Who owns the site in a full membership?
    let owner = ["ephpm-0", "ephpm-1"]
        .into_iter()
        .find(|id| gate::responsible_for_site(&nodes, id, site) == Responsibility::Yes)
        .unwrap();

    // Tick 1: the owner fires.
    let now1 = at(2026, 1, 1, 0, 2);
    n0.tick(now1, &nodes).await;
    n1.tick(now1, &nodes).await;
    assert_eq!(
        fire_count(&n0, "cat-job").await + fire_count(&n1, "cat-job").await,
        1,
        "tick 1 fires exactly once"
    );

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
    let tick2_fires = (fire_count(&n0, "cat-job").await + fire_count(&n1, "cat-job").await) - 1;
    assert_eq!(tick2_fires, 1, "tick 2 fires exactly once on the new owner — no double, no drop");
}

#[tokio::test]
async fn lease_unreachable_fails_closed() {
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease { unreachable: true, ..Default::default() });
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    cursor.set("j", at(2026, 1, 1, 0, 0));
    let s = scheduler(
        vec![every_minute_job("j", None)],
        false,
        "solo",
        lease,
        cursor,
        Arc::new(RecordingExec::default()),
        4,
    );
    s.tick(at(2026, 1, 1, 0, 2), &[]).await;
    assert_eq!(fire_count(&s, "j").await, 0, "an unreachable lease owner never runs");
    assert!(skip_count(&s, "j").await >= 1, "the skip is recorded (lease_lost)");
}

#[tokio::test]
async fn forbid_policy_skips_an_overlapping_run() {
    // A slow executor keeps the first run registered; a second due tick with
    // concurrency_policy = forbid is skipped as overlap.
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let runs = Arc::new(Mutex::new(Vec::new()));
    let executor: Arc<dyn JobExecutor> =
        Arc::new(RecordingExec { runs: Arc::clone(&runs), sleep_ms: 400 });
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
async fn node_cap_skips_when_full() {
    // max_concurrent = 1, a slow run occupying the slot → a second job is at_cap.
    let lease: Arc<dyn LeaseStore> = Arc::new(InMemLease::default());
    let cursor: Arc<dyn CursorStore> = Arc::new(InMemCursor::default());
    let executor: Arc<dyn JobExecutor> =
        Arc::new(RecordingExec { runs: Arc::new(Mutex::new(Vec::new())), sleep_ms: 400 });
    let s = scheduler(
        vec![every_minute_job("a", None), every_minute_job("b", None)],
        false,
        "solo",
        lease,
        cursor.clone(),
        executor,
        1,
    );
    cursor.set("a", at(2026, 1, 1, 0, 0));
    cursor.set("b", at(2026, 1, 1, 0, 0));
    s.tick(at(2026, 1, 1, 0, 2), &[]).await;

    let fires = fire_count(&s, "a").await + fire_count(&s, "b").await;
    assert_eq!(fires, 1, "only one job runs with max_concurrent = 1");
    let skips = skip_count(&s, "a").await + skip_count(&s, "b").await;
    assert!(skips >= 1, "the second job is skipped at_cap");
}


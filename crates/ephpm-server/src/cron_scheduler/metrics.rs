//! Prometheus metric names for the cron scheduler.
//!
//! Recorded at their call sites in [`super`] with the `metrics` facade
//! (`counter!`/`histogram!`/`gauge!`), labelled `job` + `site` + `tier` as
//! noted. Documented here so there is one place to read the metric surface;
//! none is a phantom — every const below is incremented/set by the scheduler.

/// Counter: runs started, by `job`, `site`, `tier` (`operator`|`directory`).
/// Incremented the moment a tick wins the gate+lease and the child is spawned.
pub const RUNS_STARTED: &str = "ephpm_cron_runs_started_total";

/// Counter: runs finished, by `job`, `site`, `tier`, and `outcome`
/// (`ok`|`failed`|`timeout`|`killed`|`spawn_error`).
pub const RUNS_TOTAL: &str = "ephpm_cron_runs_total";

/// Histogram: wall-clock duration of a run, by `job`, `site`.
pub const DURATION_SECONDS: &str = "ephpm_cron_duration_seconds";

/// Counter: ticks skipped, by `job`, `site`, and `reason`. The reason set is
/// exactly [`SKIP_OVERLAP`], [`SKIP_LEASE_LOST`], [`SKIP_NOT_OWNER`],
/// [`SKIP_MISSED_VALVE`], [`SKIP_AT_CAP`] — its presence on a node is how
/// exactly-once coverage is confirmed (the non-running nodes count
/// `not_owner`/`lease_lost`).
pub const SKIPPED_TOTAL: &str = "ephpm_cron_skipped_total";

/// Gauge: the last scheduled instant (unix seconds) a job fired, by `job`,
/// `site` — `time() - this` is the staleness an alert watches.
pub const LAST_SCHEDULE_TIMESTAMP: &str = "ephpm_cron_last_schedule_timestamp_seconds";

/// `reason` label: a previous run of this job was still active under
/// `concurrency_policy = "forbid"`.
pub const SKIP_OVERLAP: &str = "overlap";
/// `reason` label: another node holds the tick's lease, or the lease owner was
/// unreachable (fail-closed skip).
pub const SKIP_LEASE_LOST: &str = "lease_lost";
/// `reason` label: this node is not the job's owner/leader (or ownership could
/// not be confirmed — fail closed).
pub const SKIP_NOT_OWNER: &str = "not_owner";
/// `reason` label: more than 100 schedules were missed; catch-up abandoned.
pub const SKIP_MISSED_VALVE: &str = "missed_valve";
/// `reason` label: the node-wide `max_concurrent` cap was reached.
pub const SKIP_AT_CAP: &str = "at_cap";

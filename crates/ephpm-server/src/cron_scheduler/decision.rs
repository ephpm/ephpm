//! The per-tick scheduling decision — pure, deterministic, and the port of
//! Kubernetes `CronJob`'s `getRecentUnmetScheduleTimes` + the ">100 missed
//! start times" safety valve + `startingDeadlineSeconds`.
//!
//! [`evaluate`] takes an explicit `now` and the `lastScheduleTime` cursor (both
//! UTC) and decides, with no I/O, whether this tick fires. The orchestrator in
//! [`super`] gates ownership, the lease, concurrency and the node-wide cap on
//! top of the [`Decision`] returned here; keeping the calendar math pure is
//! what makes the catch-up and DST behaviour unit-testable without a clock.

use chrono::{DateTime, Duration as ChronoDuration, Timelike, Utc};
use chrono_tz::Tz;
use ephpm_config::CronSchedule;

/// The number of missed schedules in the window `(cursor, now]` beyond which
/// catch-up is abandoned (the Kubernetes `tooManyMissedTimes` guard). At or
/// below this, at most one tick is caught up (bounded by the deadline); above
/// it, the cursor jumps to `now` and nothing fires.
pub const MISSED_SCHEDULE_VALVE: usize = 100;

/// What a single tick evaluation decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is due in `(cursor, now]`; the cursor is left untouched.
    Idle,
    /// Fire at `tick_ts`. The orchestrator advances the cursor to `tick_ts`
    /// only once the run actually starts (so a gate/lease loss does not lose
    /// the tick for a peer that still owns it).
    Fire {
        /// The canonical UTC fire instant — identical on every node, and the
        /// value the per-tick lease key is built from.
        tick_ts: DateTime<Utc>,
    },
    /// A tick was due but is deliberately not fired: no catch-up (strict,
    /// default), the deadline window elapsed, or the >100 valve tripped. The
    /// cursor advances to `advance_to` so the window never re-processes it.
    SkipDue {
        /// Where to move `lastScheduleTime` to.
        advance_to: DateTime<Utc>,
        /// `true` only for the >100-missed valve — the one loud skip.
        valve: bool,
    },
}

/// Decide whether `schedule` (in `tz`) fires at `now`, given the
/// `lastScheduleTime` cursor and an optional catch-up `starting_deadline`.
///
/// * `starting_deadline = None` (default): strict, **no catch-up** — only a
///   tick whose instant is the current minute fires; an older missed tick is
///   skipped (cursor advances past it).
/// * `starting_deadline = Some(d)`: the single most-recent missed tick fires if
///   `now - tick <= d`; otherwise it is skipped.
/// * more than [`MISSED_SCHEDULE_VALVE`] missed ticks: give up, jump the cursor
///   to `now`, fire nothing.
#[must_use]
pub fn evaluate(
    schedule: &CronSchedule,
    tz: Tz,
    now: DateTime<Utc>,
    cursor: DateTime<Utc>,
    starting_deadline: Option<std::time::Duration>,
) -> Decision {
    let occ = schedule.occurrences_in(cursor, now, tz, MISSED_SCHEDULE_VALVE);
    let Some(&candidate) = occ.last() else {
        return Decision::Idle;
    };
    if occ.len() > MISSED_SCHEDULE_VALVE {
        return Decision::SkipDue { advance_to: now, valve: true };
    }

    match starting_deadline {
        None => {
            if candidate == floor_minute(now) {
                Decision::Fire { tick_ts: candidate }
            } else {
                Decision::SkipDue { advance_to: candidate, valve: false }
            }
        }
        Some(deadline) => {
            let deadline = ChronoDuration::from_std(deadline).unwrap_or(ChronoDuration::MAX);
            if now - candidate <= deadline {
                Decision::Fire { tick_ts: candidate }
            } else {
                Decision::SkipDue { advance_to: candidate, valve: false }
            }
        }
    }
}

/// Truncate to the start of the minute.
fn floor_minute(ts: DateTime<Utc>) -> DateTime<Utc> {
    ts.with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::UTC;
    use std::time::Duration;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn every_minute() -> CronSchedule {
        CronSchedule::parse("* * * * *").unwrap()
    }

    fn daily_3am() -> CronSchedule {
        CronSchedule::parse("0 3 * * *").unwrap()
    }

    #[test]
    fn idle_when_nothing_due() {
        // Cursor is the last fire; now is a few seconds into the same minute,
        // and a daily schedule has nothing due.
        let cursor = at(2026, 6, 1, 3, 0, 0);
        let now = at(2026, 6, 1, 3, 0, 20);
        assert_eq!(evaluate(&daily_3am(), UTC, now, cursor, None), Decision::Idle);
    }

    #[test]
    fn fires_current_minute() {
        // Last fired at 00:00; now 00:01:05 → the 00:01 tick is current.
        let cursor = at(2026, 1, 1, 0, 0, 0);
        let now = at(2026, 1, 1, 0, 1, 5);
        assert_eq!(
            evaluate(&every_minute(), UTC, now, cursor, None),
            Decision::Fire { tick_ts: at(2026, 1, 1, 0, 1, 0) }
        );
    }

    #[test]
    fn strict_skips_missed_ticks_without_catch_up() {
        // The scheduler was blocked: last fired 00:00, now 00:05:02. The
        // newest due tick is 00:05 (current) → fire it; 00:01..00:04 are
        // silently skipped (no catch-up). Cursor will advance to 00:05.
        let cursor = at(2026, 1, 1, 0, 0, 0);
        let now = at(2026, 1, 1, 0, 5, 2);
        assert_eq!(
            evaluate(&every_minute(), UTC, now, cursor, None),
            Decision::Fire { tick_ts: at(2026, 1, 1, 0, 5, 0) }
        );

        // A daily job whose only due tick is in the past (not the current
        // minute) is skipped, cursor advanced past it — no catch-up.
        let cursor = at(2026, 1, 1, 2, 0, 0);
        let now = at(2026, 1, 1, 9, 0, 0); // well past 03:00
        assert_eq!(
            evaluate(&daily_3am(), UTC, now, cursor, None),
            Decision::SkipDue { advance_to: at(2026, 1, 1, 3, 0, 0), valve: false }
        );
    }

    #[test]
    fn deadline_fires_recent_missed_within_window() {
        // Daily 03:00; cursor yesterday; now 03:04 (4 min late) with a 10-min
        // deadline → catch up the 03:00 tick.
        let cursor = at(2026, 1, 1, 3, 0, 0);
        let now = at(2026, 1, 2, 3, 4, 0);
        assert_eq!(
            evaluate(&daily_3am(), UTC, now, cursor, Some(Duration::from_secs(600))),
            Decision::Fire { tick_ts: at(2026, 1, 2, 3, 0, 0) }
        );
    }

    #[test]
    fn deadline_skips_when_window_elapsed() {
        // Same, but now is 20 min late with a 10-min deadline → skip.
        let cursor = at(2026, 1, 1, 3, 0, 0);
        let now = at(2026, 1, 2, 3, 20, 0);
        assert_eq!(
            evaluate(&daily_3am(), UTC, now, cursor, Some(Duration::from_secs(600))),
            Decision::SkipDue { advance_to: at(2026, 1, 2, 3, 0, 0), valve: false }
        );
    }

    #[test]
    fn deadline_only_the_most_recent_missed_runs() {
        // Three days of missed 03:00 ticks, generous deadline: only the most
        // recent (newest) fires; the cursor advances to it, dropping the
        // older two.
        let cursor = at(2026, 1, 1, 3, 0, 0);
        let now = at(2026, 1, 4, 3, 1, 0);
        assert_eq!(
            evaluate(&daily_3am(), UTC, now, cursor, Some(Duration::from_secs(86_400))),
            Decision::Fire { tick_ts: at(2026, 1, 4, 3, 0, 0) }
        );
    }

    #[test]
    fn valve_trips_above_one_hundred_missed() {
        // Every-minute schedule, cursor ~3 hours ago → >100 missed ticks:
        // give up, advance to now, fire nothing.
        let now = at(2026, 1, 1, 3, 0, 30);
        let cursor = at(2026, 1, 1, 0, 0, 0);
        match evaluate(&every_minute(), UTC, now, cursor, Some(Duration::from_secs(600))) {
            Decision::SkipDue { advance_to, valve } => {
                assert!(valve, "the >100 valve must flag the loud skip");
                assert_eq!(advance_to, now);
            }
            other => panic!("expected the valve to trip, got {other:?}"),
        }
    }

    #[test]
    fn exactly_one_hundred_missed_is_not_the_valve() {
        // 100 missed ticks is at the boundary — catch-up still applies (the
        // valve is strictly ">100"). With a deadline the most recent fires.
        let now = at(2026, 1, 1, 1, 40, 10); // 100 minutes after cursor
        let cursor = at(2026, 1, 1, 0, 0, 0);
        assert_eq!(
            evaluate(&every_minute(), UTC, now, cursor, Some(Duration::from_secs(600))),
            Decision::Fire { tick_ts: at(2026, 1, 1, 1, 40, 0) }
        );
    }
}

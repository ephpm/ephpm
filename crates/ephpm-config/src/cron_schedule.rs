//! Five-field cron schedule parsing and DST-aware evaluation.
//!
//! This module is the single implementation of cron calendar math for ePHPm.
//! `Config::validate` uses it to reject a bad `schedule`/`timezone` at startup,
//! and the distributed scheduler in `ephpm-server` reuses the exact same
//! [`CronSchedule`] type to decide when a job fires — so the two can never
//! disagree about a fire instant (which is what the per-tick lease key is built
//! from; see the scheduler's exactly-once model).
//!
//! # Grammar
//!
//! A standard five-field Vixie cron expression: `minute hour day-of-month
//! month day-of-week`, one-minute granularity (there is deliberately **no**
//! seconds field — a poll plus a gossip lease round-trip costs seconds, so a
//! sub-minute cadence cannot be honoured, and five fields make sub-minute
//! expression impossible by construction). Each field is a comma list of
//! `*`, `N`, `A-B`, `*/S`, or `A-B/S`; month accepts `JAN`..`DEC` and
//! day-of-week `SUN`..`SAT` (case-insensitive). Day-of-week is `0`..`6` with
//! `0 == 7 == Sunday`.
//!
//! When **both** day-of-month and day-of-week are restricted (neither is `*`),
//! a day matches if **either** matches — the historical Vixie behaviour.
//!
//! # Timezone and DST
//!
//! Evaluation is always against an explicit [`chrono::DateTime<Utc>`] instant,
//! resolved into the job's IANA timezone. All coordination (the tick instant,
//! the lease key, `lastScheduleTime`) is therefore in UTC and identical on
//! every node regardless of its own `/etc/localtime`. DST policy mirrors
//! Kubernetes `CronJob` 1.27+ `.spec.timeZone`:
//!
//! * a wall-clock minute that does **not exist** (spring-forward gap) is
//!   skipped — no UTC instant maps to it, so it simply never fires;
//! * a wall-clock minute that occurs **twice** (fall-back overlap) fires
//!   **once**, at the first (earliest) occurrence.

use chrono::{DateTime, Datelike, LocalResult, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// The largest span [`CronSchedule::next_after`] will search before giving up
/// and returning `None`. Four years comfortably covers `0 0 29 2 *`
/// (Feb 29th) and any sane expression; an expression with no occurrence in
/// four years is a misconfiguration, not a schedule.
const MAX_SEARCH_MINUTES: i64 = 4 * 366 * 24 * 60;

/// A parsed five-field cron expression.
///
/// Construct with [`CronSchedule::parse`]. Evaluation is pure and allocation
/// free on the hot path ([`CronSchedule::fires_at`] is five bitmask lookups
/// plus at most one timezone resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    /// Allowed minutes, bit `n` set ⇒ minute `n` (0..=59).
    minute: u64,
    /// Allowed hours, bit `n` ⇒ hour `n` (0..=23).
    hour: u64,
    /// Allowed days of month, bit `n` ⇒ day `n` (1..=31).
    dom: u64,
    /// Allowed months, bit `n` ⇒ month `n` (1..=12).
    month: u64,
    /// Allowed days of week, bit `n` ⇒ weekday `n` (0..=6, 0 = Sunday).
    dow: u8,
    /// `true` when the day-of-month field was `*` (unrestricted).
    dom_star: bool,
    /// `true` when the day-of-week field was `*` (unrestricted).
    dow_star: bool,
    /// The original expression, kept verbatim for display/status.
    source: String,
}

impl CronSchedule {
    /// Parse a five-field cron expression.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message naming the offending field when the
    /// expression does not have exactly five whitespace-separated fields or a
    /// field is malformed or out of range.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "a cron schedule must have exactly 5 fields \
                 (minute hour day-of-month month day-of-week), got {} in {expr:?}",
                fields.len()
            ));
        }
        let minute = parse_field(fields[0], 0, 59, "minute", None)?;
        let hour = parse_field(fields[1], 0, 23, "hour", None)?;
        let dom = parse_field(fields[2], 1, 31, "day-of-month", None)?;
        let month = parse_field(fields[3], 1, 12, "month", Some(&MONTH_NAMES))?;
        let dow_raw = parse_field(fields[4], 0, 7, "day-of-week", Some(&DOW_NAMES))?;
        // Collapse `7` (Sunday alias) onto `0`.
        let dow = {
            let mut m = u8::try_from(dow_raw & 0x7f).unwrap_or(0);
            if dow_raw & (1 << 7) != 0 {
                m |= 1;
            }
            m
        };
        Ok(Self {
            minute,
            hour,
            dom,
            month,
            dow,
            dom_star: fields[2] == "*",
            dow_star: fields[4] == "*",
            source: expr.trim().to_string(),
        })
    }

    /// The original expression text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Does this schedule fire at exactly `instant`, interpreted in `tz`?
    ///
    /// `instant` is treated at one-minute resolution (seconds and below are
    /// ignored). The DST rules in the module docs are applied: an ambiguous
    /// (fall-back) local minute fires only at its earliest occurrence.
    #[must_use]
    pub fn fires_at(&self, instant: DateTime<Utc>, tz: Tz) -> bool {
        let local = instant.with_timezone(&tz);
        let minute = local.minute();
        let hour = local.hour();
        let dom = local.day();
        let month = local.month();
        let dow = local.weekday().num_days_from_sunday();

        if !bit(self.minute, minute)
            || !bit(self.hour, hour)
            || !bit(self.month, month)
        {
            return false;
        }
        let day_of_month_hit = bit(self.dom, dom);
        let weekday_hit = (self.dow & (1u8 << dow)) != 0;
        let day_ok = match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => day_of_month_hit,
            (true, false) => weekday_hit,
            // Vixie: when both are restricted, either matching day fires.
            (false, false) => day_of_month_hit || weekday_hit,
        };
        if !day_ok {
            return false;
        }

        // Fall-back dedup: an ambiguous wall-clock minute fires once, at the
        // earliest UTC instant mapping to it. The wall clock is taken from the
        // naive local time (no timezone re-resolution — `DateTime::<Tz>::with_*`
        // returns `None` inside a DST fold, which would wrongly drop the fire)
        // and truncated to the minute on the tz-free `NaiveDateTime`.
        let wall = local
            .naive_local()
            .with_second(0)
            .and_then(|w| w.with_nanosecond(0))
            .unwrap_or_else(|| local.naive_local());
        match tz.from_local_datetime(&wall) {
            LocalResult::Single(_) => true,
            // Fall-back: fire once, at the earliest real-time occurrence. The
            // order of chrono's two values is not relied upon — the earlier
            // UTC instant is taken explicitly.
            LocalResult::Ambiguous(a, b) => {
                let first = a.with_timezone(&Utc).min(b.with_timezone(&Utc));
                floor_minute(instant) == first
            }
            LocalResult::None => false,
        }
    }

    /// The next firing instant strictly after `after`, or `None` if there is
    /// none within [`MAX_SEARCH_MINUTES`].
    #[must_use]
    pub fn next_after(&self, after: DateTime<Utc>, tz: Tz) -> Option<DateTime<Utc>> {
        let mut cur = floor_minute(after) + chrono::Duration::minutes(1);
        for _ in 0..MAX_SEARCH_MINUTES {
            if self.fires_at(cur, tz) {
                return Some(cur);
            }
            cur += chrono::Duration::minutes(1);
        }
        None
    }

    /// Every firing instant in the half-open window `(after, now]`, in
    /// ascending order, collecting at most `limit + 1` so a caller can detect
    /// an over-the-limit backlog (the ">100 missed schedules" valve) without
    /// enumerating an unbounded set.
    #[must_use]
    pub fn occurrences_in(
        &self,
        after: DateTime<Utc>,
        now: DateTime<Utc>,
        tz: Tz,
        limit: usize,
    ) -> Vec<DateTime<Utc>> {
        let mut out = Vec::new();
        let mut cur = after;
        while let Some(next) = self.next_after(cur, tz) {
            if next > now {
                break;
            }
            out.push(next);
            cur = next;
            if out.len() > limit {
                break;
            }
        }
        out
    }
}

/// Parse an IANA timezone name (e.g. `"America/Chicago"`, `"UTC"`).
///
/// # Errors
///
/// Returns a message when the name is not a known IANA zone.
pub fn parse_timezone(name: &str) -> Result<Tz, String> {
    name.trim()
        .parse::<Tz>()
        .map_err(|_| format!("{name:?} is not a valid IANA timezone name (e.g. \"UTC\", \"America/Chicago\")"))
}

/// Truncate to the start of the minute (seconds and nanoseconds to zero).
fn floor_minute(ts: DateTime<Utc>) -> DateTime<Utc> {
    ts.with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(ts)
}

/// Is bit `n` set in `mask`?
#[inline]
fn bit(mask: u64, n: u32) -> bool {
    n < 64 && (mask & (1u64 << n)) != 0
}

const MONTH_NAMES: [(&str, u32); 12] = [
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const DOW_NAMES: [(&str, u32); 7] = [
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// Parse one cron field into a bitmask over `[min, max]`.
fn parse_field(
    field: &str,
    min: u32,
    max: u32,
    name: &str,
    names: Option<&[(&str, u32)]>,
) -> Result<u64, String> {
    let mut mask: u64 = 0;
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty entry in {name} field {field:?}"));
        }
        // Split an optional `/step`.
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("invalid step {s:?} in {name} field {field:?}"))?;
                if step == 0 {
                    return Err(format!("step may not be zero in {name} field {field:?}"));
                }
                (r, step)
            }
            None => (part, 1),
        };

        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (
                parse_value(a, name, field, names)?,
                parse_value(b, name, field, names)?,
            )
        } else {
            let v = parse_value(range_part, name, field, names)?;
            (v, v)
        };

        if lo < min || hi > max || lo > hi {
            return Err(format!(
                "{name} value out of range in {field:?} (allowed {min}..={max})"
            ));
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1u64 << v;
            v += step;
        }
    }
    Ok(mask)
}

/// Parse a single cron value, honouring symbolic month/day names.
fn parse_value(
    tok: &str,
    name: &str,
    field: &str,
    names: Option<&[(&str, u32)]>,
) -> Result<u32, String> {
    let tok = tok.trim();
    if let Some(names) = names {
        let lower = tok.to_ascii_lowercase();
        if let Some((_, v)) = names.iter().find(|(n, _)| *n == lower) {
            return Ok(*v);
        }
    }
    tok.parse::<u32>()
        .map_err(|_| format!("invalid {name} value {tok:?} in {field:?}"))
}

/// Compute the UTC instant for a given UTC wall-clock, for tests.
#[cfg(test)]
fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.from_utc_datetime(
        &chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::America::Chicago;
    use chrono_tz::UTC;

    #[test]
    fn rejects_wrong_field_count() {
        assert!(CronSchedule::parse("* * * *").is_err());
        assert!(CronSchedule::parse("* * * * * *").is_err());
        assert!(CronSchedule::parse("").is_err());
    }

    #[test]
    fn rejects_out_of_range_and_bad_syntax() {
        assert!(CronSchedule::parse("60 * * * *").is_err());
        assert!(CronSchedule::parse("* 24 * * *").is_err());
        assert!(CronSchedule::parse("* * 0 * *").is_err()); // dom min is 1
        assert!(CronSchedule::parse("* * * 13 *").is_err());
        assert!(CronSchedule::parse("* * * * 8").is_err());
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
        assert!(CronSchedule::parse("5-3 * * * *").is_err()); // inverted range
        assert!(CronSchedule::parse("abc * * * *").is_err());
    }

    #[test]
    fn every_minute_fires_every_minute() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        assert!(s.fires_at(utc(2026, 3, 1, 12, 34), UTC));
        assert!(s.fires_at(utc(2026, 3, 1, 0, 0), UTC));
    }

    #[test]
    fn daily_at_three_am_utc() {
        let s = CronSchedule::parse("0 3 * * *").unwrap();
        assert!(s.fires_at(utc(2026, 6, 15, 3, 0), UTC));
        assert!(!s.fires_at(utc(2026, 6, 15, 3, 1), UTC));
        assert!(!s.fires_at(utc(2026, 6, 15, 2, 0), UTC));
        let next = s.next_after(utc(2026, 6, 15, 3, 0), UTC).unwrap();
        assert_eq!(next, utc(2026, 6, 16, 3, 0));
    }

    #[test]
    fn step_and_list_syntax() {
        let s = CronSchedule::parse("*/15 * * * *").unwrap();
        assert!(s.fires_at(utc(2026, 1, 1, 0, 0), UTC));
        assert!(s.fires_at(utc(2026, 1, 1, 0, 15), UTC));
        assert!(s.fires_at(utc(2026, 1, 1, 0, 45), UTC));
        assert!(!s.fires_at(utc(2026, 1, 1, 0, 7), UTC));

        let s = CronSchedule::parse("0,30 * * * *").unwrap();
        assert!(s.fires_at(utc(2026, 1, 1, 9, 0), UTC));
        assert!(s.fires_at(utc(2026, 1, 1, 9, 30), UTC));
        assert!(!s.fires_at(utc(2026, 1, 1, 9, 15), UTC));
    }

    #[test]
    fn named_month_and_weekday() {
        let s = CronSchedule::parse("0 0 * JAN SUN").unwrap();
        // Sunday 4 Jan 2026 is a Sunday in January.
        assert!(s.fires_at(utc(2026, 1, 4, 0, 0), UTC));
        // Sunday 1 Feb 2026 — right weekday, wrong month.
        assert!(!s.fires_at(utc(2026, 2, 1, 0, 0), UTC));
    }

    #[test]
    fn dom_or_dow_when_both_restricted() {
        // Vixie: "1st of month OR every Monday".
        let s = CronSchedule::parse("0 0 1 * MON").unwrap();
        // 1 Jan 2026 is a Thursday → matches via day-of-month.
        assert!(s.fires_at(utc(2026, 1, 1, 0, 0), UTC));
        // 5 Jan 2026 is a Monday → matches via day-of-week.
        assert!(s.fires_at(utc(2026, 1, 5, 0, 0), UTC));
        // 6 Jan 2026 is a Tuesday, not the 1st → no match.
        assert!(!s.fires_at(utc(2026, 1, 6, 0, 0), UTC));
    }

    #[test]
    fn timezone_shifts_the_fire_instant() {
        // 03:00 America/Chicago in summer (CDT, UTC-5) is 08:00 UTC.
        let s = CronSchedule::parse("0 3 * * *").unwrap();
        assert!(s.fires_at(utc(2026, 7, 1, 8, 0), Chicago));
        assert!(!s.fires_at(utc(2026, 7, 1, 3, 0), Chicago));
    }

    #[test]
    fn dst_spring_forward_gap_is_skipped() {
        // US DST 2026 starts 2026-03-08: clocks jump 02:00 → 03:00 local, so
        // 02:30 America/Chicago does not exist that day. A "30 2 * * *" job
        // must NOT fire on 2026-03-08 (no UTC instant maps to local 02:30),
        // but must fire the day before and after.
        let s = CronSchedule::parse("30 2 * * *").unwrap();
        // 2026-03-07 02:30 CST (UTC-6) = 08:30 UTC.
        assert!(s.fires_at(utc(2026, 3, 7, 8, 30), Chicago));
        // On the gap day there is no firing instant anywhere in the 24h.
        let day_start = utc(2026, 3, 8, 6, 0); // ~midnight local
        let next = s.next_after(day_start, Chicago).unwrap();
        // The next 02:30 is 2026-03-09 (CDT, UTC-5) = 07:30 UTC.
        assert_eq!(next, utc(2026, 3, 9, 7, 30));
    }

    #[test]
    fn dst_fall_back_runs_once() {
        // US DST 2026 ends 2026-11-01: clocks fall 02:00 → 01:00, so 01:30
        // local occurs twice. A "30 1 * * *" job must fire exactly once.
        let s = CronSchedule::parse("30 1 * * *").unwrap();
        // First (earliest) 01:30 is CDT (UTC-5) = 06:30 UTC; the second is
        // CST (UTC-6) = 07:30 UTC.
        // Discover the two real UTC instants for ambiguous local 01:30.
        let wall = chrono::NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let (earliest, latest) = match Chicago.from_local_datetime(&wall) {
            LocalResult::Ambiguous(a, b) => {
                let ua = a.with_timezone(&Utc);
                let ub = b.with_timezone(&Utc);
                (ua.min(ub), ua.max(ub))
            }
            other => panic!("expected ambiguous local 01:30, got {other:?}"),
        };
        assert!(s.fires_at(earliest, Chicago), "earliest occurrence fires");
        assert!(!s.fires_at(latest, Chicago), "second occurrence does not");

        // And the window evaluation yields exactly one occurrence that day.
        let occ = s.occurrences_in(
            utc(2026, 11, 1, 0, 0),
            utc(2026, 11, 1, 12, 0),
            Chicago,
            100,
        );
        assert_eq!(occ, vec![earliest]);
    }

    #[test]
    fn occurrences_window_and_valve() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        let after = utc(2026, 1, 1, 0, 0);
        let now = utc(2026, 1, 1, 0, 5);
        let occ = s.occurrences_in(after, now, UTC, 100);
        assert_eq!(
            occ,
            vec![
                utc(2026, 1, 1, 0, 1),
                utc(2026, 1, 1, 0, 2),
                utc(2026, 1, 1, 0, 3),
                utc(2026, 1, 1, 0, 4),
                utc(2026, 1, 1, 0, 5),
            ]
        );
        // Over-limit: 10 minutes of an every-minute schedule with limit 3
        // collects limit+1 then stops (valve detection).
        let now = utc(2026, 1, 1, 0, 10);
        let occ = s.occurrences_in(after, now, UTC, 3);
        assert_eq!(occ.len(), 4);
    }

    #[test]
    fn timezone_parse() {
        assert!(parse_timezone("UTC").is_ok());
        assert!(parse_timezone("America/Chicago").is_ok());
        assert!(parse_timezone("Not/AZone").is_err());
        assert!(parse_timezone("").is_err());
    }
}

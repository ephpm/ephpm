//! The tenant directory tier — per-site `.ephpm/cron/` jobs, discovered live.
//!
//! A site opted in by the operator ([`ephpm_config::CronConfig`]'s
//! `directory_cron` / `directory_cron_sites`) may schedule its own PHP jobs by
//! dropping files into `<site-root>/.ephpm/cron/`, which the responsible node
//! re-scans every tick (no restart). The directory is under the hidden-`.ephpm`
//! prefix, so it is never web-served.
//!
//! Two run-parts styles, both **PHP-only** (every file runs as
//! `ephpm php --site <that site>`, which applies the SAME `open_basedir` +
//! `disable_functions` sandbox as that site's HTTP PHP — issue #698, see
//! `crate::router::site_sandbox_ini_defines` and `ephpm::site_sandbox_cli_defines`.
//! The tenant can already run arbitrary PHP in its app, so scheduling PHP that
//! runs under its own web sandbox grants no new privilege — an invariant this
//! must preserve: a `--site` run that skipped that sandbox would let a tenant
//! cron script read the config and every other tenant's derived credentials):
//!
//! * **cadence subdirectories** `hourly/ daily/ weekly/ monthly/` — every
//!   `*.php` runs on that cadence at the fixed clock (operator-overridable);
//! * **`jobs/`** — each `*.php` carries a header comment `// @schedule <expr>`
//!   (and optional `// @timezone <IANA>`); a file with no valid `@schedule` is
//!   ignored with a WARN.
//!
//! A tenant may **tighten** (fewer files) but never self-enable or loosen the
//! operator's limits (the opt-in and `max_jobs_per_site` live in `[cron]`).

use std::path::{Path, PathBuf};

use ephpm_config::CronSchedule;

/// The fixed-clock schedules for the cadence subdirectories, with operator
/// overrides already applied.
#[derive(Debug, Clone)]
pub struct CadenceSchedules {
    /// `hourly/` — default `0 * * * *`.
    pub hourly: CronSchedule,
    /// `daily/` — default `0 0 * * *`.
    pub daily: CronSchedule,
    /// `weekly/` — default `0 0 * * 0` (Sunday midnight).
    pub weekly: CronSchedule,
    /// `monthly/` — default `0 0 1 * *` (1st at midnight).
    pub monthly: CronSchedule,
}

impl CadenceSchedules {
    /// Build from optional operator overrides; a `None` uses the fixed clock.
    ///
    /// # Errors
    ///
    /// When an override is not a valid five-field cron expression. (The config
    /// validation already rejects those at startup; this is a belt-and-braces
    /// re-parse at scan construction.)
    pub fn new(
        hourly: Option<&str>,
        daily: Option<&str>,
        weekly: Option<&str>,
        monthly: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self {
            hourly: CronSchedule::parse(hourly.unwrap_or("0 * * * *"))?,
            daily: CronSchedule::parse(daily.unwrap_or("0 0 * * *"))?,
            weekly: CronSchedule::parse(weekly.unwrap_or("0 0 * * 0"))?,
            monthly: CronSchedule::parse(monthly.unwrap_or("0 0 1 * *"))?,
        })
    }
}

/// One discovered directory-tier job.
#[derive(Debug, Clone)]
pub struct DirectoryJob {
    /// The globally-unique job name (site-scoped), also the lease/metric key.
    pub name: String,
    /// The PHP script to run, absolute.
    pub script: PathBuf,
    /// The schedule (cadence clock or the file's `@schedule`).
    pub schedule: CronSchedule,
    /// The file's `@timezone` override, if any.
    pub timezone: Option<String>,
}

/// Scan a site's `.ephpm/cron/` and return its jobs (capped at `max_jobs`).
///
/// Files beyond `max_jobs` (in a deterministic sorted order) are dropped with a
/// WARN. A missing directory yields an empty list (not an error).
#[must_use]
pub fn scan_site(
    site_key: &str,
    site_root: &Path,
    cadence: &CadenceSchedules,
    max_jobs: usize,
) -> Vec<DirectoryJob> {
    let base = site_root.join(".ephpm").join("cron");
    if !base.is_dir() {
        return Vec::new();
    }

    let mut jobs = Vec::new();
    for (sub, schedule) in [
        ("hourly", &cadence.hourly),
        ("daily", &cadence.daily),
        ("weekly", &cadence.weekly),
        ("monthly", &cadence.monthly),
    ] {
        for script in php_files(&base.join(sub)) {
            let name = job_name(site_key, sub, &script);
            jobs.push(DirectoryJob { name, script, schedule: schedule.clone(), timezone: None });
        }
    }

    // `jobs/` — explicit per-file schedule via a header comment.
    for script in php_files(&base.join("jobs")) {
        match parse_header(&script) {
            Some((schedule, timezone)) => {
                let name = job_name(site_key, "jobs", &script);
                jobs.push(DirectoryJob { name, script, schedule, timezone });
            }
            None => {
                tracing::warn!(
                    site = site_key,
                    file = %script.display(),
                    "cron: directory jobs/ file has no valid `// @schedule` header; ignored"
                );
            }
        }
    }

    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    if jobs.len() > max_jobs {
        tracing::warn!(
            site = site_key,
            found = jobs.len(),
            max = max_jobs,
            "cron: site has more directory jobs than [cron] max_jobs_per_site; extra ignored"
        );
        jobs.truncate(max_jobs);
    }
    jobs
}

/// The site-scoped, unique job name for a directory file.
fn job_name(site_key: &str, bucket: &str, script: &Path) -> String {
    let file = script
        .file_name()
        .map_or_else(|| "unknown.php".to_string(), |f| f.to_string_lossy().into_owned());
    format!("site:{site_key}:dir:{bucket}:{file}")
}

/// The `*.php` files directly inside `dir`, sorted; empty if `dir` is absent.
fn php_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("php")))
        .collect();
    files.sort();
    files
}

/// Parse a `jobs/` file's header for `@schedule` (required) and `@timezone`.
///
/// Reads only the first bytes of the file. Accepts `//` or `#` comment
/// prefixes. Returns `None` when there is no valid five-field `@schedule`.
fn parse_header(script: &Path) -> Option<(CronSchedule, Option<String>)> {
    let text = read_head(script, 4096)?;
    let mut schedule = None;
    let mut timezone = None;
    for line in text.lines().take(40) {
        if let Some(rest) = directive(line, "@schedule") {
            if let Ok(s) = CronSchedule::parse(rest) {
                schedule = Some(s);
            }
        } else if let Some(rest) = directive(line, "@timezone")
            && !rest.is_empty()
        {
            timezone = Some(rest.to_string());
        }
    }
    schedule.map(|s| (s, timezone))
}

/// If `line` is a comment carrying `marker`, return the trimmed remainder.
fn directive<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let line = line.trim();
    let body = line
        .strip_prefix("//")
        .or_else(|| line.strip_prefix('#'))
        .or_else(|| line.strip_prefix("*"))?
        .trim_start();
    let rest = body.strip_prefix(marker)?;
    Some(rest.trim())
}

/// Read up to `max` bytes of a file as UTF-8 (lossy), or `None` on error.
fn read_head(path: &Path, max: usize) -> Option<String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn cadence() -> CadenceSchedules {
        CadenceSchedules::new(None, None, None, None).unwrap()
    }

    #[test]
    fn missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_site("s", dir.path(), &cadence(), 20).is_empty());
    }

    #[test]
    fn cadence_files_are_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let daily = dir.path().join(".ephpm/cron/daily");
        fs::create_dir_all(&daily).unwrap();
        fs::write(daily.join("rotate.php"), "<?php\n").unwrap();
        fs::write(daily.join("notes.txt"), "ignored\n").unwrap();

        let jobs = scan_site("shop", dir.path(), &cadence(), 20);
        assert_eq!(jobs.len(), 1, "only the .php file is a job");
        assert_eq!(jobs[0].name, "site:shop:dir:daily:rotate.php");
        // Daily default fires at 00:00.
        assert_eq!(jobs[0].schedule.source(), "0 0 * * *");
    }

    #[test]
    fn jobs_header_schedule_and_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let jobs_dir = dir.path().join(".ephpm/cron/jobs");
        fs::create_dir_all(&jobs_dir).unwrap();
        fs::write(
            jobs_dir.join("sync.php"),
            "<?php\n// @schedule */15 * * * *\n// @timezone America/Chicago\n",
        )
        .unwrap();
        // No header → ignored.
        fs::write(jobs_dir.join("bad.php"), "<?php\necho 'hi';\n").unwrap();

        let jobs = scan_site("blog", dir.path(), &cadence(), 20);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].schedule.source(), "*/15 * * * *");
        assert_eq!(jobs[0].timezone.as_deref(), Some("America/Chicago"));
    }

    #[test]
    fn max_jobs_caps_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let hourly = dir.path().join(".ephpm/cron/hourly");
        fs::create_dir_all(&hourly).unwrap();
        for i in 0..5 {
            fs::write(hourly.join(format!("j{i}.php")), "<?php\n").unwrap();
        }
        let jobs = scan_site("s", dir.path(), &cadence(), 3);
        assert_eq!(jobs.len(), 3, "capped at max_jobs_per_site");
    }

    /// E4 — a `jobs/` file whose `@schedule` is present but invalid (six
    /// fields, i.e. a seconds field; or garbage) is ignored like a file with
    /// no header, while its valid sibling is discovered. A `@timezone` on its
    /// own enables nothing.
    #[test]
    fn invalid_schedule_header_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let jobs_dir = dir.path().join(".ephpm/cron/jobs");
        fs::create_dir_all(&jobs_dir).unwrap();
        fs::write(jobs_dir.join("seconds.php"), "<?php\n// @schedule */30 * * * * *\n").unwrap();
        fs::write(jobs_dir.join("garbage.php"), "<?php\n// @schedule every day\n").unwrap();
        fs::write(jobs_dir.join("tz-only.php"), "<?php\n// @timezone UTC\n").unwrap();
        fs::write(jobs_dir.join("good.php"), "<?php\n// @schedule 5 4 * * *\n").unwrap();

        let jobs = scan_site("s", dir.path(), &cadence(), 20);
        assert_eq!(jobs.len(), 1, "only the valid header is a job: {jobs:?}");
        assert_eq!(jobs[0].name, "site:s:dir:jobs:good.php");
        assert_eq!(jobs[0].schedule.source(), "5 4 * * *");
        assert_eq!(jobs[0].timezone, None);
    }

    #[test]
    fn hash_and_star_comment_styles_parse() {
        let dir = tempfile::tempdir().unwrap();
        let jobs_dir = dir.path().join(".ephpm/cron/jobs");
        fs::create_dir_all(&jobs_dir).unwrap();
        fs::write(jobs_dir.join("a.php"), "<?php\n# @schedule 0 2 * * *\n").unwrap();
        let jobs = scan_site("s", dir.path(), &cadence(), 20);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].schedule.source(), "0 2 * * *");
    }
}

//! systemd backend for the `ephpm` service manager.
//!
//! Writes `/etc/systemd/system/ephpm.service` on registration and shells out to
//! `systemctl` for lifecycle operations. The unit template is intentionally
//! minimal: `StandardOutput=append:` / `StandardError=append:` send the
//! server's output to `/var/log/ephpm/ephpm.log`. That means the journal does
//! **not** receive it — `journalctl -u ephpm` shows only systemd's own
//! start/stop lines — so `ephpm logs` reads the file.

use std::process::Command;

use super::{Paths, Result, ServiceError, ServiceOwner, StatusReport};

/// Render the `[Service]`-section lines that opt the unit into running as a
/// dedicated non-root user, plus a conservative sandboxing profile.
///
/// `AmbientCapabilities`/`CapabilityBoundingSet` grant exactly
/// `CAP_NET_BIND_SERVICE` so a non-root `ExecStart` can still bind `[server]
/// listen` on a privileged port (80/443) — without it, opting into `--user`
/// would silently break the common "web service on port 80" case.
/// `ProtectSystem=strict` makes the whole filesystem read-only except `/dev`,
/// `/proc`, `/sys`, and the paths listed in `ReadWritePaths`; those are the
/// directories ePHPm's own code needs to write after startup — the data dir
/// (per-site SQLite files, ACME cache), the log directory, the document root
/// (multi-tenant `sites_dir` installs typically nest under or equal
/// `document_root`; a `sites_dir` configured elsewhere needs adding to
/// `ReadWritePaths` by hand), `/tmp` and `/var/tmp` (PHP sessions and
/// uploads, the generated php.ini, per-vhost temp roots — `strict` would
/// otherwise make `/tmp` read-only too), and any config-derived directory in
/// [`Paths::extra_writable_dirs`] (an absolute `[db.sqlite] path`/`dir`, an
/// ACME `cache_dir`). `WorkingDirectory` is the data dir, so a *relative*
/// `[db.sqlite] path` (the default `ephpm.db`) or `cache_dir` (default
/// `certs`) lands there instead of in `/`. Each entry is `-`-prefixed (systemd's
/// ignore-if-missing marker) because `install()` never creates
/// `document_root`, and a caller providing their own pre-seeded config with a
/// `sites_dir` outside it may not create it either — an absent path would
/// otherwise make the unit fail to start instead of just not getting that one
/// extra write target. Returns an empty string when `owner` is `None` so the
/// default unit is unaffected.
fn owner_directives(paths: &Paths, owner: Option<&ServiceOwner>) -> String {
    let Some(ServiceOwner { user, group }) = owner else {
        return String::new();
    };
    let log_dir = paths.log_file.parent().unwrap_or(&paths.log_file);
    let extra = paths.extra_writable_dirs.iter().fold(String::new(), |mut s, d| {
        s.push(' ');
        s.push_str(&read_write_path_entry(d));
        s
    });
    format!(
        "User={user}\n\
Group={group}\n\
WorkingDirectory={data_dir}\n\
AmbientCapabilities=CAP_NET_BIND_SERVICE\n\
CapabilityBoundingSet=CAP_NET_BIND_SERVICE\n\
NoNewPrivileges=true\n\
ProtectSystem=strict\n\
ProtectHome=true\n\
ReadWritePaths=-{data_dir} -{log_dir} -{document_root} -/tmp -/var/tmp{extra}\n",
        data_dir = paths.data_dir.display(),
        log_dir = log_dir.display(),
        document_root = paths.document_root.display(),
    )
}

/// One `-`-prefixed `ReadWritePaths=` entry for a config-derived directory.
///
/// The list is whitespace-separated and systemd expands `%` specifiers in
/// it, so a path from the operator's config (`/srv/my data`, `/srv/100%`)
/// would otherwise split into two entries — the second silently ignored as
/// non-absolute — or expand into a different path. `%` is doubled, and an
/// entry containing whitespace, a quote or a backslash is double-quoted with
/// C-style escaping (systemd unquotes before reading the `-` prefix).
fn read_write_path_entry(dir: &std::path::Path) -> String {
    let raw = format!("-{}", dir.display()).replace('%', "%%");
    if raw.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\')) {
        let escaped = raw
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t");
        format!("\"{escaped}\"")
    } else {
        raw
    }
}

/// Render the systemd unit file body. `owner` is `None` for the historical
/// root-with-no-sandboxing unit (the pre-`--user` output plus `TimeoutStopSec`)
/// or `Some` to run the service as a dedicated non-root user/group — see
/// [`owner_directives`].
///
/// `TimeoutStopSec` raises systemd's 90 s default before it sends `SIGKILL`:
/// a graceful stop drains requests for up to `[server.timeouts] shutdown`
/// (30 s default), then checkpoints every embedded database outside that
/// budget (measured at 45–74 s under load), then gives the runtime up to
/// 15 s. 180 s covers that with room to spare.
fn unit_body(paths: &Paths, owner: Option<&ServiceOwner>) -> String {
    format!(
        "[Unit]\n\
Description=ePHPm — embedded PHP application server\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={binary} serve --config {config}\n\
Restart=on-failure\n\
RestartSec=2s\n\
TimeoutStopSec=180\n\
LimitNOFILE=65536\n\
{owner_directives}\
StandardOutput=append:{log}\n\
StandardError=append:{log}\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        binary = paths.binary.display(),
        config = paths.config.display(),
        log = paths.log_file.display(),
        owner_directives = owner_directives(paths, owner),
    )
}

/// Write the unit file and reload systemd so the new unit is visible.
pub(super) fn register(paths: &Paths, owner: Option<&ServiceOwner>) -> Result<()> {
    if let Some(parent) = paths.service_unit.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServiceError::io(parent, e))?;
    }
    std::fs::write(&paths.service_unit, unit_body(paths, owner))
        .map_err(|e| ServiceError::io(&paths.service_unit, e))?;
    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "ephpm.service"])?;
    Ok(())
}

/// Disable the unit and remove the unit file.
pub(super) fn deregister(paths: &Paths) -> Result<()> {
    // Best-effort disable — ignore failures so an orphaned unit file still gets
    // cleaned up.
    let _ = run_systemctl(&["disable", "ephpm.service"]);
    if paths.service_unit.exists() {
        std::fs::remove_file(&paths.service_unit)
            .map_err(|e| ServiceError::io(&paths.service_unit, e))?;
    }
    let _ = run_systemctl(&["daemon-reload"]);
    Ok(())
}

pub(super) fn start(_paths: &Paths) -> Result<()> {
    run_systemctl(&["start", "ephpm.service"])
}

pub(super) fn stop(_paths: &Paths) -> Result<()> {
    run_systemctl(&["stop", "ephpm.service"])
}

pub(super) fn restart(_paths: &Paths) -> Result<()> {
    run_systemctl(&["restart", "ephpm.service"])
}

pub(super) fn status(_paths: &Paths) -> Result<StatusReport> {
    // ActiveState + MainPID + ActiveEnterTimestamp in one call.
    let out = Command::new("systemctl")
        .args([
            "show",
            "ephpm.service",
            "--no-page",
            "--property=ActiveState,MainPID,ActiveEnterTimestamp",
        ])
        .output()
        .map_err(|e| ServiceError::command("systemctl", e.to_string()))?;
    if !out.status.success() {
        return Err(ServiceError::command(
            "systemctl",
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut state = "unknown".to_string();
    let mut pid: Option<u32> = None;
    let mut started: Option<String> = None;
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            state = v.to_string();
        } else if let Some(v) = line.strip_prefix("MainPID=") {
            pid = v.parse::<u32>().ok().filter(|p| *p != 0);
        } else if let Some(v) = line.strip_prefix("ActiveEnterTimestamp=") {
            started = if v.is_empty() { None } else { Some(v.to_string()) };
        }
    }

    Ok(StatusReport { state, pid, uptime: started })
}

/// Where `ephpm logs` reads from.
#[derive(Debug, PartialEq, Eq)]
enum LogSource {
    /// The unit's `StandardOutput=append:` file — where the server's output goes.
    File,
    /// The journal: only for a unit that does not write the log file (e.g. a
    /// hand-written unit without `StandardOutput=append:`).
    Journal,
}

/// Pick the log source. The unit `install` writes sends stdout/stderr to the
/// log file via `StandardOutput=append:`, so the journal only ever holds
/// systemd's own lifecycle lines ("Started …", "Stopped …"). This used to try
/// `journalctl -u ephpm.service` first — which exits 0 even with nothing
/// useful to show — so the fallback to the file never ran and `ephpm logs`
/// (and `logs -f`) never showed a single line of server output.
fn log_source(log_file_exists: bool) -> LogSource {
    if log_file_exists { LogSource::File } else { LogSource::Journal }
}

pub(super) fn logs(paths: &Paths, follow: bool) -> Result<()> {
    // `try_exists`, not `exists`: a log file this user cannot even stat (a
    // non-root `ephpm logs` against a 0750 log dir) must surface the
    // permission error, not be mistaken for "no log file" and fall through to
    // a journal that holds only lifecycle lines.
    let exists = paths.log_file.try_exists().map_err(|e| ServiceError::io(&paths.log_file, e))?;
    if log_source(exists) == LogSource::File {
        return super::tail_file(&paths.log_file, follow);
    }
    let mut cmd = Command::new("journalctl");
    cmd.arg("-u").arg("ephpm.service").arg("--no-pager");
    if follow {
        cmd.arg("-f");
    } else {
        cmd.arg("-n").arg("200");
    }
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(ServiceError::command(
            "journalctl",
            format!(
                "exited with {s}, and the service log file {} does not exist",
                paths.log_file.display()
            ),
        )),
        Err(e) => Err(ServiceError::command(
            "journalctl",
            format!("{e} (and the service log file {} does not exist)", paths.log_file.display()),
        )),
    }
}

/// Invoke `systemctl <args...>` and propagate failure as a `ServiceError`.
fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| ServiceError::command("systemctl", e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::command("systemctl", String::from_utf8_lossy(&out.stderr).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn test_paths() -> Paths {
        Paths {
            binary: PathBuf::from("/usr/local/bin/ephpm"),
            config: PathBuf::from("/etc/ephpm/ephpm.toml"),
            service_unit: PathBuf::from("/etc/systemd/system/ephpm.service"),
            data_dir: PathBuf::from("/var/lib/ephpm"),
            document_root: PathBuf::from("/var/www/html"),
            log_file: PathBuf::from("/var/log/ephpm/ephpm.log"),
            extra_writable_dirs: Vec::new(),
        }
    }

    #[test]
    fn unit_body_renders_paths() {
        let paths = test_paths();
        let body = unit_body(&paths, None);
        assert!(
            body.contains("ExecStart=/usr/local/bin/ephpm serve --config /etc/ephpm/ephpm.toml")
        );
        assert!(body.contains("StandardOutput=append:/var/log/ephpm/ephpm.log"));
        assert!(body.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn unit_body_without_owner_omits_user_and_hardening() {
        // Backward compatibility: no `--user` must produce exactly what
        // pre-#XXX callers got — root, no `User=`/`Group=`, no sandboxing.
        let paths = test_paths();
        let body = unit_body(&paths, None);
        assert!(!body.contains("User="));
        assert!(!body.contains("Group="));
        assert!(!body.contains("ProtectSystem"));
        assert!(!body.contains("NoNewPrivileges"));
        assert!(!body.contains("AmbientCapabilities"));
    }

    #[test]
    fn unit_body_with_owner_sets_user_group_and_hardening() {
        let paths = test_paths();
        let owner = ServiceOwner { user: "ephpm-web".to_string(), group: "ephpm-web".to_string() };
        let body = unit_body(&paths, Some(&owner));
        assert!(body.contains("User=ephpm-web"));
        assert!(body.contains("Group=ephpm-web"));
        assert!(body.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE"));
        assert!(body.contains("CapabilityBoundingSet=CAP_NET_BIND_SERVICE"));
        assert!(body.contains("NoNewPrivileges=true"));
        assert!(body.contains("ProtectSystem=strict"));
        assert!(body.contains("ProtectHome=true"));
        assert!(body.contains(
            "ReadWritePaths=-/var/lib/ephpm -/var/log/ephpm -/var/www/html -/tmp -/var/tmp\n"
        ));
        // Relative config paths (`[db.sqlite] path = "ephpm.db"`) must land in
        // the data dir, not `/` (root-owned and read-only under strict).
        assert!(body.contains("WorkingDirectory=/var/lib/ephpm\n"));
        // Unaffected by the owner directives.
        assert!(
            body.contains("ExecStart=/usr/local/bin/ephpm serve --config /etc/ephpm/ephpm.toml")
        );
        assert!(body.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn unit_body_with_owner_lists_config_derived_dirs() {
        let mut paths = test_paths();
        paths.extra_writable_dirs = vec![PathBuf::from("/srv/db"), PathBuf::from("/srv/acme")];
        let owner = ServiceOwner { user: "ephpm-web".to_string(), group: "ephpm-web".to_string() };
        let body = unit_body(&paths, Some(&owner));
        assert!(body.contains("-/var/tmp -/srv/db -/srv/acme\n"), "{body}");
    }

    #[test]
    fn read_write_path_entries_survive_spaces_and_specifiers() {
        // Unquoted, `/srv/my data` would split into `-/srv/my` and `data` (the
        // second ignored as non-absolute) and `%` would be expanded by systemd.
        assert_eq!(read_write_path_entry(Path::new("/srv/db")), "-/srv/db");
        assert_eq!(read_write_path_entry(Path::new("/srv/100%")), "-/srv/100%%");
        assert_eq!(read_write_path_entry(Path::new("/srv/my data")), "\"-/srv/my data\"");
        assert_eq!(read_write_path_entry(Path::new("/srv/a\"b")), "\"-/srv/a\\\"b\"");
    }

    #[test]
    fn unit_body_without_owner_has_no_working_directory() {
        // The root unit stays byte-identical to the historical one: moving
        // its working directory would move an existing relative database.
        assert!(!unit_body(&test_paths(), None).contains("WorkingDirectory"));
    }

    #[test]
    fn logs_read_the_file_the_unit_writes() {
        // The unit sends stdout/stderr to the file, so the journal has only
        // lifecycle lines; the file is the source whenever it exists.
        assert_eq!(log_source(true), LogSource::File);
        assert_eq!(log_source(false), LogSource::Journal);
        let body = unit_body(&test_paths(), None);
        assert!(body.contains("StandardOutput=append:/var/log/ephpm/ephpm.log"));
    }

    #[test]
    fn unit_gives_a_graceful_stop_time_to_close_databases() {
        // systemd's 90 s default would SIGKILL mid final-checkpoint.
        let owner = ServiceOwner { user: "ephpm-web".to_string(), group: "ephpm-web".to_string() };
        for owner in [None, Some(&owner)] {
            assert!(unit_body(&test_paths(), owner).contains("\nTimeoutStopSec=180\n"));
        }
    }
}

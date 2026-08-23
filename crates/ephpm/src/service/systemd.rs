//! systemd backend for the `ephpm` service manager.
//!
//! Writes `/etc/systemd/system/ephpm.service` on registration and shells out to
//! `systemctl` for lifecycle operations. The unit template is intentionally
//! minimal: it relies on the daemon's own logging configuration to write to
//! `/var/log/ephpm/ephpm.log`, while also forwarding output to the systemd
//! journal so `journalctl -u ephpm` works.

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
/// three directories ePHPm's own code needs to write after startup — the
/// data dir (per-site SQLite files, ACME cache), the log directory, and the
/// document root (multi-tenant `sites_dir` installs typically nest under or
/// equal `document_root`; a `sites_dir` configured elsewhere needs adding to
/// `ReadWritePaths` by hand). Each entry is `-`-prefixed (systemd's
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
    format!(
        "User={user}\n\
Group={group}\n\
AmbientCapabilities=CAP_NET_BIND_SERVICE\n\
CapabilityBoundingSet=CAP_NET_BIND_SERVICE\n\
NoNewPrivileges=true\n\
ProtectSystem=strict\n\
ProtectHome=true\n\
ReadWritePaths=-{data_dir} -{log_dir} -{document_root}\n",
        data_dir = paths.data_dir.display(),
        log_dir = log_dir.display(),
        document_root = paths.document_root.display(),
    )
}

/// Render the systemd unit file body. `owner` is `None` for the historical
/// root-with-no-sandboxing unit (byte-identical to the pre-`--user` output)
/// or `Some` to run the service as a dedicated non-root user/group — see
/// [`owner_directives`].
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

pub(super) fn logs(paths: &Paths, follow: bool) -> Result<()> {
    // Prefer journalctl when available — it's the canonical systemd log source.
    let mut cmd = Command::new("journalctl");
    cmd.arg("-u").arg("ephpm.service").arg("--no-pager");
    if follow {
        cmd.arg("-f");
    } else {
        cmd.arg("-n").arg("200");
    }
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) | Err(_) => super::tail_file(&paths.log_file, follow),
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
    use std::path::PathBuf;

    use super::*;

    fn test_paths() -> Paths {
        Paths {
            binary: PathBuf::from("/usr/local/bin/ephpm"),
            config: PathBuf::from("/etc/ephpm/ephpm.toml"),
            service_unit: PathBuf::from("/etc/systemd/system/ephpm.service"),
            data_dir: PathBuf::from("/var/lib/ephpm"),
            document_root: PathBuf::from("/var/www/html"),
            log_file: PathBuf::from("/var/log/ephpm/ephpm.log"),
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
        assert!(body.contains("ReadWritePaths=-/var/lib/ephpm -/var/log/ephpm -/var/www/html"));
        // Unaffected by the owner directives.
        assert!(
            body.contains("ExecStart=/usr/local/bin/ephpm serve --config /etc/ephpm/ephpm.toml")
        );
        assert!(body.contains("WantedBy=multi-user.target"));
    }
}

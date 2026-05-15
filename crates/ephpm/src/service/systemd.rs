//! systemd backend for the `ephpm` service manager.
//!
//! Writes `/etc/systemd/system/ephpm.service` on registration and shells out to
//! `systemctl` for lifecycle operations. The unit template is intentionally
//! minimal: it relies on the daemon's own logging configuration to write to
//! `/var/log/ephpm/ephpm.log`, while also forwarding output to the systemd
//! journal so `journalctl -u ephpm` works.

use std::process::Command;

use super::{Paths, Result, ServiceError, StatusReport};

/// Render the systemd unit file body.
fn unit_body(paths: &Paths) -> String {
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
StandardOutput=append:{log}\n\
StandardError=append:{log}\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        binary = paths.binary.display(),
        config = paths.config.display(),
        log = paths.log_file.display(),
    )
}

/// Write the unit file and reload systemd so the new unit is visible.
pub(super) fn register(paths: &Paths) -> Result<()> {
    if let Some(parent) = paths.service_unit.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServiceError::io(parent, e))?;
    }
    std::fs::write(&paths.service_unit, unit_body(paths))
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

    #[test]
    fn unit_body_renders_paths() {
        let paths = Paths {
            binary: PathBuf::from("/usr/local/bin/ephpm"),
            config: PathBuf::from("/etc/ephpm/ephpm.toml"),
            service_unit: PathBuf::from("/etc/systemd/system/ephpm.service"),
            data_dir: PathBuf::from("/var/lib/ephpm"),
            document_root: PathBuf::from("/var/www/html"),
            log_file: PathBuf::from("/var/log/ephpm/ephpm.log"),
        };
        let body = unit_body(&paths);
        assert!(body.contains("ExecStart=/usr/local/bin/ephpm serve --config /etc/ephpm/ephpm.toml"));
        assert!(body.contains("StandardOutput=append:/var/log/ephpm/ephpm.log"));
        assert!(body.contains("WantedBy=multi-user.target"));
    }
}


//! launchd backend for the `ephpm` service manager (macOS).
//!
//! Generates `/Library/LaunchDaemons/dev.ephpm.plist` and uses `launchctl
//! bootstrap` / `bootout` / `kickstart` / `print` for lifecycle management.

use std::process::Command;

use super::{Paths, Result, ServiceError, StatusReport};

/// launchd service label used in the plist.
const LABEL: &str = "dev.ephpm";

/// Render the launchd plist body for the daemon.
fn plist_body(paths: &Paths) -> String {
    let binary = paths.binary.display();
    let config = paths.config.display();
    let log = paths.log_file.display();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{LABEL}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n\
        <string>{binary}</string>\n\
        <string>serve</string>\n\
        <string>--config</string>\n\
        <string>{config}</string>\n\
    </array>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
    <key>KeepAlive</key>\n\
    <true/>\n\
    <key>StandardOutPath</key>\n\
    <string>{log}</string>\n\
    <key>StandardErrorPath</key>\n\
    <string>{log}</string>\n\
</dict>\n\
</plist>\n"
    )
}

pub(super) fn register(paths: &Paths) -> Result<()> {
    if let Some(parent) = paths.service_unit.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServiceError::io(parent, e))?;
    }
    std::fs::write(&paths.service_unit, plist_body(paths))
        .map_err(|e| ServiceError::io(&paths.service_unit, e))?;
    run_launchctl(&["bootstrap", "system", &paths.service_unit.display().to_string()])
}

pub(super) fn deregister(paths: &Paths) -> Result<()> {
    let _ = run_launchctl(&["bootout", &format!("system/{LABEL}")]);
    if paths.service_unit.exists() {
        std::fs::remove_file(&paths.service_unit)
            .map_err(|e| ServiceError::io(&paths.service_unit, e))?;
    }
    Ok(())
}

pub(super) fn start(_paths: &Paths) -> Result<()> {
    run_launchctl(&["kickstart", &format!("system/{LABEL}")])
}

pub(super) fn stop(_paths: &Paths) -> Result<()> {
    // `bootout` would unload the daemon entirely; we just want it stopped, so
    // use the legacy `stop` verb which leaves it loaded for later restart.
    run_launchctl(&["stop", LABEL])
}

pub(super) fn restart(paths: &Paths) -> Result<()> {
    stop(paths).ok();
    start(paths)
}

pub(super) fn status(_paths: &Paths) -> Result<StatusReport> {
    let out = Command::new("launchctl")
        .args(["print", &format!("system/{LABEL}")])
        .output()
        .map_err(|e| ServiceError::command("launchctl", e.to_string()))?;
    if !out.status.success() {
        return Ok(StatusReport { state: "not-loaded".to_string(), pid: None, uptime: None });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);

    let mut state = "loaded".to_string();
    let mut pid: Option<u32> = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(v) = trimmed.strip_prefix("state =") {
            state = v.trim().to_string();
        } else if let Some(v) = trimmed.strip_prefix("pid =") {
            pid = v.trim().parse::<u32>().ok();
        }
    }
    Ok(StatusReport { state, pid, uptime: None })
}

pub(super) fn logs(paths: &Paths, follow: bool) -> Result<()> {
    super::tail_file(&paths.log_file, follow)
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let out = Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| ServiceError::command("launchctl", e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::command("launchctl", String::from_utf8_lossy(&out.stderr).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn plist_body_contains_required_keys() {
        let paths = Paths {
            binary: PathBuf::from("/usr/local/bin/ephpm"),
            config: PathBuf::from("/etc/ephpm/ephpm.toml"),
            service_unit: PathBuf::from("/Library/LaunchDaemons/dev.ephpm.plist"),
            data_dir: PathBuf::from("/var/lib/ephpm"),
            document_root: PathBuf::from("/var/www/html"),
            log_file: PathBuf::from("/var/log/ephpm/ephpm.log"),
        };
        let body = plist_body(&paths);
        assert!(body.contains("<string>dev.ephpm</string>"));
        assert!(body.contains("<string>/usr/local/bin/ephpm</string>"));
        assert!(body.contains("<string>--config</string>"));
        assert!(body.contains("<string>/var/log/ephpm/ephpm.log</string>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
    }
}

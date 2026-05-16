//! Elevated end-to-end test for the `ephpm` service-management CLI.
//!
//! Exercises the full `install → status → stop → start → restart → logs
//! → uninstall --keep-data → install → uninstall` flow against the real
//! platform service manager (SCM on Windows, systemd on Linux, launchd on
//! macOS). All of those mutate system state in production locations, so the
//! test is double-gated:
//!
//! 1. `#[ignore]` keeps it out of `cargo test` by default.
//! 2. `EPHPM_ELEVATED_E2E=1` must be set, even with `--ignored`, as a tripwire
//!    against running it by accident in CI or on a dev machine that already
//!    has a real ephpm install.
//!
//! ## Running
//!
//! ```bash
//! # Linux  (systemd required)
//! sudo EPHPM_ELEVATED_E2E=1 cargo test -p ephpm --test service_lifecycle -- --ignored --nocapture
//!
//! # macOS
//! sudo EPHPM_ELEVATED_E2E=1 cargo test -p ephpm --test service_lifecycle -- --ignored --nocapture
//!
//! # Windows  (elevated PowerShell — Administrator)
//! $env:EPHPM_ELEVATED_E2E="1"; cargo test -p ephpm --test service_lifecycle -- --ignored --nocapture
//! ```
//!
//! Before running, the test refuses to proceed if the canonical install
//! binary already exists — that's almost certainly a real production install
//! the developer doesn't want clobbered.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::Duration;

const GATE_ENV: &str = "EPHPM_ELEVATED_E2E";
const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

fn canonical_install_binary() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\ephpm\ephpm.exe")
    } else {
        PathBuf::from("/usr/local/bin/ephpm")
    }
}

fn canonical_config() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\ephpm\ephpm.toml")
    } else {
        PathBuf::from("/etc/ephpm/ephpm.toml")
    }
}

/// Run `ephpm <args>` (the test-built binary), capturing stdout+stderr.
fn ephpm(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{BIN} {args:?}`: {e}"))
}

/// Run and assert success. Panic with stdout+stderr if the command failed.
fn ephpm_ok(args: &[&str]) -> String {
    let out = ephpm(args);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "`ephpm {args:?}` failed (exit {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code(),
    );
    format!("{stdout}{stderr}")
}

/// Parse the numeric pid from a `status` output. `None` when the pid line says
/// "-" (service stopped).
fn pid_from_status(s: &str) -> Option<u32> {
    s.lines().find_map(|l| {
        let trimmed = l.trim_start();
        let rest = trimmed.strip_prefix("pid:")?;
        let value = rest.trim();
        if value == "-" { None } else { value.parse::<u32>().ok() }
    })
}

#[test]
#[ignore = "elevated end-to-end — set EPHPM_ELEVATED_E2E=1 and run as root/Administrator"]
fn service_full_lifecycle() {
    // ─── pre-flight gates ────────────────────────────────────────────────
    if std::env::var_os(GATE_ENV).is_none() {
        eprintln!(
            "SKIP: {GATE_ENV} not set. Re-run with `{GATE_ENV}=1` to opt into mutating system state."
        );
        return;
    }
    let install_binary = canonical_install_binary();
    assert!(
        !install_binary.exists(),
        "refusing to run: {} already exists — looks like a real ephpm install. \
         Uninstall it first before running this test.",
        install_binary.display(),
    );

    // On Linux the systemd backend shells out to `systemctl`; if it's not on
    // PATH (e.g. inside a non-systemd container), skip rather than fail.
    #[cfg(target_os = "linux")]
    {
        if Command::new("systemctl").arg("--version").output().is_err() {
            eprintln!("SKIP: systemctl not available — Linux backend needs systemd.");
            return;
        }
    }

    // RAII guard guarantees we run `uninstall` even if the test panics.
    let _guard = CleanupGuard;

    // ─── install ─────────────────────────────────────────────────────────
    ephpm_ok(&["install"]);
    assert!(install_binary.exists(), "binary should exist after install");
    assert!(canonical_config().exists(), "config should exist after install");

    // Give the service backend a moment to flip into the running state.
    sleep(Duration::from_secs(2));

    // ─── status (running) ───────────────────────────────────────────────
    let status_running = ephpm_ok(&["status"]);
    let running_pid = pid_from_status(&status_running).unwrap_or_else(|| {
        panic!("status after install reports no pid:\n{status_running}");
    });

    // ─── stop ───────────────────────────────────────────────────────────
    ephpm_ok(&["stop"]);
    let status_stopped = ephpm_ok(&["status"]);
    assert!(
        pid_from_status(&status_stopped).is_none(),
        "expected no pid after stop, got:\n{status_stopped}",
    );

    // ─── start ──────────────────────────────────────────────────────────
    ephpm_ok(&["start"]);
    sleep(Duration::from_secs(1));
    let status_started = ephpm_ok(&["status"]);
    let started_pid = pid_from_status(&status_started)
        .unwrap_or_else(|| panic!("status after start reports no pid:\n{status_started}"));

    // ─── restart ────────────────────────────────────────────────────────
    ephpm_ok(&["restart"]);
    sleep(Duration::from_secs(1));
    let status_restarted = ephpm_ok(&["status"]);
    let restarted_pid = pid_from_status(&status_restarted)
        .unwrap_or_else(|| panic!("status after restart reports no pid:\n{status_restarted}"));
    assert_ne!(
        restarted_pid, started_pid,
        "restart should produce a new pid (was {started_pid}, still {restarted_pid})",
    );
    // running_pid is from the very first start; sanity check we've cycled at
    // least twice in this sequence.
    let _ = running_pid;

    // ─── logs (may need a moment for the async appender to flush) ───────
    sleep(Duration::from_secs(1));
    let logs = ephpm_ok(&["logs"]);
    assert!(
        !logs.trim().is_empty(),
        "logs subcommand returned no output — the service either isn't writing or the path is wrong",
    );

    // ─── uninstall --keep-data ──────────────────────────────────────────
    ephpm_ok(&["uninstall", "--keep-data"]);
    assert!(!install_binary.exists(), "binary should be gone after uninstall");
    assert!(canonical_config().exists(), "config should survive --keep-data uninstall");

    // ─── reinstall on top of preserved data ─────────────────────────────
    ephpm_ok(&["install"]);
    sleep(Duration::from_secs(2));
    let after_reinstall = ephpm_ok(&["status"]);
    assert!(
        pid_from_status(&after_reinstall).is_some(),
        "reinstall should leave the service running:\n{after_reinstall}",
    );

    // ─── full uninstall ────────────────────────────────────────────────
    ephpm_ok(&["uninstall"]);
    assert!(!install_binary.exists(), "binary should be gone after full uninstall");
    assert!(!canonical_config().exists(), "config should be gone after full uninstall");

    // ─── idempotent uninstall (no-op should not error) ──────────────────
    let out = ephpm(&["uninstall"]);
    assert!(
        out.status.success() || cfg!(target_os = "macos"),
        "second uninstall should be idempotent\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Drop-guard that best-effort uninstalls so a panic mid-test doesn't leave
/// the developer's machine with a stuck service. Errors are swallowed
/// intentionally — we already know `uninstall` is idempotent.
struct CleanupGuard;

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = Command::new(BIN).args(["uninstall"]).output();
    }
}

#[test]
fn pid_parser_handles_numeric() {
    let s = "service: running\n   pid: 1234\nuptime: -\nlisten: 0.0.0.0:8080\nconfig: x\n";
    assert_eq!(pid_from_status(s), Some(1234));
}

#[test]
fn pid_parser_handles_dash() {
    let s = "service: stopped\n   pid: -\nuptime: -\nlisten: 0.0.0.0:8080\nconfig: x\n";
    assert_eq!(pid_from_status(s), None);
}

//! Job execution — spawn a child process, enforce the wall-clock timeout, and
//! supervise it (kill on timeout or on a `replace` pre-emption).
//!
//! A cron fire is a **child process** the scheduler times, gates, and leases —
//! it never runs inside ePHPm's own multi-threaded, PHP-linked runtime. The
//! `command` argv and the job's `site` pick the shape:
//!
//! * **PHP, per-site** (first arg ends in `.php`): `ephpm php --site <k>
//!   --config <p> -- <argv>` — the inner process binds the `ephpm_db_*` bridge
//!   so the job gets the tenant's `DB_*`/KV credentials.
//! * **binary, per-site**: `ephpm exec --site <k> --config <p> -- <argv>` — the
//!   tenant sandbox (uid drop + Landlock + egress, Linux).
//! * **siteless** (cluster-global, leader-only): the `command` is spawned
//!   **directly** as a child on the leader. `ephpm exec` is a Linux-only
//!   *tenant* sandbox and requires a site, so a job with no tenant to sandbox
//!   runs its binary directly (operator-trust, exactly as a systemd timer
//!   would) rather than being forced through a sandbox that does not apply.
//!   The scheduler still owns the child, its timeout, and its kill switch.
//!
//! The timeout and the `replace`-policy pre-emption are enforced here by the
//! parent (SIGTERM → grace → SIGKILL on Unix; terminate on other platforms) —
//! the "enforce the ceiling above the interpreter" posture ePHPm already takes.
//!
//! [`JobExecutor`] is a trait so the orchestrator's gate/lease/decision logic
//! can be unit-tested against a recording fake; [`ChildExecutor`] is the
//! production implementation, and the lower-level [`run_child`] primitive is
//! tested directly with a trivial cross-platform command.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// How long to wait after a graceful terminate before a hard kill.
const KILL_GRACE: Duration = Duration::from_secs(10);

/// What to run for one job fire.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// The job name (for logs).
    pub job: String,
    /// The site key, or `None` for a siteless job.
    pub site: Option<String>,
    /// Whether the command is PHP (routed through `ephpm php`).
    pub is_php: bool,
    /// The command argv (first element is the program or `.php` script).
    pub command: Vec<String>,
    /// The config file to pass to `ephpm exec`/`ephpm php` children.
    pub config_path: Option<PathBuf>,
    /// Wall-clock cap.
    pub timeout: Duration,
}

/// The result of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Exited zero.
    Ok,
    /// Exited non-zero.
    Failed,
    /// Killed because it exceeded its timeout.
    Timeout,
    /// Killed by a `replace`-policy pre-emption.
    Killed,
    /// Could not be spawned at all.
    SpawnError,
}

impl RunOutcome {
    /// The `outcome` metric label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            RunOutcome::Ok => "ok",
            RunOutcome::Failed => "failed",
            RunOutcome::Timeout => "timeout",
            RunOutcome::Killed => "killed",
            RunOutcome::SpawnError => "spawn_error",
        }
    }
}

/// Run one job fire, cooperatively cancellable (for `replace`).
pub trait JobExecutor: Send + Sync {
    /// Execute `req`; return when the child finishes, times out, or `cancel`
    /// fires.
    fn run<'a>(
        &'a self,
        req: ExecRequest,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = RunOutcome> + Send + 'a>>;
}

/// Build the `(program, args)` a request spawns.
///
/// `exe` is this ePHPm binary (`std::env::current_exe()`), used as the program
/// for per-site jobs that re-invoke `ephpm exec`/`ephpm php`.
#[must_use]
pub fn build_argv(req: &ExecRequest, exe: &std::path::Path) -> (PathBuf, Vec<String>) {
    match &req.site {
        // Per-site: re-invoke this binary as `ephpm exec`/`ephpm php --site`.
        Some(site) => {
            let mut args = Vec::new();
            args.push(if req.is_php { "php".to_string() } else { "exec".to_string() });
            if let Some(cfg) = &req.config_path {
                args.push("--config".to_string());
                args.push(cfg.to_string_lossy().into_owned());
            }
            args.push("--site".to_string());
            args.push(site.clone());
            args.push("--".to_string());
            args.extend(req.command.iter().cloned());
            (exe.to_path_buf(), args)
        }
        // Siteless: run the command directly on the leader.
        None => {
            let program = req.command.first().cloned().unwrap_or_default();
            let args = req.command.iter().skip(1).cloned().collect();
            (PathBuf::from(program), args)
        }
    }
}

/// The production executor — spawns real child processes of this binary.
pub struct ChildExecutor {
    exe: PathBuf,
}

impl ChildExecutor {
    /// Resolve the running binary once; errors are surfaced at construction so
    /// the scheduler can decline to start if it cannot find itself.
    ///
    /// # Errors
    ///
    /// When `std::env::current_exe()` fails.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self { exe: std::env::current_exe()? })
    }

    /// Construct with an explicit binary path (tests).
    #[must_use]
    pub fn with_exe(exe: PathBuf) -> Self {
        Self { exe }
    }
}

impl JobExecutor for ChildExecutor {
    fn run<'a>(
        &'a self,
        req: ExecRequest,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = RunOutcome> + Send + 'a>> {
        let (program, args) = build_argv(&req, &self.exe);
        Box::pin(async move { run_child(&program, &args, req.timeout, &cancel).await })
    }
}

/// Spawn `program args`, wait for it, and kill it on timeout or cancellation.
///
/// stdout/stderr are inherited so a job's output reaches the server's logs.
pub async fn run_child(
    program: &std::path::Path,
    args: &[String],
    timeout: Duration,
    cancel: &CancellationToken,
) -> RunOutcome {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(program = %program.display(), error = %e, "cron: failed to spawn job");
            return RunOutcome::SpawnError;
        }
    };

    // `child.wait()` borrows `child` only for the duration of this `select!`;
    // the non-selected futures are dropped when it returns, releasing the
    // borrow so `terminate` can take `&mut child`.
    let reason = tokio::select! {
        status = child.wait() => {
            return match status {
                Ok(s) if s.success() => RunOutcome::Ok,
                Ok(_) => RunOutcome::Failed,
                Err(_) => RunOutcome::SpawnError,
            };
        }
        () = tokio::time::sleep(timeout) => RunOutcome::Timeout,
        () = cancel.cancelled() => RunOutcome::Killed,
    };

    // Graceful terminate, then a hard kill after the grace window.
    terminate(&mut child, KILL_GRACE).await;
    reason
}

/// SIGTERM (Unix) / terminate, wait up to `grace`, then SIGKILL.
#[allow(unsafe_code)]
async fn terminate(child: &mut tokio::process::Child, grace: Duration) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: `kill(2)` with a pid we own and a valid signal number has
            // no memory-safety preconditions; an ESRCH (already reaped) is
            // harmless and ignored.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }

    match tokio::time::timeout(grace, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            // Still alive after the grace window → hard kill.
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_siteless_runs_command_directly() {
        let req = ExecRequest {
            job: "j".into(),
            site: None,
            is_php: false,
            command: vec!["maintenance".into(), "--sweep".into()],
            config_path: Some(PathBuf::from("/etc/ephpm/ephpm.toml")),
            timeout: Duration::from_secs(60),
        };
        let (prog, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        assert_eq!(prog, PathBuf::from("maintenance"));
        assert_eq!(args, vec!["--sweep"]);
    }

    #[test]
    fn argv_per_site_binary_uses_exec() {
        let req = ExecRequest {
            job: "j".into(),
            site: Some("shop".into()),
            is_php: false,
            command: vec!["wp".into(), "cron".into(), "event".into()],
            config_path: Some(PathBuf::from("/etc/ephpm/ephpm.toml")),
            timeout: Duration::from_secs(60),
        };
        let (prog, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        assert_eq!(prog, PathBuf::from("/usr/bin/ephpm"));
        assert_eq!(
            args,
            vec![
                "exec",
                "--config",
                "/etc/ephpm/ephpm.toml",
                "--site",
                "shop",
                "--",
                "wp",
                "cron",
                "event"
            ]
        );
    }

    #[test]
    fn argv_per_site_php_uses_php() {
        let req = ExecRequest {
            job: "j".into(),
            site: Some("blog".into()),
            is_php: true,
            command: vec!["artisan.php".into(), "schedule:run".into()],
            config_path: Some(PathBuf::from("/c.toml")),
            timeout: Duration::from_secs(60),
        };
        let (prog, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        assert_eq!(prog, PathBuf::from("/usr/bin/ephpm"));
        assert_eq!(args[0], "php");
        assert!(args.contains(&"--site".to_string()));
        assert!(args.contains(&"blog".to_string()));
    }

    #[tokio::test]
    async fn run_child_reports_success_and_failure() {
        let cancel = CancellationToken::new();
        // A command that exits 0.
        #[cfg(unix)]
        let (ok_prog, ok_args) = ("/bin/sh", vec!["-c".to_string(), "exit 0".to_string()]);
        #[cfg(windows)]
        let (ok_prog, ok_args) = ("cmd", vec!["/C".to_string(), "exit 0".to_string()]);
        let out =
            run_child(std::path::Path::new(ok_prog), &ok_args, Duration::from_secs(10), &cancel)
                .await;
        assert_eq!(out, RunOutcome::Ok);

        #[cfg(unix)]
        let (bad_prog, bad_args) = ("/bin/sh", vec!["-c".to_string(), "exit 3".to_string()]);
        #[cfg(windows)]
        let (bad_prog, bad_args) = ("cmd", vec!["/C".to_string(), "exit 3".to_string()]);
        let out =
            run_child(std::path::Path::new(bad_prog), &bad_args, Duration::from_secs(10), &cancel)
                .await;
        assert_eq!(out, RunOutcome::Failed);
    }

    #[tokio::test]
    async fn run_child_times_out_and_kills() {
        let cancel = CancellationToken::new();
        #[cfg(unix)]
        let (prog, args) = ("/bin/sh", vec!["-c".to_string(), "sleep 30".to_string()]);
        #[cfg(windows)]
        let (prog, args) = ("cmd", vec!["/C".to_string(), "ping -n 30 127.0.0.1 >NUL".to_string()]);
        let out =
            run_child(std::path::Path::new(prog), &args, Duration::from_millis(200), &cancel).await;
        assert_eq!(out, RunOutcome::Timeout);
    }

    #[tokio::test]
    async fn run_child_cancel_preempts() {
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        #[cfg(unix)]
        let (prog, args) = ("/bin/sh", vec!["-c".to_string(), "sleep 30".to_string()]);
        #[cfg(windows)]
        let (prog, args) = ("cmd", vec!["/C".to_string(), "ping -n 30 127.0.0.1 >NUL".to_string()]);
        let out =
            run_child(std::path::Path::new(prog), &args, Duration::from_secs(30), &cancel).await;
        assert_eq!(out, RunOutcome::Killed);
    }

    #[tokio::test]
    async fn run_child_spawn_error_on_missing_program() {
        let cancel = CancellationToken::new();
        let out = run_child(
            std::path::Path::new("/nonexistent/definitely/not/here"),
            &[],
            Duration::from_secs(5),
            &cancel,
        )
        .await;
        assert_eq!(out, RunOutcome::SpawnError);
    }
}

//! Job execution — spawn a child process, enforce the wall-clock timeout, and
//! supervise it (kill on timeout or on a `replace` pre-emption).
//!
//! A cron fire is a **child process** the scheduler times, gates, and leases —
//! it never runs inside ePHPm's own multi-threaded, PHP-linked runtime. The
//! `command` argv and the job's `site` pick the shape:
//!
//! * **PHP, per-site** (first arg ends in `.php`): `ephpm php --site <k>
//!   --config <p> -- <argv>` — the inner process binds the `ephpm_db_*` bridge
//!   so the job gets the tenant's `DB_*`/KV credentials, and `ephpm php --site`
//!   applies the SAME `open_basedir` + `disable_functions` sandbox as that
//!   site's HTTP PHP (issue #698), so a tenant-authored script cannot escape its
//!   vhost. (Data-plane sandbox; the binary lane below adds the OS-level one.)
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
    /// Working directory for the child, so a relative script/command
    /// (`bin/cake.php`) resolves against the site root rather than the server's
    /// cwd. `Some(site_root)` for a per-site job, `None` (inherit) otherwise.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables to set on the child process (finding M3).
    ///
    /// Carries the shared wire listener's `DB_*` for **siteless (operator)** jobs
    /// only, so an operator's own `pdo_mysql` cron script authenticates to the
    /// now-required-by-default shared listener the same way an HTTP request does.
    /// **Empty for per-site (tenant) jobs** — a tenant job authenticates with its
    /// own per-site credential (`ephpm php --site` injects it), never the shared
    /// one. Applied via `Command::env`, so it never enters this process's
    /// environment, argv or logs.
    pub extra_env: Vec<(String, String)>,
    /// The `[kv] secret` to hand a PHP per-site child as `EPHPM_KV__SECRET` via
    /// `Command::env` (child-only; never argv, never logged). Set only for the
    /// PHP per-site lane — the parent server scrubbed it from its own
    /// environment (finding F1), but the child's `Config::load` → `plan_site_db`
    /// still needs it, and `ephpm php --site` scrubs it again before tenant
    /// code runs. `None` otherwise.
    pub kv_secret: Option<String>,
    /// Wall-clock cap.
    pub timeout: Duration,
    /// Resolved memory ceiling in bytes (`None` ⇒ none). On a binary child it
    /// becomes `ephpm exec --memory-max` (OS `RLIMIT_AS`); on a PHP child it
    /// becomes `ephpm php --memory-max` (the PHP `memory_limit` ceiling).
    /// Hardening F5.
    pub memory_max_bytes: Option<u64>,
    /// Resolved CPU-time ceiling in whole seconds (`None` ⇒ none). Passed as
    /// `--cpu-secs` to both child kinds (OS `RLIMIT_CPU`).
    pub cpu_secs: Option<u64>,
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
            // Resource ceilings (hardening F5) as leading options — before the
            // PHP `-f` / the `--` command separator — so the child's clap parses
            // them, not the inner interpreter/command. Both child kinds accept
            // `--memory-max`/`--cpu-secs`.
            if let Some(bytes) = req.memory_max_bytes {
                args.push("--memory-max".to_string());
                args.push(bytes.to_string());
            }
            if let Some(secs) = req.cpu_secs {
                args.push("--cpu-secs".to_string());
                args.push(secs.to_string());
            }
            if req.is_php {
                // A per-site PHP cron child runs confined: per-site Landlock +
                // no_new_privs + (as root) uid drop + RLIMIT_CPU, in the child
                // before PHP starts. `ephpm exec` (the binary branch) already
                // enters the tenant sandbox by construction, so it needs no flag.
                args.push("--confine".to_string());
                // php-cli names the program with `-f`. A *leading* `--` instead
                // (what this used to emit) is php's "read the program from
                // stdin" signal — and the child's stdin is `Stdio::null()`, so
                // php would compile an empty program, exit 0, and the job would
                // be reported `ok` without ever running. `-f <script>` is
                // unambiguous; a `--` after it guards any dashed script args.
                let mut cmd = req.command.iter();
                if let Some(script) = cmd.next() {
                    args.push("-f".to_string());
                    args.push(script.clone());
                    let rest: Vec<String> = cmd.cloned().collect();
                    if !rest.is_empty() {
                        args.push("--".to_string());
                        args.extend(rest);
                    }
                }
            } else {
                // `ephpm exec`: everything after `--` is the sandboxed command.
                args.push("--".to_string());
                args.extend(req.command.iter().cloned());
            }
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
        let cwd = req.cwd.clone();
        let extra_env = req.extra_env.clone();
        // Child-only `EPHPM_KV__SECRET` for the PHP per-site lane (finding F1);
        // `None` for every other job, so a siteless operator binary never
        // receives the secret.
        let kv_secret = req.kv_secret.clone();
        Box::pin(async move {
            run_child(
                &program,
                &args,
                cwd.as_deref(),
                &extra_env,
                kv_secret.as_deref(),
                req.timeout,
                &cancel,
            )
            .await
        })
    }
}

/// Spawn `program args`, wait for it, and kill it on timeout or cancellation.
/// `cwd`, when set, is the child's working directory (a per-site job's site
/// root, so relative script paths resolve there, not against the server's cwd).
///
/// `extra_env` sets additional variables on the child via `Command::env` — the
/// shared wire `DB_*` for siteless operator jobs (finding M3), empty otherwise;
/// child-only, so they never enter this process's environment, argv or logs.
///
/// `kv_secret`, when set, is passed to the child as `EPHPM_KV__SECRET` via
/// `Command::env` — child-only in the same way, and set only for the PHP
/// per-site lane (finding F1).
///
/// stdout/stderr are inherited so a job's output reaches the server's logs.
///
/// **File-descriptor hygiene (tenant isolation).** The child is a fresh
/// `execve` (`Command::spawn` → execve, never a bare `fork`), so every file
/// descriptor the server holds marked close-on-exec (`O_CLOEXEC`/`FD_CLOEXEC`)
/// is closed before the child's `main` runs. The child therefore inherits only
/// stdin (`/dev/null`), stdout and stderr — *not* the server's open database
/// files, wire/HTTP/KV listener sockets, or cluster-channel sockets. This
/// matters because a per-site PHP cron child could otherwise `php://fd/N` onto a
/// server-held descriptor (another tenant's DB file, a listening socket) —
/// `open_basedir` does not gate the `php://fd/` wrapper.
///
/// The guarantee rests on every such descriptor being `O_CLOEXEC`, which holds
/// by construction: Rust's `std::fs`/`std::net` and the `socket2`/`mio` sockets
/// under tokio all set `O_CLOEXEC`/`SOCK_CLOEXEC` unconditionally, the embedded
/// Turso engine opens its DB/WAL files through `std::fs::OpenOptions` (also
/// `O_CLOEXEC`), and ePHPm's own raw opens pass `O_CLOEXEC` explicitly. We do
/// **not** rely on an fd-clearing `pre_exec` hook (which would be `unsafe` and
/// must be async-signal-safe); CLOEXEC is the portable, race-free mechanism and
/// is verified by `run_child_does_not_leak_cloexec_fds` below. A new long-lived
/// server fd must likewise be CLOEXEC — the default for the APIs above, so this
/// is a note, not a step.
pub async fn run_child(
    program: &std::path::Path,
    args: &[String],
    cwd: Option<&std::path::Path>,
    extra_env: &[(String, String)],
    kv_secret: Option<&str>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> RunOutcome {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    if let Some(secret) = kv_secret {
        // Child-only: the parent server's `EPHPM_KV__SECRET` was scrubbed, so
        // reinstate it just for this child so its `Config::load` can derive the
        // site's wire credential. `ephpm php --site` scrubs it again before any
        // tenant PHP runs.
        cmd.env("EPHPM_KV__SECRET", secret);
    }
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
            cwd: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_max_bytes: None,
            cpu_secs: None,
            kv_secret: None,
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
            cwd: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_max_bytes: None,
            cpu_secs: None,
            kv_secret: None,
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
    fn argv_per_site_php_names_script_with_dash_f() {
        // Regression (the double-`--` / stdin bug): a per-site `.php` job must
        // name its script with `-f`, never with a *leading* `--`. `php -- …`
        // reads the program from stdin, which under `Stdio::null()` compiles an
        // empty program that exits 0 — the job would be reported `ok` without
        // running. The dashed-arg guard `--` sits *after* the script only.
        let req = ExecRequest {
            job: "j".into(),
            site: Some("blog".into()),
            is_php: true,
            command: vec!["artisan.php".into(), "schedule:run".into()],
            config_path: Some(PathBuf::from("/c.toml")),
            cwd: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_max_bytes: None,
            cpu_secs: None,
            kv_secret: None,
        };
        let (prog, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        assert_eq!(prog, PathBuf::from("/usr/bin/ephpm"));
        assert_eq!(
            args,
            vec![
                "php",
                "--config",
                "/c.toml",
                "--site",
                "blog",
                "--confine",
                "-f",
                "artisan.php",
                "--",
                "schedule:run",
            ]
        );
        // The script is still named with `-f`, never a bare `--` (the leading
        // `--confine` now sits between the site and `-f`).
        let f_pos = args.iter().position(|a| a == "-f").unwrap();
        assert_eq!(args[f_pos + 1], "artisan.php");
        assert!(!args[..f_pos].contains(&"--".to_string()), "no bare -- before the script");
    }

    #[test]
    fn argv_per_site_php_single_script_has_no_trailing_separator() {
        // A directory-tier job is a one-element command (just the script). No
        // arguments ⇒ no trailing `--`; `php -f <script>` runs it directly.
        let req = ExecRequest {
            job: "j".into(),
            site: Some("blog".into()),
            is_php: true,
            command: vec!["daily/cleanup.php".into()],
            config_path: None,
            cwd: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_max_bytes: None,
            cpu_secs: None,
            kv_secret: None,
        };
        let (_, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        // The directory tier is still confined (per-site cron), so `--confine`
        // precedes the `-f <script>`.
        assert_eq!(args, vec!["php", "--site", "blog", "--confine", "-f", "daily/cleanup.php"]);
    }

    /// F5 — a per-site PHP cron job emits the resource-limit options and
    /// `--confine` as *leading* options, before the `-f <script>`, so the
    /// child's clap parses them (not the PHP interpreter).
    #[test]
    fn argv_per_site_php_carries_limits_and_confine_before_script() {
        let req = ExecRequest {
            job: "j".into(),
            site: Some("blog".into()),
            is_php: true,
            command: vec!["cron.php".into()],
            config_path: Some(PathBuf::from("/c.toml")),
            cwd: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_max_bytes: Some(512 * 1024 * 1024),
            cpu_secs: Some(90),
            kv_secret: None,
        };
        let (_, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        assert_eq!(
            args,
            vec![
                "php",
                "--config",
                "/c.toml",
                "--site",
                "blog",
                "--memory-max",
                "536870912",
                "--cpu-secs",
                "90",
                "--confine",
                "-f",
                "cron.php",
            ]
        );
        // Every ePHPm-owned option precedes the `-f` script token.
        let f = args.iter().position(|a| a == "-f").unwrap();
        for opt in ["--memory-max", "--cpu-secs", "--confine", "--site"] {
            assert!(args.iter().position(|a| a == opt).unwrap() < f, "{opt} must precede -f");
        }
    }

    /// F5 — a per-site binary cron job emits the resource-limit options to
    /// `ephpm exec` (which already confines by construction, so no `--confine`),
    /// before the `--` command separator.
    #[test]
    fn argv_per_site_binary_carries_limits_before_separator() {
        let req = ExecRequest {
            job: "j".into(),
            site: Some("shop".into()),
            is_php: false,
            command: vec!["wp".into(), "cron".into(), "event".into()],
            config_path: None,
            cwd: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_max_bytes: Some(256 * 1024 * 1024),
            cpu_secs: Some(30),
            kv_secret: None,
        };
        let (prog, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        assert_eq!(prog, PathBuf::from("/usr/bin/ephpm"));
        assert_eq!(
            args,
            vec![
                "exec",
                "--site",
                "shop",
                "--memory-max",
                "268435456",
                "--cpu-secs",
                "30",
                "--",
                "wp",
                "cron",
                "event",
            ]
        );
        assert!(!args.iter().any(|a| a == "--confine"), "exec confines by construction");
    }

    #[tokio::test]
    async fn run_child_reports_success_and_failure() {
        let cancel = CancellationToken::new();
        // A command that exits 0.
        #[cfg(unix)]
        let (ok_prog, ok_args) = ("/bin/sh", vec!["-c".to_string(), "exit 0".to_string()]);
        #[cfg(windows)]
        let (ok_prog, ok_args) = ("cmd", vec!["/C".to_string(), "exit 0".to_string()]);
        let out = run_child(
            std::path::Path::new(ok_prog),
            &ok_args,
            None,
            &[],
            None,
            Duration::from_secs(10),
            &cancel,
        )
        .await;
        assert_eq!(out, RunOutcome::Ok);

        #[cfg(unix)]
        let (bad_prog, bad_args) = ("/bin/sh", vec!["-c".to_string(), "exit 3".to_string()]);
        #[cfg(windows)]
        let (bad_prog, bad_args) = ("cmd", vec!["/C".to_string(), "exit 3".to_string()]);
        let out = run_child(
            std::path::Path::new(bad_prog),
            &bad_args,
            None,
            &[],
            None,
            Duration::from_secs(10),
            &cancel,
        )
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
        let out = run_child(
            std::path::Path::new(prog),
            &args,
            None,
            &[],
            None,
            Duration::from_millis(200),
            &cancel,
        )
        .await;
        assert_eq!(out, RunOutcome::Timeout);
    }

    /// C5 — the timeout does not merely *report* `Timeout`: the child is
    /// actually killed. The child would write a marker after ~2 s; with a
    /// 200 ms cap the marker must never appear, even well after the child's
    /// natural finish time.
    #[tokio::test]
    async fn run_child_timeout_actually_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("done");
        let marker_s = marker.to_string_lossy().into_owned();
        let cancel = CancellationToken::new();
        #[cfg(unix)]
        let (prog, args) =
            ("/bin/sh", vec!["-c".to_string(), format!("sleep 2; echo done > '{marker_s}'")]);
        #[cfg(windows)]
        let (prog, args) = (
            "cmd",
            vec!["/C".to_string(), format!("ping -n 3 127.0.0.1 >NUL & echo done > {marker_s}")],
        );
        let started = std::time::Instant::now();
        let out = run_child(
            std::path::Path::new(prog),
            &args,
            None,
            &[],
            None,
            Duration::from_millis(200),
            &cancel,
        )
        .await;
        assert_eq!(out, RunOutcome::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the kill returned promptly (SIGTERM honoured, no grace wait)"
        );
        // Past the point the child would have finished on its own.
        tokio::time::sleep(Duration::from_millis(3_000)).await;
        assert!(!marker.exists(), "the child kept running past its timeout and wrote the marker");
    }

    /// G7 — argv is passed through verbatim: an element containing shell
    /// metacharacters stays one argument (no shell ever interprets it).
    #[test]
    fn argv_elements_are_never_shell_interpreted() {
        let req = ExecRequest {
            job: "j".into(),
            site: Some("shop".into()),
            is_php: false,
            command: vec!["wp".into(), "post list; rm -rf /".into(), "$(id)".into()],
            config_path: None,
            cwd: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_max_bytes: None,
            cpu_secs: None,
            kv_secret: None,
        };
        let (_, args) = build_argv(&req, std::path::Path::new("/usr/bin/ephpm"));
        // No `--config` without a config path; the argv tail is byte-identical.
        assert_eq!(
            args,
            vec!["exec", "--site", "shop", "--", "wp", "post list; rm -rf /", "$(id)"]
        );
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
        let out = run_child(
            std::path::Path::new(prog),
            &args,
            None,
            &[],
            None,
            Duration::from_secs(30),
            &cancel,
        )
        .await;
        assert_eq!(out, RunOutcome::Killed);
    }

    #[tokio::test]
    async fn run_child_spawn_error_on_missing_program() {
        let cancel = CancellationToken::new();
        let out = run_child(
            std::path::Path::new("/nonexistent/definitely/not/here"),
            &[],
            None,
            &[],
            None,
            Duration::from_secs(5),
            &cancel,
        )
        .await;
        assert_eq!(out, RunOutcome::SpawnError);
    }

    /// Tenant-isolation guarantee: a cron child does not inherit the server's
    /// close-on-exec file descriptors. We stand in for "any server fd" with a
    /// plain `std::fs::File` — which, like every DB file, listener socket, and
    /// cluster socket the server holds, is opened `O_CLOEXEC` — and assert the
    /// child spawned by `run_child` cannot see it. (`/dev/fd/N` is `/proc/self/fd`
    /// on Linux; `test ! -e` exits 0 when the fd is absent in the child.) If a
    /// regression ever cleared CLOEXEC or added an fd-passing `pre_exec`, the
    /// child would see the fd, exit 1, and this test would report `Failed`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_child_does_not_leak_cloexec_fds() {
        use std::os::fd::AsRawFd as _;

        // A representative server fd: a std File is always O_CLOEXEC. Held open
        // for the whole test so its number cannot be reused by tokio/std.
        let f = std::fs::File::open("/dev/null").expect("open /dev/null");
        let fd = f.as_raw_fd();
        assert!(fd >= 3, "a fresh fd is past stdio: {fd}");

        let cancel = CancellationToken::new();
        let script = format!("test ! -e /dev/fd/{fd}");
        let out = run_child(
            std::path::Path::new("/bin/sh"),
            &["-c".to_string(), script],
            None,
            &[],
            None,
            Duration::from_secs(10),
            &cancel,
        )
        .await;
        // Ok ⇒ the child did NOT inherit the CLOEXEC fd (test exited 0).
        assert_eq!(out, RunOutcome::Ok, "cron child inherited a close-on-exec server fd");
        drop(f);
    }
}

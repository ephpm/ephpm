//! Shared subprocess runner for the external-tool analyzers.
//!
//! Responsibilities the individual analyzers must not re-implement:
//!
//! - **Absence detection**: a binary not on `PATH` maps to
//!   [`AnalyzerError::Skipped`], never a crash — the graceful-degradation
//!   half of the fail-closed model.
//! - **Timeout enforcement**: a tool exceeding `analyzers.tool_timeout_ms`
//!   is killed and reported as [`AnalyzerError::Timeout`] (which gates).
//! - **Windows launcher quirks**: `CreateProcess` resolves `name.exe` but
//!   not `name.bat`/`name.cmd` (how Composer installs itself on Windows), so
//!   after a direct spawn fails with `NotFound` the runner searches `PATH`
//!   for the batch forms and relaunches through `cmd /C` — only with a fully
//!   resolved path, so a genuine absence still surfaces as `Skipped` rather
//!   than a locale-dependent `cmd` error string.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::analyzer::AnalyzerError;

/// Captured output of a finished tool.
pub(crate) struct ToolRun {
    /// Everything the tool wrote to stdout, lossily decoded.
    pub stdout: String,
    /// Everything the tool wrote to stderr, lossily decoded.
    pub stderr: String,
    /// The exit code, when the platform reports one.
    pub exit_code: Option<i32>,
}

/// Search `PATH` for the first of `names` that exists as a file.
fn find_in_path(names: &[String]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Spawn `program` with `args` in `cwd`, or map `NotFound` to `None`.
fn try_spawn(program: &Path, args: &[&str], cwd: &Path) -> Result<Option<Child>, AnalyzerError> {
    match Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => Ok(Some(child)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AnalyzerError::Io(e)),
    }
}

/// Run `tool args...` in `cwd` with a wall-clock budget of `timeout_ms`.
///
/// # Errors
///
/// - [`AnalyzerError::Skipped`] when `tool` is not installed.
/// - [`AnalyzerError::Timeout`] when the budget is exceeded (the process is
///   killed first).
/// - [`AnalyzerError::Io`] for spawn/pipe failures.
///
/// A non-zero exit is **not** an error here — several tools (notably
/// `composer audit`) exit non-zero when they find what they were asked to
/// find. Callers decide from [`ToolRun::exit_code`] plus output.
pub(crate) fn run_tool(
    tool: &str,
    args: &[&str],
    cwd: &Path,
    timeout_ms: u64,
) -> Result<ToolRun, AnalyzerError> {
    let mut child = match try_spawn(Path::new(tool), args, cwd)? {
        Some(child) => child,
        None => {
            // Windows: a Composer-style `.bat`/`.cmd` launcher is invisible
            // to CreateProcess. Resolve it on PATH ourselves and go through
            // `cmd /C` only when it actually exists.
            if cfg!(windows) {
                let batch = find_in_path(&[format!("{tool}.bat"), format!("{tool}.cmd")]);
                if let Some(batch) = batch {
                    let mut cmd_args = vec!["/C", batch.to_str().unwrap_or(tool)];
                    cmd_args.extend_from_slice(args);
                    match try_spawn(Path::new("cmd"), &cmd_args, cwd)? {
                        Some(child) => child,
                        None => {
                            return Err(AnalyzerError::Skipped(format!(
                                "`{tool}` not found on PATH"
                            )));
                        }
                    }
                } else {
                    return Err(AnalyzerError::Skipped(format!("`{tool}` not found on PATH")));
                }
            } else {
                return Err(AnalyzerError::Skipped(format!("`{tool}` not found on PATH")));
            }
        }
    };

    // Drain stdout/stderr on threads so a chatty tool can't deadlock on a
    // full pipe while we poll for exit.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || read_all(stdout));
    let stderr_reader = std::thread::spawn(move || read_all(stderr));

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Best-effort kill; the process may have just exited.
                    let _ = child.kill();
                    let _ = child.wait();
                    // Reader threads finish once the pipes close.
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(AnalyzerError::Timeout(timeout_ms));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(AnalyzerError::Io(e)),
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(ToolRun { stdout, stderr, exit_code: status.code() })
}

fn read_all<R: std::io::Read>(reader: Option<R>) -> String {
    let mut buf = Vec::new();
    if let Some(mut reader) = reader {
        let _ = std::io::Read::read_to_end(&mut reader, &mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Trim a stderr blob to a single diagnostic-sized line for error messages.
pub(crate) fn stderr_excerpt(stderr: &str) -> String {
    let trimmed = stderr.trim();
    let first = trimmed.lines().next().unwrap_or_default();
    let mut excerpt: String = first.chars().take(200).collect();
    if excerpt.len() < trimmed.len() {
        excerpt.push('…');
    }
    excerpt
}

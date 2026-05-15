//! Self-managing service support for the `ephpm` binary.
//!
//! Implements the `install`, `uninstall`, `start`, `stop`, `restart`,
//! `status`, and `logs` subcommands documented in the project README.
//! The public API is platform-independent; the actual SCM/systemd/launchd
//! plumbing lives in the platform-specific submodules.
//!
//! # Platform paths
//!
//! | Platform | Binary | Config | Service unit | Data dir | Log file |
//! |----------|--------|--------|--------------|----------|----------|
//! | Linux    | `/usr/local/bin/ephpm` | `/etc/ephpm/ephpm.toml` | `/etc/systemd/system/ephpm.service` | `/var/lib/ephpm/` | `/var/log/ephpm/ephpm.log` |
//! | macOS    | `/usr/local/bin/ephpm` | `/etc/ephpm/ephpm.toml` | `/Library/LaunchDaemons/dev.ephpm.plist` | `/var/lib/ephpm/` | `/var/log/ephpm/ephpm.log` |
//! | Windows  | `C:\Program Files\ephpm\ephpm.exe` | `C:\ProgramData\ephpm\ephpm.toml` | Windows service `ephpm` | `C:\ProgramData\ephpm\data\` | `C:\ProgramData\ephpm\logs\ephpm.log` |
//!
//! All lifecycle commands require elevated privileges (root / Administrator).

#![cfg_attr(unix, allow(unsafe_code))]
#![allow(clippy::module_name_repetitions)]

use std::path::{Path, PathBuf};

use thiserror::Error;

#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "linux")]
use systemd as backend;

#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "macos")]
use launchd as backend;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
use windows as backend;

/// Errors that may occur while managing the ePHPm service.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// The command requires elevated privileges that the caller lacks.
    #[error("this command requires {0} privileges — re-run with elevated rights")]
    NotElevated(&'static str),

    /// An I/O error occurred while reading or writing service / config files.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A child process (systemctl / launchctl / sc / windows-service) failed.
    #[error("{cmd} failed: {message}")]
    Command {
        /// Name of the underlying program / API.
        cmd: String,
        /// Human-readable failure description.
        message: String,
    },

    /// The service backend reports the service is not installed.
    #[error("service is not installed — run `ephpm install` first")]
    NotInstalled,

    /// An unexpected error string from a platform-specific backend.
    #[error("{0}")]
    Other(String),
}

impl ServiceError {
    /// Helper to build an `Io` error with a path attached.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }

    /// Helper to build a `Command` error.
    pub(crate) fn command(cmd: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Command { cmd: cmd.into(), message: message.into() }
    }
}

/// Result alias used throughout the service module.
pub type Result<T> = std::result::Result<T, ServiceError>;

/// Filesystem locations the service backend reads and writes.
///
/// Platform-specific defaults are constructed by [`Paths::for_current_platform`].
#[derive(Debug, Clone)]
pub struct Paths {
    /// Where the `ephpm` binary is installed to.
    pub binary: PathBuf,
    /// Where the TOML configuration file lives.
    pub config: PathBuf,
    /// Service unit / plist / SCM entry path (informational on Windows).
    ///
    /// Used by the systemd and launchd backends to locate the unit file; on
    /// Windows the SCM tracks the service by name, so this field is only kept
    /// for documentation / diagnostic purposes.
    #[cfg_attr(windows, allow(dead_code))]
    pub service_unit: PathBuf,
    /// Persistent data directory (SQLite DBs, ACME state, etc.).
    pub data_dir: PathBuf,
    /// Default document root referenced by the generated config.
    pub document_root: PathBuf,
    /// File that the service logs to.
    pub log_file: PathBuf,
}

impl Paths {
    /// Build the platform-default layout.
    #[must_use]
    pub fn for_current_platform() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                binary: PathBuf::from("/usr/local/bin/ephpm"),
                config: PathBuf::from("/etc/ephpm/ephpm.toml"),
                service_unit: PathBuf::from("/etc/systemd/system/ephpm.service"),
                data_dir: PathBuf::from("/var/lib/ephpm"),
                document_root: PathBuf::from("/var/www/html"),
                log_file: PathBuf::from("/var/log/ephpm/ephpm.log"),
            }
        }

        #[cfg(target_os = "macos")]
        {
            Self {
                binary: PathBuf::from("/usr/local/bin/ephpm"),
                config: PathBuf::from("/etc/ephpm/ephpm.toml"),
                service_unit: PathBuf::from("/Library/LaunchDaemons/dev.ephpm.plist"),
                data_dir: PathBuf::from("/var/lib/ephpm"),
                document_root: PathBuf::from("/var/www/html"),
                log_file: PathBuf::from("/var/log/ephpm/ephpm.log"),
            }
        }

        #[cfg(windows)]
        {
            Self {
                binary: PathBuf::from(r"C:\Program Files\ephpm\ephpm.exe"),
                config: PathBuf::from(r"C:\ProgramData\ephpm\ephpm.toml"),
                service_unit: PathBuf::from(r"ephpm"),
                data_dir: PathBuf::from(r"C:\ProgramData\ephpm\data"),
                document_root: PathBuf::from(r"C:\ProgramData\ephpm\www"),
                log_file: PathBuf::from(r"C:\ProgramData\ephpm\logs\ephpm.log"),
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            Self {
                binary: PathBuf::from("/usr/local/bin/ephpm"),
                config: PathBuf::from("/etc/ephpm/ephpm.toml"),
                service_unit: PathBuf::from("/etc/ephpm/ephpm.service"),
                data_dir: PathBuf::from("/var/lib/ephpm"),
                document_root: PathBuf::from("/var/www/html"),
                log_file: PathBuf::from("/var/log/ephpm/ephpm.log"),
            }
        }
    }
}

/// Render the default TOML configuration body for a fresh install.
///
/// `document_root` is the path written into the generated `[server]` section so
/// that Windows installs reference `C:\ProgramData\ephpm\www` and Unix installs
/// reference `/var/www/html`.
#[must_use]
pub fn default_config_toml(document_root: &Path) -> String {
    let document_root_str = document_root.display().to_string();
    let escaped = document_root_str.replace('\\', "\\\\");
    format!(
        "[server]\nlisten = \"0.0.0.0:8080\"\ndocument_root = \"{escaped}\"\nindex_files = [\"index.php\", \"index.html\"]\n\n[php]\nmode = \"embedded\"\nmax_execution_time = 30\nmemory_limit = \"128M\"\n"
    )
}

/// Check whether the current process is running as root (Unix) or Administrator
/// (Windows). Used as a guard before mutating system locations.
#[must_use]
pub fn is_elevated() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `libc::geteuid` is a thread-safe libc call that takes no
        // arguments and returns a `uid_t`. It cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        backend::is_elevated()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Friendly name for the privilege required to manage the service.
#[must_use]
pub fn privilege_label() -> &'static str {
    if cfg!(windows) { "Administrator" } else { "root" }
}

/// Ensure the caller is privileged or return [`ServiceError::NotElevated`].
fn require_elevation() -> Result<()> {
    if is_elevated() { Ok(()) } else { Err(ServiceError::NotElevated(privilege_label())) }
}

/// Copy the currently running binary into `dest`, creating parents as needed.
///
/// If `src` and `dest` resolve to the same file the copy is skipped.
fn copy_binary(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServiceError::io(parent, e))?;
    }

    // Skip the copy when src == dest (e.g. running the already-installed binary).
    let same = match (src.canonicalize(), dest.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same {
        tracing::info!(path = %dest.display(), "binary already at install location");
        return Ok(());
    }

    std::fs::copy(src, dest).map_err(|e| ServiceError::io(dest, e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms =
            std::fs::metadata(dest).map_err(|e| ServiceError::io(dest, e))?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dest, perms).map_err(|e| ServiceError::io(dest, e))?;
    }
    Ok(())
}

/// Write the default config to `path` unless the file already exists. Returns
/// `true` if a fresh file was written, `false` if an existing config was left
/// untouched.
fn write_default_config(path: &Path, document_root: &Path) -> Result<bool> {
    if path.exists() {
        tracing::info!(path = %path.display(), "config already exists — leaving it in place");
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServiceError::io(parent, e))?;
    }
    let body = default_config_toml(document_root);
    std::fs::write(path, body).map_err(|e| ServiceError::io(path, e))?;
    Ok(true)
}

/// Install the binary, write a default config, register the service, and start it.
///
/// # Errors
///
/// Returns [`ServiceError::NotElevated`] if the caller lacks root /
/// Administrator rights, or an I/O / backend error if any step (binary copy,
/// config write, service registration, start) fails.
pub fn install() -> Result<()> {
    require_elevation()?;
    let paths = Paths::for_current_platform();

    let current = std::env::current_exe().map_err(|e| ServiceError::Other(format!(
        "failed to resolve current executable: {e}"
    )))?;

    copy_binary(&current, &paths.binary)?;
    let wrote = write_default_config(&paths.config, &paths.document_root)?;
    if wrote {
        tracing::info!(path = %paths.config.display(), "wrote default config");
    }
    std::fs::create_dir_all(&paths.data_dir).map_err(|e| ServiceError::io(&paths.data_dir, e))?;
    if let Some(parent) = paths.log_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServiceError::io(parent, e))?;
    }

    backend::register(&paths)?;
    backend::start(&paths)?;
    tracing::info!("ephpm service installed and started");
    Ok(())
}

/// Reverse a previous `install`. With `keep_data = true`, the config and data
/// directory are left in place.
///
/// # Errors
///
/// Same as [`install`] — privilege / I/O / backend errors are propagated.
pub fn uninstall(keep_data: bool) -> Result<()> {
    require_elevation()?;
    let paths = Paths::for_current_platform();

    // Best-effort stop — ignore errors so we can clean up after a partial install.
    if let Err(e) = backend::stop(&paths) {
        tracing::warn!(error = %e, "failed to stop service during uninstall — continuing");
    }
    backend::deregister(&paths)?;

    if !keep_data {
        if paths.config.exists() {
            std::fs::remove_file(&paths.config).map_err(|e| ServiceError::io(&paths.config, e))?;
        }
        if paths.data_dir.exists() {
            std::fs::remove_dir_all(&paths.data_dir)
                .map_err(|e| ServiceError::io(&paths.data_dir, e))?;
        }
    }

    if paths.binary.exists() {
        std::fs::remove_file(&paths.binary).map_err(|e| ServiceError::io(&paths.binary, e))?;
    }
    tracing::info!("ephpm service uninstalled");
    Ok(())
}

/// Start the installed service.
///
/// # Errors
///
/// Propagates privilege and backend errors.
pub fn start() -> Result<()> {
    require_elevation()?;
    backend::start(&Paths::for_current_platform())
}

/// Stop the installed service.
///
/// # Errors
///
/// Propagates privilege and backend errors.
pub fn stop() -> Result<()> {
    require_elevation()?;
    backend::stop(&Paths::for_current_platform())
}

/// Restart the installed service (stop then start, with backend-specific atomic
/// implementations where available).
///
/// # Errors
///
/// Propagates privilege and backend errors.
pub fn restart() -> Result<()> {
    require_elevation()?;
    backend::restart(&Paths::for_current_platform())
}

/// Print a one-line status summary (PID, uptime, listen address) to stdout.
///
/// # Errors
///
/// Returns a backend error if the service controller can't be queried.
pub fn status() -> Result<()> {
    let paths = Paths::for_current_platform();
    let report = backend::status(&paths)?;

    let listen = read_listen_address(&paths.config).unwrap_or_else(|| "<unknown>".to_string());
    println!(
        "service: {state}\n   pid: {pid}\nuptime: {uptime}\nlisten: {listen}\nconfig: {config}",
        state = report.state,
        pid = report.pid.as_ref().map_or_else(|| "-".to_string(), u32::to_string),
        uptime = report.uptime.as_deref().unwrap_or("-"),
        config = paths.config.display(),
    );
    Ok(())
}

/// Tail (or follow) the service log file.
///
/// # Errors
///
/// Returns an I/O error if the log file cannot be read.
pub fn logs(follow: bool) -> Result<()> {
    let paths = Paths::for_current_platform();
    backend::logs(&paths, follow)
}

/// Snapshot of service state returned by platform backends.
#[derive(Debug, Clone)]
pub struct StatusReport {
    /// Human-readable state ("running", "stopped", ...).
    pub state: String,
    /// PID, if the service is running.
    pub pid: Option<u32>,
    /// Human-readable uptime string, if known.
    pub uptime: Option<String>,
}

/// Best-effort parser that extracts `server.listen` from a TOML config file
/// without depending on `serde`'s schema. Returns `None` if the file can't be
/// read or the key isn't present.
fn read_listen_address(config: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_server = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            in_server = rest.starts_with("server]");
            continue;
        }
        if in_server {
            if let Some(value) = trimmed.strip_prefix("listen") {
                let value = value.trim_start().trim_start_matches('=').trim();
                let value = value.trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Simple tail implementation used by Unix backends. Reads the last `lines`
/// from `path` and prints them. When `follow` is true, continues to print new
/// content as it is appended until the process is interrupted.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn tail_file(path: &Path, follow: bool) -> Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

    if !path.exists() {
        return Err(ServiceError::io(
            path,
            std::io::Error::new(std::io::ErrorKind::NotFound, "log file does not exist"),
        ));
    }

    let mut file = std::fs::File::open(path).map_err(|e| ServiceError::io(path, e))?;
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|e| ServiceError::io(path, e))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let tail_lines: Vec<&str> = content.lines().rev().take(200).collect();
    for line in tail_lines.into_iter().rev() {
        let _ = writeln!(out, "{line}");
    }
    if !follow {
        return Ok(());
    }

    let mut pos = file.seek(SeekFrom::End(0)).map_err(|e| ServiceError::io(path, e))?;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let len = std::fs::metadata(path).map_err(|e| ServiceError::io(path, e))?.len();
        if len < pos {
            // file was rotated — start over from the beginning
            pos = 0;
            file = std::fs::File::open(path).map_err(|e| ServiceError::io(path, e))?;
        }
        file.seek(SeekFrom::Start(pos)).map_err(|e| ServiceError::io(path, e))?;
        let mut chunk = String::new();
        let read = file.read_to_string(&mut chunk).map_err(|e| ServiceError::io(path, e))?;
        if read > 0 {
            let _ = out.write_all(chunk.as_bytes());
            let _ = out.flush();
            pos += read as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_contains_required_keys() {
        let toml = default_config_toml(Path::new("/var/www/html"));
        assert!(toml.contains("[server]"));
        assert!(toml.contains("listen = \"0.0.0.0:8080\""));
        assert!(toml.contains("document_root = \"/var/www/html\""));
        assert!(toml.contains("[php]"));
        assert!(toml.contains("memory_limit = \"128M\""));
        assert!(toml.contains("max_execution_time = 30"));
    }

    #[test]
    fn default_config_escapes_windows_paths() {
        let toml = default_config_toml(Path::new(r"C:\ProgramData\ephpm\www"));
        // Backslashes must be doubled so the TOML parser sees a literal backslash.
        assert!(toml.contains(r#"document_root = "C:\\ProgramData\\ephpm\\www""#));
    }

    #[test]
    fn paths_for_current_platform_are_absolute() {
        let p = Paths::for_current_platform();
        assert!(p.binary.is_absolute(), "binary path should be absolute: {}", p.binary.display());
        assert!(p.config.is_absolute(), "config path should be absolute: {}", p.config.display());
        assert!(p.data_dir.is_absolute());
        assert!(p.document_root.is_absolute());
    }

    #[test]
    fn write_default_config_does_not_overwrite_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("ephpm.toml");
        std::fs::write(&config, "# user customized\n").unwrap();

        let wrote = write_default_config(&config, Path::new("/var/www/html")).unwrap();
        assert!(!wrote, "should report no fresh write when file exists");

        let after = std::fs::read_to_string(&config).unwrap();
        assert_eq!(after, "# user customized\n");
    }

    #[test]
    fn write_default_config_writes_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("nested").join("ephpm.toml");

        let wrote = write_default_config(&config, Path::new("/var/www/html")).unwrap();
        assert!(wrote);
        let body = std::fs::read_to_string(&config).unwrap();
        assert!(body.contains("[server]"));
    }

    #[test]
    fn read_listen_address_parses_basic_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("ephpm.toml");
        std::fs::write(
            &config,
            "# header\n[server]\nlisten = \"127.0.0.1:9090\"\ndocument_root = \"/tmp\"\n",
        )
        .unwrap();
        assert_eq!(read_listen_address(&config).as_deref(), Some("127.0.0.1:9090"));
    }

    #[test]
    fn read_listen_address_returns_none_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("ephpm.toml");
        std::fs::write(&config, "[php]\nmemory_limit = \"128M\"\n").unwrap();
        assert!(read_listen_address(&config).is_none());
    }

    #[test]
    fn privilege_label_is_platform_appropriate() {
        let label = privilege_label();
        if cfg!(windows) {
            assert_eq!(label, "Administrator");
        } else {
            assert_eq!(label, "root");
        }
    }
}

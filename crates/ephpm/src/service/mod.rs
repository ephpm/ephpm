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
//!
//! `install` optionally takes a [`ServiceOwner`] (`ephpm install --user
//! --group`) so the installed service runs as a dedicated non-root account
//! instead of the elevated installer. Currently implemented on Linux
//! (systemd `User=`/`Group=` plus a conservative hardening profile) and
//! macOS (launchd `UserName`/`GroupName`); the Windows SCM backend rejects
//! `Some(owner)` rather than silently ignoring it, since it has no secure way
//! to collect and store the account password SCM requires.

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
    ///
    /// Currently only constructed by the Windows SCM backend; the systemd
    /// and launchd backends fall back to running the underlying CLI and
    /// surface its error verbatim via [`ServiceError::Command`].
    #[cfg_attr(not(windows), allow(dead_code))]
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
    /// Extra directories, outside `data_dir` / the log dir / `document_root`,
    /// that the configured server writes to (an absolute `[db.sqlite] path`
    /// or `dir`, an ACME `cache_dir`). Filled by `install` from the config
    /// for a `--user` install, and added to the systemd unit's
    /// `ReadWritePaths`. Empty from [`Paths::for_current_platform`].
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub extra_writable_dirs: Vec<PathBuf>,
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
                extra_writable_dirs: Vec::new(),
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
                extra_writable_dirs: Vec::new(),
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
                extra_writable_dirs: Vec::new(),
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
                extra_writable_dirs: Vec::new(),
            }
        }
    }
}

/// Render the default TOML configuration body for a fresh install.
///
/// `document_root` is the path written into the generated `[server]` section so
/// that Windows installs reference `C:\ProgramData\ephpm\www` and Unix installs
/// reference `/var/www/html`.
///
/// The body must round-trip through `Config::load` + `Config::validate` — the
/// installer writes this file and then *starts* the service, so anything
/// `validate()` rejects turns a fresh install into a service that dies on
/// boot. `default_config_parses_and_validates` pins that.
#[must_use]
pub fn default_config_toml(document_root: &Path) -> String {
    let document_root_str = document_root.display().to_string();
    let escaped = document_root_str.replace('\\', "\\\\");
    format!(
        "[server]\nlisten = \"0.0.0.0:8080\"\ndocument_root = \"{escaped}\"\nindex_files = [\"index.php\", \"index.html\"]\n\n[php]\nmode = \"per_request\"\nmax_execution_time = 30\n"
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

/// Recursively `chown` `path` to `owner.user:owner.group` by shelling out to
/// the `chown` binary — present on every Unix ePHPm supports, and it accepts
/// names directly so this needs no `getpwnam`/`getgrnam` FFI. Matches the
/// existing pattern in this module of shelling out to platform CLIs
/// (`systemctl`, `launchctl`) rather than linking their APIs directly.
#[cfg(unix)]
fn chown_path(path: &Path, owner: &ServiceOwner) -> Result<()> {
    chown_impl(path, owner, true)
}

#[cfg(unix)]
fn chown_impl(path: &Path, owner: &ServiceOwner, recursive: bool) -> Result<()> {
    let spec = format!("{}:{}", owner.user, owner.group);
    let mut cmd = std::process::Command::new("chown");
    if recursive {
        cmd.arg("-R");
    }
    let out = cmd
        .arg(&spec)
        .arg(path)
        .output()
        .map_err(|e| ServiceError::command("chown", e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::command(
            format!("chown {spec} {}", path.display()),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

/// A directory the configured server writes to, derived from the config by
/// [`config_writable_dirs`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
struct WritableDir {
    /// Absolute directory path.
    path: PathBuf,
    /// Whether ePHPm owns everything inside it (per-site DB dir, ACME cache)
    /// — chowned recursively — or only puts its own files there (the parent
    /// of a single-file `[db.sqlite] path`) — chowned non-recursively, since
    /// SQLite must create its `-wal`/`-shm` siblings next to the database.
    recursive: bool,
}

/// The directories the server will write, per `config`, with relative paths
/// resolved against `working_dir` (the unit's `WorkingDirectory`).
///
/// Covers `[db.sqlite] path` (its parent directory), `[db.sqlite] dir`
/// (per-site databases) and an ACME `[server.tls] cache_dir`. Deduplicated,
/// in that order.
/// `..` components are folded lexically, so a relative `path = "../app.db"`
/// becomes `/var/lib/app.db` rather than `/var/lib/ephpm/../app.db` — which
/// would otherwise pass a `starts_with(data_dir)` check while pointing
/// outside it.
#[cfg_attr(not(unix), allow(dead_code))]
fn config_writable_dirs(config: &ephpm_config::Config, working_dir: &Path) -> Vec<WritableDir> {
    let abs = |p: &Path| {
        normalize_lexically(&if p.is_absolute() { p.to_path_buf() } else { working_dir.join(p) })
    };
    let mut dirs: Vec<WritableDir> = Vec::new();
    let mut push = |path: PathBuf, recursive: bool| {
        if let Some(existing) = dirs.iter_mut().find(|d| d.path == path) {
            existing.recursive |= recursive;
        } else {
            dirs.push(WritableDir { path, recursive });
        }
    };
    if let Some(sqlite) = config.db.sqlite.as_ref() {
        let db = sqlite.path.trim();
        if !db.is_empty()
            && db != ":memory:"
            && !db.starts_with("file:")
            && let Some(parent) = abs(Path::new(db)).parent()
        {
            push(parent.to_path_buf(), false);
        }
        if let Some(dir) = sqlite.dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            push(abs(Path::new(dir)), true);
        }
    }
    // Same predicate privilege-drop uses to decide the ACME cache matters.
    if let Some(tls) = config.server.tls.as_ref()
        && tls.is_acme()
    {
        push(abs(&tls.cache_dir), true);
    }
    dirs
}

/// Fold `.` and `..` components without touching the filesystem (the
/// directories may not exist yet). `..` at the root stays at the root.
#[cfg_attr(not(unix), allow(dead_code))]
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_)) | None
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The numeric uid of `user`, via `id -u` (same shell-out approach as
/// [`chown_impl`], no `getpwnam` FFI). `None` if it cannot be resolved.
#[cfg(unix)]
fn user_uid(user: &str) -> Option<u32> {
    let out = std::process::Command::new("id").arg("-u").arg(user).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Create the config-derived writable directories for a `--user` install and
/// give the service account write access to them. Returns the ones outside
/// `data_dir` (which the caller chowns recursively anyway), for the unit's
/// `ReadWritePaths`.
///
/// A directory this install **creates** is chowned to the service account. A
/// directory that **already existed** outside `data_dir` is left alone — it
/// may be a shared system directory such as `/var/lib` (an operator's
/// `path = "/var/lib/app.db"`), and recursively re-owning something the
/// installer did not create is not its call — and a warning names it so the
/// operator can grant access deliberately.
#[cfg(unix)]
fn prepare_config_dirs(paths: &Paths, owner: &ServiceOwner) -> Result<Vec<PathBuf>> {
    let config = match ephpm_config::Config::load(&paths.config) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(
                path = %paths.config.display(),
                error = %e,
                "could not parse the config to find the directories the service writes; \
                 only the data and log directories were given to the service user"
            );
            return Ok(Vec::new());
        }
    };
    use std::os::unix::fs::MetadataExt as _;

    let service_uid = user_uid(&owner.user);
    let mut extra = Vec::new();
    for dir in config_writable_dirs(&config, &paths.data_dir) {
        let existing_uid = std::fs::metadata(&dir.path).ok().map(|m| m.uid());
        std::fs::create_dir_all(&dir.path).map_err(|e| ServiceError::io(&dir.path, e))?;
        if dir.path.starts_with(&paths.data_dir) {
            continue; // covered by the recursive chown of data_dir
        }
        // Already owned by the service account (typically: created and
        // chowned by a previous `install --user` run) — nothing to do, and no
        // point warning on every re-install.
        if existing_uid.is_some() && existing_uid == service_uid {
            extra.push(dir.path);
            continue;
        }
        if existing_uid.is_some() {
            tracing::warn!(
                path = %dir.path.display(),
                user = %owner.user,
                "the configured server writes to this existing directory; it was not \
                 re-owned — make sure the service user can write to it (chown/setfacl)"
            );
        } else {
            chown_impl(&dir.path, owner, dir.recursive)?;
        }
        extra.push(dir.path);
    }
    Ok(extra)
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
/// `owner` is `None` for the historical default (service runs as the elevated
/// installing account, unit has no sandboxing) or `Some` to run the service as
/// a dedicated non-root user/group — see [`ServiceOwner`].
///
/// # Errors
///
/// Returns [`ServiceError::NotElevated`] if the caller lacks root /
/// Administrator rights, or an I/O / backend error if any step (binary copy,
/// config write, service registration, start) fails. On Windows, returns
/// [`ServiceError::Other`] if `owner` is `Some` — the SCM backend does not
/// yet support installing under a specific account (see `windows::register`).
pub fn install(owner: Option<&ServiceOwner>) -> Result<()> {
    // `install` copies only the executable. A shared-engine Windows build
    // also needs php8ts.dll, its dependency DLLs and ext\ beside it; an
    // installed copy without them would fail to start with no ePHPm log.
    if ephpm_php::shared::SharedRuntime::is_shared_build() {
        return Err(ServiceError::Other(
            "`ephpm install` is not supported for the shared-engine Windows build: it copies \
             only ephpm.exe, and this build also needs php8ts.dll, its dependency DLLs and ext\\ \
             beside it. Install the default (static) build, or register the service from the \
             shared build's own directory yourself."
                .into(),
        ));
    }
    require_elevation()?;
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut paths = Paths::for_current_platform();

    let current = std::env::current_exe()
        .map_err(|e| ServiceError::Other(format!("failed to resolve current executable: {e}")))?;

    copy_binary(&current, &paths.binary)?;
    let wrote = write_default_config(&paths.config, &paths.document_root)?;
    if wrote {
        tracing::info!(path = %paths.config.display(), "wrote default config");
    }
    std::fs::create_dir_all(&paths.data_dir).map_err(|e| ServiceError::io(&paths.data_dir, e))?;
    if let Some(parent) = paths.log_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ServiceError::io(parent, e))?;
    }

    // The directories above were just created by `require_elevation()`'s
    // caller — root/Administrator — so they're root-owned. That's the
    // "root-only assumption" `owner: Some` needs to skip: once the unit runs
    // as a non-root `User=`, that account needs write access to the data and
    // log directories itself (ePHPm's own tracing layer opens the log file
    // directly, not just via systemd's `StandardOutput=append:`). Chowning
    // is a no-op for `owner: None`, and idempotent on re-install.
    #[cfg(unix)]
    if let Some(owner) = owner {
        // Directories the *configured* server writes outside the data dir
        // (an absolute `[db.sqlite] path`/`dir`, an ACME `cache_dir`).
        // Relative ones resolve inside `data_dir`: the non-root unit runs
        // with `WorkingDirectory=<data_dir>` (without it, the default
        // `[db.sqlite] path = "ephpm.db"` resolved to `/ephpm.db` — root's
        // directory, read-only under `ProtectSystem=strict`).
        paths.extra_writable_dirs = prepare_config_dirs(&paths, owner)?;
        chown_path(&paths.data_dir, owner)?;
        if let Some(parent) = paths.log_file.parent() {
            chown_path(parent, owner)?;
        }
    }

    backend::register(&paths, owner)?;
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
        // Clear out the log dir + any shared parent (`C:\ProgramData\ephpm`,
        // `/var/log/ephpm`) that the install created. `remove_dir_all` on the
        // log dir handles open log files; the parent removal is best-effort
        // (only succeeds when empty).
        if let Some(log_dir) = paths.log_file.parent()
            && log_dir.exists()
        {
            let _ = std::fs::remove_dir_all(log_dir);
        }
        if let Some(config_parent) = paths.config.parent() {
            let _ = std::fs::remove_dir(config_parent);
        }
    }

    if paths.binary.exists() {
        remove_with_retry(&paths.binary)?;
    }
    // Best-effort: drop the install directory if it's now empty. The Unix
    // backends install into /usr/local/bin which is shared; `remove_dir` only
    // succeeds when empty, so this is safe.
    if let Some(parent) = paths.binary.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    tracing::info!("ephpm service uninstalled");
    Ok(())
}

/// Try to delete `path` up to ~3 seconds, sleeping 100ms between attempts.
/// On Windows the SCM may still hold a handle to a just-exited service
/// binary for a brief moment after `query_status` reports Stopped, so a
/// naive `remove_file` races against the kernel releasing the lock.
fn remove_with_retry(path: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if std::time::Instant::now() < deadline => {
                let kind = e.kind();
                // On Windows the locked-file error surfaces as PermissionDenied.
                // Anything else (NotFound, etc.) we surface immediately.
                if kind != std::io::ErrorKind::PermissionDenied {
                    return Err(ServiceError::io(path, e));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(ServiceError::io(path, e)),
        }
    }
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

/// Non-root identity the installed service should run as.
///
/// When `install()` is called with `None`, the generated unit is byte-for-byte
/// identical to the pre-#XXX output: no `User=`/`Group=`, no sandboxing
/// directives, service runs as whatever account performed the (elevated)
/// install — historically root. Passing `Some` opts into running the service
/// as `user`/`group` plus a conservative systemd hardening profile (see
/// `systemd::unit_body`). This is what lets a multi-tenant deployment run
/// ePHPm under a dedicated non-root account instead of hand-rolling the unit.
#[derive(Debug, Clone)]
pub struct ServiceOwner {
    /// Unix user (or Windows/macOS equivalent) the service process runs as.
    pub user: String,
    /// Primary group the service process runs as.
    pub group: String,
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
        if in_server && let Some(value) = trimmed.strip_prefix("listen") {
            let value = value.trim_start().trim_start_matches('=').trim();
            let value = value.trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// How much of the end of the log file [`tail_file`] reads to find the last
/// 200 lines.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const TAIL_WINDOW_BYTES: u64 = 1024 * 1024;

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
    // Only the end of the file matters for the last 200 lines; don't pull a
    // multi-GB log into memory. Bytes, not `read_to_string`: PHP output can
    // carry non-UTF-8 bytes, which used to make the whole command fail.
    let len = file.metadata().map_err(|e| ServiceError::io(path, e))?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_WINDOW_BYTES)))
        .map_err(|e| ServiceError::io(path, e))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| ServiceError::io(path, e))?;
    let content = String::from_utf8_lossy(&bytes);
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
        let mut chunk = Vec::new();
        let read = file.read_to_end(&mut chunk).map_err(|e| ServiceError::io(path, e))?;
        if read > 0 {
            let _ = out.write_all(&chunk);
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
        assert!(toml.contains("mode = \"per_request\""));
        // No memory_limit: an explicit one is honoured as written in serve mode,
        // so the template leaves the per-request limit to the autotune.
        assert!(!toml.contains("memory_limit"));
        assert!(toml.contains("max_execution_time = 30"));
    }

    #[test]
    fn default_config_parses_and_validates() {
        // `install()` writes this file and then immediately starts the
        // service, so the generated body must survive exactly what the serve
        // path does to it: `Config::load` followed by `Config::validate`.
        // Asserting on substrings alone previously let `mode = "embedded"`
        // ship — a value `validate()` rejects, so every fresh install came up
        // dead.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ephpm.toml");
        std::fs::write(&path, default_config_toml(Path::new("/var/www/html")))
            .expect("write generated config");

        let config = ephpm_config::Config::load(&path).expect("generated config must parse");
        config.validate().expect("generated config must pass validate()");
        assert_eq!(config.php.mode, ephpm_config::PhpMode::PerRequest);
        assert_eq!(config.php.max_execution_time, 30);
        assert_eq!(config.php.memory_limit, None);
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

    fn load_toml(body: &str) -> ephpm_config::Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ephpm.toml");
        std::fs::write(&path, body).unwrap();
        ephpm_config::Config::load(&path).expect("config must parse")
    }

    #[test]
    fn config_writable_dirs_resolves_relative_paths_into_working_dir() {
        // The default `[db.sqlite] path = "ephpm.db"` must land in the data
        // dir (the --user unit's WorkingDirectory), not `/`.
        let work = Path::new("/var/lib/ephpm");
        let config = load_toml("[db.sqlite]\n");
        assert_eq!(
            config_writable_dirs(&config, work),
            [WritableDir { path: work.to_path_buf(), recursive: false }]
        );
    }

    #[test]
    #[cfg(unix)] // Unix absolute paths; the function is only used on Unix.
    fn config_writable_dirs_covers_db_path_dir_and_acme_cache() {
        let work = Path::new("/var/lib/ephpm");
        let config = load_toml(
            "[server.tls]\ndomains = [\"example.com\"]\nemail = \"a@example.com\"\n\
             cache_dir = \"/srv/acme\"\n\
             [db.sqlite]\npath = \"/srv/data/app.db\"\ndir = \"sites\"\n",
        );
        assert_eq!(
            config_writable_dirs(&config, work),
            [
                WritableDir { path: PathBuf::from("/srv/data"), recursive: false },
                WritableDir { path: work.join("sites"), recursive: true },
                WritableDir { path: PathBuf::from("/srv/acme"), recursive: true },
            ]
        );
    }

    #[test]
    #[cfg(unix)] // Unix absolute paths; the function is only used on Unix.
    fn config_writable_dirs_folds_parent_components() {
        // `../app.db` relative to the data dir is `/var/lib`, which is NOT
        // inside the data dir — a lexical `starts_with` on the unfolded
        // `/var/lib/ephpm/../` path would have wrongly said it was.
        let work = Path::new("/var/lib/ephpm");
        let config = load_toml("[db.sqlite]\npath = \"../app.db\"\ndir = \"./sites/../tenants\"\n");
        let dirs = config_writable_dirs(&config, work);
        assert_eq!(
            dirs,
            [
                WritableDir { path: PathBuf::from("/var/lib"), recursive: false },
                WritableDir { path: work.join("tenants"), recursive: true },
            ]
        );
        assert!(!dirs[0].path.starts_with(work));
        assert_eq!(normalize_lexically(Path::new("/../a/./b/..")), PathBuf::from("/a"));
    }

    #[test]
    fn config_writable_dirs_skips_memory_db_and_manual_tls() {
        let config = load_toml(
            "[server.tls]\ncert = \"/etc/ssl/a.pem\"\nkey = \"/etc/ssl/a.key\"\n\
             [db.sqlite]\npath = \":memory:\"\n",
        );
        assert_eq!(config_writable_dirs(&config, Path::new("/var/lib/ephpm")), []);
        assert_eq!(
            config_writable_dirs(&load_toml(""), Path::new("/var/lib/ephpm")),
            [],
            "no database, no TLS: nothing beyond the data/log dirs"
        );
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

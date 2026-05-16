//! Windows Service Control Manager (SCM) backend for the `ephpm` service.
//!
//! Uses the [`windows-service`] crate to register, control, and run as a
//! Windows service. The service is registered as auto-start under the name
//! `ephpm` with display name "ePHPm — Embedded PHP Manager".
//!
//! When the SCM launches the service binary it invokes `ephpm service-run`
//! (handled by [`run_as_service`]). For interactive lifecycle commands the
//! binary connects to the SCM directly via [`ServiceManager`].
//!
//! `unsafe` blocks are required for `is_elevated` (token query) and the
//! `define_windows_service!` macro-generated FFI shim. Each block carries a
//! `// SAFETY:` comment.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::{Paths, Result, ServiceError, StatusReport};

/// SCM service name.
const SERVICE_NAME: &str = "ephpm";
/// User-facing display name shown in services.msc.
const SERVICE_DISPLAY: &str = "ePHPm — Embedded PHP Manager";
/// SCM expects `OwnProcess` for stand-alone service binaries.
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Check whether the current process is running with Administrator privileges.
///
/// Returns `false` if the elevation query fails for any reason.
#[must_use]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: `OpenProcessToken` is a Win32 API that writes the token handle
    // through the out pointer. We pass a valid `HANDLE` storage location and
    // the documented `TOKEN_QUERY` access right.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) == 0 {
            return false;
        }
        #[repr(C)]
        struct Elevation {
            token_is_elevated: u32,
        }
        let mut elevation = Elevation { token_is_elevated: 0 };
        let mut ret_len: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            std::ptr::addr_of_mut!(elevation).cast(),
            u32::try_from(std::mem::size_of::<Elevation>()).unwrap_or(0),
            &raw mut ret_len,
        );
        CloseHandle(token);
        ok != 0 && elevation.token_is_elevated != 0
    }
}

pub(super) fn register(paths: &Paths) -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&OsStr>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(scm_err)?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: paths.binary.clone(),
        launch_arguments: vec![
            OsString::from("service-run"),
            OsString::from("--config"),
            OsString::from(paths.config.as_os_str()),
        ],
        dependencies: vec![],
        account_name: None, // run as LocalSystem
        account_password: None,
    };

    // CreateService will fail if a service of the same name already exists, in
    // which case we update its configuration in place via the existing handle.
    match manager.create_service(
        &info,
        ServiceAccess::QUERY_STATUS | ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
    ) {
        Ok(_) => {}
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_EXISTS) =>
        {
            // Already installed — open and update binary path / args.
            let svc = manager
                .open_service(
                    SERVICE_NAME,
                    ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_STATUS,
                )
                .map_err(scm_err)?;
            svc.change_config(&info).map_err(scm_err)?;
        }
        Err(e) => return Err(scm_err(e)),
    }

    add_to_system_path(paths)?;
    Ok(())
}

pub(super) fn deregister(paths: &Paths) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .map_err(scm_err)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS) {
        Ok(svc) => svc.delete().map_err(scm_err)?,
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {}
        Err(e) => return Err(scm_err(e)),
    }
    // Best-effort PATH cleanup — leave the SCM cleanup intact even if we can't
    // mutate the registry for some reason.
    if let Err(e) = remove_from_system_path(paths) {
        tracing::warn!(error = %e, "failed to remove install dir from system PATH");
    }
    Ok(())
}

pub(super) fn start(_paths: &Paths) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .map_err(scm_err)?;
    let svc = manager
        .open_service(SERVICE_NAME, ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .map_err(scm_err)?;
    let status = svc.query_status().map_err(scm_err)?;
    if !matches!(status.current_state, ServiceState::Stopped | ServiceState::StopPending) {
        return Ok(());
    }
    svc.start::<&OsStr>(&[]).map_err(scm_err)
}

pub(super) fn stop(_paths: &Paths) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .map_err(scm_err)?;
    let svc = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
        .map_err(scm_err)?;
    let status = svc.query_status().map_err(scm_err)?;
    match status.current_state {
        ServiceState::Stopped => return Ok(()),
        ServiceState::StopPending => {}
        _ => {
            svc.stop().map_err(scm_err)?;
        }
    }
    wait_for_state(&svc, ServiceState::Stopped, Duration::from_secs(30))
}

pub(super) fn restart(paths: &Paths) -> Result<()> {
    stop(paths)?;
    start(paths)
}

/// Poll `svc` until it reaches `target` state or `timeout` elapses. Returns an
/// error only on SCM query failure or timeout. Used by `stop` and `uninstall`
/// so callers can be sure the service has fully transitioned before they touch
/// the binary file on disk.
fn wait_for_state(
    svc: &windows_service::service::Service,
    target: ServiceState,
    timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let s = svc.query_status().map_err(scm_err)?;
        if s.current_state == target {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(ServiceError::command(
                "Windows SCM",
                format!("timed out waiting for service to reach {target:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn status(_paths: &Paths) -> Result<StatusReport> {
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .map_err(scm_err)?;
    let svc = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            return Err(ServiceError::NotInstalled);
        }
        Err(e) => return Err(scm_err(e)),
    };
    let s = svc.query_status().map_err(scm_err)?;
    let state = match s.current_state {
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "starting",
        ServiceState::StopPending => "stopping",
        ServiceState::Running => "running",
        ServiceState::ContinuePending => "continuing",
        ServiceState::PausePending => "pausing",
        ServiceState::Paused => "paused",
    };
    Ok(StatusReport { state: state.to_string(), pid: s.process_id, uptime: None })
}

pub(super) fn logs(paths: &Paths, follow: bool) -> Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

    if !paths.log_file.exists() {
        return Err(ServiceError::io(
            &paths.log_file,
            std::io::Error::new(std::io::ErrorKind::NotFound, "log file does not exist"),
        ));
    }
    let mut file =
        std::fs::File::open(&paths.log_file).map_err(|e| ServiceError::io(&paths.log_file, e))?;
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|e| ServiceError::io(&paths.log_file, e))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let tail: Vec<&str> = content.lines().rev().take(200).collect();
    for line in tail.into_iter().rev() {
        let _ = writeln!(out, "{line}");
    }
    if !follow {
        return Ok(());
    }
    let mut pos = file.seek(SeekFrom::End(0)).map_err(|e| ServiceError::io(&paths.log_file, e))?;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let len = std::fs::metadata(&paths.log_file)
            .map_err(|e| ServiceError::io(&paths.log_file, e))?
            .len();
        if len < pos {
            pos = 0;
            file = std::fs::File::open(&paths.log_file)
                .map_err(|e| ServiceError::io(&paths.log_file, e))?;
        }
        file.seek(SeekFrom::Start(pos)).map_err(|e| ServiceError::io(&paths.log_file, e))?;
        let mut chunk = String::new();
        let read =
            file.read_to_string(&mut chunk).map_err(|e| ServiceError::io(&paths.log_file, e))?;
        if read > 0 {
            let _ = out.write_all(chunk.as_bytes());
            let _ = out.flush();
            pos += read as u64;
        }
    }
}

/// Append the install directory to the system `Path` environment variable so
/// `ephpm` is callable from a fresh shell. Idempotent.
fn add_to_system_path(paths: &Paths) -> Result<()> {
    let Some(install_dir) = paths.binary.parent() else {
        return Ok(());
    };
    let install_str = install_dir.display().to_string();

    let key = "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment";
    let existing = query_registry_path(key).unwrap_or_default();

    let already_present = existing
        .split(';')
        .any(|p| Path::new(p.trim()).eq_ignore_ascii_case_path(Path::new(&install_str)));
    if already_present {
        return Ok(());
    }
    let new_value =
        if existing.is_empty() { install_str.clone() } else { format!("{existing};{install_str}") };

    let out = std::process::Command::new("reg")
        .args(["add", key, "/v", "Path", "/t", "REG_EXPAND_SZ", "/d", &new_value, "/f"])
        .output()
        .map_err(|e| ServiceError::command("reg add", e.to_string()))?;
    if !out.status.success() {
        return Err(ServiceError::command(
            "reg add",
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(())
}

/// Remove the install directory from the system `Path` environment variable.
/// Idempotent: a no-op when the entry is not present.
fn remove_from_system_path(paths: &Paths) -> Result<()> {
    let Some(install_dir) = paths.binary.parent() else {
        return Ok(());
    };
    let install_path = Path::new(install_dir);

    let key = "HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment";
    let Some(existing) = query_registry_path(key) else { return Ok(()) };

    let filtered: Vec<&str> = existing
        .split(';')
        .filter(|p| !Path::new(p.trim()).eq_ignore_ascii_case_path(install_path))
        .collect();
    if filtered.len() == existing.split(';').count() {
        // Install dir wasn't present — nothing to do.
        return Ok(());
    }
    let new_value = filtered.join(";");

    let out = std::process::Command::new("reg")
        .args(["add", key, "/v", "Path", "/t", "REG_EXPAND_SZ", "/d", &new_value, "/f"])
        .output()
        .map_err(|e| ServiceError::command("reg add", e.to_string()))?;
    if !out.status.success() {
        return Err(ServiceError::command(
            "reg add",
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(())
}

/// Read a `REG_SZ` / `REG_EXPAND_SZ` value named `Path` from `key` via the
/// `reg` CLI. Returns `None` when the key/value is missing or the shell-out
/// fails. Returns `Some("")` for an empty value.
///
/// `reg query` prints lines like `    Path    REG_EXPAND_SZ    <value>` with
/// multiple spaces between fields. Anything that simply splits on whitespace
/// chokes on paths containing spaces (e.g. `C:\Program Files\ephpm`), so we
/// strip the name + known type prefixes off the front instead.
fn query_registry_path(key: &str) -> Option<String> {
    let out = std::process::Command::new("reg").args(["query", key, "/v", "Path"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix("Path") else { continue };
        // Ensure "Path" was a whole word (next char is whitespace) so we don't
        // match value names like "PathExt".
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let after_name = rest.trim_start();
        for ty in ["REG_EXPAND_SZ", "REG_SZ", "REG_MULTI_SZ"] {
            if let Some(after_type) = after_name.strip_prefix(ty) {
                return Some(after_type.trim_end_matches(['\r', '\n']).trim().to_string());
            }
        }
    }
    None
}

/// Map `windows_service::Error` into our typed error.
fn scm_err(e: windows_service::Error) -> ServiceError {
    ServiceError::command("Windows SCM", e.to_string())
}

/// SCM error code constants we need to disambiguate "already exists" from real errors.
const ERROR_SERVICE_EXISTS: i32 = 1073;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

// ─────────────────────────────────────────────────────────────────────────────
// Service-main entry point (invoked by SCM when the service starts)
// ─────────────────────────────────────────────────────────────────────────────

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Service-main handler. Captures the config path from SCM-passed arguments
/// (or falls back to the default install location) and runs the normal HTTP
/// server loop.
fn service_main(arguments: Vec<OsString>) {
    if let Err(e) = service_main_inner(&arguments) {
        // We can't reliably stdout from a Windows service — at least record
        // the failure to the event log via tracing if a subscriber is set.
        tracing::error!(error = %e, "ephpm service failed");
    }
}

fn service_main_inner(
    arguments: &[OsString],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Default to the canonical install path if SCM did not pass --config.
    let paths = Paths::for_current_platform();
    let mut config_path: PathBuf = paths.config.clone();
    let mut iter = arguments.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == OsStr::new("--config") {
            if let Some(v) = iter.next() {
                config_path = PathBuf::from(v);
            }
        }
    }

    // SCM detaches stdout/stderr from the service process, so tracing output
    // would otherwise vanish. Make sure the log directory exists and tell
    // `run_serve_sync` (via env var) to route the main tracing layer to that
    // file instead of stderr. The Unix backends rely on systemd/launchd's
    // built-in stdout redirection so this only matters on Windows.
    if let Some(parent) = paths.log_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // SAFETY: `set_var` is sound here because no other threads have been
    // spawned yet — SCM has just invoked our service-main and tokio / tracing
    // initialization has not started. Setting the env var before any reader
    // races us is the documented safe pattern.
    unsafe {
        std::env::set_var("EPHPM_SERVICE_LOG_FILE", &paths.log_file);
    }

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            windows_service::service::ServiceControl::Stop
            | windows_service::service::ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            windows_service::service::ServiceControl::Interrogate => {
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: windows_service::service::ServiceControlAccept::STOP
            | windows_service::service::ServiceControlAccept::SHUTDOWN,
        exit_code: windows_service::service::ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Spawn the actual server on a worker thread so this one can wait on the
    // shutdown signal.
    let config_clone = config_path.clone();
    let server_handle = std::thread::spawn(move || -> anyhow::Result<()> {
        crate::run_serve_with_config(config_clone)
    });

    // Block until SCM asks us to stop.
    let _ = shutdown_rx.recv();

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::StopPending,
        controls_accepted: windows_service::service::ServiceControlAccept::empty(),
        exit_code: windows_service::service::ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })?;

    // The server doesn't have a graceful-shutdown signal we can poke from here
    // yet, so we exit the process. SCM will reap our worker threads.
    drop(server_handle);

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: windows_service::service::ServiceControlAccept::empty(),
        exit_code: windows_service::service::ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

/// Entry point for the hidden `service-run` subcommand. SCM invokes the binary
/// with this argument when it starts the service; we hand control to the
/// `windows_service` dispatcher which calls back into `service_main`.
///
/// # Errors
///
/// Returns an error if the SCM service dispatcher fails to register.
pub fn run_as_service() -> std::result::Result<(), Box<dyn std::error::Error>> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

/// Tiny case-insensitive path comparison helper. Implemented as a private
/// extension trait so the call site reads naturally.
trait PathCaseExt {
    fn eq_ignore_ascii_case_path(&self, other: &Path) -> bool;
}

impl PathCaseExt for Path {
    fn eq_ignore_ascii_case_path(&self, other: &Path) -> bool {
        let a = self.to_string_lossy();
        let b = other.to_string_lossy();
        a.eq_ignore_ascii_case(&b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_constants_are_known_values() {
        // Sanity check — these are documented SCM error codes; if they change
        // the compile-time `const` must be updated.
        assert_eq!(ERROR_SERVICE_EXISTS, 1073_i32);
        assert_eq!(ERROR_SERVICE_DOES_NOT_EXIST, 1060_i32);
    }
}

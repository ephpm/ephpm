//! Internal loopback KV endpoint for the `ephpm php --site` CLI wire.
//!
//! The public RESP listener (`[kv.redis_compat]`) is **off by default**, so the
//! co-located CLI cannot rely on it to reach the server's KV. This module binds
//! a second, always-available RESP listener on an ephemeral **loopback** port
//! whenever the deployment is multi-tenant with `[kv] secret` set — exactly the
//! condition under which the DB CLI wire works, and the condition under which
//! per-site HMAC `AUTH` can scope a connection to one vhost's store.
//!
//! It reuses [`ephpm_kv::server::serve_on`] and the same `MultiTenantStore` the
//! router and the public listener use, so a CLI `AUTH <site> <derived>` lands
//! in the identical `Arc<Store>` that PHP's `ephpm_kv_*` write through for that
//! vhost, and so a CLI write replicates across the cluster through the server's
//! own store with no new code.
//!
//! # Discovery
//!
//! The port is ephemeral, so the server writes a small runtime file recording
//! it; the CLI, which has the same `--config`, derives the same path and reads
//! it. The path is keyed on the canonical `sites_dir` (both processes compute
//! it identically from config) and lives in a **per-user, `0700` runtime
//! directory** — `$XDG_RUNTIME_DIR/ephpm` when the service manager provides one
//! (a tmpfs owned by this uid), else an `ephpm-run-<uid>` directory the server
//! creates and owns under the system temp dir. The file itself is created with
//! `O_EXCL` + `0600` (the open never follows a symlink and there is no
//! post-write `chmod` window), and the reader accepts it only when it is a
//! regular file owned by the current user with no group/other permission bits.
//! The CLI additionally probes the port before using it, so a stale file left
//! by a crashed server is ignored.
//!
//! # Security
//!
//! Loopback-only; per-site HMAC `AUTH` in multi-tenant mode, identical to the
//! public listener. The trust boundary is the one `ephpm exec` and the DB wire
//! already document: with `[kv] secret` set, whoever can read the config can
//! derive any tenant's password and impersonate that tenant — every per-site
//! password is a pure function of the secret and the public site name.
//!
//! What the discovery file must **not** become is a second, weaker path to that
//! boundary. It records the port the CLI sends its per-site `AUTH <site>
//! <hmac>` to, so a world-writable `/tmp/ephpm-kv-endpoint-*.json` that any
//! local user could pre-create (pointing at a listener they control) would let
//! someone who *cannot* read the config harvest a tenant's derived password
//! from the next `ephpm php --site` run. The private runtime directory, the
//! `O_EXCL` create and the owner/mode check on read together keep the file
//! writable and trustable only by the uid that runs the server and the CLI.

use std::hash::{Hash as _, Hasher as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ephpm_config::Config;
use serde::{Deserialize, Serialize};

/// Maximum concurrent connections to the internal endpoint. Modest: the only
/// clients are co-located CLI/cron invocations, one connection each.
const INTERNAL_MAX_CONNECTIONS: usize = 128;

/// Largest single RESP input the internal endpoint accepts. Deliberately
/// independent of (and far above) `[kv.redis_compat] max_input_buffer`: the
/// in-process `ephpm_kv_*` path a request uses has no per-value cap, so a CLI
/// cache-warmer writing a rendered page must not be cut off at the public
/// listener's 1 MiB. Still bounds a runaway (a CLI is trusted but may be buggy).
const INTERNAL_MAX_INPUT_BUFFER: usize = 64 * 1024 * 1024;

/// Idle-close timeout for the internal endpoint: **off**. The public listener
/// closes idle sockets after `[kv.redis_compat] idle_timeout_secs` (default
/// 300), but a CLI/cron client legitimately sits between KV calls for minutes
/// while it does DB or compute work; closing its connection mid-script makes the
/// next `ephpm_kv_*` call fail once (then redial) — a silent one-shot failure on
/// what is often a lock release. The ≤128 co-located clients are cheap to keep.
const INTERNAL_IDLE_TIMEOUT_SECS: u64 = 0;

/// The runtime discovery file's contents.
#[derive(Debug, Serialize, Deserialize)]
struct EndpointFile {
    /// Loopback TCP port the internal RESP listener is bound to.
    port: u16,
    /// PID of the writing server (diagnostics only; staleness is decided by
    /// the CLI's connect probe, not this).
    pid: u32,
}

/// The deterministic discovery-file path for `config`, or `None` when the
/// deployment is not multi-tenant (single-site has no per-site identity, and
/// the CLI wire is not offered there).
///
/// Keyed on the canonical `sites_dir`: both `ephpm serve` and
/// `ephpm php --site … --config` compute it from the same config, so they agree
/// without sharing any other state.
#[must_use]
pub fn endpoint_path(config: &Config) -> Option<PathBuf> {
    let sites_dir = config.server.sites_dir.as_ref()?;
    let canonical = std::fs::canonicalize(sites_dir).unwrap_or_else(|_| sites_dir.clone());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.to_string_lossy().hash(&mut hasher);
    let digest = hasher.finish();
    Some(runtime_base_dir().join(format!("ephpm-kv-endpoint-{digest:016x}.json")))
}

/// The per-user base directory for ePHPm runtime files. `$XDG_RUNTIME_DIR`
/// (a `0700` tmpfs owned by this uid — no shared-`/tmp` race) when the service
/// manager provides one, else a `0700` `ephpm-run-<uid>` under the system temp
/// directory that the server creates and owns. The CLI and the server resolve
/// this identically because they run as the same uid with the same environment;
/// keeping the port/pid file out of world-writable `/tmp` is what stops another
/// local user planting a file that redirects the CLI's per-site `AUTH`.
fn runtime_base_dir() -> PathBuf {
    #[cfg(unix)]
    {
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("ephpm");
            }
        }
        // SAFETY: `geteuid` takes no arguments and is always safe to call.
        let uid = unsafe { libc::geteuid() };
        std::env::temp_dir().join(format!("ephpm-run-{uid}"))
    }
    #[cfg(not(unix))]
    {
        // Windows' per-user temp dir is already not world-writable; scope a
        // subdirectory under it for tidiness and parity.
        std::env::temp_dir().join("ephpm-run")
    }
}

/// Read the internal endpoint's loopback address for `config`, or `None` when
/// no discovery file exists or it cannot be parsed. Does **not** probe the
/// port — the caller decides reachability.
#[must_use]
pub fn read_endpoint(config: &Config) -> Option<SocketAddr> {
    let path = endpoint_path(config)?;
    let mut file = open_trusted(&path)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).ok()?;
    let parsed: EndpointFile = serde_json::from_slice(&bytes).ok()?;
    Some(SocketAddr::from(([127, 0, 0, 1], parsed.port)))
}

/// Open the discovery file, accepting it only when it is a regular file owned
/// by the current user with no group/other permission bits — a file another
/// user planted (or a symlink) is refused, so a redirected port is never
/// trusted. The checks run on the opened descriptor (`fstat`), not a second
/// path lookup, so there is no re-stat TOCTOU. On non-Unix the per-user temp
/// dir is already the isolation boundary, so a plain open is used.
fn open_trusted(path: &std::path::Path) -> Option<std::fs::File> {
    let file = std::fs::File::open(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = file.metadata().ok()?;
        // SAFETY: `geteuid` takes no arguments and is always safe to call.
        let euid = unsafe { libc::geteuid() };
        if !meta.is_file() || meta.uid() != euid || meta.mode() & 0o077 != 0 {
            return None;
        }
    }
    Some(file)
}

/// A running internal KV endpoint. Dropping it removes the discovery file and
/// aborts the accept task (the process is usually exiting anyway).
#[derive(Debug)]
pub struct InternalKvEndpoint {
    path: PathBuf,
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl InternalKvEndpoint {
    /// The loopback port the endpoint is listening on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for InternalKvEndpoint {
    fn drop(&mut self) {
        self.task.abort();
        // Remove the discovery file only if it still records our PID. During an
        // overlapping restart the new server may have already replaced it with
        // its own; deleting that would silently disable the wire until the next
        // restart. A missing/unreadable file is left alone (harmless — the CLI
        // probes the port anyway).
        if endpoint_file_pid(&self.path) == Some(std::process::id()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The PID recorded in the discovery file at `path`, or `None` if it is absent
/// or unreadable. Used only by [`InternalKvEndpoint::drop`] to avoid deleting a
/// successor server's file.
fn endpoint_file_pid(path: &std::path::Path) -> Option<u32> {
    let bytes = std::fs::read(path).ok()?;
    let file: EndpointFile = serde_json::from_slice(&bytes).ok()?;
    Some(file.pid)
}

/// Start the internal loopback KV endpoint when the deployment qualifies
/// (multi-tenant + `[kv] secret`), returning a guard that keeps it alive and
/// cleans up the discovery file on drop.
///
/// Returns `Ok(None)` when the deployment does not qualify (single-site, or no
/// secret) — not an error, just "the CLI wire is not offered here". A bind or
/// file-write failure is logged and also yields `None` rather than failing
/// server startup: the CLI wire is a convenience, and the server must still
/// come up without it.
///
/// Must be called from within the tokio runtime (it spawns the accept task).
#[must_use]
pub fn start(
    config: &Config,
    store: &Arc<ephpm_kv::store::Store>,
    multi_tenant: Option<&ephpm_kv::multi_tenant::MultiTenantStore>,
) -> Option<InternalKvEndpoint> {
    // Mirror the DB CLI wire's bar: multi-tenant + secret. Single-site has no
    // per-site identity to scope AUTH to, and without a secret a loopback KV
    // listener could not authenticate anyone per site.
    if config.server.sites_dir.is_none() || !config.kv.secret_is_set() {
        return None;
    }
    let (Some(multi_tenant), Some(path)) = (multi_tenant, endpoint_path(config)) else {
        return None;
    };

    // Bind an ephemeral loopback port with std, then hand the socket to tokio.
    let std_listener = match std::net::TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "internal KV endpoint: bind failed; CLI KV wire disabled");
            return None;
        }
    };
    let port = match std_listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            tracing::warn!(error = %e, "internal KV endpoint: local_addr failed");
            return None;
        }
    };
    if let Err(e) = std_listener.set_nonblocking(true) {
        tracing::warn!(error = %e, "internal KV endpoint: set_nonblocking failed");
        return None;
    }
    let listener = match tokio::net::TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "internal KV endpoint: from_std failed");
            return None;
        }
    };

    if let Err(e) = write_endpoint_file(&path, port) {
        tracing::warn!(error = %e, path = %path.display(),
            "internal KV endpoint: could not write discovery file; CLI KV wire disabled");
        return None;
    }

    let server_config = ephpm_kv::server::ServerConfig {
        // `listen` is unused — the socket is already bound; kept for clarity.
        listen: format!("127.0.0.1:{port}"),
        password: None,
        secret: config.kv.secret.clone(),
        max_connections: INTERNAL_MAX_CONNECTIONS,
        max_input_buffer: INTERNAL_MAX_INPUT_BUFFER,
        idle_timeout_secs: INTERNAL_IDLE_TIMEOUT_SECS,
    };
    let store = Arc::clone(store);
    let multi_tenant = multi_tenant.clone();
    let task = tokio::spawn(async move {
        if let Err(e) =
            ephpm_kv::server::serve_on(store, listener, server_config, Some(multi_tenant)).await
        {
            tracing::error!(error = %e, "internal KV endpoint accept loop failed");
        }
    });

    tracing::info!(
        port,
        path = %path.display(),
        "internal KV endpoint listening on loopback for `ephpm php --site` (CLI KV wire)"
    );
    Some(InternalKvEndpoint { path, port, task })
}

/// Write the discovery file into the private runtime directory. The directory
/// is ensured to be a `0700` directory this user owns; the file is then created
/// with `O_EXCL` + `0600`, so the open never follows a symlink an attacker
/// planted and there is no window where the file exists with wider permissions.
/// fsync is unnecessary: the CLI probes the port, so a torn file just reads as
/// absent.
fn write_endpoint_file(path: &std::path::Path, port: u16) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let file = EndpointFile { port, pid: std::process::id() };
    let json = serde_json::to_vec(&file).map_err(std::io::Error::other)?;

    // Replace our own stale file, then create fresh with O_EXCL. In the private
    // 0700 directory the only possible pre-existing entry is one we wrote.
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut handle = opts.open(path)?;
    std::io::Write::write_all(&mut handle, &json)?;
    Ok(())
}

/// Ensure `dir` is a private (`0700`) directory owned by the current user,
/// creating it if absent. An existing entry is accepted only when it is a real
/// directory (not a symlink) owned by us; anything else — including a directory
/// another user planted — is refused, so the discovery file can never be
/// written through an attacker-controlled path. `/tmp`'s sticky bit prevents a
/// different user from removing or swapping a directory we already own.
#[cfg(unix)]
fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // `symlink_metadata` does not follow a final symlink.
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: `geteuid` takes no arguments and is always safe to call.
    let euid = unsafe { libc::geteuid() };
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("runtime directory {} is not a directory", dir.display()),
        ));
    }
    if meta.uid() != euid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("runtime directory {} is not owned by this user", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        // We own it, so tighten it; if that fails, refuse rather than write into
        // a group/other-accessible directory.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multi_site_config(sites_dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.server.sites_dir = Some(sites_dir.to_path_buf());
        config.kv.secret = Some("s3cr3t".into());
        config
    }

    #[test]
    fn endpoint_path_is_none_without_sites_dir() {
        let config = Config::default();
        assert!(endpoint_path(&config).is_none(), "single-site has no CLI KV wire");
    }

    #[test]
    fn endpoint_path_is_deterministic_for_the_same_sites_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config = multi_site_config(tmp.path());
        let a = endpoint_path(&config).expect("path");
        let b = endpoint_path(&config).expect("path");
        assert_eq!(a, b, "the same config must resolve to the same discovery path");
        assert!(a.to_string_lossy().contains("ephpm-kv-endpoint-"));
    }

    #[test]
    fn different_sites_dirs_get_different_paths() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let pa = endpoint_path(&multi_site_config(a.path())).unwrap();
        let pb = endpoint_path(&multi_site_config(b.path())).unwrap();
        assert_ne!(pa, pb, "distinct deployments must not collide on one discovery file");
    }

    #[test]
    fn write_then_read_round_trips_the_port() {
        let tmp = tempfile::tempdir().unwrap();
        let config = multi_site_config(tmp.path());
        let path = endpoint_path(&config).unwrap();
        // Clean any stale file from a previous run of this test.
        let _ = std::fs::remove_file(&path);
        write_endpoint_file(&path, 54321).expect("write");
        let addr = read_endpoint(&config).expect("read back");
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 54321)));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_endpoint_is_none_when_no_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let config = multi_site_config(tmp.path());
        let path = endpoint_path(&config).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(read_endpoint(&config).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn write_creates_a_private_runtime_dir_and_0600_file() {
        use std::os::unix::fs::MetadataExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let config = multi_site_config(tmp.path());
        let path = endpoint_path(&config).unwrap();
        let _ = std::fs::remove_file(&path);
        write_endpoint_file(&path, 5555).expect("write");

        let dir = path.parent().unwrap();
        let dmeta = std::fs::symlink_metadata(dir).unwrap();
        assert!(dmeta.is_dir(), "runtime path's parent must be a real directory");
        assert_eq!(dmeta.mode() & 0o777, 0o700, "runtime dir must be 0700, not world-writable");

        let fmeta = std::fs::metadata(&path).unwrap();
        assert_eq!(fmeta.mode() & 0o777, 0o600, "discovery file must be 0600");
        std::fs::remove_file(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_endpoint_refuses_a_group_or_other_accessible_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let config = multi_site_config(tmp.path());
        let path = endpoint_path(&config).unwrap();
        let _ = std::fs::remove_file(&path);
        write_endpoint_file(&path, 4444).expect("write");
        assert!(read_endpoint(&config).is_some(), "the 0600 file we wrote reads back");

        // Loosen the mode the way a mistake — or a hostile `/tmp` — would.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_endpoint(&config).is_none(),
            "a group/other-accessible discovery file must be refused, not trusted"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

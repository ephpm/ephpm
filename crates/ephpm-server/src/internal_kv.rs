//! Internal loopback KV endpoint for the `ephpm php --site` CLI wire (#700).
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
//! it identically from config), lives in the OS temp directory, and is created
//! `0600` on Unix. The CLI never trusts the file blindly — it probes the port
//! before using it, so a stale file left by a crashed server is simply ignored.
//!
//! # Security
//!
//! Loopback-only; per-site HMAC `AUTH` in multi-tenant mode, identical to the
//! public listener. The trust boundary is the one `ephpm exec` and the DB wire
//! already document: with `[kv] secret` set, whoever can read the config can
//! derive any tenant's password and impersonate that tenant — every per-site
//! password is a pure function of the secret and the public site name.

use std::hash::{Hash as _, Hasher as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ephpm_config::Config;
use serde::{Deserialize, Serialize};

/// Maximum concurrent connections to the internal endpoint. Modest: the only
/// clients are co-located CLI/cron invocations, one connection each.
const INTERNAL_MAX_CONNECTIONS: usize = 128;

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
    Some(std::env::temp_dir().join(format!("ephpm-kv-endpoint-{digest:016x}.json")))
}

/// Read the internal endpoint's loopback address for `config`, or `None` when
/// no discovery file exists or it cannot be parsed. Does **not** probe the
/// port — the caller decides reachability.
#[must_use]
pub fn read_endpoint(config: &Config) -> Option<SocketAddr> {
    let path = endpoint_path(config)?;
    let bytes = std::fs::read(&path).ok()?;
    let file: EndpointFile = serde_json::from_slice(&bytes).ok()?;
    Some(SocketAddr::from(([127, 0, 0, 1], file.port)))
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
        let _ = std::fs::remove_file(&self.path);
    }
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
        max_input_buffer: config.kv.redis_compat.max_input_buffer,
        idle_timeout_secs: config.kv.redis_compat.idle_timeout_secs,
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

/// Write the discovery file atomically-ish (truncate + write + fsync is
/// unnecessary here: the CLI probes the port, so a torn file just reads as
/// absent). Sets `0600` on Unix so the port/pid are not world-readable.
fn write_endpoint_file(path: &std::path::Path, port: u16) -> std::io::Result<()> {
    let file = EndpointFile { port, pid: std::process::id() };
    let json = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
    std::fs::write(path, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
}

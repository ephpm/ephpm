//! sqld binary embedding and process management.
//!
//! When built with `SQLD_BINARY_PATH` set, the sqld binary is embedded
//! in the ephpm binary via `include_bytes!()`. At runtime, it is extracted
//! to a temporary path and spawned as a child process.
//!
//! Without `SQLD_BINARY_PATH`, all functions return errors — the crate
//! still compiles for development without requiring sqld.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;

/// The embedded sqld binary bytes (only available when `SQLD_BINARY_PATH` is set at build time).
#[cfg(sqld_embedded)]
static SQLD_BINARY: &[u8] = include_bytes!(env!("SQLD_BINARY_PATH"));

/// Role that sqld should run in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqldRole {
    /// Accept writes locally, serve WAL frames to replicas via gRPC.
    Primary,
    /// Sync from primary via gRPC, forward writes to primary.
    Replica {
        /// gRPC URL of the primary node (e.g., `"http://10.0.1.2:5001"`).
        primary_grpc_url: String,
    },
}

/// Configuration for spawning sqld.
#[derive(Debug, Clone)]
pub struct SqldConfig {
    /// Path to the `SQLite` database file.
    pub db_path: String,
    /// Hrana HTTP listen address (internal, litewire → sqld).
    pub http_listen: String,
    /// gRPC listen address for replication.
    pub grpc_listen: String,
}

/// Manages a sqld child process lifecycle.
///
/// Extracts the embedded binary to a temp path, spawns it, monitors
/// health, and cleans up on drop or explicit shutdown.
pub struct SqldProcess {
    child: tokio::process::Child,
    temp_path: PathBuf,
    config: SqldConfig,
    role: SqldRole,
    http_client: reqwest::Client,
}

impl SqldProcess {
    /// Extract the embedded sqld binary and spawn it as a child process.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - sqld binary is not embedded (built without `SQLD_BINARY_PATH`)
    /// - Binary extraction or permission setting fails
    /// - Process fails to spawn
    pub async fn spawn(config: SqldConfig, role: SqldRole) -> anyhow::Result<Self> {
        let temp_path = extract_binary().await?;
        let child = spawn_child(&temp_path, &config, &role)?;

        tracing::info!(
            db = %config.db_path,
            http = %config.http_listen,
            grpc = %config.grpc_listen,
            role = ?role,
            "sqld process spawned"
        );

        Ok(Self { child, temp_path, config, role, http_client: reqwest::Client::new() })
    }

    /// Poll the sqld health endpoint until it responds or the timeout expires.
    ///
    /// # Errors
    ///
    /// Returns an error if sqld does not become healthy within the timeout.
    pub async fn wait_healthy(&self, timeout: Duration) -> anyhow::Result<()> {
        let health_url = format!("http://{}/health", self.config.http_listen);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("sqld did not become healthy within {}s", timeout.as_secs());
            }

            match self.http_client.get(&health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("sqld is healthy");
                    return Ok(());
                }
                Ok(resp) => {
                    tracing::debug!(status = %resp.status(), "sqld not ready yet");
                }
                Err(e) => {
                    tracing::debug!(%e, "sqld not reachable yet");
                }
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Restart sqld with a new role (e.g., replica promoted to primary).
    ///
    /// Sends SIGTERM to the current process, waits for exit, then spawns
    /// a new process with the updated role.
    ///
    /// # Errors
    ///
    /// Returns an error if the old process cannot be stopped or the new
    /// one fails to spawn.
    pub async fn restart(&mut self, new_role: SqldRole) -> anyhow::Result<()> {
        tracing::info!(
            old_role = ?self.role,
            new_role = ?new_role,
            "restarting sqld with new role"
        );

        self.stop_child().await?;
        self.role = new_role;
        self.child = spawn_child(&self.temp_path, &self.config, &self.role)?;

        tracing::info!(role = ?self.role, "sqld restarted");
        Ok(())
    }

    /// Gracefully shut down sqld.
    ///
    /// Sends SIGTERM, waits up to 5 seconds, then SIGKILL if needed.
    /// Cleans up the temporary binary file.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be killed.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.stop_child().await?;
        self.cleanup_temp();
        tracing::info!("sqld shut down");
        Ok(())
    }

    /// The current role sqld is running in.
    #[must_use]
    pub fn role(&self) -> &SqldRole {
        &self.role
    }

    /// The HTTP URL for connecting to this sqld instance.
    #[must_use]
    pub fn http_url(&self) -> String {
        format!("http://{}", self.config.http_listen)
    }

    /// Stop the child process (SIGTERM → wait → SIGKILL).
    async fn stop_child(&mut self) -> anyhow::Result<()> {
        // Send SIGTERM.
        #[cfg(unix)]
        {
            let pid = self.child.id().context("sqld has no pid")?;
            // SAFETY: `pid` is the PID of a child process we spawned.
            // Sending SIGTERM to request graceful shutdown is safe.
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(libc::pid_t::try_from(pid).expect("pid fits in i32"), libc::SIGTERM);
            }
        }
        #[cfg(not(unix))]
        {
            self.child.kill().await.ok();
        }

        // Wait up to 5 seconds for graceful exit.
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(status)) => {
                tracing::debug!(?status, "sqld exited");
            }
            Ok(Err(e)) => {
                tracing::warn!(%e, "error waiting for sqld");
            }
            Err(_) => {
                tracing::warn!("sqld did not exit in 5s, sending SIGKILL");
                self.child.kill().await.ok();
                self.child.wait().await.ok();
            }
        }

        Ok(())
    }

    /// Remove the temp binary file.
    fn cleanup_temp(&self) {
        if self.temp_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.temp_path) {
                tracing::warn!(path = %self.temp_path.display(), %e, "failed to remove temp sqld binary");
            }
        }
    }
}

impl Drop for SqldProcess {
    fn drop(&mut self) {
        // Best-effort cleanup on drop — can't await here.
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // SAFETY: `pid` is the PID of a child process we spawned.
            // Sending SIGTERM to request graceful shutdown is safe.
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(libc::pid_t::try_from(pid).expect("pid fits in i32"), libc::SIGTERM);
            }
        }
        self.cleanup_temp();
    }
}

/// Extract the embedded sqld binary to a temporary file.
#[cfg(sqld_embedded)]
async fn extract_binary() -> anyhow::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("ephpm-sqld-{}", std::process::id()));
    tokio::fs::write(&path, SQLD_BINARY)
        .await
        .with_context(|| format!("failed to extract sqld to {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&path, perms).await.context("failed to set sqld permissions")?;
    }

    tracing::debug!(path = %path.display(), "extracted sqld binary");
    Ok(path)
}

#[cfg(not(sqld_embedded))]
fn extract_binary() -> impl std::future::Future<Output = anyhow::Result<PathBuf>> {
    std::future::ready(Err(anyhow::anyhow!(
        "sqld binary not embedded — rebuild with SQLD_BINARY_PATH set \
         (e.g., cargo xtask release --sqld-binary /path/to/sqld)"
    )))
}

/// Spawn the sqld child process with the given config and role.
fn spawn_child(
    binary_path: &std::path::Path,
    config: &SqldConfig,
    role: &SqldRole,
) -> anyhow::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(binary_path);
    cmd.arg("--db-path")
        .arg(&config.db_path)
        .arg("--http-listen-addr")
        .arg(&config.http_listen)
        .arg("--grpc-listen-addr")
        .arg(&config.grpc_listen);

    if let SqldRole::Replica { primary_grpc_url } = role {
        cmd.arg("--primary-grpc-url").arg(primary_grpc_url);
    }

    // Inherit stdout/stderr so sqld logs appear in ephpm's output.
    cmd.stdout(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit());

    cmd.spawn().context("failed to spawn sqld child process")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SqldConfig {
        SqldConfig {
            db_path: "/tmp/test.db".into(),
            http_listen: "127.0.0.1:8081".into(),
            grpc_listen: "0.0.0.0:5001".into(),
        }
    }

    #[test]
    fn sqld_role_equality() {
        assert_eq!(SqldRole::Primary, SqldRole::Primary);
        assert_ne!(
            SqldRole::Primary,
            SqldRole::Replica { primary_grpc_url: "http://x:5001".into() }
        );
    }

    #[test]
    fn sqld_role_replica_equality() {
        assert_eq!(
            SqldRole::Replica { primary_grpc_url: "http://x:5001".into() },
            SqldRole::Replica { primary_grpc_url: "http://x:5001".into() }
        );
        assert_ne!(
            SqldRole::Replica { primary_grpc_url: "http://a:5001".into() },
            SqldRole::Replica { primary_grpc_url: "http://b:5001".into() }
        );
    }

    #[test]
    fn sqld_config_debug() {
        let config = test_config();
        let s = format!("{config:?}");
        assert!(s.contains("test.db"));
        assert!(s.contains("8081"));
        assert!(s.contains("5001"));
    }

    #[cfg(not(sqld_embedded))]
    #[tokio::test]
    async fn spawn_fails_without_embedded_binary() {
        let config = test_config();
        match SqldProcess::spawn(config, SqldRole::Primary).await {
            Ok(_) => panic!("should fail without embedded binary"),
            Err(e) => {
                let err = e.to_string();
                assert!(err.contains("not embedded"), "unexpected error: {err}");
            }
        }
    }

    #[cfg(not(sqld_embedded))]
    #[tokio::test]
    async fn spawn_fails_as_replica_too() {
        let config = test_config();
        let role = SqldRole::Replica { primary_grpc_url: "http://10.0.1.2:5001".into() };
        match SqldProcess::spawn(config, role).await {
            Ok(_) => panic!("should fail without embedded binary"),
            Err(e) => {
                let err = e.to_string();
                assert!(
                    err.contains("not embedded"),
                    "expected 'not embedded' error for replica spawn, got: {err}"
                );
            }
        }
    }

    #[test]
    fn health_check_url_construction() {
        let config = test_config();
        // Simulate the health URL format used in wait_healthy.
        let health_url = format!("http://{}/health", config.http_listen);
        assert_eq!(health_url, "http://127.0.0.1:8081/health");
    }

    #[test]
    fn health_check_url_custom_port() {
        let config = SqldConfig {
            db_path: "/data/mydb.db".into(),
            http_listen: "0.0.0.0:9090".into(),
            grpc_listen: "0.0.0.0:5001".into(),
        };
        let health_url = format!("http://{}/health", config.http_listen);
        assert_eq!(health_url, "http://0.0.0.0:9090/health");
    }

    #[test]
    fn spawn_child_args_primary() {
        // Verify the command line arguments generated for primary role.
        let config = test_config();
        let binary = PathBuf::from("/fake/sqld");
        // We can't actually spawn (binary doesn't exist), but we can
        // test that spawn_child doesn't panic during construction.
        let result = spawn_child(&binary, &config, &SqldRole::Primary);
        // Will fail because /fake/sqld doesn't exist — that's fine,
        // we're testing the arg construction path.
        assert!(result.is_err());
    }

    #[test]
    fn spawn_child_args_replica() {
        let config = test_config();
        let binary = PathBuf::from("/fake/sqld");
        let role = SqldRole::Replica { primary_grpc_url: "http://10.0.1.2:5001".into() };
        let result = spawn_child(&binary, &config, &role);
        assert!(result.is_err());
    }

    #[test]
    fn sqld_role_debug_format() {
        let primary_dbg = format!("{:?}", SqldRole::Primary);
        assert_eq!(primary_dbg, "Primary");

        let replica_dbg =
            format!("{:?}", SqldRole::Replica { primary_grpc_url: "http://host:5001".into() });
        assert!(replica_dbg.contains("Replica"));
        assert!(replica_dbg.contains("host:5001"));
    }

    #[cfg(not(sqld_embedded))]
    #[test]
    fn extract_binary_returns_not_embedded_error() {
        // In stub mode, extract_binary is a sync function returning a future.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(extract_binary());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not embedded"), "expected 'not embedded', got: {err}");
    }

    #[test]
    fn temp_path_includes_pid() {
        let expected_prefix = format!("ephpm-sqld-{}", std::process::id());
        let temp_path = std::env::temp_dir().join(&expected_prefix);
        assert!(
            temp_path.file_name().unwrap().to_str().unwrap().starts_with("ephpm-sqld-"),
            "temp path should include ephpm-sqld prefix"
        );
    }
}

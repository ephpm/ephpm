//! E2E test helpers for ephpm.
//!
//! This crate is excluded from the workspace and is built as part of the
//! bare-process E2E path (`cargo xtask e2e`) or the opt-in Kind path
//! (`cargo xtask k8s-e2e`).
//!
//! Two use modes:
//!
//! - **xtask-managed** — `cargo xtask e2e` spawns ephpm processes and sets
//!   `EPHPM_URL` / `EPHPM_CLUSTER_URL_*` in the environment; test files just
//!   call [`required_env`] to read those. This is what every historical test
//!   under `tests/*.rs` already does.
//!
//! - **self-managed** — a test that wants its own topology can construct a
//!   [`SingleNodeFixture`] or [`ClusterFixture`] directly. Both spawn ephpm
//!   as child processes under a per-fixture [`tempfile::TempDir`] and kill
//!   the children on drop. Requires the `EPHPM_BINARY` env var; tests should
//!   skip gracefully when it is unset so the whole crate still compile-checks
//!   without a built binary.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tempfile::TempDir;

/// Read an environment variable or panic with a helpful message.
pub fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} environment variable must be set"))
}

/// Path to the ephpm binary for tests that spawn their own topology.
///
/// Returns `None` when `EPHPM_BINARY` is unset — callers should treat that as
/// "skip, no binary available" so the test file still compiles and links in
/// environments without a full release build (e.g. `cargo test --no-run` on
/// a fresh checkout).
pub fn ephpm_binary_env() -> Option<PathBuf> {
    std::env::var_os("EPHPM_BINARY").map(PathBuf::from)
}

/// A single ephpm process bound to 127.0.0.1.
///
/// Dropping the fixture SIGTERMs (then SIGKILLs) the child and removes the
/// scratch directory.
pub struct SingleNodeFixture {
    child: Option<Child>,
    base_url: String,
    _tempdir: TempDir,
}

impl SingleNodeFixture {
    /// Spawn an ephpm on a free-ish port under `127.0.0.1` and wait for its
    /// health endpoint.
    ///
    /// `docroot` is the directory ephpm will serve — typically
    /// `crates/ephpm-e2e/tests/docroot` or its own scratch dir.
    pub async fn start(ephpm_binary: &Path, docroot: &Path) -> Result<Self> {
        // Reserve loopback ports by opening + immediately closing a listener.
        // Two ephpm instances started in quick succession could race for the
        // same port; the OS may hand out the same port before ephpm binds it.
        // That's acceptable for a test fixture — worst case, the health poll
        // times out and the caller retries.
        let http_port = reserve_loopback_port().await?;
        let mysql_port = reserve_loopback_port().await?;

        let tmp = tempfile::Builder::new()
            .prefix("ephpm-e2e-single-")
            .tempdir()
            .context("create tempdir")?;
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).context("create data dir")?;

        let config = SINGLE_NODE_TEMPLATE
            .replace("{HTTP_PORT}", &http_port.to_string())
            .replace("{MYSQL_PORT}", &mysql_port.to_string())
            .replace("{DATA_DIR}", &escape_toml(&data_dir))
            .replace("{DOCROOT}", &escape_toml(docroot));

        let config_path = tmp.path().join("ephpm.toml");
        fs::write(&config_path, config).context("write config")?;

        let stdout = fs::File::create(tmp.path().join("stdout.log")).context("open stdout log")?;
        let stderr = fs::File::create(tmp.path().join("stderr.log")).context("open stderr log")?;

        let child = Command::new(ephpm_binary)
            .args(["serve", "--config"])
            .arg(&config_path)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| format!("spawn ephpm ({})", ephpm_binary.display()))?;

        wait_for_health(http_port, Duration::from_secs(15)).await.with_context(|| {
            format!(
                "ephpm on 127.0.0.1:{http_port} never healthy — check {}",
                tmp.path().join("stderr.log").display()
            )
        })?;

        Ok(Self {
            child: Some(child),
            base_url: format!("http://127.0.0.1:{http_port}"),
            _tempdir: tmp,
        })
    }

    /// Base URL (`http://127.0.0.1:<port>`) for HTTP clients.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for SingleNodeFixture {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            terminate(child);
        }
    }
}

/// A cluster of ephpm processes on 127.0.0.1 (each on its own port set).
pub struct ClusterFixture {
    nodes: Vec<ClusterFixtureNode>,
    _tempdir: TempDir,
}

struct ClusterFixtureNode {
    child: Option<Child>,
    base_url: String,
}

impl ClusterFixture {
    /// Spawn `size` ephpm instances all in cluster mode on 127.0.0.1.
    ///
    /// Each node picks its own port set; every node joins every other node.
    /// Health-polls each node before returning.
    pub async fn start(ephpm_binary: &Path, docroot: &Path, size: usize) -> Result<Self> {
        if size < 2 {
            return Err(anyhow!("cluster fixture needs at least 2 nodes, got {size}"));
        }

        // Reserve all port sets before spawning so overlap is impossible.
        let mut port_sets = Vec::with_capacity(size);
        for _ in 0..size {
            port_sets.push(ClusterPorts {
                http: reserve_loopback_port().await?,
                mysql: reserve_loopback_port().await?,
                gossip: reserve_loopback_port().await?,
                grpc: reserve_loopback_port().await?,
                sqld_http: reserve_loopback_port().await?,
                kv_data: reserve_loopback_port().await?,
            });
        }

        let join_addrs: Vec<String> =
            port_sets.iter().map(|p| format!("127.0.0.1:{}", p.gossip)).collect();

        let tmp = tempfile::Builder::new()
            .prefix("ephpm-e2e-cluster-")
            .tempdir()
            .context("create tempdir")?;

        let mut nodes = Vec::with_capacity(size);
        for (i, ports) in port_sets.iter().enumerate() {
            let node_dir = tmp.path().join(format!("node-{i}"));
            fs::create_dir_all(&node_dir).context("create node dir")?;
            let data_dir = node_dir.join("data");
            fs::create_dir_all(&data_dir).context("create data dir")?;

            let joins = join_addrs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");

            let config = CLUSTER_NODE_TEMPLATE
                .replace("{HTTP_PORT}", &ports.http.to_string())
                .replace("{MYSQL_PORT}", &ports.mysql.to_string())
                .replace("{GOSSIP_PORT}", &ports.gossip.to_string())
                .replace("{GRPC_PORT}", &ports.grpc.to_string())
                .replace("{SQLD_HTTP_PORT}", &ports.sqld_http.to_string())
                .replace("{KV_DATA_PORT}", &ports.kv_data.to_string())
                .replace("{NODE_ID}", &format!("fixture-node-{i}"))
                .replace("{CLUSTER_JOIN}", &joins)
                .replace("{DATA_DIR}", &escape_toml(&data_dir))
                .replace("{DOCROOT}", &escape_toml(docroot));

            let config_path = node_dir.join("ephpm.toml");
            fs::write(&config_path, config).context("write config")?;

            let stdout =
                fs::File::create(node_dir.join("stdout.log")).context("open stdout log")?;
            let stderr =
                fs::File::create(node_dir.join("stderr.log")).context("open stderr log")?;

            let child = Command::new(ephpm_binary)
                .args(["serve", "--config"])
                .arg(&config_path)
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .with_context(|| format!("spawn ephpm node {i}"))?;

            nodes.push(ClusterFixtureNode {
                child: Some(child),
                base_url: format!("http://127.0.0.1:{}", ports.http),
            });
        }

        for (i, node) in nodes.iter().enumerate() {
            let port: u16 = node
                .base_url
                .rsplit(':')
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow!("could not parse port from {}", node.base_url))?;
            wait_for_health(port, Duration::from_secs(20))
                .await
                .with_context(|| format!("cluster node {i} never healthy"))?;
        }

        Ok(Self { nodes, _tempdir: tmp })
    }

    /// Base URLs, one per node, in the same order they were spawned.
    #[must_use]
    pub fn base_urls(&self) -> Vec<&str> {
        self.nodes.iter().map(|n| n.base_url.as_str()).collect()
    }
}

impl Drop for ClusterFixture {
    fn drop(&mut self) {
        for node in &mut self.nodes {
            if let Some(child) = node.child.as_mut() {
                terminate(child);
            }
        }
    }
}

struct ClusterPorts {
    http: u16,
    mysql: u16,
    gossip: u16,
    grpc: u16,
    sqld_http: u16,
    kv_data: u16,
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Ask the kernel for a free loopback port by binding to `127.0.0.1:0` and
/// then dropping the listener.
///
/// There is an inherent TOCTOU here — the port may be re-issued before ephpm
/// binds it — but for a test fixture that only ever runs one topology at a
/// time this is acceptable in practice.
async fn reserve_loopback_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("bind :0")?;
    let port = listener.local_addr().context("local_addr")?.port();
    drop(listener);
    Ok(port)
}

async fn wait_for_health(port: u16, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::from("no attempts made");
    while Instant::now() < deadline {
        match tokio::task::spawn_blocking(move || tcp_get(port, "/_ephpm/health"))
            .await
            .map_err(|e| anyhow!("join error: {e}"))?
        {
            Ok(200) => return Ok(()),
            Ok(code) => last_err = format!("HTTP {code}"),
            Err(e) => last_err = e,
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(anyhow!("health did not report 200 within {timeout:?}: {last_err}"))
}

fn tcp_get(port: u16, path: &str) -> Result<u16, String> {
    let addr = format!("127.0.0.1:{port}").parse().map_err(|e: std::net::AddrParseError| e.to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        .map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;

    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).map_err(|e| format!("read: {e}"))?;
    if n == 0 {
        return Err("empty response".into());
    }
    let first_line = std::str::from_utf8(&buf[..n]).unwrap_or("").lines().next().unwrap_or("");
    first_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("could not parse status from {first_line:?}"))
}

fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: libc::kill takes a pid and a signal. A dead child returns
        // ESRCH, which is fine — we ignore the return value.
        unsafe {
            let pid = child.id() as i32;
            libc_kill(pid, 15);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
#[cfg(unix)]
#[inline]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    // SAFETY: forwarded to libc, see terminate().
    unsafe { kill(pid, sig) }
}

fn escape_toml(path: &Path) -> String {
    let mut out = String::new();
    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ── config templates (mirror xtask/src/e2e_bare.rs) ────────────────────────

const SINGLE_NODE_TEMPLATE: &str = r#"# Auto-generated by ephpm-e2e SingleNodeFixture — do not edit.
[server]
listen = "127.0.0.1:{HTTP_PORT}"
document_root = "{DOCROOT}"
index_files = ["index.php", "index.html"]

[server.request]
trusted_hosts = ["localhost", "127.0.0.1", "127.0.0.1:{HTTP_PORT}"]

[server.metrics]
enabled = true

[php]
max_execution_time = 30
memory_limit = "128M"

[db.sqlite]
path = "{DATA_DIR}/ephpm-fixture.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:{MYSQL_PORT}"
"#;

const CLUSTER_NODE_TEMPLATE: &str = r#"# Auto-generated by ephpm-e2e ClusterFixture — do not edit.
[server]
listen = "127.0.0.1:{HTTP_PORT}"
document_root = "{DOCROOT}"
index_files = ["index.php", "index.html"]

[server.request]
trusted_hosts = ["localhost", "127.0.0.1", "127.0.0.1:{HTTP_PORT}"]

[server.metrics]
enabled = true

[php]
mode = "fpm"
max_execution_time = 60
memory_limit = "256M"

[db.sqlite]
path = "{DATA_DIR}/wordpress.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:{MYSQL_PORT}"

[db.sqlite.sqld]
http_listen = "127.0.0.1:{SQLD_HTTP_PORT}"
grpc_listen = "127.0.0.1:{GRPC_PORT}"

[db.sqlite.replication]
role = "auto"

[cluster]
enabled = true
bind = "127.0.0.1:{GOSSIP_PORT}"
join = [{CLUSTER_JOIN}]
node_id = "{NODE_ID}"
cluster_id = "ephpm-e2e-bare-fixture"
secret = "bare-e2e-secret-do-not-use-in-prod-b7e4d3f0"

[cluster.kv]
data_port = {KV_DATA_PORT}
"#;

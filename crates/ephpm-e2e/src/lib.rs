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

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
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
        // Ports stay *held* by live probe sockets (see `PortReserver`) until
        // the moment this fixture spawns its child, so nothing else on the box
        // can be handed one in between.
        let lease = PortReserver::new().lease(&[PortKind::Tcp, PortKind::Tcp])?;
        let (http_port, mysql_port) = (lease.port(0), lease.port(1));

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

        // Hand the ports off: release the probes and spawn in the same breath.
        lease.release();
        let mut child = Command::new(ephpm_binary)
            .args(["serve", "--config"])
            .arg(&config_path)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| format!("spawn ephpm ({})", ephpm_binary.display()))?;

        wait_for_health(&mut child, http_port, Duration::from_secs(15)).await.with_context(|| {
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

/// How long each cluster node gets to answer `/_ephpm/health`.
///
/// Sized against measured cold-start, not guessed: two ephpm processes
/// starting at once take ~18-25s from `spawn` to first 200 on a Windows host
/// (a ~120 MB binary to page in, plus PHP runtime init), and the previous 20s
/// sat inside that spread — which showed up as intermittent "never healthy"
/// with an *empty* stderr, i.e. a node that was fine and merely slow.
///
/// A generous deadline costs nothing on a healthy node (the poll returns as
/// soon as it gets a 200) and nothing on a genuinely broken one either:
/// [`wait_for_health`] returns immediately when the child exits, which is how
/// a bind clash reports. Only the "hung but alive" case waits this long.
const CLUSTER_HEALTH_TIMEOUT: Duration = Duration::from_secs(90);

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

        // Reserve all port sets before spawning so overlap is impossible. Each
        // set stays *held* by its own probe sockets until that node spawns.
        let port_sets = reserve_cluster_ports(size)?;

        let join_addrs: Vec<String> =
            port_sets.iter().map(|p| format!("127.0.0.1:{}", p.gossip)).collect();

        let tmp = tempfile::Builder::new()
            .prefix("ephpm-e2e-cluster-")
            .tempdir()
            .context("create tempdir")?;

        let mut nodes = Vec::with_capacity(size);
        for (i, ports) in port_sets.into_iter().enumerate() {
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

            // Hand this node's ports off: release its probes, spawn at once.
            let http_port = ports.http;
            ports.lease.release();
            let child = Command::new(ephpm_binary)
                .args(["serve", "--config"])
                .arg(&config_path)
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .with_context(|| format!("spawn ephpm node {i}"))?;

            nodes.push(ClusterFixtureNode {
                child: Some(child),
                base_url: format!("http://127.0.0.1:{http_port}"),
            });
        }

        for (i, node) in nodes.iter_mut().enumerate() {
            let port: u16 = node
                .base_url
                .rsplit(':')
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow!("could not parse port from {}", node.base_url))?;
            let child = node.child.as_mut().ok_or_else(|| anyhow!("node {i} has no child"))?;
            if let Err(e) = wait_for_health(child, port, CLUSTER_HEALTH_TIMEOUT).await {
                // Inline the child's stderr rather than pointing at it: the
                // tempdir is deleted when this `Err` unwinds the fixture, so a
                // path in the message would name a file the reader can never
                // open. A bind clash — the failure mode this fixture is most
                // prone to, see `GOSSIP_PORT_SPAN` — says so in one line.
                let log = tmp.path().join(format!("node-{i}")).join("stderr.log");
                let detail = fs::read_to_string(&log)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|e| format!("(stderr.log unreadable: {e})"));
                return Err(e.context(format!(
                    "cluster node {i} (http {port}) never healthy; its stderr said: {detail}"
                )));
            }
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
    kv_data: u16,
    /// Probes holding this node's ports until it is spawned. See [`PortLease`].
    lease: PortLease,
}

/// Ports past `[cluster] bind` that a node claims without anyone reserving
/// them.
///
/// `ephpm-cluster` derives the cluster-channel listener from the gossip bind
/// address as `gossip_socket_addr().port() + 2` (see `cluster_channel.rs`), so
/// a node with gossip on `G` also owns `G + 2`. Reserving `G..=G + 2` keeps
/// that derived port out of every other node's port set.
///
/// This is what made `bare_process_smoke` fail the first time it ever ran
/// (issue #239): the kernel hands out ephemeral ports consecutively, so node
/// 0's gossip `G` put its channel on `G + 2` — which had already been handed
/// to node 1 as its HTTP port. Node 1 then died with "failed to bind ...
/// (os error 10048 / EADDRINUSE)" and the fixture reported "never healthy".
const CLUSTER_CHANNEL_PORT_OFFSET: u16 = 2;

/// Size of the port window a gossip port claims: the bind port itself through
/// its derived cluster-channel port inclusive.
const GOSSIP_PORT_SPAN: u16 = CLUSTER_CHANNEL_PORT_OFFSET + 1;

/// Probe sockets held open while a port plan is being assembled.
///
/// Holding them is load-bearing: dropping a probe returns its port to the
/// ephemeral pool, and the kernel walks that pool in order, so an early
/// release is exactly how a rejected port comes back as the answer to the
/// next request.
///
/// **`std::net`, not `tokio::net`, and that is not incidental.** A probe never
/// does any I/O — it exists to occupy a port — so it has no use for the
/// reactor; but more importantly, tokio's sockets come from mio, and mio
/// creates Windows sockets with `WSASocketW(..., WSA_FLAG_OVERLAPPED)` **without**
/// `WSA_FLAG_NO_HANDLE_INHERIT`, which `std` does pass. An inheritable socket
/// is duplicated into every child `Command::spawn` starts (stdio redirection
/// implies `bInheritHandles = TRUE`), so a probe held across one node's spawn
/// leaks into *that node's process* and stays bound there after this process
/// drops it — and the next node dies with `os error 10048` on a port nothing
/// visibly holds. `std` sockets are `HANDLE_FLAG_INHERIT`-clear on Windows and
/// `SOCK_CLOEXEC` on Unix, so they vanish from the child exactly as intended.
#[derive(Default)]
struct PortProbes {
    tcp: Vec<std::net::TcpListener>,
    udp: Vec<std::net::UdpSocket>,
}

/// What a reserved loopback port will actually be bound as by the child.
///
/// The protocol matters: a port that accepts a TCP bind can still refuse a UDP
/// one — on Windows, Hyper-V/WinNAT reserve large UDP-only ranges and the bind
/// fails with `WSAEACCES` (issue #239) — so a gossip port has to be probed with
/// a real UDP socket, not a TCP one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortKind {
    /// A TCP listener: HTTP, MySQL, Hrana, KV data plane, cluster channel.
    Tcp,
    /// A UDP socket: gossip, where the cluster-channel port is configured
    /// explicitly (`[cluster.channel] listen`) rather than derived.
    Udp,
    /// A gossip (UDP) port whose **derived** cluster-channel port is claimed
    /// alongside it.
    ///
    /// `ephpm-cluster` derives the channel listener from the gossip bind
    /// address as `gossip_socket_addr().port() + 2` (see `cluster_channel.rs`),
    /// so a node with gossip on `G` also owns `G + 2`. Nobody configures that
    /// port, so nobody would otherwise probe it — and the kernel hands out
    /// ephemeral ports consecutively, which is how node 0's `G + 2` once landed
    /// on node 1's HTTP port and killed it with EADDRINUSE (issue #239). Use
    /// this variant when the node's channel port is derived; use [`Self::Udp`]
    /// when it is pinned explicitly.
    GossipWithDerivedChannel,
}

/// A set of loopback ports **held open** by live probe sockets until the child
/// that will bind them is spawned.
///
/// This is the whole point of the type. Asking the kernel for a port by binding
/// `127.0.0.1:0`, reading the number and *closing the socket* leaves a TOCTOU
/// window in which the port goes straight back into the ephemeral pool — and a
/// live ephpm cluster churning gossip, CDC and MySQL connections drains that
/// pool continuously. That is how a port reserved for a node reached
/// `Address already in use` before the node ever spawned, reported as "node N
/// never became healthy" ~1 run in 25 (issue #438). The window is proportional
/// to reserve→spawn distance, and a fixture that reserves every node's ports up
/// front but spawns one of them ~100s later has a *very* wide one.
///
/// Holding the probe closes the window instead of narrowing it: the port cannot
/// be re-issued to anything while the probe lives. Call [`release`](Self::release)
/// immediately before `Command::spawn` so the handoff gap is a few
/// instructions rather than the length of a test phase.
///
/// A held TCP probe accepts nothing and a held UDP probe reads nothing; peers
/// dialing a not-yet-spawned node see a connection that is reset (or datagrams
/// dropped) when the probe is released, which is indistinguishable from the
/// closed port they would have seen anyway.
pub struct PortLease {
    ports: Vec<u16>,
    probes: PortProbes,
}

impl PortLease {
    /// The `index`-th port of this lease, in the order the kinds were requested.
    ///
    /// # Panics
    ///
    /// Panics when `index` is past the number of ports requested.
    #[must_use]
    pub fn port(&self, index: usize) -> u16 {
        self.ports[index]
    }

    /// Every port in this lease, in request order.
    #[must_use]
    pub fn ports(&self) -> &[u16] {
        &self.ports
    }

    /// Release the probe sockets, handing the ports to the child.
    ///
    /// Consuming `self` is deliberate: it makes "the ports are no longer
    /// protected" a state you cannot use by accident, and puts the release at
    /// the exact statement it belongs next to — the `spawn`.
    pub fn release(self) {
        drop(self.probes);
    }
}

/// Hands out loopback ports that stay reserved until they are handed off.
///
/// One reserver serves a whole topology: ports it has already issued are
/// remembered forever, so a port released by an earlier [`PortLease`] (whose
/// node is by then binding it) is never offered to a later one.
#[derive(Default)]
pub struct PortReserver {
    claimed: BTreeSet<u16>,
}

impl PortReserver {
    /// A reserver with nothing claimed yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve one port per entry in `kinds`, returning a lease that **holds**
    /// them until [`PortLease::release`].
    ///
    /// # Errors
    ///
    /// Fails when the kernel cannot produce a bindable, not-already-claimed
    /// port of the requested kind within [`MAX_PORT_ATTEMPTS`] tries.
    pub fn lease(&mut self, kinds: &[PortKind]) -> Result<PortLease> {
        let mut probes = PortProbes::default();
        let mut ports = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let port = match kind {
                PortKind::Tcp => claim_tcp_port(&mut probes, &mut self.claimed)?,
                PortKind::Udp => claim_udp_port(&mut probes, &mut self.claimed)?,
                PortKind::GossipWithDerivedChannel => {
                    claim_gossip_port(&mut probes, &mut self.claimed)?
                }
            };
            ports.push(port);
        }
        Ok(PortLease { ports, probes })
    }
}

/// Reserve a non-overlapping port set per node, each set held by its own lease
/// until that node spawns.
fn reserve_cluster_ports(size: usize) -> Result<Vec<ClusterPorts>> {
    let mut reserver = PortReserver::new();
    let mut sets = Vec::with_capacity(size);

    for _ in 0..size {
        let lease = reserver.lease(&[
            PortKind::Tcp,
            PortKind::Tcp,
            PortKind::Tcp,
            PortKind::GossipWithDerivedChannel,
        ])?;
        sets.push(ClusterPorts {
            http: lease.port(0),
            mysql: lease.port(1),
            kv_data: lease.port(2),
            gossip: lease.port(3),
            lease,
        });
    }

    Ok(sets)
}

/// Number of attempts any single port claim gets before giving up.
pub const MAX_PORT_ATTEMPTS: usize = 64;

/// Claim one unclaimed loopback **UDP** port, for a gossip listener whose
/// cluster-channel port is configured explicitly rather than derived.
fn claim_udp_port(probes: &mut PortProbes, claimed: &mut BTreeSet<u16>) -> Result<u16> {
    for _ in 0..MAX_PORT_ATTEMPTS {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").context("bind udp :0")?;
        let port = socket.local_addr().context("local_addr")?.port();
        probes.udp.push(socket);
        if claimed.insert(port) {
            return Ok(port);
        }
    }
    Err(anyhow!("could not reserve a free loopback UDP port in {MAX_PORT_ATTEMPTS} attempts"))
}

/// Claim one unclaimed loopback TCP port.
fn claim_tcp_port(probes: &mut PortProbes, claimed: &mut BTreeSet<u16>) -> Result<u16> {
    for _ in 0..MAX_PORT_ATTEMPTS {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").context("bind :0")?;
        let port = listener.local_addr().context("local_addr")?.port();
        probes.tcp.push(listener);
        if claimed.insert(port) {
            return Ok(port);
        }
    }
    Err(anyhow!("could not reserve a free loopback TCP port in {MAX_PORT_ATTEMPTS} attempts"))
}

/// Claim a gossip port `G` that is genuinely bindable as **UDP**, and whose
/// derived cluster-channel port `G + 2` is genuinely bindable as **TCP**.
///
/// Claims the whole `G..G + GOSSIP_PORT_SPAN` window so no other node can be
/// handed the channel port.
fn claim_gossip_port(probes: &mut PortProbes, claimed: &mut BTreeSet<u16>) -> Result<u16> {
    for _ in 0..MAX_PORT_ATTEMPTS {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").context("bind udp :0")?;
        let port = socket.local_addr().context("local_addr")?.port();
        probes.udp.push(socket);

        let span: Vec<u16> = match (0..GOSSIP_PORT_SPAN)
            .map(|off| port.checked_add(off))
            .collect::<Option<Vec<_>>>()
        {
            Some(ports) if !ports.iter().any(|p| claimed.contains(p)) => ports,
            _ => continue,
        };

        // The channel port is TCP and nobody probes it but us. If it is taken
        // (or excluded), this whole gossip port is unusable — try another.
        let channel = port + CLUSTER_CHANNEL_PORT_OFFSET;
        let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", channel)) else {
            continue;
        };
        probes.tcp.push(listener);

        claimed.extend(span);
        return Ok(port);
    }
    Err(anyhow!(
        "could not reserve a loopback UDP gossip port with a free TCP channel port \
         (gossip + {CLUSTER_CHANNEL_PORT_OFFSET}) in {MAX_PORT_ATTEMPTS} attempts"
    ))
}

/// Poll `child`'s health endpoint until it answers 200, the child exits, or
/// `timeout` elapses.
///
/// The child-exit check is a backstop for a port that got taken anyway (see
/// [`PortLease`], which is what stops that happening): a stolen port makes the
/// child fail its bind and exit, and without the check the poll could get a 200
/// from whatever *else* is listening there and hand the test a stranger's
/// server.
async fn wait_for_health(child: &mut Child, port: u16, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::from("no attempts made");
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().context("try_wait on ephpm child")? {
            return Err(anyhow!(
                "ephpm exited ({status}) before reporting healthy — \
                 possibly its reserved port was taken; check its stderr log"
            ));
        }
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
    escape_toml_str(&path.to_string_lossy())
}

/// Escape `value` for use inside a TOML basic (double-quoted) string.
///
/// Mandatory for any **path** written into a generated config: a Windows path
/// interpolated raw makes `C:\Users\...` a TOML parse error (`\U` starts an
/// 8-digit unicode escape), so the server never even reaches the behaviour the
/// test is about — it dies on "invalid unicode 8-digit hex code" and the
/// assertion failure names the wrong thing entirely.
#[must_use]
pub fn escape_toml_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
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
mode = "per_request"
max_execution_time = 60
memory_limit = "256M"

[db.sqlite]
path = "{DATA_DIR}/wordpress.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:{MYSQL_PORT}"

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

//! End-to-end proof of the AF_UNIX connect fence (`[server.tenant_network]
//! unix_socket_policy`) — multi-tenant breakout audit finding **F2**.
//!
//! Spawns a real multi-tenant ePHPm with the fence on and two vhosts, binds
//! real unix sockets on the host, and has tenant PHP try to reach them through
//! the three client paths the finding names: `stream_socket_client('unix://')`,
//! PDO `unix_socket=`, and curl `CURLOPT_UNIX_SOCKET_PATH` — plus a datagram
//! `socket_sendto()`, which never calls `connect()`. It asserts:
//!
//! 1. **deny-all by default** — a site with no allowlist entry gets `EPERM`
//!    straight from the syscall on every path (not `ENOENT`, not a timeout);
//! 2. **per-site allow** — the site that lists a socket reaches it and reads
//!    the server's marker, the other site does not; an abstract-namespace
//!    entry (`@name`) works too;
//! 3. **ePHPm's own threads are unaffected** — the node keeps serving, and an
//!    untagged process in the *same cgroup* (this test) reaches the "host"
//!    socket the tenants cannot;
//! 4. **fail-closed** — with `unix_socket_policy = "required"` and a fence
//!    that cannot attach, the node refuses to start and says why; with
//!    `"warn"` it starts and logs the exposure.
//!
//! ## Gating
//!
//! The fence needs Linux ≥ 6.7 with BTF and `CAP_BPF` + `CAP_NET_ADMIN` (or
//! `CAP_SYS_ADMIN`). Where any of that is missing the suite **skips loudly**
//! (exit 0) so an unprivileged E2E leg never reds; set
//! `EPHPM_REQUIRE_UNIX_FENCE_TESTS=1` on a privileged lane to turn a skip
//! into a failure. `EPHPM_BINARY` (set by `cargo xtask e2e` for self-managed
//! suites) is required.
//!
//! ## Running locally (root, Linux ≥ 6.7)
//!
//! ```text
//! cargo xtask e2e --tests tenant_unix_socket --ephpm-binary target/release/ephpm
//! ```

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use ephpm_e2e::{MultiSiteFixture, ephpm_binary_env, escape_toml_str};

const SITE_A: &str = "a.unixfence.test";
const SITE_B: &str = "b.unixfence.test";

/// The tenant probe. One script, both sites. `?via=` picks the client path,
/// `?target=` the destination (a path, or `@name` for an abstract socket);
/// the answer is JSON: `{ok, errno, err, data}`.
///
/// `fsockopen` is deliberately not among the paths: the multi-tenant hardening
/// preset removes it from the function table, so it never reaches the kernel.
const PROBE_PHP: &str = r#"<?php
header('Content-Type: application/json');
$via = $_GET['via'] ?? 'stream';
$target = $_GET['target'] ?? '';
if ($target !== '' && $target[0] === '@') {
    $target = "\0" . substr($target, 1); // abstract namespace
}
$out = ['via' => $via, 'ok' => false, 'errno' => null, 'err' => null, 'data' => null];
switch ($via) {
    case 'stream':
        $errno = 0; $errstr = '';
        $fp = @stream_socket_client("unix://$target", $errno, $errstr, 2);
        if ($fp === false) {
            $out['errno'] = $errno; $out['err'] = $errstr;
        } else {
            stream_set_timeout($fp, 2);
            $out['ok'] = true; $out['data'] = fread($fp, 64); fclose($fp);
        }
        break;
    case 'socket':
        $s = socket_create(AF_UNIX, SOCK_STREAM, 0);
        if (@socket_connect($s, $target)) {
            $out['ok'] = true; $out['data'] = socket_read($s, 64);
        } else {
            $out['errno'] = socket_last_error($s); $out['err'] = socket_strerror($out['errno']);
        }
        socket_close($s);
        break;
    case 'dgram':
        // sendto() on an unconnected datagram socket: the sendmsg_unix hook,
        // not connect_unix.
        $s = socket_create(AF_UNIX, SOCK_DGRAM, 0);
        $n = @socket_sendto($s, "ping", 4, 0, $target);
        if ($n === false) {
            $out['errno'] = socket_last_error($s); $out['err'] = socket_strerror($out['errno']);
        } else {
            $out['ok'] = true; $out['data'] = $n;
        }
        socket_close($s);
        break;
    case 'pdo':
        if (!extension_loaded('pdo_mysql')) { $out['err'] = 'pdo_mysql not loaded'; $out['errno'] = -1; break; }
        try {
            $pdo = new PDO("mysql:unix_socket=$target;dbname=x", 'u', 'p', [PDO::ATTR_TIMEOUT => 2]);
            $out['ok'] = true; // only if the server completed a MySQL handshake (it will not)
        } catch (PDOException $e) {
            $out['err'] = $e->getMessage();
            // "SQLSTATE[HY000] [2002] Permission denied" -> 2002 is the driver code; keep the text
            $out['errno'] = (int) ($e->errorInfo[1] ?? $e->getCode());
        }
        break;
    case 'curl':
        if (!function_exists('curl_init')) { $out['err'] = 'curl not loaded'; $out['errno'] = -1; break; }
        $ch = curl_init('http://localhost/');
        curl_setopt($ch, CURLOPT_UNIX_SOCKET_PATH, $target);
        curl_setopt($ch, CURLOPT_RETURNTRANSFER, true);
        curl_setopt($ch, CURLOPT_TIMEOUT, 2);
        $body = curl_exec($ch);
        if ($body === false) {
            $out['errno'] = curl_errno($ch); $out['err'] = curl_error($ch);
        } else {
            $out['ok'] = true; $out['data'] = $body;
        }
        curl_close($ch);
        break;
    default:
        $out['err'] = 'unknown via';
}
echo json_encode($out);
"#;

/// Why the fence cannot run here, or `None` when it can.
fn unsupported_reason() -> Option<String> {
    // Kernel >= 6.7.
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut parts = release.trim().split(['.', '-']);
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if (major, minor) < (6, 7) {
        return Some(format!(
            "kernel {} is older than 6.7 (no cgroup/connect_unix hook)",
            release.trim()
        ));
    }
    if !Path::new("/sys/kernel/btf/vmlinux").exists() {
        return Some("no /sys/kernel/btf/vmlinux (kernel built without BTF)".into());
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return Some("no cgroup v2 unified hierarchy at /sys/fs/cgroup".into());
    }
    // Effective capabilities: CAP_SYS_ADMIN (21), or CAP_BPF (39) + CAP_NET_ADMIN (12).
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let cap_eff = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .unwrap_or(0);
    let has = |bit: u32| cap_eff & (1u64 << bit) != 0;
    if !(has(21) || (has(39) && has(12))) {
        return Some(format!(
            "missing CAP_BPF + CAP_NET_ADMIN (or CAP_SYS_ADMIN); CapEff = {cap_eff:#018x} — run as root"
        ));
    }
    None
}

/// `Some(binary)` when the suite can run, `None` after printing why it skips.
/// Panics instead of skipping when `EPHPM_REQUIRE_UNIX_FENCE_TESTS=1`.
fn gate() -> Option<std::path::PathBuf> {
    let require = std::env::var("EPHPM_REQUIRE_UNIX_FENCE_TESTS").is_ok_and(|v| v == "1");
    let Some(binary) = ephpm_binary_env() else {
        assert!(!require, "EPHPM_BINARY must be set when EPHPM_REQUIRE_UNIX_FENCE_TESTS=1");
        eprintln!("SKIP tenant_unix_socket: EPHPM_BINARY not set");
        return None;
    };
    if let Some(reason) = unsupported_reason() {
        assert!(
            !require,
            "EPHPM_REQUIRE_UNIX_FENCE_TESTS=1 but the AF_UNIX fence cannot run here: {reason}"
        );
        eprintln!(
            "SKIP tenant_unix_socket: {reason}. The AF_UNIX fence needs Linux >= 6.7 with BTF and \
             CAP_BPF + CAP_NET_ADMIN; set EPHPM_REQUIRE_UNIX_FENCE_TESTS=1 to make this a failure."
        );
        return None;
    }
    Some(binary)
}

/// Serve `marker` to every stream client, forever.
fn serve_marker(listener: UnixListener, marker: &'static str) {
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let mut s = s;
            let _ = s.write_all(marker.as_bytes());
        }
    });
}

#[derive(Debug, serde::Deserialize)]
struct Probe {
    ok: bool,
    errno: Option<i64>,
    err: Option<String>,
    data: Option<serde_json::Value>,
}

async fn probe(base_url: &str, host: &str, via: &str, target: &str) -> Probe {
    let url = format!("{base_url}/index.php?via={via}&target={}", urlencoding::encode(target));
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Host", host)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} (Host: {host}) failed: {e}"));
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("body");
    assert_eq!(status, 200, "probe must run (Host: {host}, via={via}): {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("probe JSON ({via}): {e}\n{body}"))
}

/// A fence denial is `EPERM` (1) from the syscall. PHP's socket/stream layers
/// report it as errno 1 / "Operation not permitted"; PDO's driver wraps it in
/// "[2002] Permission denied"; curl in CURLE_COULDNT_CONNECT (7) with the text.
fn assert_denied(p: &Probe, what: &str) {
    assert!(!p.ok, "{what}: must be DENIED, but connected: {p:?}");
    let text = p.err.clone().unwrap_or_default();
    let eperm = p.errno == Some(1)
        || text.contains("Permission denied")
        || text.contains("Operation not permitted")
        || text.contains("not permitted");
    assert!(eperm, "{what}: must fail with EPERM (fence), got {p:?}");
}

#[tokio::test]
async fn tenant_unix_connects_are_fenced_per_site() {
    let Some(binary) = gate() else { return };

    // Host-side sockets. `host.sock` stands in for docker.sock: never allowlisted.
    let dir = tempfile::tempdir().expect("tempdir");
    let host_sock = dir.path().join("host.sock");
    let a_sock = dir.path().join("a.sock");
    let abstract_name = format!("ephpm-e2e-unixfence-{}", std::process::id());
    let dgram_path = dir.path().join("dgram.sock");

    serve_marker(UnixListener::bind(&host_sock).unwrap(), "HOST-SECRET");
    serve_marker(UnixListener::bind(&a_sock).unwrap(), "A-OK");
    let abstract_addr = SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap();
    serve_marker(UnixListener::bind_addr(&abstract_addr).unwrap(), "ABSTRACT-OK");
    let dgram_srv = UnixDatagram::bind(&dgram_path).unwrap();
    dgram_srv.set_read_timeout(Some(Duration::from_millis(500))).ok();

    let extra = format!(
        "[server.tenant_network]\nunix_socket_policy = \"required\"\n\n\
         [server.tenant_network.unix_socket_allow]\n\"{SITE_A}\" = [\"{}\", \"@{abstract_name}\"]\n",
        escape_toml_str(a_sock.to_str().unwrap()),
    );
    let mut node =
        MultiSiteFixture::prepare(&binary, &[(SITE_A, PROBE_PHP), (SITE_B, PROBE_PHP)], &extra)
            .expect("prepare multi-site node");
    if let Err(e) = node.serve().await {
        panic!(
            "node with unix_socket_policy = \"required\" did not start: {e}\n--- stderr ---\n{}",
            node.logs()
        );
    }
    let base = node.base_url().to_string();
    let host = host_sock.to_str().unwrap();
    let a = a_sock.to_str().unwrap();
    let dgram = dgram_path.to_str().unwrap();
    let abs = format!("@{abstract_name}");

    // (1) deny-all by default — site B has no allowlist entry.
    for via in ["stream", "socket", "pdo"] {
        let p = probe(&base, SITE_B, via, host).await;
        if p.errno == Some(-1) {
            eprintln!("note: {via} unavailable in this PHP build ({:?}); skipped", p.err);
            continue;
        }
        assert_denied(&p, &format!("site B -> host.sock via {via}"));
    }
    // curl reports only CURLE_COULDNT_CONNECT (7) with no errno text, so the
    // controls below are what pin this on the fence: the same socket is
    // reachable untagged (3) and site A's own socket is reachable via curl (2).
    let p = probe(&base, SITE_B, "curl", host).await;
    if p.errno != Some(-1) {
        assert!(
            !p.ok && p.errno == Some(7),
            "site B -> host.sock via curl must not connect: {p:?}"
        );
    }
    assert_denied(&probe(&base, SITE_B, "dgram", dgram).await, "site B -> dgram sendto()");
    // ...including a socket allowlisted for ANOTHER site.
    assert_denied(
        &probe(&base, SITE_B, "stream", a).await,
        "site B -> a.sock (allowlisted for A only)",
    );
    assert_denied(
        &probe(&base, SITE_B, "stream", &abs).await,
        "site B -> abstract (allowlisted for A only)",
    );

    // (2) per-site allow — site A reaches its two sockets and nothing else.
    let p = probe(&base, SITE_A, "stream", a).await;
    assert!(p.ok, "site A -> a.sock via stream must connect: {p:?}");
    assert_eq!(p.data, Some(serde_json::Value::String("A-OK".into())), "marker: {p:?}");
    let p = probe(&base, SITE_A, "socket", a).await;
    assert!(p.ok, "site A -> a.sock via socket_connect must connect: {p:?}");
    let p = probe(&base, SITE_A, "stream", &abs).await;
    assert!(p.ok, "site A -> abstract via stream must connect: {p:?}");
    assert_eq!(p.data, Some(serde_json::Value::String("ABSTRACT-OK".into())), "marker: {p:?}");
    // curl speaks HTTP to our marker server and gets a non-HTTP reply: the
    // connect itself succeeded (anything but CURLE_COULDNT_CONNECT), which is
    // what the fence governs.
    let p = probe(&base, SITE_A, "curl", a).await;
    if p.errno != Some(-1) {
        assert!(p.ok || p.errno != Some(7), "site A -> a.sock via curl must not be fenced: {p:?}");
    }
    assert_denied(
        &probe(&base, SITE_A, "stream", host).await,
        "site A -> host.sock (not allowlisted)",
    );
    assert_denied(&probe(&base, SITE_A, "dgram", dgram).await, "site A -> dgram (not allowlisted)");

    // (3) ePHPm itself is unaffected: the node still serves, and this process
    // — untagged, in the same cgroup the programs attach to (ePHPm inherits
    // ours) — reaches the host socket the tenants cannot.
    let mut s =
        UnixStream::connect(&host_sock).expect("untagged connect to host.sock must succeed");
    s.set_read_timeout(Some(Duration::from_millis(500))).ok();
    let mut buf = [0u8; 32];
    let n = std::io::Read::read(&mut s, &mut buf).unwrap_or(0);
    assert_eq!(&buf[..n], b"HOST-SECRET", "untagged reader gets the marker");
    let health = reqwest::get(format!("{base}/_ephpm/health")).await.expect("health").status();
    assert_eq!(health.as_u16(), 200, "node healthy with the fence attached");
    // And the fence announced itself.
    let log = node.logs();
    assert!(
        log.contains("AF_UNIX connect fence attached"),
        "startup log must announce the fence; stderr was:\n{log}"
    );
}

#[tokio::test]
async fn required_policy_refuses_to_start_when_the_fence_cannot_attach() {
    let Some(binary) = gate() else { return };

    // A cgroup path that does not exist makes the attach fail deterministically
    // on every kernel — the same code path a missing capability takes.
    let extra = format!(
        "[server.tenant_network]\nunix_socket_policy = \"required\"\ncgroup_path = \"{}\"\n",
        escape_toml_str("/sys/fs/cgroup/ephpm-e2e-does-not-exist"),
    );
    let mut node =
        MultiSiteFixture::prepare(&binary, &[(SITE_A, PROBE_PHP)], &extra).expect("prepare");
    let started = node.serve().await;
    let log = node.logs();
    assert!(
        started.is_err(),
        "node must REFUSE to start with required + unattachable fence; stderr:\n{log}"
    );
    assert!(
        log.contains("unix_socket_policy") || log.contains("tenant_network"),
        "refusal must name the policy; stderr was:\n{log}"
    );
}

#[tokio::test]
async fn warn_policy_starts_unfenced_and_logs_the_exposure() {
    let Some(binary) = gate() else { return };

    let dir = tempfile::tempdir().expect("tempdir");
    let host_sock = dir.path().join("host.sock");
    serve_marker(UnixListener::bind(&host_sock).unwrap(), "HOST-SECRET");

    let extra = format!(
        "[server.tenant_network]\nunix_socket_policy = \"warn\"\ncgroup_path = \"{}\"\n",
        escape_toml_str("/sys/fs/cgroup/ephpm-e2e-does-not-exist"),
    );
    let mut node =
        MultiSiteFixture::prepare(&binary, &[(SITE_A, PROBE_PHP)], &extra).expect("prepare");
    node.serve().await.unwrap_or_else(|e| panic!("warn mode must start: {e}\n{}", node.logs()));
    let log = node.logs();
    assert!(
        log.contains("UNFENCED") || log.contains("could NOT be attached"),
        "warn mode must log the exposure; stderr was:\n{log}"
    );
    // Unfenced: the tenant reaches the host socket (this is the exposure).
    let p = probe(node.base_url(), SITE_A, "stream", host_sock.to_str().unwrap()).await;
    assert!(p.ok, "warn mode leaves tenant unix connects unfenced: {p:?}");
}

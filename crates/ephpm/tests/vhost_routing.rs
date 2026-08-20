//! Local-process e2e test for `*.localhost` virtual-host routing.
//!
//! Spawns the real `ephpm dev --sites <tempdir>` binary as a child process,
//! creates a few subdirectories that should be picked up as vhosts, then
//! hits the loopback listener with custom `Host:` headers and asserts that
//! each site serves its own content. The same shape covers the existing
//! Kind-based `crates/ephpm-e2e/tests/vhosts.rs` without needing a cluster.
//!
//! Why this lives in `crates/ephpm/tests/` and not `crates/ephpm-e2e/`:
//! `ephpm-e2e` is excluded from the workspace and only builds inside the
//! Kind/Tilt container image. This test runs from a plain `cargo test`
//! against the in-tree binary, so it's part of the `ephpm` crate's
//! integration tests. No special infrastructure required.
//!
//! Run with: `cargo test -p ephpm --test vhost_routing -- --nocapture`

use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

/// Pick a free port on 127.0.0.1 by binding to port 0 and reading back the
/// kernel-assigned port. Mirrors what `ephpm dev` does internally.
///
/// This is only a *starting hint* for `ephpm dev`'s own free-port scan, not
/// a reservation: another process may grab the port between the drop here
/// and the server's bind, in which case `run_dev`'s `find_free_port` moves
/// on to the next free port. That is why `spawn_dev` reports back the port
/// the server actually bound (parsed from its `HTTP listening` log line)
/// instead of trusting this one — the bind-drop-rebind race can shift the
/// server onto a neighboring port, but it can no longer point the test's
/// requests at a stranger's socket.
fn pick_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Extract the bound port from an `HTTP listening` log line, e.g.
/// `... HTTP listening addr=127.0.0.1:43121`. Parses the digits after the
/// literal `127.0.0.1:` — matching on the value rather than `addr=` so that
/// ANSI styling around the field name cannot break the parse.
fn parse_listening_port(line: &str) -> Option<u16> {
    let tail = &line[line.rfind("127.0.0.1:")? + "127.0.0.1:".len()..];
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Spawn `ephpm dev --sites <sites> --document-root <doc_root> --port <port>`
/// and block until it logs `HTTP listening` (or 15 seconds elapse), returning
/// the child and the port the server **actually** bound — which is `port`
/// unless something else claimed it first, in which case the dev server's
/// free-port scan picked a nearby one.
///
/// Both stdout (banner) and stderr (tracing) must be drained — otherwise a
/// piped child blocks on its first write past the pipe buffer.
fn spawn_dev(sites: &PathBuf, doc_root: &PathBuf, port: u16) -> (Child, u16) {
    let mut cmd = Command::new(BIN);
    cmd.arg("dev")
        .arg("--sites")
        .arg(sites)
        .arg("--document-root")
        .arg(doc_root)
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn ephpm dev");

    let (tx, rx) = std::sync::mpsc::channel::<Option<u16>>();
    let tx_stdout = tx.clone();
    let stdout = child.stdout.take().expect("stdout piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("ephpm[out]: {line}");
            if line.contains("HTTP listening") {
                let _ = tx_stdout.send(parse_listening_port(&line));
            }
        }
    });

    let stderr = child.stderr.take().expect("stderr piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("ephpm[err]: {line}");
            if line.contains("HTTP listening") {
                let _ = tx.send(parse_listening_port(&line));
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(parsed) = rx.try_recv() {
            let Some(bound_port) = parsed else {
                let _ = child.kill();
                let _ = child.wait();
                panic!("'HTTP listening' was logged but no 127.0.0.1:<port> could be parsed");
            };
            return (child, bound_port);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("ephpm dev did not log 'HTTP listening' within 15s");
}

/// Drop-guard so a panic doesn't leak an `ephpm` process holding the port.
struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Blocking HTTP GET with a custom `Host` header. Uses `reqwest`'s blocking
/// client because the rest of the test is synchronous and we don't need a
/// tokio runtime just for three GETs.
fn get_with_host(url: &str, host: &str) -> (u16, String) {
    let rt =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async {
        let resp = reqwest::Client::new()
            .get(url)
            .header("Host", host)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {url} with Host: {host} failed: {e}"));
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        (status, body)
    })
}

#[test]
fn dev_sites_routes_localhost_subdomains() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sites = temp.path().join("sites");
    let doc_root = temp.path().join("fallback");

    for site in ["blog", "shop", "wiki"] {
        let dir = sites.join(site);
        std::fs::create_dir_all(&dir).expect("mkdir site");
        std::fs::write(dir.join("index.html"), format!("<h1>{site}</h1>"))
            .expect("write index.html");
    }
    std::fs::create_dir_all(&doc_root).expect("mkdir doc_root");
    std::fs::write(doc_root.join("index.html"), "<h1>fallback</h1>").expect("write fallback");

    let (child, port) = spawn_dev(&sites, &doc_root, pick_port());
    let _server = ServerGuard(child);
    let base = format!("http://127.0.0.1:{port}/");

    // Each named vhost serves its own content.
    for site in ["blog", "shop", "wiki"] {
        let host = format!("{site}.localhost");
        let (status, body) = get_with_host(&base, &host);
        assert_eq!(status, 200, "{host} returned {status}");
        assert!(
            body.contains(&format!("<h1>{site}</h1>")),
            "{host} served the wrong body: {body:?}",
        );
    }

    // Unknown vhost falls back to document_root.
    let (status, body) = get_with_host(&base, "nothing.localhost");
    assert_eq!(status, 200, "fallback returned {status}");
    assert!(body.contains("<h1>fallback</h1>"), "fallback wrong body: {body:?}");

    // Lazy discovery — directory created after server startup must still
    // resolve without a restart.
    let lazy = sites.join("lazy");
    std::fs::create_dir(&lazy).expect("mkdir lazy");
    std::fs::write(lazy.join("index.html"), "<h1>lazy</h1>").expect("write lazy");
    let (status, body) = get_with_host(&base, "lazy.localhost");
    assert_eq!(status, 200, "lazy.localhost returned {status}");
    assert!(body.contains("<h1>lazy</h1>"), "lazy.localhost wrong body: {body:?}");

    // Host without the `.localhost` suffix matches the same bare directory
    // name — covers tools that send `Host: blog` directly.
    let (status, body) = get_with_host(&base, "blog");
    assert_eq!(status, 200);
    assert!(body.contains("<h1>blog</h1>"));
}

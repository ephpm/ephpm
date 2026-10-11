//! Local-process regression test: PHP diagnostics must not leak into
//! `ephpm serve` responses, and must reach ePHPm's log instead.
//!
//! PHP's built-in default is `display_errors=1`. ePHPm used to leave it there,
//! so on a production `ephpm serve` any warning rendered the script's absolute
//! filesystem path into the HTTP body, while `log_errors` was effectively off
//! for ePHPm's log — the diagnostic went to the client and nowhere else.
//!
//! Spawns the real binary. Needs a PHP-linked build (`PHP_SDK_PATH` set when
//! building); on a stub build the PHP probe does not execute and every test
//! here skips with a message rather than failing.
//!
//! Run with: `cargo test -p ephpm --test php_error_display -- --nocapture`

use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

/// Unique token the probe script raises as a warning.
const PROBE: &str = "ephpm-probe-warning-7f3a";

fn pick_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// A running `ephpm` child plus every line it has written to stdout/stderr.
struct Server {
    child: Child,
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn log_contains(&self, needle: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if self.log.lock().unwrap().iter().any(|l| l.contains(needle)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Spawn `cmd`, drain both pipes into a shared log, and wait for the server's
/// `HTTP listening` line on 127.0.0.1:`port`.
fn spawn(mut cmd: Command, port: u16) -> Server {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).env_remove("RUST_LOG");
    let mut child = cmd.spawn().expect("spawn ephpm");
    let log = Arc::new(Mutex::new(Vec::new()));
    for pipe in [
        Box::new(child.stdout.take().expect("stdout")) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().expect("stderr")),
    ] {
        let log = Arc::clone(&log);
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                eprintln!("ephpm: {line}");
                log.lock().unwrap().push(line);
            }
        });
    }
    let server = Server { child, port, log };
    assert!(
        server.log_contains("HTTP listening", Duration::from_secs(20)),
        "ephpm did not log 'HTTP listening' within 20s"
    );
    server
}

fn get(port: u16, path: &str) -> (u16, String) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(ephpm_server::tls::install_default_crypto_provider);
    let rt =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async {
        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .expect("GET");
        let status = resp.status().as_u16();
        (status, resp.text().await.expect("body"))
    })
}

/// Write the probe docroot: `alive.php` (stub detection) and `warn.php`.
fn write_docroot(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir docroot");
    std::fs::write(dir.join("alive.php"), "<?php echo 'php-' . 'alive';").expect("alive.php");
    std::fs::write(
        dir.join("warn.php"),
        format!("<?php echo 'before|'; trigger_error('{PROBE}', E_USER_WARNING); echo '|after';"),
    )
    .expect("warn.php");
}

/// `true` when the spawned binary actually executes PHP.
fn php_is_linked(server: &Server) -> bool {
    let (_, body) = get(server.port, "/alive.php");
    if body.contains("php-alive") {
        true
    } else {
        eprintln!("binary is not PHP-linked (stub build) — skipping");
        false
    }
}

fn spawn_serve(root: &Path, extra_toml: &str) -> Server {
    let docroot = root.join("www");
    write_docroot(&docroot);
    let port = pick_port();
    let docroot_toml = docroot.display().to_string().replace('\\', "\\\\");
    let config = root.join("ephpm.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\ndocument_root = \"{docroot_toml}\"\n\n\
             [kv.redis_compat]\nenabled = false\n\n{extra_toml}"
        ),
    )
    .expect("write config");
    let mut cmd = Command::new(BIN);
    cmd.arg("serve").arg("--config").arg(&config);
    spawn(cmd, port)
}

#[test]
fn serve_does_not_render_php_warnings_and_logs_them() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(temp.path(), "");
    if !php_is_linked(&server) {
        return;
    }

    let (status, body) = get(server.port, "/warn.php");
    assert_eq!(status, 200, "warn.php returned {status}: {body}");
    assert_eq!(body, "before||after", "serve leaked a PHP diagnostic into the response");
    assert!(!body.contains(PROBE));
    assert!(!body.contains("warn.php"), "absolute script path leaked: {body}");

    assert!(
        server.log_contains(PROBE, Duration::from_secs(5)),
        "the PHP warning did not reach ePHPm's log"
    );
}

#[test]
fn serve_ini_overrides_still_win() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server =
        spawn_serve(temp.path(), "[php]\nini_overrides = [[\"display_errors\", \"On\"]]\n");
    if !php_is_linked(&server) {
        return;
    }

    let (_, body) = get(server.port, "/warn.php");
    assert!(body.contains(PROBE), "operator display_errors=On override was not honoured: {body}");
}

#[test]
fn dev_still_shows_php_warnings() {
    let temp = tempfile::tempdir().expect("tempdir");
    let docroot = temp.path().join("www");
    write_docroot(&docroot);
    let port = pick_port();
    let mut cmd = Command::new(BIN);
    cmd.arg("dev")
        .arg("--document-root")
        .arg(&docroot)
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"));
    let server = spawn(cmd, port);
    if !php_is_linked(&server) {
        return;
    }

    let (_, body) = get(server.port, "/warn.php");
    assert!(body.contains(PROBE), "dev mode must keep PHP diagnostics visible: {body}");
}

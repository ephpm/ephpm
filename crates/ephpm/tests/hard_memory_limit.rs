//! End-to-end: PHP's hard memory limit must cost one request, not the server.
//!
//! PHP 8.3.36 / 8.4.27 / 8.5.12 added `memory_hard_limit_percent`: an
//! allocation that cannot fail gracefully and would cross it calls
//! `_exit(137)`. In ePHPm that would end every tenant's server at once. The
//! binary is linked with `--wrap=_exit` on Linux, which turns the exit on a PHP
//! thread into a contained 500 and a retired thread. These tests drive the
//! real binary in both execution modes, with `[php] crash_containment` left at
//! its default (off) — the hard-limit containment does not depend on it.
//!
//! On an SDK without the hard limit the same script is an ordinary
//! out-of-memory fatal (also a 500); the server must survive either way, and
//! the containment-specific log assertions only run on a patched SDK.
//!
//! Spawns the real binary. Compiled only for Linux builds linked with the
//! `_exit` wrap (`php_exit_wrapped`, set by this crate's `build.rs`).
//!
//! Run with: `cargo test -p ephpm --test hard_memory_limit -- --nocapture`

#![cfg(php_exit_wrapped)]

use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

/// One internal call that asks for 200 MB at once: `sprintf()` grows its
/// output with a plain (non-fallible) reallocation, far past the 32 MB hard
/// limit that a 16M `memory_limit` implies.
const BOMB: &str = "ini_set('memory_limit', '16M'); $s = sprintf(\"%'x200000000s\", ''); \
                    echo strlen($s);";

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

    fn get(&self, path: &str) -> (u16, String) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(ephpm_server::tls::install_default_crypto_provider);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("client")
                .get(format!("http://127.0.0.1:{}{path}", self.port))
                .send()
                .await
                .unwrap_or_else(|e| panic!("GET {path}: {e} (did the server die?)"));
            let status = resp.status().as_u16();
            let body = resp.text().await.expect("body");
            (status, body)
        })
    }

    /// The process must still be running.
    fn assert_alive(&mut self, after: &str) {
        let exited = self.child.try_wait().expect("try_wait");
        assert!(exited.is_none(), "ephpm exited after {after}: {exited:?}");
    }
}

/// Write `files` into a docroot and a config with `php_toml` as the `[php]`
/// body, spawn `ephpm serve`, and wait until it listens and runs PHP.
fn spawn_serve(root: &Path, files: &[(&str, &str)], php_toml: &str, probe: &str) -> Server {
    let docroot = root.join("www");
    std::fs::create_dir_all(&docroot).expect("mkdir docroot");
    for (name, body) in files {
        std::fs::write(docroot.join(name), body).expect("write docroot file");
    }
    let port = pick_port();
    let docroot_toml = docroot.display().to_string().replace('\\', "\\\\");
    let config = root.join("ephpm.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\ndocument_root = \"{docroot_toml}\"\n\n\
             [server.timeouts]\nrequest = 30\n\n\
             [kv.redis_compat]\nenabled = false\n\n\
             [php]\n{php_toml}"
        ),
    )
    .expect("write config");

    let mut cmd = Command::new(BIN);
    cmd.arg("serve").arg("--config").arg(&config);
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
    let (status, body) = server.get(probe);
    assert!(body.contains("php-alive"), "PHP did not execute the probe: {status}: {body}");
    server
}

/// Hit the bomb `rounds` times, checking after each that the process is alive
/// and a normal request on a (possibly replacement) PHP thread succeeds.
fn bomb_and_survive(server: &mut Server, bomb: &str, probe: &str, rounds: usize) {
    for round in 0..rounds {
        let (status, body) = server.get(bomb);
        assert_eq!(status, 500, "round {round}: the over-limit request must fail with 500: {body}");
        server.assert_alive("a hard-memory-limit request");
        let (status, body) = server.get(probe);
        assert_eq!(
            (status, body.as_str()),
            (200, "php-alive"),
            "round {round}: the server must keep serving after a hard-memory-limit request"
        );
    }
    if cfg!(php_hard_memory_limit) {
        assert!(
            server.log_contains("Hard memory limit of", Duration::from_secs(5)),
            "PHP's own hard-limit message did not reach the log — was the hard limit hit?"
        );
        assert!(
            server.log_contains("hit the hard memory limit", Duration::from_secs(5)),
            "ePHPm did not log the contained hard-limit exit"
        );
    }
    server.assert_alive("the last probe");
}

#[test]
fn per_request_mode_survives_the_hard_memory_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let bomb = format!("<?php {BOMB}");
    let mut server = spawn_serve(
        temp.path(),
        &[("alive.php", "<?php echo 'php-' . 'alive';"), ("bomb.php", bomb.as_str())],
        // Two threads, so a replacement thread is spawned while the other
        // keeps serving; three rounds also exercise pthread_t reuse.
        "concurrency = 2\n",
        "/alive.php",
    );
    bomb_and_survive(&mut server, "/bomb.php", "/alive.php", 3);
}

#[test]
fn worker_mode_survives_the_hard_memory_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let worker = format!(
        "<?php\nwhile (($env = \\Ephpm\\Worker\\take_request()) !== null) {{\n\
         $path = parse_url($env->serverVars()['REQUEST_URI'] ?? '/', PHP_URL_PATH);\n\
         if ($path === '/bomb') {{ {BOMB} }}\n\
         \\Ephpm\\Worker\\send_response(200, ['Content-Type' => 'text/plain'], 'php-' . 'alive');\n\
         }}\n"
    );
    let mut server = spawn_serve(
        temp.path(),
        &[("worker.php", worker.as_str())],
        "mode = \"worker\"\nconcurrency = 1\nqueue_depth = 8\n\n[php.worker]\nscript = \"worker.php\"\n",
        "/",
    );
    bomb_and_survive(&mut server, "/bomb", "/", 3);
}

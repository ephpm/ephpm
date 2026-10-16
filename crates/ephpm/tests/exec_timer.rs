//! Local-process tests for native `max_execution_time` enforcement (PHP's
//! per-thread execution timers) in worker and per-request mode.
//!
//! Worker mode arms the timer once per request and disarms it only when the
//! worker parks idle; when the next request is already queued, the re-arm
//! replaces the previous request's still-running countdown in place. These
//! tests pin the properties that optimisation must keep:
//!
//! * a runaway script still dies with the PHP fatal, and the worker recovers;
//! * `set_time_limit()` inside a request still re-arms live;
//! * no time carries from one request into the next — neither back-to-back
//!   on a worker that never parks, nor across an idle park longer than the
//!   limit;
//! * a previous request's `set_time_limit(0)` does not leak "unlimited" into
//!   the next request, and `ini_get()` reports the configured value again.
//!
//! Every worker server runs a single worker (`concurrency = 1`), so all
//! requests share one PHP thread and therefore one timer.
//!
//! Spawns the real binary. Compiled only for builds whose PHP SDK has
//! per-thread execution timers (`php_max_exec_timers`, set by this crate's
//! `build.rs` — Linux ZTS); elsewhere (stub builds, macOS, Windows)
//! `max_execution_time` is not natively enforced and this file is empty.
//!
//! Run with: `cargo test -p ephpm --test exec_timer -- --nocapture`

#![cfg(php_max_exec_timers)]

use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

/// The configured `[php] max_execution_time` (seconds) every server uses.
const LIMIT_SECS: u64 = 3;

/// Busy-work per request in the carry-over tests: under the limit alone, but
/// any two of them together exceed it.
const SPIN_MS: u64 = 1_800;

/// Worker entrypoint. Routes on the path:
/// `/spin?ms=N` burns N ms of CPU; `/forever` burns until killed;
/// `/unlimit?ms=N` calls `set_time_limit(0)` then burns N ms;
/// `/limit?s=N` calls `set_time_limit(N)` then burns forever;
/// `/ini` reports `ini_get('max_execution_time')`.
const WORKER_PHP: &str = r"<?php
function burn(float $ms): void {
    $end = hrtime(true) + (int) ($ms * 1e6);
    while (hrtime(true) < $end) { }
}
while (($env = \Ephpm\Worker\take_request()) !== null) {
    $s = $env->serverVars();
    $path = parse_url($s['REQUEST_URI'] ?? '/', PHP_URL_PATH);
    parse_str($s['QUERY_STRING'] ?? '', $q);
    switch ($path) {
        case '/spin': burn((float) ($q['ms'] ?? 0)); $out = 'spun'; break;
        case '/forever': while (true) { } // never returns
        case '/unlimit': set_time_limit(0); burn((float) ($q['ms'] ?? 0)); $out = 'unlimited'; break;
        case '/limit': set_time_limit((int) $q['s']); while (true) { }
        case '/ini': $out = (string) ini_get('max_execution_time'); break;
        default: $out = 'php-' . 'alive';
    }
    \Ephpm\Worker\send_response(200, ['Content-Type' => 'text/plain'], $out);
}
";

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

    /// GET `path`; returns status, body and how long the request took.
    fn get(&self, path: &str) -> (u16, String, Duration) {
        get(self.port, path)
    }
}

fn get(port: u16, path: &str) -> (u16, String, Duration) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(ephpm_server::tls::install_default_crypto_provider);
    let rt =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async {
        let start = Instant::now();
        let resp = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("client")
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .expect("GET");
        let status = resp.status().as_u16();
        let body = resp.text().await.expect("body");
        (status, body, start.elapsed())
    })
}

/// Write `files` into a docroot, write a config with `php_toml` as the
/// `[php]` body, spawn `ephpm serve`, and wait until it listens and runs PHP.
fn spawn_serve(root: &Path, files: &[(&str, &str)], php_toml: &str) -> Server {
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
             [php]\nmax_execution_time = {LIMIT_SECS}\n{php_toml}"
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
    // The worker answers any path; per-request mode serves alive.php. This
    // file only compiles for a PHP-linked build (the cfg comes from the SDK),
    // so PHP not executing is a failure, not a skip.
    let (status, body, _) = server.get("/alive.php");
    assert!(body.contains("php-alive"), "PHP did not execute the probe: {status}: {body}");
    server
}

fn spawn_worker(root: &Path) -> Server {
    spawn_serve(
        root,
        &[("worker.php", WORKER_PHP)],
        // queue_depth > 1 so `queued()` can line up three jobs behind the
        // single worker without the overflow being shed.
        "mode = \"worker\"\nconcurrency = 1\nqueue_depth = 8\n\n[php.worker]\nscript = \"worker.php\"\n",
    )
}

/// Assert a request was killed by the execution timer: a 5xx, after at least
/// `min` (it ran up to its limit) and well before the 30 s request backstop.
fn assert_timed_out(what: &str, (status, body, took): (u16, String, Duration), min: Duration) {
    assert!(status >= 500, "{what}: expected a 5xx from the execution timer, got {status}: {body}");
    assert!(took >= min, "{what}: fired after {took:?}, before its {min:?} limit");
    assert!(
        took < Duration::from_secs(20),
        "{what}: took {took:?} — the execution timer did not fire (request backstop answered)"
    );
}

/// Issue `paths` so they queue behind each other on the single worker: each
/// one is sent while the previous is still executing, so the worker takes
/// the next job straight from its queue without ever parking (the path that
/// skips the idle disarm). Returns the results in send order.
fn queued(port: u16, paths: &[&'static str]) -> Vec<(u16, String, Duration)> {
    let handles: Vec<_> = paths
        .iter()
        .map(|&p| {
            let h = std::thread::spawn(move || get(port, p));
            // Enough for the request to reach the server and queue in order,
            // far less than the busy-work each one does.
            std::thread::sleep(Duration::from_millis(200));
            h
        })
        .collect();
    handles.into_iter().map(|h| h.join().expect("request thread")).collect()
}

#[test]
fn worker_runaway_script_times_out_and_worker_recovers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server = spawn_worker(temp.path());

    assert_timed_out("/forever", server.get("/forever"), Duration::from_secs(LIMIT_SECS));
    assert!(
        server.log_contains("Maximum execution time", Duration::from_secs(5)),
        "the PHP timeout fatal did not reach ePHPm's log"
    );

    // The worker is replaced and serves again — and is itself time-limited.
    let (status, body, _) = server.get("/ini");
    assert_eq!((status, body.as_str()), (200, "3"), "worker did not recover after the timeout");
    assert_timed_out(
        "/forever (respawned worker)",
        server.get("/forever"),
        Duration::from_secs(LIMIT_SECS),
    );
}

#[test]
fn worker_back_to_back_requests_get_a_fresh_limit_each() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server = spawn_worker(temp.path());
    let spin: &'static str = Box::leak(format!("/spin?ms={SPIN_MS}").into_boxed_str());

    // Queued: the worker never parks, so each re-arm replaces the previous
    // request's running countdown. Three together run 5.4 s on one thread
    // against a 3 s limit; any carry-over would kill the second or third.
    for (i, (status, body, _)) in queued(server.port, &[spin, spin, spin]).into_iter().enumerate() {
        assert_eq!((status, body.as_str()), (200, "spun"), "queued request #{i} was cut short");
    }

    // Sequential, so the worker parks (and disarms) between requests.
    for i in 0..3 {
        let (status, body, _) = server.get(spin);
        assert_eq!((status, body.as_str()), (200, "spun"), "sequential request #{i} was cut short");
    }
}

#[test]
fn worker_idle_longer_than_the_limit_does_not_fire_into_the_next_request() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server = spawn_worker(temp.path());

    let (status, _, _) = server.get("/spin?ms=500");
    assert_eq!(status, 200);
    // Idle past the limit: the previous request's countdown would have
    // expired by now had the parked worker not disarmed it.
    std::thread::sleep(Duration::from_secs(LIMIT_SECS + 1));
    let (status, body, _) = server.get(&format!("/spin?ms={SPIN_MS}"));
    assert_eq!(
        (status, body.as_str()),
        (200, "spun"),
        "a stale countdown fired after an idle park"
    );
    // And the timer still works after the park/disarm cycle.
    assert_timed_out(
        "/forever after idle",
        server.get("/forever"),
        Duration::from_secs(LIMIT_SECS),
    );
}

#[test]
fn worker_set_time_limit_is_live_and_does_not_leak() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server = spawn_worker(temp.path());

    // set_time_limit(N) inside a request re-arms live, below the configured 3 s.
    let (status, body, took) = server.get("/limit?s=1");
    assert!(status >= 500, "set_time_limit(1) did not fire: {status}: {body}");
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_millis(2_500),
        "set_time_limit(1) fired after {took:?}"
    );

    // set_time_limit(0) in one request must not make the next one unlimited,
    // queued (no park) or sequential (parked) — and ini_get() must report the
    // configured limit again (#279).
    let results = queued(server.port, &["/unlimit?ms=600", "/forever"]);
    assert_eq!((results[0].0, results[0].1.as_str()), (200, "unlimited"));
    assert_timed_out(
        "/forever queued after set_time_limit(0)",
        results[1].clone(),
        Duration::from_secs(LIMIT_SECS),
    );

    let (status, _, _) = server.get("/unlimit?ms=0");
    assert_eq!(status, 200);
    let (status, body, _) = server.get("/ini");
    assert_eq!((status, body.as_str()), (200, "3"), "ini_get leaked a previous set_time_limit(0)");
    assert_timed_out(
        "/forever after set_time_limit(0)",
        server.get("/forever"),
        Duration::from_secs(LIMIT_SECS),
    );
}

#[test]
fn per_request_mode_timeout_and_set_time_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let burn = "function burn($ms) { $e = hrtime(true) + (int) ($ms * 1e6); while (hrtime(true) < $e) { } }";
    let spin = format!("<?php {burn} burn({SPIN_MS}); echo 'spun';");
    let unlimit = format!("<?php {burn} set_time_limit(0); burn(600); echo 'unlimited';");
    let server = spawn_serve(
        temp.path(),
        &[
            ("alive.php", "<?php echo 'php-' . 'alive';"),
            ("forever.php", "<?php while (true) { }"),
            ("ini.php", "<?php echo ini_get('max_execution_time');"),
            ("limit.php", "<?php set_time_limit(1); while (true) { }"),
            ("spin.php", &spin),
            ("unlimit.php", &unlimit),
        ],
        // One PHP thread, so the set_time_limit(0) request and the next one
        // share a timer.
        "mode = \"per_request\"\nconcurrency = 1\n",
    );

    assert_timed_out("forever.php", server.get("/forever.php"), Duration::from_secs(LIMIT_SECS));
    let (status, body, took) = server.get("/limit.php");
    assert!(
        status >= 500 && took < Duration::from_millis(2_500),
        "set_time_limit(1): {status} after {took:?}: {body}"
    );
    for i in 0..3 {
        let (status, body, _) = server.get("/spin.php");
        assert_eq!((status, body.as_str()), (200, "spun"), "spin.php #{i} was cut short");
    }
    let (status, _, _) = server.get("/unlimit.php");
    assert_eq!(status, 200);
    let (status, body, _) = server.get("/ini.php");
    assert_eq!((status, body.as_str()), (200, "3"));
    assert_timed_out(
        "forever.php after set_time_limit(0)",
        server.get("/forever.php"),
        Duration::from_secs(LIMIT_SECS),
    );
}

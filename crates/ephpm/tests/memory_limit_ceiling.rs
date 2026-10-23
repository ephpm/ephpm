//! End-to-end: `[php] memory_limit` is honoured in `ephpm serve`, and
//! `[php] memory_limit_max` bounds what a script can raise it to, in both
//! execution modes.
//!
//! Regression for the v0.13.0 RC stress findings W1 (a tenant's
//! `ini_set('memory_limit', '-1')` lifted the soft limit and, with it, PHP's
//! hard limit) and W2 (an explicit `[php] memory_limit` was silently replaced
//! by the autotuned value in serve mode).
//!
//! Spawns the real binary. Needs a PHP-linked build (`PHP_SDK_PATH` set when
//! building); on a stub build PHP does not execute and every test skips with a
//! message rather than failing.
//!
//! Run with: `cargo test -p ephpm --test memory_limit_ceiling -- --nocapture`

use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

/// `ini_set()` probes, answered as `ret|after;ret|after;…` — `-1`, above the
/// ceiling, at the ceiling, then lowering and raising back under it.
const PROBE: &str = "$out = []; \
    foreach (['-1', '512M', '256M', '64M', '200M'] as $v) { \
        $r = ini_set('memory_limit', $v); \
        $out[] = var_export($r, true) . '|' . ini_get('memory_limit'); \
    } \
    echo ini_get('memory_limit') === '200M' ? '' : 'X', implode(';', $out);";

/// What [`PROBE`] prints when PHP starts at 128M with a 256M ceiling.
const EXPECTED: &str = "false|128M;false|128M;'128M'|256M;'256M'|64M;'64M'|200M";

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

    fn try_get(&self, path: &str, timeout: Duration) -> Result<(u16, String), reqwest::Error> {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(ephpm_server::tls::install_default_crypto_provider);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("client")
                .get(format!("http://127.0.0.1:{}{path}", self.port))
                .send()
                .await?;
            let status = resp.status().as_u16();
            Ok((status, resp.text().await?))
        })
    }

    fn get(&self, path: &str) -> (u16, String) {
        self.try_get(path, Duration::from_secs(30)).unwrap_or_else(|e| panic!("GET {path}: {e}"))
    }

    /// `true` when the binary actually executes PHP. A stub build answers the
    /// probe with something else, or (worker mode) not at all.
    fn php_is_linked(&self, probe: &str) -> bool {
        match self.try_get(probe, Duration::from_secs(10)) {
            Ok((_, body)) if body.contains("php-alive") => true,
            _ => {
                eprintln!("binary is not PHP-linked (stub build) — skipping");
                false
            }
        }
    }
}

/// Write `files` into a docroot and a config with `php_toml` as the `[php]`
/// body, spawn `ephpm serve`, and wait for it to listen — or to exit, in
/// which case the server's log is returned as the error.
fn spawn_serve(root: &Path, files: &[(&str, &str)], php_toml: &str) -> Result<Server, Vec<String>> {
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
             [kv.redis_compat]\nenabled = false\n\n[php]\n{php_toml}"
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
    let mut server = Server { child, port, log };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if server.log_contains("HTTP listening", Duration::ZERO) {
            return Ok(server);
        }
        if server.child.try_wait().expect("try_wait").is_some() {
            // Let the pipe readers drain the last lines.
            std::thread::sleep(Duration::from_millis(200));
            return Err(server.log.lock().unwrap().clone());
        }
        assert!(Instant::now() < deadline, "ephpm neither listened nor exited within 20s");
        std::thread::sleep(Duration::from_millis(50));
    }
}

const ALIVE: (&str, &str) = ("alive.php", "<?php echo 'php-' . 'alive';");

#[test]
fn per_request_mode_bounds_memory_limit_raises() {
    let temp = tempfile::tempdir().expect("tempdir");
    let probe = format!("<?php {PROBE}");
    let server = spawn_serve(
        temp.path(),
        &[ALIVE, ("probe.php", probe.as_str())],
        "memory_limit = \"128M\"\nmemory_limit_max = \"256M\"\nconcurrency = 2\n",
    )
    .unwrap_or_else(|log| panic!("ephpm exited at startup: {log:#?}"));
    if !server.php_is_linked("/alive.php") {
        return;
    }
    let (status, body) = server.get("/probe.php");
    assert_eq!((status, body.as_str()), (200, EXPECTED));
    assert!(
        server.log_contains("ePHPm refused memory_limit=-1", Duration::from_secs(5)),
        "the refused raise was not logged"
    );
    assert!(
        server.log_contains("PHP memory_limit ceiling", Duration::from_secs(1)),
        "the startup line naming the ceiling is missing"
    );
}

#[test]
fn worker_mode_bounds_memory_limit_raises() {
    let temp = tempfile::tempdir().expect("tempdir");
    let worker = format!(
        "<?php\nwhile (($env = \\Ephpm\\Worker\\take_request()) !== null) {{\n\
         $path = parse_url($env->serverVars()['REQUEST_URI'] ?? '/', PHP_URL_PATH);\n\
         ob_start();\n\
         if ($path === '/probe') {{ {PROBE} ini_restore('memory_limit'); }} \
         else {{ echo 'php-' . 'alive'; }}\n\
         \\Ephpm\\Worker\\send_response(200, ['Content-Type' => 'text/plain'], ob_get_clean());\n\
         }}\n"
    );
    let server = spawn_serve(
        temp.path(),
        &[("worker.php", worker.as_str())],
        "mode = \"worker\"\nconcurrency = 1\nqueue_depth = 8\nmemory_limit = \"128M\"\n\
         memory_limit_max = \"256M\"\n\n[php.worker]\nscript = \"worker.php\"\n",
    )
    .unwrap_or_else(|log| panic!("ephpm exited at startup: {log:#?}"));
    if !server.php_is_linked("/") {
        return;
    }
    // Twice: the persistent worker request must enforce it on every iteration.
    for round in 0..2 {
        let (status, body) = server.get("/probe");
        assert_eq!((status, body.as_str()), (200, EXPECTED), "round {round}");
    }
}

#[test]
fn an_explicit_memory_limit_is_honoured_and_is_the_default_ceiling() {
    // W2: serve mode used to replace an explicit [php] memory_limit with the
    // autotuned value on any host with a detectable memory budget.
    let temp = tempfile::tempdir().expect("tempdir");
    let server = spawn_serve(
        temp.path(),
        &[
            ALIVE,
            (
                "limit.php",
                "<?php $a = ini_get('memory_limit'); $r = ini_set('memory_limit', '97M'); \
                 echo $a, '|', var_export($r, true), '|', ini_get('memory_limit');",
            ),
        ],
        "memory_limit = \"96M\"\n",
    )
    .unwrap_or_else(|log| panic!("ephpm exited at startup: {log:#?}"));
    if !server.php_is_linked("/alive.php") {
        return;
    }
    let (status, body) = server.get("/limit.php");
    assert_eq!((status, body.as_str()), (200, "96M|false|96M"));
}

#[test]
fn a_startup_memory_limit_above_the_ceiling_fails_startup() {
    let temp = tempfile::tempdir().expect("tempdir");
    match spawn_serve(
        temp.path(),
        &[ALIVE],
        "memory_limit_max = \"256M\"\nini_overrides = [[\"memory_limit\", \"1G\"]]\n",
    ) {
        Err(log) => assert!(
            log.iter().any(|l| l.contains("above [php] memory_limit_max")),
            "startup failed for another reason: {log:#?}"
        ),
        Ok(server) => {
            assert!(
                !server.php_is_linked("/alive.php"),
                "a linked server must refuse to start with memory_limit above memory_limit_max"
            );
        }
    }
}

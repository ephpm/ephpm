//! End-to-end: the real `ephpm` binary wires the `ephpm_kv_*` SAPI functions
//! to its embedded KV store.
//!
//! The functions' semantics are covered in-process by
//! `crates/ephpm-php/tests/kv_functions.rs`. This test checks the one thing
//! that suite cannot: that `serve()` actually installs the store behind them,
//! so a script running in the shipped binary gets real answers instead of the
//! "KV store not wired" failure values. It drives `tests/php/kv_sapi_test.php`
//! through HTTP.
//!
//! Needs a PHP-linked build (`PHP_SDK_PATH` set); against a stub binary it
//! fails loudly rather than passing vacuously, hence `#[ignore]`.
//!
//! Run with: `cargo nextest run -p ephpm --test kv_sapi_integration --run-ignored all`

use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

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
    fn start(config: &Path, port: u16) -> Self {
        let mut child = Command::new(BIN)
            .arg("serve")
            .arg("--config")
            .arg(config)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ephpm");
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
        let server = Self { child, port, log };
        let deadline = Instant::now() + Duration::from_secs(20);
        while !server.log.lock().unwrap().iter().any(|l| l.contains("HTTP listening")) {
            assert!(Instant::now() < deadline, "ephpm did not log 'HTTP listening' within 20s");
            std::thread::sleep(Duration::from_millis(50));
        }
        server
    }

    fn get(&self, path: &str) -> (u16, String) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(ephpm_server::tls::install_default_crypto_provider);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let resp = reqwest::Client::new()
                .get(format!("http://127.0.0.1:{}{path}", self.port))
                .send()
                .await
                .expect("GET");
            let status = resp.status().as_u16();
            (status, resp.text().await.expect("body"))
        })
    }
}

#[test]
#[ignore = "needs a PHP-linked build (PHP_SDK_PATH)"]
fn kv_sapi_functions_reach_the_embedded_store() {
    let docroot = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/php");
    let docroot = docroot.canonicalize().expect("tests/php docroot");
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("ephpm.toml");
    let port = pick_port();
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\ndocument_root = {docroot:?}\n\n\
             [php]\nconcurrency = 2\n"
        ),
    )
    .expect("write config");

    let server = Server::start(&config, port);
    let (status, body) = server.get("/kv_sapi_test.php?test=all");
    assert!(
        !body.contains("ePHPm Stub"),
        "the ephpm binary under test is a stub build — build with PHP_SDK_PATH set"
    );
    let result: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("non-JSON response ({e}), status {status}: {body}"));
    assert_eq!(status, 200, "kv_sapi_test.php failed: {result}");
    assert_eq!(result["passed"], true, "kv_sapi_test.php failed: {result}");
    for check in ["set_get", "del", "exists", "incr", "expire"] {
        assert_eq!(result["details"][check], "PASS", "{check} did not run: {result}");
    }
}

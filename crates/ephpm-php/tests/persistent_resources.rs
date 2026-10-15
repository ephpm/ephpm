//! Linked test: a retiring PHP thread releases its persistent resources.
//!
//! Since PHP 8.4, `ts_free_thread()` no longer destroys a thread's
//! `EG(persistent_list)` (the per-thread lists are destroyed only at
//! `zend_shutdown()`, for threads still registered then). ePHPm retires PHP
//! threads long before that — every worker-mode recycle, every exiting pool
//! thread — so `ephpm_thread_shutdown()` must release the list itself, or every
//! `pfsockopen()` / persistent `stream_socket_client()` connection (e.g. Predis
//! with `'persistent' => true`) stays open for the life of the process: one
//! leaked socket per worker recycle, until the process runs out of descriptors.
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Run with:
//!
//! ```text
//! cargo test -p ephpm-php --release --test persistent_resources -- --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
use tempfile::TempDir;

static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();

fn init_once() {
    INIT.get_or_init(|| {
        PhpRuntime::init().expect("php_embed_init");
        PhpRuntime::finalize_for_http().expect("finalize_for_http");
    });
}

fn script_dir() -> &'static Path {
    SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path()
}

fn request(script: &Path) -> PhpRequest {
    let name = script.file_name().unwrap().to_string_lossy().into_owned();
    PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script.to_path_buf(),
        headers: vec![("host".into(), "localhost".into())],
        body: Vec::new(),
        content_type: None,
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars: Vec::new(),
        middleware: Vec::new(),
    }
}

/// A persistent socket opened on a PHP thread is closed when that thread
/// retires — the peer reads EOF instead of a connection that never ends.
#[test]
fn retiring_thread_closes_its_persistent_sockets() {
    init_once();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();

    let script: PathBuf = script_dir().join("pfsock.php");
    std::fs::write(
        &script,
        format!(
            r#"<?php
$s = pfsockopen('127.0.0.1', {port}, $errno, $errstr, 5);
if ($s === false) {{ echo "connect failed: $errstr"; return; }}
fwrite($s, "hello");
echo "ok";
"#
        ),
    )
    .expect("write script");

    // One request on a fresh OS thread with its own TSRM context. The thread
    // then exits, which retires its PHP context through
    // ephpm_thread_shutdown() (the same teardown a recycled worker runs).
    let worker = std::thread::spawn(move || {
        let resp = PhpRuntime::execute(request(&script)).expect("execute");
        PhpRuntime::finish_request().expect("finish_request");
        String::from_utf8_lossy(&resp.body).trim().to_string()
    });

    let (mut conn, _) = listener.accept().expect("PHP connects");
    let body = worker.join().expect("PHP thread");
    assert_eq!(body, "ok", "script output");

    // Persistent means the socket outlives the REQUEST — but not the thread.
    // After the thread is gone the peer must see EOF promptly.
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = Vec::new();
    match conn.read_to_end(&mut buf) {
        Ok(_) => assert_eq!(buf, b"hello", "only the script's write, then EOF"),
        Err(e) => panic!(
            "persistent socket still open after its PHP thread retired ({e}); \
             EG(persistent_list) was not released"
        ),
    }
}

//! Linked tests for the eager request teardown (`PhpRuntime::finish_request`).
//!
//! `PhpRuntime::execute` leaves the PHP request open — shutdown functions have
//! run and the response is complete — and `finish_request` runs
//! `php_request_shutdown()`: global objects' destructors and every extension's
//! `RSHUTDOWN`. Before v0.11.3 that teardown ran lazily at the top of the
//! thread's *next* request, so a session's write + lock release waited for an
//! unrelated request on the same thread, and a second request on the same
//! session that landed on another thread blocked on the lock forever (files
//! handler) — sessions hung under any multi-thread pool.
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Run with:
//!
//! ```text
//! cargo test -p ephpm-php --release --test request_teardown -- --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
use ephpm_php::response::PhpResponse;
use serial_test::serial;
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

fn write_script(name: &str, body: &str) -> PathBuf {
    let script = script_dir().join(name);
    std::fs::write(&script, body).expect("write test script");
    script
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

fn body(resp: &PhpResponse) -> String {
    String::from_utf8_lossy(&resp.body).trim().to_string()
}

/// Run one request on a FRESH OS thread (its own TSRM context), finishing it
/// the way the per-request pool does, and give up after `timeout` — a request
/// blocked on a session lock held by another thread never returns.
fn run_on_new_thread(script: PathBuf, timeout: Duration) -> Option<PhpResponse> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let resp = PhpRuntime::execute(request(&script)).expect("execute");
        PhpRuntime::finish_request().expect("finish_request");
        let _ = tx.send(resp);
    });
    rx.recv_timeout(timeout).ok()
}

/// A global object's `__destruct` runs in `finish_request`, not before the
/// response is captured and not deferred to a later request.
#[test]
#[serial]
fn global_destructor_runs_at_finish_request() {
    init_once();
    let marker = script_dir().join("destruct.marker");
    let _ = std::fs::remove_file(&marker);
    let script = write_script(
        "destruct.php",
        &format!(
            r#"<?php
class Marker {{
    public function __destruct() {{
        file_put_contents({path:?}, "destructed");
    }}
}}
$keep = new Marker();
echo "body";
"#,
            path = marker.to_string_lossy()
        ),
    );

    let resp = PhpRuntime::execute(request(&script)).expect("execute");
    assert_eq!(resp.status, 200);
    assert_eq!(body(&resp), "body");
    assert!(
        !marker.exists(),
        "destructors belong to php_request_shutdown, which must not have run before the \
         response was captured"
    );

    PhpRuntime::finish_request().expect("finish_request");
    assert_eq!(
        std::fs::read_to_string(&marker).ok().as_deref(),
        Some("destructed"),
        "finish_request must run the global object's destructor immediately — no follow-up \
         request on this thread"
    );

    // Idempotent: no request is open any more.
    PhpRuntime::finish_request().expect("second finish_request is a no-op");
}

/// The session hang: two sequential requests sharing a session id, each on its
/// own thread. With the teardown deferred, the first thread kept the session
/// file's lock until ITS next request, so the second request blocked forever.
#[test]
#[serial]
fn session_lock_is_released_by_finish_request() {
    init_once();
    let save_path = script_dir().join("sessions");
    std::fs::create_dir_all(&save_path).expect("session dir");
    // No session_write_close(): the session module writes and unlocks in its
    // RSHUTDOWN, which is exactly the path that used to be deferred.
    let script = write_script(
        "session_counter_autoclose.php",
        &format!(
            r#"<?php
ini_set('session.save_handler', 'files');
ini_set('session.save_path', {path:?});
ini_set('session.use_cookies', '0');
ini_set('session.cache_limiter', '');
session_id('ephpmteardownsessiontest0001');
session_start();
$_SESSION['n'] = ($_SESSION['n'] ?? 0) + 1;
echo $_SESSION['n'];
"#,
            path = save_path.to_string_lossy()
        ),
    );

    for expected in 1..=3 {
        let resp = run_on_new_thread(script.clone(), Duration::from_secs(10)).unwrap_or_else(|| {
            panic!(
                "request {expected} on a fresh thread did not complete within 10s — the session \
                 lock taken by a previous request was never released"
            )
        });
        assert_eq!(resp.status, 200, "request {expected}");
        assert_eq!(body(&resp), expected.to_string(), "session counter must increment");
    }
}

/// A request that is never finished is still closed correctly by the next
/// `execute` on the same thread (the fallback every non-pool caller relies on).
#[test]
#[serial]
fn unfinished_request_is_closed_by_the_next_execute() {
    init_once();
    let marker = script_dir().join("fallback.marker");
    let _ = std::fs::remove_file(&marker);
    let script = write_script(
        "fallback.php",
        &format!(
            r#"<?php
$o = new class {{
    public function __destruct() {{
        file_put_contents({path:?}, "x", FILE_APPEND);
    }}
}};
echo "ok";
"#,
            path = marker.to_string_lossy()
        ),
    );

    assert_eq!(body(&PhpRuntime::execute(request(&script)).expect("first")), "ok");
    assert!(!marker.exists());
    assert_eq!(body(&PhpRuntime::execute(request(&script)).expect("second")), "ok");
    assert_eq!(
        std::fs::read_to_string(&marker).ok().as_deref(),
        Some("x"),
        "the second execute must have torn down the first request (exactly once)"
    );
    PhpRuntime::finish_request().expect("finish");
    assert_eq!(std::fs::read_to_string(&marker).ok().as_deref(), Some("xx"));
}

/// `at_request_start` tasks run inside the next request on the thread, exactly
/// once, and are not carried into a later one.
#[test]
#[serial]
fn at_request_start_task_runs_once_inside_the_next_request() {
    init_once();
    let script = write_script("plain.php", "<?php echo 'plain';");
    let ran = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&ran);
    PhpRuntime::at_request_start(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(ran.load(Ordering::SeqCst), 0, "queued, not run yet");

    assert_eq!(body(&PhpRuntime::execute(request(&script)).expect("execute")), "plain");
    PhpRuntime::finish_request().expect("finish");
    assert_eq!(ran.load(Ordering::SeqCst), 1, "ran inside the request");

    assert_eq!(body(&PhpRuntime::execute(request(&script)).expect("execute")), "plain");
    PhpRuntime::finish_request().expect("finish");
    assert_eq!(ran.load(Ordering::SeqCst), 1, "not carried into a later request");
}

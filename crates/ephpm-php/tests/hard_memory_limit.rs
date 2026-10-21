//! PHP's hard memory limit must retire one thread, never end the process.
//!
//! PHP 8.3.36 / 8.4.27 / 8.5.12 made `memory_limit` a soft limit and added
//! `memory_hard_limit_percent` (default 200% of `memory_limit`). An allocation
//! that cannot fail gracefully and would cross the hard limit calls
//! `_exit(137)`. In ePHPm every tenant runs on a thread of one process, so
//! that exit would take the whole server down. On Linux the crash guard
//! intercepts it (`-Wl,--wrap=_exit`, see `crash_guard.c`): the request fails
//! with [`PhpError::Contained`], its thread is retired, and the process keeps
//! serving.
//!
//! The trigger is `sprintf()` with a very wide padding: its output buffer is
//! grown with a plain (non-fallible) `zend_string_extend`, so a single call
//! asks for far more than the hard limit in one allocation.
//!
//! On an SDK without the hard limit the same script is an ordinary
//! out-of-memory fatal, i.e. [`PhpError::Bailout`], and the tests assert that
//! instead — the point there is only that the process survives either way.
//!
//! Each hard-limit request runs on its own thread, because a contained request
//! retires the thread that ran it.
//!
//! Requires a real libphp link on Linux. Run with:
//! `cargo test -p ephpm-php --test hard_memory_limit`.

#![cfg(all(test, php_linked, target_os = "linux"))]

use std::path::PathBuf;
use std::sync::OnceLock;

use ephpm_php::request::PhpRequest;
use ephpm_php::response::PhpResponse;
use ephpm_php::{PhpError, PhpRuntime, crash_guard};
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

fn script(name: &str, body: &str) -> PathBuf {
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir for scripts")).path();
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write test script");
    path
}

fn request(path: PathBuf, name: &str) -> PhpRequest {
    PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: path.parent().unwrap().to_path_buf(),
        script_filename: path,
        headers: Vec::new(),
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

/// Run one request on a fresh thread, the way a pool thread would: execute,
/// then (if nothing was contained) the post-response teardown.
fn run_on_fresh_thread(name: &'static str, body: &'static str) -> Result<PhpResponse, PhpError> {
    init_once();
    let path = script(name, body);
    std::thread::Builder::new()
        .name(format!("hard-limit-{name}"))
        .stack_size(ephpm_php::PHP_THREAD_STACK)
        .spawn(move || {
            let out = PhpRuntime::execute(request(path, name));
            if !matches!(out, Err(PhpError::Contained(_))) {
                PhpRuntime::finish_request().expect("teardown");
                PhpRuntime::worker_thread_shutdown();
            }
            out
        })
        .expect("spawn request thread")
        .join()
        .expect("request thread must not panic or take the process down")
}

fn text(resp: &PhpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

/// A healthy request still works — the process and the engine are intact.
fn assert_still_serving() {
    let resp = run_on_fresh_thread("alive.php", "<?php echo 'alive';")
        .expect("a normal request after a hard-limit hit must succeed");
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "alive");
}

/// The expected failure of a request that crossed the hard limit.
fn assert_hard_limit_outcome(out: Result<PhpResponse, PhpError>) {
    if cfg!(php_hard_memory_limit) {
        assert!(
            crash_guard::exit_containment_available(),
            "this test target must be linked with -Wl,--wrap=_exit"
        );
        match out {
            Err(PhpError::Contained(msg)) => {
                assert!(msg.starts_with("hard memory limit"), "unexpected cause: {msg}");
            }
            other => panic!("expected a contained hard-limit exit, got {other:?}"),
        }
    } else {
        // No hard limit in this engine: the same overshoot is a plain
        // memory_limit fatal.
        assert!(matches!(out, Err(PhpError::Bailout(_))), "expected a bailout, got {out:?}");
    }
}

#[test]
#[serial]
fn hard_limit_in_the_script_is_contained_and_the_server_keeps_serving() {
    assert_still_serving();
    let out = run_on_fresh_thread(
        "overshoot.php",
        "<?php ini_set('memory_limit', '16M'); \
         $s = sprintf(\"%'x200000000s\", ''); echo strlen($s);",
    );
    assert_hard_limit_outcome(out);
    assert_still_serving();
}

/// A shutdown function runs inside `ephpm_run_request` too; a hard-limit exit
/// there is contained the same way.
#[test]
#[serial]
fn hard_limit_in_a_shutdown_function_is_contained() {
    let out = run_on_fresh_thread(
        "overshoot_shutdown.php",
        "<?php ini_set('memory_limit', '16M'); \
         register_shutdown_function(function () { \
             $s = sprintf(\"%'x200000000s\", ''); echo strlen($s); \
         }); \
         echo 'body';",
    );
    assert_hard_limit_outcome(out);
    assert_still_serving();
}

/// Lowering `memory_hard_limit_percent` to 100 (it is `PHP_INI_ALL`) makes the
/// hard limit equal the soft one — still one thread, not the process.
#[cfg(php_hard_memory_limit)]
#[test]
#[serial]
fn script_lowering_the_hard_limit_to_the_soft_limit_is_contained() {
    let out = run_on_fresh_thread(
        "overshoot_percent.php",
        "<?php ini_set('memory_limit', '16M'); ini_set('memory_hard_limit_percent', '100'); \
         $s = sprintf(\"%'x20000000s\", ''); echo strlen($s);",
    );
    assert_hard_limit_outcome(out);
    assert_still_serving();
}

/// Resident set size of this process, in bytes (`VmRSS` in `/proc/self/status`).
#[cfg(php_hard_memory_limit)]
fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .expect("VmRSS in /proc/self/status");
    kib * 1024
}

/// Each contained request had already filled ~12 MB of its Zend heap. That heap
/// is freed on the contained thread itself, before the thread exits, so
/// repeating the hit does not grow the process by ~12 MB a time.
///
/// The contained threads are kept alive until RSS is measured. Otherwise a
/// later thread could reuse an exited one's `pthread_t`, and TSRM's stale-entry
/// cleanup would free (or, before the TSRM entry was detached, double-free) the
/// heap behind the test's back and hide a leak.
#[cfg(php_hard_memory_limit)]
#[test]
#[serial]
fn contained_requests_give_their_zend_heap_back() {
    const ROUNDS: u64 = 8;
    const FILL_MB: u64 = 12;
    let body: &'static str = "<?php ini_set('memory_limit', '16M'); \
         $fill = str_repeat('a', 12 * 1024 * 1024); \
         $s = sprintf(\"%'x200000000s\", ''); echo strlen($fill . $s);";

    // Warm up: first-use allocations (TSRM, opcache, thread stacks) are not
    // what this test measures.
    assert_hard_limit_outcome(run_on_fresh_thread("fill_warmup.php", body));
    assert_still_serving();

    let path = script("fill.php", body);
    let before = rss_bytes();
    let mut held = Vec::new();
    for _ in 0..ROUNDS {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let path = path.clone();
        let handle = std::thread::Builder::new()
            .stack_size(ephpm_php::PHP_THREAD_STACK)
            .spawn(move || {
                let out = PhpRuntime::execute(request(path, "fill.php"));
                done_tx.send(out).expect("report outcome");
                // Stay alive (and keep this pthread_t) until RSS is measured.
                let _ = release_rx.recv();
            })
            .expect("spawn");
        assert_hard_limit_outcome(done_rx.recv().expect("outcome"));
        held.push((release_tx, handle));
    }
    let grown = rss_bytes().saturating_sub(before);
    for (release, handle) in held {
        drop(release);
        handle.join().expect("held thread");
    }
    let leaked_if_kept = ROUNDS * FILL_MB * 1024 * 1024;
    assert!(
        grown < leaked_if_kept / 2,
        "RSS grew by {grown} bytes over {ROUNDS} contained requests whose threads are \
         still alive; keeping each abandoned heap would have grown it by at least \
         {leaked_if_kept}"
    );
    assert_still_serving();
}

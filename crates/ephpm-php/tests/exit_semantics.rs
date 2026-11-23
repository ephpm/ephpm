//! `exit()` / `die()` must end a request the way php-fpm and mod_php end it:
//! the output produced so far is delivered with the status the script set,
//! wherever the `exit()` happens.
//!
//! The trap these tests pin: PHP runs some userland callbacks with no calling
//! frame — a `set_exception_handler()` handler, a `register_shutdown_function()`
//! callback, an output-buffer callback run while buffers are flushed at request
//! end. An `exit()` inside one of those finishes its unwind and *then* the
//! engine calls `zend_bailout()` (zend_throw_exception_internal). ePHPm treats
//! a bailout as a truncated response and answers 500 (#255), so until the C
//! layer learned to tell the two apart, Yii 2's error pages, every phpMyAdmin
//! page and MantisBT/FOSSBilling error pages came back as a bare 500.
//!
//! The other half of the contract is pinned too: a REAL bailout — even one that
//! follows an `exit()`-bailout in an earlier phase of the same request — still
//! discards the response.
//!
//! Requires a real libphp link (`php_linked`); in stub mode the file compiles
//! to nothing. Run with: `cargo test -p ephpm-php --test exit_semantics`.

#![cfg(all(test, php_linked))]

use std::path::PathBuf;
use std::sync::OnceLock;

use ephpm_php::request::PhpRequest;
use ephpm_php::response::PhpResponse;
use ephpm_php::{PhpError, PhpRuntime};
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
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir for exit scripts")).path();
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write test script");
    path
}

fn run(name: &str, body: &str) -> Result<PhpResponse, PhpError> {
    init_once();
    let path = script(name, body);
    PhpRuntime::execute(PhpRequest {
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
    })
}

fn ok(name: &str, body: &str) -> PhpResponse {
    run(name, body).unwrap_or_else(|e| panic!("{name}: expected a delivered response, got {e:?}"))
}

fn text(resp: &PhpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn header<'a>(resp: &'a PhpResponse, name: &str) -> Option<&'a str> {
    resp.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

// ── exit() in the script body (never bailed; pinned so it stays that way) ──

#[test]
#[serial]
fn echo_then_exit_delivers_the_output() {
    let resp = ok("echo_exit.php", "<?php echo 'echo-exit body'; exit; echo 'UNREACHABLE';");
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "echo-exit body");
}

/// The exit status is a process concept; it does not become an HTTP status.
#[test]
#[serial]
fn exit_with_a_status_code_still_answers_200() {
    let resp = ok("echo_exit3.php", "<?php echo 'echo-exit3 body'; exit(3);");
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "echo-exit3 body");
}

#[test]
#[serial]
fn header_and_status_then_exit_with_no_body() {
    let resp = ok(
        "header_403_exit.php",
        "<?php header('X-Exit-Fixture: forbidden'); http_response_code(403); exit;",
    );
    assert_eq!(resp.status, 403);
    assert_eq!(header(&resp, "X-Exit-Fixture"), Some("forbidden"));
    assert!(resp.body.is_empty());
}

#[test]
#[serial]
fn exit_inside_an_open_output_buffer_flushes_it() {
    let resp = ok("ob_exit.php", "<?php ob_start(); echo 'buffered-before-exit'; exit;");
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "buffered-before-exit");
}

#[test]
#[serial]
fn die_with_a_message_prints_it() {
    let resp = ok("die_msg.php", "<?php echo 'before|'; die('die message');");
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "before|die message");
}

// ── exit() from a frameless callback (these bailed out and 500ed) ──────────

/// phpMyAdmin renders every page from a shutdown function that ends in
/// `exit`. php-fpm stops the remaining shutdown functions and sends the page.
#[test]
#[serial]
fn exit_in_a_shutdown_function_delivers_the_response() {
    let resp = ok(
        "shutdown_exit.php",
        "<?php
        register_shutdown_function(function () { echo '|shutdown-1'; exit; });
        register_shutdown_function(function () { echo '|SHUTDOWN-2-MUST-NOT-RUN'; });
        echo 'main';",
    );
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "main|shutdown-1");
}

/// (No output before the status is set: with `output_buffering` off the first
/// echo sends the headers, exactly as under php-fpm without a buffer.)
#[test]
#[serial]
fn exit_in_a_shutdown_function_keeps_its_status_and_headers() {
    let resp = ok(
        "shutdown_exit_status.php",
        "<?php
        register_shutdown_function(function () {
            http_response_code(503); header('X-Shutdown: yes'); echo 'maintenance'; exit(2);
        });",
    );
    assert_eq!(resp.status, 503);
    assert_eq!(header(&resp, "X-Shutdown"), Some("yes"));
    assert_eq!(text(&resp), "maintenance");
}

/// A shutdown function that exits with nothing to add — the empty-body exit
/// must not be turned into an error either.
#[test]
#[serial]
fn bare_exit_in_a_shutdown_function_with_a_redirect() {
    let resp = ok(
        "shutdown_redirect.php",
        "<?php
        register_shutdown_function(function () { header('Location: /elsewhere', true, 302); exit; });",
    );
    assert_eq!(resp.status, 302);
    assert_eq!(header(&resp, "Location"), Some("/elsewhere"));
    assert!(resp.body.is_empty());
}

#[test]
#[serial]
fn exit_in_a_shutdown_function_after_ob_end_flush() {
    let resp = ok(
        "ob_shutdown_exit.php",
        "<?php ob_start();
        register_shutdown_function(function () { ob_end_flush(); exit; });
        echo 'ob-shutdown-exit body';",
    );
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "ob-shutdown-exit body");
}

/// Yii 2's ErrorHandler renders the error page (with the right status) from
/// its exception handler and then calls `exit(1)`.
#[test]
#[serial]
fn exit_in_an_exception_handler_delivers_the_error_page() {
    let resp = ok(
        "exc_handler_exit.php",
        "<?php
        set_exception_handler(function ($e) {
            http_response_code(404); echo 'handled: ', $e->getMessage(); exit(1);
        });
        throw new Exception('boom');",
    );
    assert_eq!(resp.status, 404);
    assert_eq!(text(&resp), "handled: boom");
}

/// The handler's page is complete; a shutdown function registered by the app
/// still runs afterwards, as in php-fpm.
#[test]
#[serial]
fn shutdown_functions_still_run_after_an_exception_handler_exits() {
    let resp = ok(
        "exc_handler_exit_shutdown.php",
        "<?php
        register_shutdown_function(function () { echo '|shutdown'; });
        set_exception_handler(function ($e) { echo 'handled'; exit; });
        throw new Exception('boom');",
    );
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "handled|shutdown");
}

/// An exit() inside an ob callback run at request end forfeits that buffer
/// (php-fpm and `php -S` send an empty 200 here), but it is not a failure.
#[test]
#[serial]
fn exit_in_an_output_buffer_callback_is_not_a_bailout() {
    let resp = ok(
        "ob_callback_exit.php",
        "<?php ob_start(function ($buf) { exit; }); echo 'ob-callback-exit body';",
    );
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty(), "the exiting handler's buffer is discarded: {}", text(&resp));
}

/// All three phases: an exit() in a shutdown function, then another in the ob
/// callback that the final flush runs. Both are exits; the request is not a
/// bailout.
#[test]
#[serial]
fn exit_in_shutdown_then_in_ob_callback_is_still_an_exit() {
    let resp = ok(
        "shutdown_then_ob_exit.php",
        "<?php
        header('X-Phases: 3');
        register_shutdown_function(function () { ob_start(function ($b) { exit; }); echo 'x'; exit; });",
    );
    assert_eq!(resp.status, 200);
    assert_eq!(header(&resp, "X-Phases"), Some("3"));
    assert!(resp.body.is_empty(), "the exiting ob handler's buffer is discarded: {}", text(&resp));
}

/// An uncaught exception records a fatal; a shutdown function then renders a
/// page and exits (the WordPress/Laravel fatal-handler shape). The page is
/// delivered, and the recorded fatal still makes it a 500.
#[test]
#[serial]
fn fatal_then_exit_in_shutdown_delivers_the_page_as_500() {
    let resp = ok(
        "uncaught_then_shutdown_exit.php",
        "<?php
        register_shutdown_function(function () {
            echo '|fatal-page:', error_get_last() ? 'yes' : 'no'; exit;
        });
        echo 'start';
        throw new RuntimeException('uncaught');",
    );
    assert_eq!(resp.status, 500);
    assert!(text(&resp).contains("|fatal-page:yes"), "body: {}", text(&resp));
}

// ── Real bailouts keep failing closed ──────────────────────────────────────

const OOM: &str = "ini_set('memory_limit', '2M'); $a = []; \
                   for ($i = 0; $i < 100; $i++) { $a[] = str_repeat('x', 1024 * 1024); }";

#[test]
#[serial]
fn out_of_memory_is_still_a_bailout() {
    let r = run("oom.php", &format!("<?php echo 'PARTIAL'; {OOM}"));
    assert!(matches!(r, Err(PhpError::Bailout(_))), "got {r:?}");
}

/// An exit()-bailout in the exception handler must not mask a real bailout in
/// a later phase (the shutdown functions) of the same request.
#[test]
#[serial]
fn real_bailout_after_an_exit_bailout_is_still_a_bailout() {
    let r = run(
        "exc_exit_then_oom.php",
        &format!(
            "<?php
            register_shutdown_function(function () {{ {OOM} }});
            set_exception_handler(function ($e) {{ echo 'handled|'; exit(1); }});
            throw new Exception('boom');"
        ),
    );
    assert!(matches!(r, Err(PhpError::Bailout(_))), "got {r:?}");
}

/// Same, but the handler set a non-200 status first — so only the bailout
/// classification (not the "fatal recorded and status still 200" rule) can
/// turn this into a 500.
#[test]
#[serial]
fn real_bailout_after_an_exit_bailout_with_a_status_is_still_a_bailout() {
    let r = run(
        "exc_404_exit_then_oom.php",
        &format!(
            "<?php
            register_shutdown_function(function () {{ {OOM} }});
            set_exception_handler(function ($e) {{
                http_response_code(404); echo 'handled|'; exit(1);
            }});
            throw new Exception('boom');"
        ),
    );
    assert!(matches!(r, Err(PhpError::Bailout(_))), "got {r:?}");
}

/// Whether this build's PHP carries the CVE-2026-129474 fiber-exit rework, so
/// that `exit()` inside a destroyed fiber's `finally` *propagates and ends the
/// request cleanly* instead of being swallowed.
///
/// The rework was backported to **8.4.28 / 8.5.13 / 8.6.0, but not to the 8.3
/// branch** — 8.3.37 still swallows the exit. That split is not a guess: the
/// nightly linked-test matrix proved 8.3.37 keeps the old behaviour on every
/// platform while 8.4.28 / 8.5.13 / 8.6.0 have the new one. So the gate is
/// per-branch at the security-release patch level, not a flat minor check (an
/// earlier version-blind pin made this test fail on 8.3).
fn fiber_finally_exit_propagates() -> bool {
    let v = PhpRuntime::php_version();
    let mut parts = v.split('.').map(|p| p.trim_matches(|c: char| !c.is_ascii_digit()));
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    match (major, minor) {
        (8, 3) => false,               // not backported to 8.3 — 8.3.37 still swallows
        (8, 4) => patch >= 28,         // 8.4.28
        (8, 5) => patch >= 13,         // 8.5.13
        _ => (major, minor) >= (8, 6), // 8.6.0 GA and anything newer
    }
}

/// `exit()` inside a destroyed fiber's `finally`.
///
/// Destroying a suspended fiber (`unset($f)`) runs its `finally` blocks. Up to
/// the Nov-2026 security release an `exit()` there was *swallowed* — the engine
/// discarded it and the main script carried on. CVE-2026-129474 (an engine
/// use-after-free in the exit/bailout path that also reworked how `exit()`
/// unwinds out of a fiber) changed that on the branches it was backported to
/// (8.4.28 / 8.5.13 / 8.6.0 — **but not 8.3**, see
/// [`fiber_finally_exit_propagates`]): the `exit()` now propagates and ends the
/// request cleanly, exactly like an `exit()` anywhere else — php-fpm and
/// `php -S` deliver the output produced so far with the status the script set,
/// and so does ePHPm (classified `SCRIPT_EXIT`, not a bailout).
///
/// So the assertion is version-gated:
/// - **new behaviour** — the request is delivered with the fiber's output and
///   the status set before it, and nothing after `unset($f)` runs;
/// - **old behaviour** — the swallowed exit lets the script run on into the OOM
///   tripwire, whose genuine bailout is reported; the swallowed exit must not
///   have vouched for it (the original contract of this test).
///
/// Either way the "a real bailout after an exit is still a bailout" contract
/// stays covered by `real_bailout_after_an_exit_bailout_is_still_a_bailout` and
/// `real_bailout_in_shutdown_after_a_plain_exit_is_still_a_bailout`, which put
/// the two events in different request phases.
#[test]
#[serial]
fn exit_in_a_destroyed_fibers_finally_ends_the_request() {
    let name = "fiber_finally_exit.php";
    let body = format!(
        "<?php
        http_response_code(201);
        echo 'before-fiber';
        $f = new Fiber(function () {{ try {{ Fiber::suspend(); }} finally {{ echo '|finally'; exit; }} }});
        $f->start();
        unset($f);
        echo '|UNREACHABLE';
        {OOM}"
    );

    if fiber_finally_exit_propagates() {
        let resp = ok(name, &body);
        assert_eq!(resp.status, 201);
        assert_eq!(text(&resp), "before-fiber|finally");
        assert!(!text(&resp).contains("UNREACHABLE"), "the fiber exit must end the request");
    } else {
        let r = run(name, &body);
        assert!(matches!(r, Err(PhpError::Bailout(_))), "swallowed-exit branch: got {r:?}");
    }
}

/// The same, with the exit in the main script body (no bailout there at all).
#[test]
#[serial]
fn real_bailout_in_shutdown_after_a_plain_exit_is_still_a_bailout() {
    let r = run(
        "exit_then_oom.php",
        &format!("<?php register_shutdown_function(function () {{ {OOM} }}); echo 'x'; exit;"),
    );
    assert!(matches!(r, Err(PhpError::Bailout(_))), "got {r:?}");
}

/// The thread is healthy afterwards: a normal request right after each kind
/// of ending still works.
#[test]
#[serial]
fn next_request_after_an_exit_bailout_is_clean() {
    let _ = ok(
        "exc_handler_exit_again.php",
        "<?php set_exception_handler(function ($e) { echo 'x'; exit; }); throw new Exception('e');",
    );
    let resp =
        ok("after.php", "<?php set_exception_handler(null); echo 'clean:', http_response_code();");
    assert_eq!(resp.status, 200);
    assert_eq!(text(&resp), "clean:200");
}

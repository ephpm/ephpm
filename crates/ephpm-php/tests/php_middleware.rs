//! Integration tests for the PHP middleware lane (`library = "php:<path>"`).
//!
//! These pin the behaviour that cannot be checked without a real engine: that a
//! middleware file runs inside the *same* PHP request as the application script
//! (sharing superglobals and output), that `exit()` short-circuits, and — the
//! one that matters most — that **every** middleware failure mode fails CLOSED,
//! i.e. the application script does not run.
//!
//! The fail-open hazard being closed here is specific and easy to reintroduce:
//! PHP 8 reports an uncaught `Throwable` with `E_DONT_BAIL`, so
//! `php_execute_script()` returns normally and the naive loop would happily
//! continue to the app script. For an auth middleware that is an auth bypass.
//!
//! Requires a real libphp link (`php_linked`); in stub mode the file compiles
//! to nothing.
//!
//! Run with: `cargo test -p ephpm-php --test php_middleware`.

#![cfg(all(test, php_linked))]

use std::path::PathBuf;
use std::sync::OnceLock;

use ephpm_php::PhpRuntime;
use ephpm_php::request::{MiddlewareOutcome, PhpMiddleware, PhpRequest};
use ephpm_php::response::PhpResponse;
use serial_test::serial;
use tempfile::TempDir;

static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();

/// Boot PHP once per test process.
///
/// Deliberately WITHOUT OPcache: the bare embed harness has no `php.ini`, and
/// wiring one up here to enable OPcache crashes on Windows (the private-SHM
/// setup the server does at startup is not reproduced by this harness). The
/// consequence matters for [`bench_php_mount_marginal_cost`] and is stated
/// there: with no opcode cache, every request recompiles every mounted file, so
/// the in-process benchmark measures an **upper bound**, not the cost a real
/// server pays.
fn init_once() {
    INIT.get_or_init(|| {
        PhpRuntime::init().expect("php_embed_init");
        PhpRuntime::finalize_for_http().expect("finalize_for_http");
    });
}

fn script_dir() -> &'static std::path::Path {
    SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir for middleware scripts")).path()
}

fn write_script(name: &str, body: &str) -> PathBuf {
    let path = script_dir().join(name);
    std::fs::write(&path, body).expect("write test script");
    path
}

/// Run `app` with `middleware` mounted ahead of it, in the one PHP request.
fn run(app: PathBuf, middleware: Vec<PhpMiddleware>) -> Result<PhpResponse, ephpm_php::PhpError> {
    init_once();
    let name = app.file_name().unwrap().to_string_lossy().into_owned();
    PhpRuntime::execute(PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: app.parent().unwrap().to_path_buf(),
        script_filename: app,
        headers: vec![("X-Probe".into(), "probe-value".into())],
        body: Vec::new(),
        content_type: None,
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars: Vec::new(),
        middleware,
    })
}

fn mount(script: PathBuf, config_json: Option<&str>) -> PhpMiddleware {
    PhpMiddleware { script, config_json: config_json.map(str::to_owned) }
}

fn body_of(resp: &PhpResponse) -> String {
    String::from_utf8_lossy(&resp.body).trim().to_string()
}

fn app_marker(name: &str) -> PathBuf {
    write_script(name, "<?php echo 'APP-RAN';")
}

/// Assert that a middleware failure kept the application script from running.
///
/// PHP has two shapes for "the middleware blew up", and both must fail closed:
///
/// * a `zend_bailout` (missing file, parse error, `E_ERROR`, OOM) unwinds out
///   of the request and surfaces as `PhpError::Bailout`, which the router turns
///   into a 500 with the partial body discarded;
/// * an uncaught `Throwable` is reported with `E_DONT_BAIL`, so execution
///   returns normally and the response is a 500 carrying PHP's fatal message.
///
/// The invariant under test is the same either way: `APP-RAN` must not appear.
fn assert_fails_closed(result: Result<PhpResponse, ephpm_php::PhpError>, what: &str) {
    match result {
        Err(ephpm_php::PhpError::Bailout(_)) => {}
        Err(other) => panic!("{what}: expected a bailout or a 500, got {other:?}"),
        Ok(resp) => {
            assert_eq!(resp.status, 500, "{what}: must 500");
            assert!(
                !String::from_utf8_lossy(&resp.body).contains("APP-RAN"),
                "FAIL-OPEN: the application script ran after {what}"
            );
        }
    }
}

// ── CONTINUE ──────────────────────────────────────────────────────────

/// A middleware that falls off the end is the lane's `ACTION_CONTINUE`: the
/// app script runs, and both wrote to the same output buffer — proof they
/// shared one PHP request rather than two.
#[test]
#[serial]
fn continue_runs_the_app_script_in_the_same_request() {
    let mw = write_script("mw_continue.php", "<?php echo 'MW;';");
    let app = app_marker("app_continue.php");

    let resp = run(app, vec![mount(mw, None)]).expect("request executed");
    assert_eq!(body_of(&resp), "MW;APP-RAN");
    assert_eq!(resp.status, 200);
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Continue, 1));
}

/// The middleware sees the request's already-built superglobals — the app's
/// `$_SERVER`, not a synthetic one — and a mutation is visible to the app.
/// This is the lane's `ACTION_REWRITE`: plain PHP assignment, no bespoke API.
#[test]
#[serial]
fn middleware_shares_superglobals_with_the_app_script() {
    let mw = write_script(
        "mw_rewrite.php",
        "<?php echo $_SERVER['HTTP_X_PROBE'] . ';'; $_SERVER['HTTP_X_ADDED'] = 'from-mw';",
    );
    let app = write_script("app_rewrite.php", "<?php echo $_SERVER['HTTP_X_ADDED'];");

    let resp = run(app, vec![mount(mw, None)]).expect("request executed");
    assert_eq!(body_of(&resp), "probe-value;from-mw");
}

/// Mounts run in the order the router queued them, and every one of them runs.
#[test]
#[serial]
fn mounts_run_in_chain_order() {
    let one = write_script("mw_order_1.php", "<?php echo '1';");
    let two = write_script("mw_order_2.php", "<?php echo '2';");
    let app = write_script("app_order.php", "<?php echo '3';");

    let resp = run(app, vec![mount(one, None), mount(two, None)]).expect("request executed");
    assert_eq!(body_of(&resp), "123");
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Continue, 2));
}

// ── RESPOND ───────────────────────────────────────────────────────────

/// `exit()` is the lane's `ACTION_RESPOND`: the app script never runs, and the
/// status, headers and body the middleware set are what the client gets.
#[test]
#[serial]
fn exit_short_circuits_with_status_headers_and_body() {
    let mw = write_script(
        "mw_respond.php",
        "<?php http_response_code(401); header('WWW-Authenticate: Bearer'); \
         echo 'denied'; exit;",
    );
    let app = app_marker("app_respond.php");

    let resp = run(app, vec![mount(mw, None)]).expect("request executed");
    assert_eq!(resp.status, 401);
    assert_eq!(body_of(&resp), "denied");
    assert!(
        !body_of(&resp).contains("APP-RAN"),
        "the application script must not run after a RESPOND verdict"
    );
    assert!(
        resp.headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("WWW-Authenticate") && v == "Bearer"),
        "middleware response headers reach the client: {:?}",
        resp.headers
    );
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Respond, 1));
}

/// A short-circuit stops the REST of the chain too, not just the app script.
#[test]
#[serial]
fn exit_skips_later_mounts() {
    let first = write_script("mw_first_exit.php", "<?php echo 'FIRST'; exit;");
    let second = write_script("mw_second.php", "<?php echo 'SECOND';");
    let app = app_marker("app_skip.php");

    let resp = run(app, vec![mount(first, None), mount(second, None)]).expect("request executed");
    assert_eq!(body_of(&resp), "FIRST");
    // Only the mount that short-circuited ran, so the metric attributes
    // `respond` to it and never counts the one that was skipped.
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Respond, 1));
}

// ── Failure semantics — all fail CLOSED ───────────────────────────────

/// An uncaught exception is the fail-open trap: `zend_exception_error` reports
/// it with `E_DONT_BAIL`, so `php_execute_script` returns normally and only
/// `PG(last_error_type)` records it. The app script must still not run.
#[test]
#[serial]
fn uncaught_exception_fails_closed() {
    let mw = write_script("mw_throw.php", "<?php throw new RuntimeException('nope');");
    let app = app_marker("app_throw.php");

    assert_fails_closed(run(app, vec![mount(mw, None)]), "middleware threw");
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Error, 1));
}

/// A hard fatal (`E_ERROR` via a call to an undefined function) likewise stops
/// the chain.
#[test]
#[serial]
fn fatal_error_fails_closed() {
    let mw = write_script("mw_fatal.php", "<?php no_such_function_at_all();");
    let app = app_marker("app_fatal.php");
    assert_fails_closed(run(app, vec![mount(mw, None)]), "a middleware fatal");
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Error, 1));
}

/// A parse error in the middleware file is a compile-time fatal — same rule.
#[test]
#[serial]
fn parse_error_fails_closed() {
    let mw = write_script("mw_parse.php", "<?php this is not php at all ((( ;");
    let app = app_marker("app_parse.php");
    assert_fails_closed(run(app, vec![mount(mw, None)]), "a middleware parse error");
}

/// A mount whose file does not exist is the case an operator hits by typo, and
/// the one where failing open would be most tempting. `ZEND_REQUIRE` turns the
/// failed open into `E_COMPILE_ERROR`, which bails out — the router maps that
/// to a 500 and the app script never runs, so a mounted policy that vanished is
/// never silently skipped.
#[test]
#[serial]
fn missing_middleware_file_fails_closed() {
    let missing = script_dir().join("definitely_not_here.php");
    let app = app_marker("app_missing.php");
    assert_fails_closed(run(app, vec![mount(missing, None)]), "a missing middleware file");
    assert_eq!(
        PhpRuntime::middleware_outcome(),
        (MiddlewareOutcome::Error, 1),
        "the mount that could not be loaded is the one the metric blames"
    );
}

/// A later mount never runs once an earlier one fataled.
#[test]
#[serial]
fn fatal_skips_later_mounts_and_the_app() {
    let boom = write_script("mw_boom.php", "<?php throw new LogicException('boom');");
    let after = write_script("mw_after_boom.php", "<?php echo 'AFTER';");
    let app = app_marker("app_after_boom.php");

    let result = run(app, vec![mount(boom, None), mount(after, None)]);
    if let Ok(resp) = &result {
        assert!(
            !String::from_utf8_lossy(&resp.body).contains("AFTER"),
            "a mount after a fatal must not run"
        );
    }
    assert_fails_closed(result, "a middleware fatal with a later mount queued");
    assert_eq!(
        PhpRuntime::middleware_outcome(),
        (MiddlewareOutcome::Error, 1),
        "only the mount that fataled ran"
    );
}

// ── ephpm_middleware_config() ─────────────────────────────────────────

/// The mount's `config` table reaches the script as JSON, and only inside the
/// middleware file — the app script sees `null`, so a mount cannot leak its
/// configuration into application code.
#[test]
#[serial]
fn config_is_visible_to_the_mount_and_null_to_the_app() {
    let mw = write_script(
        "mw_config.php",
        "<?php $c = json_decode(ephpm_middleware_config() ?? '{}', true); \
         echo $c['realm'] . ';';",
    );
    let app = write_script(
        "app_config.php",
        "<?php echo ephpm_middleware_config() === null ? 'null' : 'LEAKED';",
    );

    let resp = run(app, vec![mount(mw, Some(r#"{"realm":"admin"}"#))]).expect("request executed");
    assert_eq!(body_of(&resp), "admin;null");
}

/// A mount with no `config` gets `null`, which is what makes the documented
/// `json_decode(ephpm_middleware_config() ?? '{}', true)` idiom safe.
#[test]
#[serial]
fn config_is_null_when_the_mount_declares_none() {
    let mw = write_script(
        "mw_no_config.php",
        "<?php echo ephpm_middleware_config() === null ? 'none' : 'unexpected';",
    );
    let app = write_script("app_no_config.php", "<?php echo ';ok';");

    let resp = run(app, vec![mount(mw, None)]).expect("request executed");
    assert_eq!(body_of(&resp), "none;ok");
}

/// Each mount sees its OWN config, not the previous mount's.
#[test]
#[serial]
fn each_mount_sees_its_own_config() {
    let one = write_script(
        "mw_cfg_1.php",
        "<?php echo json_decode(ephpm_middleware_config(), true)['id'];",
    );
    let two = write_script(
        "mw_cfg_2.php",
        "<?php echo json_decode(ephpm_middleware_config(), true)['id'];",
    );
    let app = write_script("app_cfg.php", "<?php echo '!';");

    let resp = run(app, vec![mount(one, Some(r#"{"id":"A"}"#)), mount(two, Some(r#"{"id":"B"}"#))])
        .expect("request executed");
    assert_eq!(body_of(&resp), "AB!");
}

// ── No middleware ─────────────────────────────────────────────────────

/// The overwhelmingly common case: no mounts, nothing changes, and the outcome
/// counters stay clean for the next request on this thread.
#[test]
#[serial]
fn no_mounts_is_unchanged_behaviour() {
    let app = app_marker("app_bare.php");
    let resp = run(app, Vec::new()).expect("request executed");
    assert_eq!(body_of(&resp), "APP-RAN");
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Continue, 0));
}

// ── Cost ──────────────────────────────────────────────────────────────

/// Measure what a `php:` mount actually costs per request.
///
/// Native middleware exists precisely to avoid entering PHP on the hot path,
/// so "how much does a PHP mount cost" is the question that decides whether
/// this lane is defensible. This measures the MARGINAL cost — the same
/// application script executed with 0, 1 and 3 mounts, all doing equivalent
/// work to the `security-headers` builtin (three `header()` calls).
///
/// **This is an upper bound, not the production cost.** This harness runs
/// without OPcache (see [`init_once`]), so every iteration recompiles every
/// mounted file. A real server has OPcache on and `zend_compile_file` — which
/// is OPcache's hook — becomes a cache lookup instead of a parse. Treat the
/// number here as "the worst this could ever cost"; the guide quotes the
/// OPcache-on figure measured against a running server.
///
/// The Rust-builtin half of the comparison lives in
/// `ephpm-server`'s `middleware::tests::bench_builtin_chain_marginal_cost`
/// (crate boundaries: `ephpm-php` does not depend on the middleware crates).
/// Both are `#[ignore]`d — they are measurements, not assertions, and their
/// absolute numbers are machine-specific.
///
/// Run with:
/// `cargo test -p ephpm-php --test php_middleware -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: prints timings, asserts nothing"]
#[serial]
fn bench_php_mount_marginal_cost() {
    const ITERATIONS: u32 = 2000;

    let app = write_script("bench_app.php", "<?php echo 'ok';");
    let header_work = "<?php header('X-Frame-Options: DENY'); \
                       header('X-Content-Type-Options: nosniff'); \
                       header('Referrer-Policy: no-referrer');";
    let mounts: Vec<PathBuf> =
        (0..3).map(|i| write_script(&format!("bench_mw_{i}.php"), header_work)).collect();

    let measure = |label: &str, mounts: Vec<PhpMiddleware>| {
        // Warm up: first execution compiles the scripts into OPcache, which is
        // a one-off the steady state must not be charged for.
        for _ in 0..50 {
            run(app.clone(), mounts.clone()).expect("warmup");
        }
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            run(app.clone(), mounts.clone()).expect("bench request");
        }
        let per_request = start.elapsed() / ITERATIONS;
        println!("{label:<28} {per_request:?} / request");
        per_request
    };

    let baseline = measure("no middleware", Vec::new());
    let one = measure("1 php: mount", vec![mount(mounts[0].clone(), None)]);
    let three = measure("3 php: mounts", mounts.iter().map(|p| mount(p.clone(), None)).collect());

    println!("\nmarginal cost of the 1st mount: {:?}", one.saturating_sub(baseline));
    println!("marginal cost per mount (3):    {:?}", (three.saturating_sub(baseline)) / 3);
}

/// Per-request state must not leak across requests on the same thread: a
/// request with mounts followed by one without must report a clean chain.
#[test]
#[serial]
fn outcome_does_not_leak_to_the_next_request_on_this_thread() {
    let mw = write_script("mw_leak.php", "<?php echo 'X'; exit;");
    let app = app_marker("app_leak_1.php");
    let resp = run(app, vec![mount(mw, None)]).expect("request executed");
    assert_eq!(PhpRuntime::middleware_outcome(), (MiddlewareOutcome::Respond, 1));

    let app2 = app_marker("app_leak_2.php");
    let resp2 = run(app2, Vec::new()).expect("request executed");
    assert_eq!(body_of(&resp2), "APP-RAN");
    assert_eq!(
        PhpRuntime::middleware_outcome(),
        (MiddlewareOutcome::Continue, 0),
        "a previous request's chain outcome leaked onto this thread"
    );
    drop(resp);
}

//! A request whose `php_request_startup()` fails must leave nothing behind on
//! its PHP thread.
//!
//! `php_request_startup()` parses the request body (`sapi_activate()` ->
//! `php_hash_environment()` -> the urlencoded / rfc1867 POST handlers) and can
//! fail half-way: a body that exhausts `memory_limit` while being parsed is the
//! client-controlled way to get there. php-src's own SAPIs never reuse a thread
//! after that without `php_request_shutdown()` (FPM and CGI exit the process,
//! apache2handler always runs its request destructor). ePHPm reuses the thread,
//! so it has to run that teardown itself — otherwise:
//!
//! - the half-started request's executor, heap and module state are never torn
//!   down, and the next request's `php_request_startup()` runs on top of them;
//! - on the PHP releases fixing CVE-2026-109618/109619 (8.3.36 / 8.4.27 /
//!   8.5.12) an out-of-memory error during request parsing is *delayed*
//!   (`EG(delayed_oom)`), and only `php_request_shutdown()` clears it. Left
//!   set, every later request on the thread runs with every callback refused
//!   (`zend_call_function()` fails: `array_map`, `usort`, framework hooks) and
//!   `php_error_docref` warnings dropped;
//! - the request's buffered per-site INI overrides (`open_basedir`,
//!   `upload_tmp_dir`, `session.save_path`, ...) were only released at the end
//!   of `ephpm_run_request`, so they were applied again — before its own —
//!   to whichever request, of whichever site, the thread served next.
//!
//! The same releases also stop an out-of-memory error from bailing out of the
//! script: it unwinds like `exit()`. Shutdown functions must still run after
//! it, as they do under php-fpm (WordPress' fatal-error page, error reporters
//! that catch OOM in a shutdown function).
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Run with:
//!
//! ```text
//! cargo test -p ephpm-php --test request_startup_failure -- --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::path::{Path, PathBuf};
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

fn script_dir() -> &'static Path {
    SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path()
}

fn write_script(name: &str, body: &str) -> PathBuf {
    let script = script_dir().join(name);
    std::fs::write(&script, body).expect("write test script");
    script
}

fn request(script: &Path, method: &str, body: Vec<u8>, content_type: Option<&str>) -> PhpRequest {
    let name = script.file_name().unwrap().to_string_lossy().into_owned();
    PhpRequest {
        method: method.into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script.to_path_buf(),
        headers: vec![("host".into(), "localhost".into())],
        body,
        content_type: content_type.map(Into::into),
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars: Vec::new(),
        middleware: Vec::new(),
    }
}

/// An `open_basedir` value covering the test scripts and the temp dir, joined
/// with PHP's `DEFAULT_DIR_SEPARATOR` (`;` on Windows, where `:` follows every
/// drive letter, and `:` elsewhere).
fn basedir_list() -> String {
    let sep = if cfg!(windows) { ';' } else { ':' };
    format!("{}{sep}{}", script_dir().display(), std::env::temp_dir().display())
}

fn body(resp: &PhpResponse) -> String {
    String::from_utf8_lossy(&resp.body).trim().to_string()
}

/// Run `f` on a fresh OS thread — its own TSRM context, as a pool thread has —
/// so nothing a test leaves on a thread can reach another test.
fn on_fresh_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().expect("PHP test thread panicked")
}

/// Reports what a request sees of the thread it runs on: whether callbacks
/// run, whether a `php_error_docref` warning is recorded, and the per-request
/// INI values a previous request could have leaked.
const PROBE: &str = r#"<?php
$sorted = [3, 1, 2];
usort($sorted, fn ($a, $b) => $a <=> $b);
@hex2bin('abc');
echo json_encode([
    'callbacks' => array_map(fn ($x) => $x * 2, [1, 2]),
    'sorted' => $sorted,
    'docref_warning' => error_get_last()['message'] ?? null,
    'memory_limit' => ini_get('memory_limit'),
    'open_basedir' => ini_get('open_basedir'),
    'upload_tmp_dir' => ini_get('upload_tmp_dir'),
]);
"#;

fn probe(script: &Path) -> String {
    let resp =
        PhpRuntime::execute(request(script, "GET", Vec::new(), None)).expect("probe request");
    PhpRuntime::finish_request().expect("finish probe request");
    assert_eq!(resp.status, 200, "probe failed: {}", body(&resp));
    body(&resp)
}

/// An urlencoded body holding one value larger than `memory_limit`: parsing it
/// into `$_POST` exhausts the limit inside `php_request_startup()`.
fn oversized_form_post(script: &Path) -> PhpRequest {
    let mut form = b"a=".to_vec();
    form.resize(6 * 1024 * 1024, b'x');
    request(script, "POST", form, Some("application/x-www-form-urlencoded"))
}

#[test]
#[serial]
fn failed_request_startup_leaves_the_thread_clean() {
    init_once();
    let probe_script = write_script("probe.php", PROBE);
    let unreachable = write_script("never_runs.php", "<?php echo 'script ran';");

    let baseline = on_fresh_thread({
        let probe_script = probe_script.clone();
        move || probe(&probe_script)
    });
    assert!(
        baseline.contains(r#""callbacks":[2,4]"#),
        "baseline probe is not what this test expects: {baseline}"
    );

    // A per-site sandbox the failing request carries, as the router buffers it.
    let tenant_basedir = basedir_list();
    let tenant_tmp = std::env::temp_dir().display().to_string();

    let (failed, after) = on_fresh_thread(move || {
        PhpRuntime::set_request_ini("memory_limit", "4M");
        PhpRuntime::set_request_ini("post_max_size", "16M");
        PhpRuntime::set_request_ini("open_basedir", &tenant_basedir);
        PhpRuntime::set_request_ini("upload_tmp_dir", &tenant_tmp);
        let failed = PhpRuntime::execute(oversized_form_post(&unreachable));
        PhpRuntime::finish_request().expect("finish after a failed startup");
        // The next request on the same thread, of a site with no overrides.
        (failed, probe(&probe_script))
    });

    match failed {
        Err(PhpError::ExecutionFailed(_)) => {}
        other => panic!("an out-of-memory request body must fail request startup, got {other:?}"),
    }
    assert_eq!(
        after, baseline,
        "the request after a failed startup must see a clean thread: callbacks run, warnings \
         are recorded, and none of the failed request's INI overrides apply"
    );
}

/// Run two failing requests back to back on one thread before the probe: the
/// buffered overrides used to accumulate until the 16-slot buffer was full and
/// the next site's own `open_basedir` was silently dropped.
#[test]
#[serial]
fn repeated_startup_failures_do_not_accumulate_ini_overrides() {
    init_once();
    let probe_script = write_script("probe_repeat.php", PROBE);
    let unreachable = write_script("never_runs_repeat.php", "<?php echo 'script ran';");
    let own_basedir = basedir_list();
    let other_tmp_dir = script_dir().join("other-tenant");
    std::fs::create_dir_all(&other_tmp_dir).expect("create the other tenant's temp dir");
    let other_tmp = other_tmp_dir.display().to_string();

    let after = on_fresh_thread(move || {
        for attempt in 0..4 {
            PhpRuntime::set_request_ini("memory_limit", "4M");
            PhpRuntime::set_request_ini("post_max_size", "16M");
            PhpRuntime::set_request_ini("upload_tmp_dir", &other_tmp);
            PhpRuntime::set_request_ini("sys_temp_dir", &other_tmp);
            let failed = PhpRuntime::execute(oversized_form_post(&unreachable));
            assert!(
                matches!(failed, Err(PhpError::ExecutionFailed(_))),
                "attempt {attempt}: request startup must fail, got {failed:?}"
            );
            PhpRuntime::finish_request().expect("finish after a failed startup");
        }
        PhpRuntime::set_request_ini("open_basedir", &own_basedir);
        let resp = PhpRuntime::execute(request(&probe_script, "GET", Vec::new(), None))
            .expect("probe request");
        PhpRuntime::finish_request().expect("finish probe request");
        (body(&resp), own_basedir)
    });

    let (probe_body, own_basedir) = after;
    let expected = format!(r#""open_basedir":{}"#, serde_json_string(&own_basedir));
    assert!(
        probe_body.contains(&expected),
        "the site's own open_basedir must apply after earlier failed startups: {probe_body}"
    );
    assert!(
        !probe_body.contains("other-tenant"),
        "a failed request's upload_tmp_dir leaked into the next request: {probe_body}"
    );
    assert!(
        !probe_body.contains(r#""memory_limit":"4M""#),
        "a failed request's memory_limit leaked into the next request: {probe_body}"
    );
}

/// JSON-encode a string the way PHP's `json_encode` does for a plain path
/// (escaping `/` as `\/`).
fn serde_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An out-of-memory error in the script still fails the request closed, and
/// the shutdown functions still run after it — as they do under php-fpm.
#[test]
#[serial]
fn shutdown_functions_run_after_out_of_memory() {
    init_once();
    let marker = script_dir().join("oom_shutdown.marker");
    let _ = std::fs::remove_file(&marker);
    let script = write_script(
        "oom_shutdown.php",
        &format!(
            r#"<?php
register_shutdown_function(function () {{
    file_put_contents({path:?}, error_get_last()['message'] ?? 'no error recorded');
}});
echo 'PARTIAL';
ini_set('memory_limit', '4M');
$a = [];
for ($i = 0; $i < 100; $i++) {{ $a[] = str_repeat('x', 1024 * 1024); }}
echo 'unreachable';
"#,
            path = marker.to_string_lossy()
        ),
    );

    let (result, probe_after) = on_fresh_thread({
        let probe_script = write_script("probe_after_oom.php", PROBE);
        move || {
            let result = PhpRuntime::execute(request(&script, "GET", Vec::new(), None));
            PhpRuntime::finish_request().expect("finish after OOM");
            (result, probe(&probe_script))
        }
    });

    assert!(
        matches!(result, Err(PhpError::Bailout(_))),
        "an out-of-memory script must still discard its truncated response: {result:?}"
    );
    let recorded = std::fs::read_to_string(&marker)
        .expect("the shutdown function must run after an out-of-memory error");
    assert!(
        recorded.contains("Allowed memory size"),
        "the shutdown function must see the out-of-memory fatal in error_get_last(): {recorded}"
    );
    assert!(
        probe_after.contains(r#""callbacks":[2,4]"#),
        "the thread must be usable after an out-of-memory request: {probe_after}"
    );
}

//! Linked end-to-end per-request timing probe for issues #133/#134.
//!
//! Executes the same small script through the full fpm-mode request path
//! (`PhpRuntime::execute`) thousands of times and prints the mean
//! microseconds per request. Run it on the commit before and after a
//! hot-path change to attribute the delta.
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Deliberately no OPcache (see `php_middleware.rs::init_once` for
//! why), so the absolute number includes a constant per-request compile of
//! the ~15-line script — the before/after *difference* is still attributable
//! because the compile cost does not change with the request-mapping code.
//!
//! ```text
//! cargo test -p ephpm-php --release --test perf_linked -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use ephpm_php::PhpRuntime;
use ephpm_php::request::PhpRequest;
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

fn write_script(name: &str, body: &str) -> PathBuf {
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path();
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write test script");
    path
}

/// A request shaped like the Laravel bench's: a dozen headers, a query
/// string, env-var injection, and a script that reads `$_SERVER` (forcing
/// the JIT superglobal registration) and emits several response headers.
fn make_request(script: PathBuf) -> PhpRequest {
    PhpRequest {
        method: "GET".into(),
        uri: "/index.php?page=2&ref=home".into(),
        path: "/index.php".into(),
        query_string: "page=2&ref=home".into(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script,
        headers: vec![
            ("host".into(), "shop.example.com".into()),
            ("user-agent".into(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko".into()),
            ("accept".into(), "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8".into()),
            ("accept-language".into(), "en-US,en;q=0.5".into()),
            ("accept-encoding".into(), "gzip, deflate, br".into()),
            ("cookie".into(), "session=eyJpdiI6IjZ4TmpX; XSRF-TOKEN=abc123".into()),
            ("connection".into(), "keep-alive".into()),
            ("upgrade-insecure-requests".into(), "1".into()),
            ("sec-fetch-dest".into(), "document".into()),
            ("sec-fetch-mode".into(), "navigate".into()),
            ("sec-fetch-site".into(), "same-origin".into()),
            ("cache-control".into(), "max-age=0".into()),
        ],
        body: Vec::new(),
        content_type: None,
        remote_addr: "203.0.113.9:52341".parse().unwrap(),
        server_name: "shop.example.com".into(),
        server_port: 443,
        is_https: true,
        protocol: "HTTP/1.1".into(),
        env_vars: vec![
            ("EPHPM_REDIS_HOST".into(), "127.0.0.1".into()),
            ("EPHPM_REDIS_PORT".into(), "6379".into()),
            ("EPHPM_REDIS_USERNAME".into(), "shop.example.com".into()),
            ("EPHPM_REDIS_PASSWORD".into(), "0123456789abcdef0123456789abcdef".into()),
        ],
        middleware: Vec::new(),
    }
}

#[test]
#[serial]
#[ignore = "timing probe — run manually in release mode with --nocapture"]
#[allow(clippy::cast_precision_loss)]
fn bench_full_request_path() {
    init_once();
    let script = write_script(
        "perf.php",
        r#"<?php
header('Cache-Control: no-cache, private');
header('X-Frame-Options: SAMEORIGIN');
header('Vary: Accept-Encoding');
setcookie('XSRF-TOKEN', 'abc123', ['path' => '/', 'samesite' => 'Lax']);
setcookie('app_session', 'def456', ['path' => '/', 'httponly' => true]);
echo 'uri=', $_SERVER['REQUEST_URI'], ' host=', $_SERVER['HTTP_HOST'],
     ' ua=', $_SERVER['HTTP_USER_AGENT'], ' q=', $_SERVER['QUERY_STRING'];
"#,
    );

    // Warmup (also sanity-checks the response once).
    let resp = PhpRuntime::execute(make_request(script.clone())).expect("warmup request");
    assert_eq!(resp.status, 200);
    assert!(String::from_utf8_lossy(&resp.body).contains("uri=/index.php?page=2&ref=home"));
    assert!(resp.headers.iter().any(|(n, v)| n == "X-Frame-Options" && v == "SAMEORIGIN"));
    for _ in 0..500 {
        PhpRuntime::execute(make_request(script.clone())).expect("warmup");
    }

    const ITERS: u32 = 5000;
    let start = Instant::now();
    for _ in 0..ITERS {
        let resp = PhpRuntime::execute(make_request(script.clone())).expect("request");
        std::hint::black_box(resp);
    }
    let per_req_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITERS);
    println!("full fpm request path (no OPcache): {per_req_us:.1} us/request over {ITERS} iters");
}

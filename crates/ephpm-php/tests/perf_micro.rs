//! Micro-benchmarks for the per-request hot-path work tracked by issues
//! #133 ($_SERVER rebuild) and #134 (response-header round-trip).
//!
//! These are `#[ignore]`d timing probes, not assertions — run them manually
//! in release mode and read the printed ns/request:
//!
//! ```text
//! cargo test -p ephpm-php --release --test perf_micro -- --ignored --nocapture
//! ```
//!
//! They compile in stub mode (no `php_linked` needed): everything measured
//! here is the pure-Rust side of the request path. The C side
//! (`php_register_variable_safe`, the smart_str header pack) is covered by
//! the linked end-to-end probe in `tests/perf_linked.rs`.

use std::ffi::CString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Instant;

use ephpm_php::request::{build_server_variables, build_server_variables_c};

/// A typical dynamic-site request: browser-grade header count plus the
/// env-var injection multi-tenant mode performs.
struct Shape {
    name: &'static str,
    headers: Vec<(String, String)>,
    env_vars: Vec<(String, String)>,
}

fn shapes() -> Vec<Shape> {
    let minimal = Shape {
        name: "minimal (bench-tool, 4 headers, 0 env)",
        headers: vec![
            ("host".into(), "example.com".into()),
            ("user-agent".into(), "hey/0.0.1".into()),
            ("content-type".into(), "application/x-www-form-urlencoded".into()),
            ("accept-encoding".into(), "gzip".into()),
        ],
        env_vars: vec![],
    };
    let typical = Shape {
        name: "typical (browser, 12 headers, 4 env)",
        headers: vec![
            ("host".into(), "shop.example.com".into()),
            ("user-agent".into(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko".into()),
            ("accept".into(), "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8".into()),
            ("accept-language".into(), "en-US,en;q=0.5".into()),
            ("accept-encoding".into(), "gzip, deflate, br".into()),
            ("cookie".into(), "laravel_session=eyJpdiI6IjZ4TmpX; XSRF-TOKEN=abc123".into()),
            ("connection".into(), "keep-alive".into()),
            ("upgrade-insecure-requests".into(), "1".into()),
            ("sec-fetch-dest".into(), "document".into()),
            ("sec-fetch-mode".into(), "navigate".into()),
            ("sec-fetch-site".into(), "same-origin".into()),
            ("cache-control".into(), "max-age=0".into()),
        ],
        env_vars: vec![
            ("EPHPM_REDIS_HOST".into(), "127.0.0.1".into()),
            ("EPHPM_REDIS_PORT".into(), "6379".into()),
            ("EPHPM_REDIS_USERNAME".into(), "shop.example.com".into()),
            ("EPHPM_REDIS_PASSWORD".into(), "0123456789abcdef0123456789abcdef".into()),
        ],
    };
    vec![minimal, typical]
}

#[allow(clippy::cast_precision_loss)]
fn time<R>(iters: u32, mut f: impl FnMut() -> R) -> f64 {
    // Warmup.
    for _ in 0..(iters / 10).max(1) {
        std::hint::black_box(f());
    }
    let start = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(f());
    }
    start.elapsed().as_nanos() as f64 / f64::from(iters)
}

/// Verbatim copy of the pre-#133 `build_server_variables` (String form) so
/// the bench's "old" leg measures the code that actually shipped, not the
/// post-fix wrapper.
#[allow(clippy::too_many_arguments)]
fn old_build_server_variables(
    method: &str,
    uri: &str,
    query_string: &str,
    script_filename: &std::path::Path,
    document_root: &std::path::Path,
    path: &str,
    server_name: &str,
    server_port: u16,
    protocol: &str,
    remote_addr: SocketAddr,
    is_https: bool,
    headers: &[(String, String)],
    env_vars: &[(String, String)],
) -> Vec<(String, String)> {
    fn cgi_header_key(name: &str) -> String {
        if name.eq_ignore_ascii_case("host") {
            return "HTTP_HOST".to_string();
        }
        if name.eq_ignore_ascii_case("cookie") {
            return "HTTP_COOKIE".to_string();
        }
        if name.eq_ignore_ascii_case("content-type") {
            return "CONTENT_TYPE".to_string();
        }
        if name.eq_ignore_ascii_case("content-length") {
            return "CONTENT_LENGTH".to_string();
        }
        let bytes = name.as_bytes();
        let mut out = String::with_capacity(5 + bytes.len());
        out.push_str("HTTP_");
        for b in bytes {
            let c = match *b {
                b'-' => b'_',
                b @ b'a'..=b'z' => b - 32,
                b => b,
            };
            out.push(char::from(c));
        }
        out
    }

    let script_name = script_filename
        .strip_prefix(document_root)
        .map_or_else(|_| path.to_owned(), |rel| format!("/{}", rel.to_string_lossy()));

    let mut vars = vec![
        ("REQUEST_METHOD".into(), method.to_owned()),
        ("REQUEST_URI".into(), uri.to_owned()),
        ("SCRIPT_FILENAME".into(), script_filename.to_string_lossy().into_owned()),
        ("SCRIPT_NAME".into(), script_name.clone()),
        ("DOCUMENT_ROOT".into(), document_root.to_string_lossy().into_owned()),
        ("SERVER_NAME".into(), server_name.to_owned()),
        ("SERVER_PORT".into(), server_port.to_string()),
        ("SERVER_SOFTWARE".into(), "ePHPm/0.1.0".into()),
        ("SERVER_PROTOCOL".into(), protocol.to_owned()),
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("QUERY_STRING".into(), query_string.to_owned()),
        ("PHP_SELF".into(), script_name),
        ("REMOTE_ADDR".into(), remote_addr.ip().to_string()),
        ("REMOTE_PORT".into(), remote_addr.port().to_string()),
        ("REDIRECT_STATUS".into(), "200".into()),
    ];
    if is_https {
        vars.push(("HTTPS".into(), "on".into()));
    }
    for (name, value) in headers {
        vars.push((cgi_header_key(name), value.clone()));
    }
    for (key, value) in env_vars {
        vars.push((key.clone(), value.clone()));
    }
    vars
}

/// #133 — the $_SERVER derivation + FFI-ready conversion, per dispatch mode.
#[test]
#[ignore = "timing probe — run manually in release mode with --nocapture"]
fn bench_server_vars() {
    let remote: SocketAddr = "203.0.113.9:52341".parse().unwrap();
    let script = PathBuf::from("/var/www/html/index.php");
    let docroot = PathBuf::from("/var/www/html");
    const ITERS: u32 = 200_000;

    for shape in shapes() {
        // Old path (pre-#133): build owned Strings, then convert every pair to
        // CString a second time — exactly what execute_php / the worker bridge
        // did before the fix.
        let old = time(ITERS, || {
            let vars = old_build_server_variables(
                "GET",
                "/products/42?ref=home",
                "ref=home",
                &script,
                &docroot,
                "/products/42",
                "shop.example.com",
                443,
                "HTTP/1.1",
                remote,
                true,
                &shape.headers,
                &shape.env_vars,
            );
            let c_vars: Vec<(CString, CString)> = vars
                .iter()
                .filter_map(|(k, v)| {
                    Some((CString::new(k.as_str()).ok()?, CString::new(v.as_str()).ok()?))
                })
                .collect();
            c_vars
        });

        // New path (#133): one FFI-ready build, static CStr for the
        // invariants — what execute_php and the worker bridge now do.
        let new = time(ITERS, || {
            build_server_variables_c(
                "GET",
                "/products/42?ref=home",
                "ref=home",
                &script,
                &docroot,
                "/products/42",
                "shop.example.com",
                443,
                "HTTP/1.1",
                remote,
                true,
                &shape.headers,
                &shape.env_vars,
            )
        });
        println!("server_vars/{:<40} old: {old:>6.0} ns/req  new: {new:>6.0} ns/req", shape.name);
    }

    // Keep the shipped String form exercised too, so a regression in the
    // wrapper (which the derivation tests rely on) would show up here.
    let shape = &shapes()[1];
    let wrapper = time(ITERS, || {
        build_server_variables(
            "GET",
            "/products/42?ref=home",
            "ref=home",
            &script,
            &docroot,
            "/products/42",
            "shop.example.com",
            443,
            "HTTP/1.1",
            remote,
            true,
            &shape.headers,
            &shape.env_vars,
        )
    });
    println!("server_vars String-form wrapper (tests only): {wrapper:>6.0} ns/req");
}

/// #134 — the response-header round-trip, Rust side.
#[test]
#[ignore = "timing probe — run manually in release mode with --nocapture"]
fn bench_response_headers() {
    // A Laravel-ish response header set.
    let headers: Vec<(&str, &str)> = vec![
        ("Content-Type", "text/html; charset=UTF-8"),
        ("Cache-Control", "no-cache, private"),
        ("Date", "Mon, 01 Sep 2026 12:00:00 GMT"),
        ("Set-Cookie", "XSRF-TOKEN=eyJpdiI6IjZ4TmpX; path=/; samesite=lax"),
        ("Set-Cookie", "laravel_session=eyJpdiI6IjZ4TmpX; path=/; httponly; samesite=lax"),
        ("X-Frame-Options", "SAMEORIGIN"),
        ("Vary", "Accept-Encoding"),
    ];
    const ITERS: u32 = 500_000;

    // Old path (pre-#134): the C side packs "Name: Value\n" text; Rust
    // re-parses it line by line. The pack half below stands in for the C
    // smart_str build (same byte traffic), the parse half is a copy of the
    // removed parse_packed_headers().
    let old = time(ITERS, || {
        let mut packed = Vec::with_capacity(256);
        for (name, value) in &headers {
            packed.extend_from_slice(name.as_bytes());
            packed.extend_from_slice(b": ");
            packed.extend_from_slice(value.as_bytes());
            packed.push(b'\n');
        }
        let parsed: Vec<(String, String)> = String::from_utf8_lossy(&packed)
            .lines()
            .filter_map(|line| {
                let line = line.trim_end_matches('\r');
                if line.is_empty() {
                    return None;
                }
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        parsed
    });
    println!("response_headers old pack+reparse path: {old:>8.0} ns/req");

    // New path (#134): the C side hands over parallel (ptr, len) arrays and
    // Rust copies each half straight into an owned String (with the same
    // trim the old parse applied). No pack buffer, no UTF-8 pass over a
    // concatenated blob, no line splitting.
    let new = time(ITERS, || {
        let parsed: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            .collect();
        parsed
    });
    println!("response_headers new structured path:   {new:>8.0} ns/req");
}

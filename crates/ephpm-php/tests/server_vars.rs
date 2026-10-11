//! Linked functional tests for `$_SERVER` registration through the SAPI.
//!
//! The wrapper used to copy at most 128 `$_SERVER` entries and silently drop
//! the rest. ePHPm's own per-site variables (`DB_*`, `EPHPM_REDIS_*`,
//! `EPHPM_NODE_ID`, `WS_*`) are registered after the request headers, so a
//! header-heavy request lost the tenant's DB credentials and WebSocket
//! context. These tests drive a real PHP request and read `$_SERVER` back.
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Run with:
//!
//! ```text
//! cargo test -p ephpm-php --test server_vars -- --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::sync::OnceLock;

use ephpm_php::PhpRuntime;
use ephpm_php::request::{EPHPM_VERSION, PhpRequest};
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

/// Run `body` as a PHP script with the given headers and injected env vars,
/// returning its output as a string.
fn run(
    name: &str,
    body: &str,
    headers: Vec<(String, String)>,
    env_vars: Vec<(String, String)>,
) -> String {
    init_once();
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path();
    let script = dir.join(name);
    std::fs::write(&script, body).expect("write test script");
    let resp = PhpRuntime::execute(PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script,
        headers,
        body: Vec::new(),
        content_type: None,
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        server_name: "localhost".into(),
        server_port: 8080,
        is_https: false,
        protocol: "HTTP/1.1".into(),
        env_vars,
        middleware: Vec::new(),
    })
    .expect("execute");
    assert_eq!(resp.status, 200, "body: {}", String::from_utf8_lossy(&resp.body));
    String::from_utf8(resp.body).expect("utf-8 output")
}

/// 200 headers puts the request well past the old 128-entry cap. Every
/// header and every ePHPm-injected per-site var must still reach PHP.
#[test]
#[serial]
fn injected_vars_survive_a_header_heavy_request() {
    let mut headers = vec![("host".to_string(), "localhost".to_string())];
    for i in 0..200 {
        headers.push((format!("x-filler-{i}"), format!("v{i}")));
    }
    let env_vars = vec![
        ("DB_HOST".to_string(), "127.0.0.1".to_string()),
        ("DB_USER".to_string(), "site-a".to_string()),
        ("DB_PASSWORD".to_string(), "secret-a".to_string()),
        ("EPHPM_NODE_ID".to_string(), "node-1".to_string()),
        ("WS_CONNECTION_ID".to_string(), "conn-1".to_string()),
    ];

    let out = run(
        "many_headers.php",
        "<?php foreach (['DB_HOST','DB_USER','DB_PASSWORD','EPHPM_NODE_ID',\
         'WS_CONNECTION_ID','HTTP_X_FILLER_0','HTTP_X_FILLER_199'] as $k) \
         { echo $k, '=', $_SERVER[$k] ?? '<missing>', \"\\n\"; } \
         echo 'COUNT=', count($_SERVER), \"\\n\";",
        headers,
        env_vars,
    );

    for line in [
        "DB_HOST=127.0.0.1",
        "DB_USER=site-a",
        "DB_PASSWORD=secret-a",
        "EPHPM_NODE_ID=node-1",
        "WS_CONNECTION_ID=conn-1",
        "HTTP_X_FILLER_0=v0",
        "HTTP_X_FILLER_199=v199",
    ] {
        assert!(out.lines().any(|l| l == line), "expected {line:?} in:\n{out}");
    }
    let count: usize = out
        .lines()
        .find_map(|l| l.strip_prefix("COUNT="))
        .and_then(|n| n.parse().ok())
        .expect("COUNT line");
    // 201 headers + 5 injected + the fixed CGI set: comfortably past 128.
    assert!(count > 201 + 5, "$_SERVER has only {count} entries:\n{out}");
}

/// `$_SERVER['SERVER_SOFTWARE']` reports the real build version, the same
/// value `ephpm --version` prints — not a frozen literal.
#[test]
#[serial]
fn server_software_reports_the_build_version() {
    let out = run(
        "server_software.php",
        "<?php echo $_SERVER['SERVER_SOFTWARE'];",
        vec![("host".into(), "localhost".into())],
        Vec::new(),
    );
    assert_eq!(out, format!("ePHPm/{EPHPM_VERSION}"));
}

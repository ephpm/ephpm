//! Linked functional tests for the structured response-header capture
//! (issue #134): the fpm path's headers must come back exactly as PHP set
//! them, without the old "Name: Value\n" pack + re-parse round-trip.
//!
//! Requires a real libphp link (`php_linked`); compiles to nothing in stub
//! mode. Run with:
//!
//! ```text
//! cargo test -p ephpm-php --release --test response_headers -- --test-threads=1
//! ```

#![cfg(all(test, php_linked))]

use std::sync::OnceLock;

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

fn run_script(name: &str, body: &str) -> PhpResponse {
    init_once();
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir")).path();
    let script = dir.join(name);
    std::fs::write(&script, body).expect("write test script");
    PhpRuntime::execute(PhpRequest {
        method: "GET".into(),
        uri: format!("/{name}"),
        path: format!("/{name}"),
        query_string: String::new(),
        document_root: script.parent().unwrap().to_path_buf(),
        script_filename: script,
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
    })
    .expect("execute")
}

fn values<'a>(resp: &'a PhpResponse, name: &str) -> Vec<&'a str> {
    resp.headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

#[test]
#[serial]
fn explicit_headers_come_back_split_and_exact() {
    let resp = run_script(
        "hdr_exact.php",
        "<?php header('X-One: alpha'); header('Cache-Control: no-cache, private'); echo 'ok';",
    );
    assert_eq!(resp.status, 200);
    assert_eq!(values(&resp, "X-One"), vec!["alpha"]);
    // A value containing ': ' must not be re-split on it.
    assert_eq!(values(&resp, "Cache-Control"), vec!["no-cache, private"]);
}

#[test]
#[serial]
fn duplicate_headers_arrive_as_distinct_entries() {
    let resp = run_script(
        "hdr_dup.php",
        "<?php setcookie('a', '1', ['path' => '/']); setcookie('b', '2', ['path' => '/']); echo 'ok';",
    );
    let cookies = values(&resp, "Set-Cookie");
    assert_eq!(cookies.len(), 2, "both Set-Cookie headers must survive: {cookies:?}");
    assert!(cookies[0].starts_with("a=1"));
    assert!(cookies[1].starts_with("b=2"));
}

#[test]
#[serial]
fn header_without_space_after_colon_is_delivered() {
    // The old fpm parse split on ": " and silently DROPPED this header; the
    // structured capture splits on the colon and skips optional whitespace.
    let resp = run_script("hdr_nospace.php", "<?php header('X-Tight:packed'); echo 'ok';");
    assert_eq!(values(&resp, "X-Tight"), vec!["packed"]);
}

#[test]
#[serial]
fn default_content_type_is_synthesized_when_script_sets_none() {
    let resp = run_script("hdr_default_ct.php", "<?php echo 'ok';");
    let cts = values(&resp, "Content-Type");
    assert_eq!(cts.len(), 1, "exactly one synthesized Content-Type: {cts:?}");
    assert!(cts[0].starts_with("text/html"), "unexpected default Content-Type: {}", cts[0]);
}

#[test]
#[serial]
fn explicit_content_type_wins_over_synthesis() {
    let resp = run_script(
        "hdr_explicit_ct.php",
        "<?php header('Content-Type: application/json'); echo '{}';",
    );
    let cts = values(&resp, "Content-Type");
    assert_eq!(cts, vec!["application/json"]);
}

//! Shared harness for the `memory_limit` ceiling tests: one engine per test
//! binary, each request on a fresh thread the way a pool thread runs it.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::OnceLock;

use ephpm_php::request::PhpRequest;
use ephpm_php::response::PhpResponse;
use ephpm_php::{PhpError, PhpRuntime};
use tempfile::TempDir;

static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();

/// Start the engine once per process, running `before` (pre-init settings)
/// first.
pub fn init_once(before: impl FnOnce()) {
    INIT.get_or_init(|| {
        before();
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

/// Run one request on a fresh thread with per-request INI values set through
/// `PhpRuntime::set_request_ini` (the router's per-site channel), then tear it
/// down as the pool would.
pub fn run(
    name: &'static str,
    body: String,
    ini: &'static [(&'static str, &'static str)],
) -> Result<PhpResponse, PhpError> {
    let path = script(name, &body);
    std::thread::Builder::new()
        .name(format!("ceiling-{name}"))
        .stack_size(ephpm_php::PHP_THREAD_STACK)
        .spawn(move || {
            for (key, value) in ini {
                PhpRuntime::set_request_ini(key, value);
            }
            let out = PhpRuntime::execute(request(path, name));
            if !matches!(out, Err(PhpError::Contained(_))) {
                PhpRuntime::finish_request().expect("teardown");
                PhpRuntime::worker_thread_shutdown();
            }
            out
        })
        .expect("spawn request thread")
        .join()
        .expect("request thread must not panic")
}

/// The response body as text.
pub fn text(resp: &PhpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

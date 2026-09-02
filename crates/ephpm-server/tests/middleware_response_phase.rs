//! Coverage for the **response phase** across the dlopen C ABI — the optional
//! `ephpm_middleware_invoke_response` symbol, the `EphpmResponseCtx` read
//! accessors, and the `EphpmResponseEdit` write-back, driven through a real
//! shared library built by `declare!(Type, response)`.
//!
//! The fixture is the `mw_response_gzip` cdylib example (a genuine gzip
//! response compressor). Like the `mw_probe_*` fixtures it is a
//! `crate-type = ["cdylib"]` example, and like them it has to be **built
//! explicitly** — `cargo build --workspace --lib --examples`, which CI runs
//! before its test step; `cargo test` alone does not emit example artifacts
//! (see `middleware_dlopen.rs`). [`fixture`] hard-fails with that build command
//! rather than skipping, so this lane can never go silently uncovered.
//!
//! The request-phase-only `mw_probe_v1` fixture (which does **not** export the
//! response symbol) is reused here to prove the presence check: a v1 module is
//! skipped by the response phase, and a chain of only such modules reports no
//! response phase at all.

use std::io::Read;
use std::path::{Path, PathBuf};

use ephpm_config::MiddlewareMount;
use ephpm_middleware::host::RequestCtx;
use ephpm_server::middleware::MiddlewareChain;
use flate2::read::GzDecoder;

/// Directory cargo drops example artifacts in (see `middleware_dlopen.rs`).
fn examples_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary has a path");
    exe.parent()
        .and_then(Path::parent)
        .expect("test binary lives at <target>/<profile>/deps/<name>")
        .join("examples")
}

/// A fixture library staged at a path unique to one test (private per-test
/// load; see `middleware_dlopen.rs::Fixture` for why the copy matters).
struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Fixture {
    fn path(&self) -> &Path {
        &self.path
    }
}

/// Stage a fixture cdylib, or fail with the exact build command — never skip.
fn fixture(stem: &str) -> Fixture {
    let file =
        format!("{}{stem}.{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_EXTENSION);
    let built = examples_dir().join(&file);
    assert!(
        built.is_file(),
        "response-phase fixture `{file}` is missing at {} — build the cdylib examples with \
         `cargo build -p ephpm-server --examples`.",
        built.display()
    );
    let dir = tempfile::tempdir().expect("stage fixture");
    let path = dir.path().join(&file);
    std::fs::copy(&built, &path).expect("copy fixture into staging dir");
    Fixture { _dir: dir, path }
}

fn mount(
    path: &Path,
    pattern: Option<&str>,
    order: u32,
    config: serde_json::Value,
) -> MiddlewareMount {
    MiddlewareMount {
        library: path.to_string_lossy().into_owned(),
        match_pattern: pattern.map(str::to_owned),
        order,
        config: Some(config),
    }
}

fn find<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut out).expect("valid gzip stream");
    out
}

/// A request context that advertises gzip support.
fn ctx_accepts_gzip() -> RequestCtx {
    RequestCtx::new(
        "GET",
        "/page.html",
        "",
        "203.0.113.5",
        "resp.example",
        &[("Accept-Encoding".to_owned(), "gzip, deflate".to_owned())],
    )
}

/// The full response-phase lifecycle over the C ABI: a gzip module reads the
/// generated body through `EphpmResponseCtx`, replaces it, and rewrites the
/// headers via `EphpmResponseEdit`. Proves the body round-trips (gunzip ==
/// original), the encoding header is set, the stale ETag is dropped, and the
/// host flagged the body as replaced.
#[test]
fn dynamic_response_module_gzips_a_buffered_response() {
    let lib = fixture("mw_response_gzip");
    let chain = MiddlewareChain::load(&[mount(lib.path(), None, 10, serde_json::json!({}))])
        .expect("gzip module loads through dlopen");
    assert!(chain.has_response_phase(), "the gzip module exports the response symbol");

    let original = b"<html><body>".repeat(64);
    let headers = vec![
        ("Content-Type".to_owned(), "text/html".to_owned()),
        ("Content-Length".to_owned(), original.len().to_string()),
        ("ETag".to_owned(), "\"abc123\"".to_owned()),
    ];

    let outcome =
        chain.run_response_phase(&ctx_accepts_gzip(), "/page.html", 200, headers, original.clone());

    assert_eq!(outcome.status, 200);
    assert!(outcome.body_replaced, "the gzip module replaced the body");
    assert_eq!(find(&outcome.headers, "Content-Encoding"), Some("gzip"));
    assert_eq!(find(&outcome.headers, "Vary"), Some("Accept-Encoding"));
    assert!(find(&outcome.headers, "ETag").is_none(), "stale ETag must be dropped");
    assert!(outcome.body.len() < original.len(), "gzip must shrink this repetitive body");
    assert_eq!(gunzip(&outcome.body), original, "gunzip(body) round-trips to the original");
}

/// The module is conservative: with no `Accept-Encoding: gzip` on the request,
/// it leaves the response untouched — proving the response phase can read the
/// **request** context across the ABI and act on it.
#[test]
fn dynamic_response_module_skips_when_client_does_not_accept_gzip() {
    let lib = fixture("mw_response_gzip");
    let chain = MiddlewareChain::load(&[mount(lib.path(), None, 10, serde_json::json!({}))])
        .expect("gzip module loads");

    let original = b"<html><body>".repeat(64);
    let ctx = RequestCtx::new("GET", "/page.html", "", "203.0.113.5", "resp.example", &[]);
    let outcome = chain.run_response_phase(
        &ctx,
        "/page.html",
        200,
        vec![("Content-Type".to_owned(), "text/html".to_owned())],
        original.clone(),
    );

    assert!(!outcome.body_replaced, "no Accept-Encoding: gzip -> body untouched");
    assert_eq!(outcome.body, original);
    assert!(find(&outcome.headers, "Content-Encoding").is_none());
}

/// A request-phase-only module (`mw_probe_v1`, no response symbol) is skipped
/// by the response phase; a chain of only such modules reports none. Mixed
/// with the gzip module, the transform still happens — the presence check is
/// per module.
#[test]
fn modules_without_the_response_symbol_are_skipped() {
    let probe = fixture("mw_probe_v1");
    let gzip = fixture("mw_response_gzip");

    // A chain of only request-phase modules has no response phase at all.
    let probe_only =
        MiddlewareChain::load(&[mount(probe.path(), None, 10, serde_json::json!({ "tag": "t" }))])
            .expect("probe loads");
    assert!(!probe_only.has_response_phase(), "a v1 module contributes no response phase");

    // Mixed chain: the probe is skipped in the response phase, the gzip module
    // still transforms.
    let mixed = MiddlewareChain::load(&[
        mount(probe.path(), None, 10, serde_json::json!({ "tag": "t" })),
        mount(gzip.path(), None, 20, serde_json::json!({})),
    ])
    .expect("mixed chain loads");
    assert!(mixed.has_response_phase());

    let original = b"abcdefghij".repeat(16);
    let outcome = mixed.run_response_phase(
        &ctx_accepts_gzip(),
        "/page.html",
        200,
        vec![("Content-Type".to_owned(), "text/plain".to_owned())],
        original.clone(),
    );
    assert!(outcome.body_replaced, "the gzip module transformed despite the v1 probe in the chain");
    assert_eq!(gunzip(&outcome.body), original);
}

/// A response module whose `match` glob does not match the request path is
/// skipped in the response phase, exactly like the request phase.
#[test]
fn response_phase_respects_the_match_glob() {
    let gzip = fixture("mw_response_gzip");
    let chain =
        MiddlewareChain::load(&[mount(gzip.path(), Some("/api/*"), 10, serde_json::json!({}))])
            .expect("gzip module loads");

    let original = b"x".repeat(256);

    // Off-glob: untouched.
    let outcome = chain.run_response_phase(
        &ctx_accepts_gzip(),
        "/page.html",
        200,
        vec![("Content-Type".to_owned(), "text/plain".to_owned())],
        original.clone(),
    );
    assert!(!outcome.body_replaced, "off-glob request must not be compressed");

    // On-glob: transformed. (Path is what the glob matches on.)
    let ctx = RequestCtx::new(
        "GET",
        "/api/data",
        "",
        "203.0.113.5",
        "resp.example",
        &[("Accept-Encoding".to_owned(), "gzip".to_owned())],
    );
    let outcome = chain.run_response_phase(
        &ctx,
        "/api/data",
        200,
        vec![("Content-Type".to_owned(), "text/plain".to_owned())],
        original.clone(),
    );
    assert!(outcome.body_replaced, "on-glob request must be compressed");
    assert_eq!(gunzip(&outcome.body), original);
}

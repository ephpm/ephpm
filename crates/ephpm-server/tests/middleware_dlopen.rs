//! Coverage for the **dlopen middleware lane** — `MiddlewareChain::load`'s
//! shared-library path: `libloading::Library::new`, the three mandatory symbol
//! resolutions, the ABI major-version handshake, and `init`/`invoke`/
//! `shutdown` across the C boundary.
//!
//! Why this file exists: the four in-tree modules are well covered, but they
//! all take the *static registry* path (`Backend::Builtin`), which never
//! touches dlopen. The dynamic lane — the one the docs promise "works out of
//! the box with the stock release binaries on every platform" — had no
//! enforced test. The single test that did exercise it skipped itself when no
//! cdylib was on disk, and no CI job ever built one: `cargo test --workspace`
//! does **not** emit a library crate's `cdylib` output (verified), so the skip
//! fired every time.
//!
//! The fixtures are `crate-type = ["cdylib"]` examples of this crate
//! (`examples/mw_probe_*.rs`), which cargo *does* build during `cargo test -p
//! ephpm-server`. [`fixture`] hard-fails with the build command when one is
//! missing — a skip here is indistinguishable from the bug this file closes.
//!
//! End-to-end coverage of a *shipped* module (`ephpm-middleware-cors`) mounted
//! by path in a real server's `[[middleware]]` config lives in the E2E suite,
//! `crates/ephpm-e2e/tests/middleware.rs`.
// The handshake test calls a module's C entrypoint directly — that is the
// point of it. Every unsafe block below carries its own SAFETY note.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use ephpm_config::MiddlewareMount;
use ephpm_middleware::host::RequestCtx;
use ephpm_server::middleware::{ChainVerdict, MiddlewareChain};

// ── fixture location ──────────────────────────────────────────────────────

/// Directory cargo drops example artifacts in, derived from this test
/// binary's own path (`<target>/<profile>/deps/<test>` →
/// `<target>/<profile>/examples`).
///
/// Deriving it from `current_exe` rather than `CARGO_TARGET_DIR` +
/// `"debug"`/`"release"` guesswork keeps this correct under a custom target
/// dir, a `--release` run, and a `--target <triple>` build alike.
fn examples_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary has a path");
    exe.parent()
        .and_then(Path::parent)
        .expect("test binary lives at <target>/<profile>/deps/<name>")
        .join("examples")
}

/// A fixture library staged at a path unique to one test.
///
/// The copy is not incidental. `declare!` keeps a module's config and host
/// table in `OnceLock`s, and both `dlopen` and `dyld` key their load cache on
/// (device, inode) — so two tests in this binary (integration tests share one
/// process) that loaded the *same* file would share one initialised instance:
/// the second `init` would silently keep the first test's config, and whether
/// that happened would depend on thread scheduling. A private copy per test
/// gives each one its own load and its own statics.
struct Fixture {
    /// Holds the staging directory open for the lifetime of the load.
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Fixture {
    fn path(&self) -> &Path {
        &self.path
    }
}

/// Stage a fixture cdylib for exclusive use by the calling test, or fail with
/// the exact build command.
///
/// Never skips. An absent artifact is a build-wiring regression, and quietly
/// passing on it is the precise failure mode this file exists to close.
fn fixture(stem: &str) -> Fixture {
    let file =
        format!("{}{stem}.{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_EXTENSION);
    let built = examples_dir().join(&file);
    assert!(
        built.is_file(),
        "middleware fixture `{file}` is missing at {} — the dlopen tests require the cdylib \
         example fixtures. Build them with `cargo build -p ephpm-server --examples`.",
        built.display()
    );
    let dir = tempfile::tempdir().expect("stage fixture");
    // Keep the file name (minus uniqueness) so loader errors stay readable.
    let path = dir.path().join(&file);
    std::fs::copy(&built, &path).expect("copy fixture into staging dir");
    Fixture { _dir: dir, path }
}

/// A mount for an explicit library path (the loader treats any value with a
/// separator or an extension as a path, bypassing the search directories).
fn mount_path(path: &Path, config: Option<serde_json::Value>) -> MiddlewareMount {
    MiddlewareMount {
        library: path.to_string_lossy().into_owned(),
        match_pattern: None,
        order: 10,
        config,
    }
}

fn find<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

// ── happy path: dlopen → init → invoke → shutdown ─────────────────────────

/// The full dynamic lifecycle against a real shared library built through the
/// real `declare!` macro. Asserts on the *content* crossing the ABI, not just
/// on "it loaded": every host-table request accessor is echoed back through
/// `response_headers`, so a mis-ordered or truncated `EphpmHostV1` shows up
/// here as a wrong value rather than passing silently.
#[test]
fn dynamic_module_loads_and_round_trips_request_context() {
    let lib = fixture("mw_probe_v1");
    let mounts = vec![mount_path(lib.path(), Some(serde_json::json!({ "tag": "fixture-alpha" })))];

    let chain = MiddlewareChain::load(&mounts).expect("probe fixture loads through dlopen");
    assert_eq!(chain.len(), 1);
    assert!(!chain.is_empty());

    let ctx = RequestCtx::new(
        "POST",
        "/app/index.php",
        "a=1&b=2",
        "198.51.100.9",
        "probe.example",
        &[("Accept".to_owned(), "text/html".to_owned())],
    );
    match chain.evaluate(&ctx, "/app/index.php") {
        ChainVerdict::Continue { rewrite_path, header_overrides, response_headers } => {
            assert!(rewrite_path.is_none());
            assert!(header_overrides.is_empty());
            // Config reached the module's `init`.
            assert_eq!(find(&response_headers, "X-Probe-Tag"), Some("fixture-alpha"));
            // Every request accessor on the host table, across the boundary.
            assert_eq!(find(&response_headers, "X-Probe-Method"), Some("POST"));
            assert_eq!(find(&response_headers, "X-Probe-Path"), Some("/app/index.php"));
            assert_eq!(find(&response_headers, "X-Probe-Query"), Some("a=1&b=2"));
            assert_eq!(find(&response_headers, "X-Probe-Remote-Ip"), Some("198.51.100.9"));
            assert_eq!(find(&response_headers, "X-Probe-Vhost"), Some("probe.example"));
            assert_eq!(find(&response_headers, "X-Probe-Accept"), Some("text/html"));
        }
        ChainVerdict::Respond { status, .. } => {
            panic!("expected CONTINUE from the probe fixture, got RESPOND {status}")
        }
    }

    // RESPOND marshaling (status + body + headers) over the same boundary.
    let ctx = RequestCtx::new("GET", "/__probe/deny", "", "198.51.100.9", "probe.example", &[]);
    match chain.evaluate(&ctx, "/__probe/deny") {
        ChainVerdict::Respond { status, body, headers } => {
            assert_eq!(status, 418);
            assert_eq!(body, b"probe denied");
            assert_eq!(find(&headers, "X-Probe-Action"), Some("respond"));
        }
        ChainVerdict::Continue { .. } => panic!("/__probe/deny must short-circuit"),
    }

    // Dropping runs `shutdown` through the ABI, then dlclose.
    drop(chain);
}

// ── ABI version handshake ─────────────────────────────────────────────────

/// The handshake, exercised directly against a real module's exported
/// `ephpm_middleware_init` — the riskiest untested piece, because a module
/// whose struct layout disagrees with the host's is memory corruption if it
/// loads.
///
/// Both directions are asserted in one test so the rejection cannot pass for
/// the wrong reason: a bumped major is refused, and the **negative control**
/// (the host's actual `ABI_V1`) is accepted by the very same symbol on the
/// very same library handle. Without the control, a module that refused
/// *everything* would look identical to a working version gate.
#[test]
fn abi_major_mismatch_is_refused_and_the_current_major_is_accepted() {
    let lib_path = fixture("mw_probe_v1");
    // SAFETY: loading a middleware library runs its initialisers with this
    // process's privileges — the documented v1 trust model, and the fixture is
    // built from this workspace.
    let lib = unsafe { libloading::Library::new(lib_path.path()) }.expect("dlopen probe fixture");
    // SAFETY: the symbol is declared with the documented ABI signature; this
    // fixture is generated by `declare!`, which emits exactly that shape.
    let init: ephpm_middleware::abi::InitFn =
        *unsafe { lib.get(ephpm_middleware::abi::SYM_INIT) }.expect("init symbol present");

    let host = std::ptr::from_ref(ephpm_middleware::host::host_table());
    let config = std::ffi::CString::new(r#"{"tag":"handshake"}"#).unwrap();

    // A host one ABI major ahead of the module must be refused. Order matters:
    // the refusal is asserted BEFORE the accepted call, because `declare!`
    // stashes the host table in a `OnceLock` and a successful init first would
    // make the refusal unfalsifiable.
    let future_major = ephpm_middleware::abi::ABI_V1 + (1 << 24);
    // SAFETY: `config` is NUL-terminated and outlives the call; the host table
    // is 'static.
    let rc = unsafe { init(future_major, config.as_ptr(), host) };
    assert_ne!(rc, 0, "a module built for ABI major 1 must refuse an ABI major 2 host");

    // A null host table is refused on the same gate.
    // SAFETY: as above; a null host pointer is an explicitly handled input.
    let rc = unsafe { init(ephpm_middleware::abi::ABI_V1, config.as_ptr(), std::ptr::null()) };
    assert_ne!(rc, 0, "a null host table must be refused");

    // Negative control: the real major, same symbol, same handle, must succeed.
    // SAFETY: as above.
    let rc = unsafe { init(ephpm_middleware::abi::ABI_V1, config.as_ptr(), host) };
    assert_eq!(rc, 0, "the host's own ABI major must be accepted (negative control)");
}

/// A module built against a *future* ABI major must abort startup with a
/// message an operator can act on — not load, and not be skipped over.
///
/// This is the version-skew direction that actually bites in production: the
/// host bumps `ABI_V1`, and modules already on disk refuse the new host. The
/// fixture emulates the post-bump world by refusing today's major.
#[test]
fn future_abi_module_aborts_startup_with_a_named_error() {
    let lib = fixture("mw_probe_abi_v2");
    let err = match MiddlewareChain::load(&[mount_path(lib.path(), None)]) {
        Ok(chain) => panic!(
            "a module declaring a different ABI major must not load (loaded {} module(s))",
            chain.len()
        ),
        Err(e) => format!("{e:#}"),
    };
    // Names the mount, says the module refused, and carries the refusal code.
    assert!(err.contains("mw_probe_abi_v2"), "{err}");
    assert!(err.contains("init returned -1"), "{err}");
    assert!(err.contains("refused to start"), "{err}");
}

// ── loader failure modes ──────────────────────────────────────────────────

/// A mount whose required symbols are incomplete must fail at startup, naming
/// the symbol — not dlopen successfully and fault on the first request.
#[test]
fn missing_required_symbol_aborts_startup_naming_the_symbol() {
    let lib = fixture("mw_probe_no_invoke");
    let err = match MiddlewareChain::load(&[mount_path(lib.path(), None)]) {
        Ok(_) => panic!("a module without ephpm_middleware_invoke must not load"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("missing required symbol"), "{err}");
    assert!(err.contains("ephpm_middleware_invoke"), "{err}");
    assert!(err.contains("mw_probe_no_invoke"), "{err}");
}

/// An explicit path that does not exist fails before dlopen, quoting the path.
#[test]
fn missing_library_path_aborts_startup_with_the_path() {
    let missing = examples_dir().join("definitely-not-here.so");
    let err = match MiddlewareChain::load(&[mount_path(&missing, None)]) {
        Ok(_) => panic!("a nonexistent library path must not load"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("explicit path"), "{err}");
    assert!(err.contains("definitely-not-here.so"), "{err}");
}

/// A file that exists but is not a loadable object must surface the platform
/// loader's own error, with the path attached — the "I copied the wrong file /
/// wrong architecture" case.
#[test]
fn garbage_library_file_aborts_startup_with_dlopen_context() {
    let dir = tempfile::tempdir().expect("tempdir");
    let junk = dir.path().join(format!("junk.{}", std::env::consts::DLL_EXTENSION));
    std::fs::write(&junk, b"\x7fELF this is not actually an object file").expect("write junk");

    let err = match MiddlewareChain::load(&[mount_path(&junk, None)]) {
        Ok(_) => panic!("a non-object file must not load as middleware"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("dlopen failed for"), "{err}");
    assert!(err.contains("junk."), "{err}");
    // The mount is named in the outer context, so the operator knows which
    // `[[middleware]]` entry to fix.
    assert!(err.contains("failed to load middleware"), "{err}");
}

/// A dynamic module whose `init` rejects its configuration must abort startup
/// carrying the module's own return code — the fail-fast contract that keeps a
/// misconfigured auth module from silently not running.
#[test]
fn dynamic_init_failure_aborts_startup() {
    // The probe fixture requires a string `tag`; mount it without one.
    let lib = fixture("mw_probe_v1");
    let err = match MiddlewareChain::load(&[mount_path(lib.path(), Some(serde_json::json!({})))]) {
        Ok(_) => panic!("the probe fixture must refuse a config with no `tag`"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("mw_probe_v1"), "{err}");
    assert!(err.contains("init returned -3"), "{err}");
}

// ── chain composition ─────────────────────────────────────────────────────

/// Two dlopened mounts in one chain, ordered, with globs. Proves multiple
/// dynamic modules compose — the router sees one uniform `ChainVerdict`, and an
/// off-glob mount is skipped. (The static builtin registry is empty in stock
/// binaries; the official modules load through this same dlopen lane, so
/// "compose two dynamic mounts" is the realistic chain.)
#[test]
fn multiple_dynamic_modules_compose_in_one_chain() {
    // Separate temp copies so dlopen's (device, inode) cache does not collapse
    // the two mounts into one shared, first-config-wins instance.
    let always = fixture("mw_probe_v1");
    let api_only = fixture("mw_probe_v1");
    let mounts = vec![
        MiddlewareMount {
            library: always.path().to_string_lossy().into_owned(),
            match_pattern: None,
            order: 10,
            config: Some(serde_json::json!({ "tag": "always" })),
        },
        MiddlewareMount {
            library: api_only.path().to_string_lossy().into_owned(),
            match_pattern: Some("/api/*".to_owned()),
            order: 20,
            config: Some(serde_json::json!({ "tag": "api-only" })),
        },
    ];
    let chain = MiddlewareChain::load(&mounts).expect("two dynamic mounts load");
    assert_eq!(chain.len(), 2);

    let tags = |rh: &[(String, String)]| -> Vec<String> {
        rh.iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("X-Probe-Tag"))
            .map(|(_, v)| v.clone())
            .collect()
    };

    // Off-glob: only the no-glob mount contributes; the /api/* mount is skipped.
    let ctx = RequestCtx::new("GET", "/index.php", "", "203.0.113.1", "mixed", &[]);
    let ChainVerdict::Continue { response_headers, .. } = chain.evaluate(&ctx, "/index.php") else {
        panic!("expected CONTINUE off-glob");
    };
    assert_eq!(tags(&response_headers), vec!["always".to_owned()]);

    // On-glob: both mounts run, in `order`.
    let ctx = RequestCtx::new("GET", "/api/v1.php", "", "203.0.113.1", "mixed", &[]);
    let ChainVerdict::Continue { response_headers, .. } = chain.evaluate(&ctx, "/api/v1.php")
    else {
        panic!("expected CONTINUE on-glob");
    };
    assert_eq!(tags(&response_headers), vec!["always".to_owned(), "api-only".to_owned()]);
}

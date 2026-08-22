//! Loads the built `api-gate` cdylib exactly the way ePHPm's loader does and
//! exercises the parts of the C ABI a unit test cannot reach: the four
//! exported symbols, the version handshake, and one `invoke` marshaled back
//! out of module memory through the raw `EphpmResponse` struct.
//!
//! This is a debugging aid for module authors — point it at your own module
//! and it tells you whether the host will accept it, without standing up a
//! server.
//!
//! ```text
//! cargo build --release -p ephpm-middleware-example
//! cargo run --release -p ephpm-middleware-example --example abi_probe
//! ```
//!
//! It resolves the library next to its own binary, so build both with the
//! same `--release`/`--target` flags.
#![allow(unsafe_code)] // The whole point: call a C ABI by hand.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

use ephpm_middleware::abi::{
    self, ACTION_CONTINUE, ACTION_RESPOND, EphpmResponse, InitFn, InvokeFn, ShutdownFn,
};
use ephpm_middleware::host::{RequestCtx, host_table, set_kv_store};

/// The cdylib sits one directory up from `.../examples/abi_probe`.
fn library_path() -> PathBuf {
    let exe = std::env::current_exe().expect("probe has a path");
    let dir = exe.parent().and_then(Path::parent).expect("<profile>/examples/<bin>");
    dir.join(format!("{}api_gate{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX))
}

/// A config the module accepts.
fn good_config() -> CString {
    CString::new(r#"{"keys":{"k-alpha":"alpha"},"prefix":"/api/","requests_per_window":1}"#)
        .expect("no interior NUL")
}

fn main() {
    // The module's KV callbacks need a store behind them, exactly as the
    // server wires one up before loading any middleware.
    set_kv_store(&ephpm_kv::store::Store::new(ephpm_kv::store::StoreConfig::default()));

    let path = library_path();
    assert!(
        path.is_file(),
        "module not built at {} — run `cargo build -p ephpm-middleware-example` with the same \
         --release/--target flags as this probe",
        path.display()
    );
    println!("module:  {}", path.display());

    // SAFETY: loading a middleware module runs its initialisers with this
    // process's privileges — the documented v1 trust model. This artifact is
    // built from this workspace.
    let lib = unsafe { libloading::Library::new(&path) }.expect("dlopen the module");

    // ── 1. all four symbols resolve ───────────────────────────────────────
    // SAFETY: each symbol is declared with the ABI signature `declare!` emits.
    let init: InitFn = *unsafe { lib.get(abi::SYM_INIT) }.expect("ephpm_middleware_init");
    // SAFETY: as above.
    let invoke: InvokeFn = *unsafe { lib.get(abi::SYM_INVOKE) }.expect("ephpm_middleware_invoke");
    // SAFETY: as above.
    let shutdown: ShutdownFn =
        *unsafe { lib.get(abi::SYM_SHUTDOWN) }.expect("ephpm_middleware_shutdown");
    // SAFETY: as above; `describe` is optional but this module exports it.
    let describe: abi::DescribeFn =
        *unsafe { lib.get(abi::SYM_DESCRIBE) }.expect("ephpm_middleware_describe");
    println!("symbols: init, invoke, shutdown, describe — all resolved");

    // SAFETY: the module returns a 'static NUL-terminated string.
    let name = unsafe { CStr::from_ptr(describe()) }.to_string_lossy().into_owned();
    println!("describe: {name}");

    let host = std::ptr::from_ref(host_table());
    let config = good_config();

    // ── 2. version handshake ──────────────────────────────────────────────
    // Refusals are asserted BEFORE the accepted call: `declare!` stashes the
    // host table and instance in `OnceLock`s, so a successful init first would
    // make the refusal unfalsifiable.
    let future_major = abi::ABI_V1 + (1 << 24);
    // SAFETY: `config` outlives the call; the host table is 'static.
    let rc = unsafe { init(future_major, config.as_ptr(), host) };
    assert_ne!(rc, 0, "a module built for ABI major 1 must refuse an ABI major 2 host");
    println!("handshake: host major 2 refused (rc = {rc})");

    // SAFETY: as above; a null host table is an explicitly handled input.
    let rc = unsafe { init(abi::ABI_V1, config.as_ptr(), std::ptr::null()) };
    assert_ne!(rc, 0, "a null host table must be refused");
    println!("handshake: null host table refused (rc = {rc})");

    let bad = CString::new(r#"{"prefix":"/api/"}"#).expect("no interior NUL");
    // SAFETY: as above.
    let rc = unsafe { init(abi::ABI_V1, bad.as_ptr(), host) };
    assert_ne!(rc, 0, "a config with no `keys` must be refused");
    println!("handshake: config without `keys` refused (rc = {rc})");

    // Negative control: without this, a module that refused *everything*
    // would look identical to a working version gate.
    // SAFETY: as above.
    let rc = unsafe { init(abi::ABI_V1, config.as_ptr(), host) };
    assert_eq!(rc, 0, "the host's own ABI major with a valid config must be accepted");
    println!("handshake: ABI_V1 + valid config accepted (rc = 0)");

    // ── 3. one CONTINUE and one RESPOND, read back through the raw struct ─
    let ctx = RequestCtx::new("GET", "/health.php", "", "198.51.100.4", "probe", &[]);
    let mut out = zeroed_response();
    // SAFETY: `ctx` outlives the call; `out` is a valid, caller-owned struct.
    let rc = unsafe { invoke(ctx.as_abi(), &raw mut out) };
    assert_eq!(rc, 0);
    assert_eq!(out.action, ACTION_CONTINUE, "an un-gated path must CONTINUE");
    println!("invoke:  /health.php -> ACTION_CONTINUE");

    let ctx = RequestCtx::new("GET", "/api/v1/users", "", "198.51.100.4", "probe", &[]);
    let mut out = zeroed_response();
    // SAFETY: as above.
    let rc = unsafe { invoke(ctx.as_abi(), &raw mut out) };
    assert_eq!(rc, 0);
    assert_eq!(out.action, ACTION_RESPOND, "a gated path with no key must RESPOND");
    assert_eq!(out.status, 401);
    // The pointer-lifetime rule in practice: these are still valid here,
    // before `invoke` is called again on this thread.
    // SAFETY: `body`/`body_len` were written by the module and remain valid.
    let body = unsafe { std::slice::from_raw_parts(out.body, out.body_len) };
    println!(
        "invoke:  /api/v1/users -> ACTION_RESPOND {} {:?} ({} header(s))",
        out.status,
        String::from_utf8_lossy(body),
        out.header_overrides_len
    );

    // SAFETY: the module was initialised above.
    unsafe { shutdown() };
    println!("shutdown: ok");
    println!("\nOK — the module satisfies the v1 ABI.");
}

/// The host zero-initialises the verdict struct before every `invoke`.
fn zeroed_response() -> EphpmResponse {
    EphpmResponse {
        action: ACTION_CONTINUE,
        status: 0,
        body: std::ptr::null(),
        body_len: 0,
        rewrite_path: std::ptr::null(),
        header_overrides: std::ptr::null(),
        header_overrides_len: 0,
        response_headers: std::ptr::null(),
        response_headers_len: 0,
    }
}

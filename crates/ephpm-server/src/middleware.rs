//! Native middleware: static registry, dlopen loader, and per-request chain.
//!
//! Builds the chain declared in `[[middleware]]` at startup (fail-fast: a
//! broken mount aborts server startup) and evaluates it per PHP-bound
//! request — before any body bytes are read, so a `RESPOND` verdict never
//! pays for the body transfer.
//!
//! Each mount resolves through two lanes, in order:
//!
//! 1. **Builtin registry** ([`builtin`]) — the four in-tree modules compiled
//!    into this binary and invoked in-process (no FFI, no dlopen). Works in
//!    every binary, including custom fully static builds where `dlopen` does
//!    not exist.
//! 2. **Shared library** — everything else goes through the dlopen path and
//!    the versioned C ABI host table, for out-of-tree modules. Works with the
//!    stock release binaries on every platform (the Linux release is
//!    glibc-dynamic).
//!
//! **Coverage (two phases):**
//!
//! - The **request phase** ([`MiddlewareChain::evaluate`], `CONTINUE`/
//!   `RESPOND`/`REWRITE`) runs before a request is served. It runs on the PHP
//!   path (inside `Router::handle_php`) **and**, as of the response-phase work
//!   (issue #395), on the **static-file** path — before the file is read —
//!   through `Router::static_request_phase`. A `RESPOND` there (e.g. an auth
//!   denial) short-circuits, so a middleware mount CAN now gate confidential
//!   static assets: the file's bytes never leave disk. It fails **closed**.
//! - The **response phase** ([`MiddlewareChain::run_response_phase`]) runs
//!   *after* the response is generated, in **reverse** chain order, letting
//!   response-capable modules transform status/headers/body (compression,
//!   ETag, header injection). It runs on PHP, static, worker-buffered, and
//!   error responses alike, but only on **buffered** bodies — a streamed
//!   response (worker `send_response_stream`, large files streamed from disk)
//!   bypasses it. It fails **safe**: a broken transform leaves the response
//!   unchanged. Only modules that export `ephpm_middleware_invoke_response`
//!   (Rust: `declare!(Type, response)`) participate.
//!
//! Internal endpoints and the security gates that answer before routing
//! (metrics/health/ACME, trusted-host/hidden-file/blocked-path 4xx) return
//! ahead of both phases and are never seen by the chain.
//!
//! v1 chain semantics: `RESPOND` short-circuits the chain immediately.
//! `REWRITE` accumulates a path override (last writer wins) and header
//! overrides (chain order); the router applies them AFTER the whole chain
//! ran, so later modules observe the ORIGINAL request context, not earlier
//! modules' rewrites. `CONTINUE` and `REWRITE` may also emit
//! `response_headers`, accumulated in chain order and appended to the final
//! client response by the router.
// SAFETY rationale for the module-level allow: this file speaks the C
// middleware ABI (dlopen + raw fn pointers); every unsafe block below
// documents the specific invariant it relies on.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};

use ::metrics::counter;
use anyhow::Context;
use ephpm_config::MiddlewareMount;
use ephpm_middleware::abi;
use ephpm_middleware::builtin::{BuiltinModule, Verdict};
use ephpm_middleware::host::{RequestCtx, ResponseCtx};

/// An ordered chain of loaded middleware modules.
///
/// Built once at startup from `[[middleware]]` mounts; evaluated per request
/// by the router. Dropping the chain calls each module's `shutdown` before
/// any library is unloaded.
pub struct MiddlewareChain {
    modules: Vec<Loaded>,
}

/// One mounted module: its identity plus the execution backend.
struct Loaded {
    /// Mount name (the config `library` string) — used in logs and metrics.
    name: String,
    /// Glob the request path must match (None = run on every request).
    match_pattern: Option<String>,
    /// How the module executes: in-process or through the C ABI.
    backend: Backend,
}

/// The two execution lanes a mounted module can run through.
enum Backend {
    /// Compiled into this binary via the static [`builtin`] registry —
    /// invoked directly, no FFI round-trip. Works in fully static binaries.
    Builtin(BuiltinModule),
    /// dlopened shared library speaking the versioned C ABI. `_lib` must
    /// outlive the fn pointers — [`Drop`] on [`MiddlewareChain`] calls
    /// `shutdown` while every library is still loaded.
    Dynamic {
        /// Per-request entrypoint resolved from the library.
        invoke: abi::InvokeFn,
        /// Optional response-phase entrypoint (`ephpm_middleware_invoke_response`).
        /// `None` when the module does not export it — the host then skips this
        /// module during the response phase (v1-module compatibility).
        invoke_response: Option<abi::InvokeResponseFn>,
        /// Shutdown entrypoint resolved from the library.
        shutdown: abi::ShutdownFn,
        /// Keeps the library (and thus the fn pointers) alive.
        _lib: libloading::Library,
    },
}

impl Loaded {
    /// Whether this module participates in the response phase.
    fn has_response_phase(&self) -> bool {
        match &self.backend {
            Backend::Builtin(builtin) => builtin.has_response_phase(),
            Backend::Dynamic { invoke_response, .. } => invoke_response.is_some(),
        }
    }
}

/// Constructor signature for a middleware compiled into this binary.
pub type BuiltinBuilder = fn(&serde_json::Value) -> Result<BuiltinModule, String>;

/// Static registry of middleware compiled into every ePHPm binary —
/// including custom fully static builds, where `dlopen` does not exist.
///
/// `[[middleware]] library` values are checked here FIRST; only unmatched
/// names fall through to the shared-library search path. Accepted spellings
/// per module: the short name and the crate name, with `-` and `_`
/// interchangeable — e.g. `"jwt"`, `"ephpm-middleware-jwt"`,
/// `"ephpm_middleware_jwt"`. (`"ratelimit"` also answers to `"rate-limit"`.)
#[must_use]
pub fn builtin(name: &str) -> Option<BuiltinBuilder> {
    let canonical = name.replace('_', "-");
    Some(match canonical.as_str() {
        "jwt" | "ephpm-middleware-jwt" => {
            BuiltinModule::init::<ephpm_middleware_builtins::jwt::Jwt>
        }
        "cors" | "ephpm-middleware-cors" => {
            BuiltinModule::init::<ephpm_middleware_builtins::cors::Cors>
        }
        "ratelimit" | "rate-limit" | "ephpm-middleware-ratelimit" => {
            BuiltinModule::init::<ephpm_middleware_builtins::ratelimit::RateLimit>
        }
        "security-headers" | "ephpm-middleware-security-headers" => {
            BuiltinModule::init::<ephpm_middleware_builtins::security_headers::SecurityHeaders>
        }
        _ => return None,
    })
}

/// Outcome of evaluating the middleware chain for one request.
pub enum ChainVerdict {
    /// Proceed to PHP dispatch, applying any accumulated rewrites.
    Continue {
        /// Replacement request path from `REWRITE` (last writer wins).
        rewrite_path: Option<String>,
        /// Accumulated request-header overrides, in chain order.
        header_overrides: Vec<(String, String)>,
        /// Headers to append to the eventual client response (CORS, security
        /// headers, ...), accumulated across modules in chain order.
        response_headers: Vec<(String, String)>,
    },
    /// Short-circuit: return this response to the client; PHP never runs.
    Respond {
        /// HTTP status code chosen by the module.
        status: u16,
        /// Response body bytes (copied out of module memory).
        body: Vec<u8>,
        /// Extra response headers set by the module.
        headers: Vec<(String, String)>,
    },
}

/// Result of running the response phase over a generated response.
pub struct ResponsePhaseOutcome {
    /// Final HTTP status.
    pub status: u16,
    /// Final response headers (UTF-8-decodable set the router handed in,
    /// after every module's edits).
    pub headers: Vec<(String, String)>,
    /// Final response body.
    pub body: Vec<u8>,
    /// Whether any module replaced the body — the router recomputes
    /// `Content-Length` when true.
    pub body_replaced: bool,
}

impl MiddlewareChain {
    /// Load and initialise every mount, sorted by `order` (stable — equal
    /// orders keep declaration order). Each `library` value is resolved
    /// against the [`builtin`] registry first; only unmatched names hit the
    /// shared-library search path and dlopen.
    ///
    /// # Errors
    ///
    /// Fails fast when a builtin module's `init` returns an error, a library
    /// cannot be resolved on disk (the error names every path tried), a
    /// required ABI symbol is missing, or a dynamic module's `init` returns
    /// non-zero.
    pub fn load(mounts: &[MiddlewareMount]) -> anyhow::Result<Self> {
        let mut modules = Vec::with_capacity(mounts.len());
        for mount in sorted_by_order(mounts) {
            let loaded = match builtin(&mount.library) {
                Some(build) => load_builtin(mount, build)?,
                None => {
                    let path = resolve_library(&mount.library)?;
                    load_module(mount, &path).with_context(|| {
                        format!(
                            "failed to load middleware \"{}\" from {}",
                            mount.library,
                            path.display()
                        )
                    })?
                }
            };
            modules.push(loaded);
        }
        Ok(Self { modules })
    }

    /// Number of loaded modules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// Whether the chain has no modules.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Module names in chain order (for the startup log line).
    #[must_use]
    pub fn module_names(&self) -> Vec<&str> {
        self.modules.iter().map(|m| m.name.as_str()).collect()
    }

    /// Evaluate the chain for one request.
    ///
    /// Walks modules in `order`, skipping those whose `match` glob does not
    /// match `path`. `RESPOND` short-circuits; `REWRITE` accumulates path and
    /// header overrides. Every module sees the ORIGINAL `ctx` (v1 semantics —
    /// rewrites are applied by the router after the chain completes).
    /// Failures fail closed as a 500: a non-zero dynamic `invoke` return, or
    /// a builtin module panic (caught in [`BuiltinModule::invoke`]).
    #[must_use]
    pub fn evaluate(&self, ctx: &RequestCtx, path: &str) -> ChainVerdict {
        let mut rewrite_path: Option<String> = None;
        let mut header_overrides: Vec<(String, String)> = Vec::new();
        let mut response_headers: Vec<(String, String)> = Vec::new();

        for module in &self.modules {
            if let Some(pattern) = &module.match_pattern
                && !path_matches(pattern, path)
            {
                continue;
            }

            let verdict = match &module.backend {
                Backend::Builtin(builtin) => builtin.invoke(ctx),
                Backend::Dynamic { invoke, .. } => invoke_dynamic(*invoke, ctx, &module.name),
            };

            let action = match &verdict {
                Verdict::Respond { .. } => "respond",
                Verdict::Rewrite { .. } => "rewrite",
                Verdict::Continue { .. } => "continue",
            };
            counter!(
                "ephpm_middleware_invocations_total",
                "module" => module.name.clone(),
                "action" => action
            )
            .increment(1);

            match verdict {
                Verdict::Respond { status, body, headers } => {
                    return ChainVerdict::Respond { status, body, headers };
                }
                Verdict::Rewrite {
                    path: new_path,
                    header_overrides: overrides,
                    response_headers: appended,
                } => {
                    if let Some(new_path) = new_path {
                        rewrite_path = Some(new_path);
                    }
                    header_overrides.extend(overrides);
                    response_headers.extend(appended);
                }
                Verdict::Continue { response_headers: appended } => {
                    response_headers.extend(appended);
                }
            }
        }

        ChainVerdict::Continue { rewrite_path, header_overrides, response_headers }
    }

    /// Whether any loaded module participates in the response phase — the
    /// router uses this to skip the whole response-phase machinery (including
    /// the body collect) when no module can transform a response.
    #[must_use]
    pub fn has_response_phase(&self) -> bool {
        self.modules.iter().any(Loaded::has_response_phase)
    }

    /// Run the **response phase** over an already-generated response.
    ///
    /// Response-capable modules whose `match` glob matches `path` run in
    /// **reverse** chain order (onion unwind). Each transforms the response in
    /// place — status, headers, body — via its staged edit, applied in the
    /// order remove-headers → set-headers → status → body. A module that
    /// replaced the body has `Content-Length` recomputed for it by the router
    /// when the response is rebuilt.
    ///
    /// Unlike [`evaluate`](Self::evaluate) this runs even when no request phase
    /// ran (the static-file path) or an upstream request-phase module
    /// short-circuited — hence the documented contract that a response handler
    /// must not assume paired per-request state. It fails **safe**: a module
    /// error or panic leaves the response untouched.
    #[must_use]
    pub fn run_response_phase(
        &self,
        req_ctx: &RequestCtx,
        path: &str,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> ResponsePhaseOutcome {
        let mut ctx = ResponseCtx::new(status, headers, body);
        for module in self.modules.iter().rev() {
            if !module.has_response_phase() {
                continue;
            }
            if let Some(pattern) = &module.match_pattern
                && !path_matches(pattern, path)
            {
                continue;
            }
            counter!(
                "ephpm_middleware_response_invocations_total",
                "module" => module.name.clone()
            )
            .increment(1);
            match &module.backend {
                Backend::Builtin(builtin) => builtin.invoke_response(req_ctx, &mut ctx),
                Backend::Dynamic { invoke_response: Some(invoke_response), .. } => {
                    invoke_response_dynamic(*invoke_response, req_ctx, &mut ctx, &module.name);
                }
                // Filtered out by `has_response_phase` above.
                Backend::Dynamic { invoke_response: None, .. } => {}
            }
        }
        let body_replaced = ctx.body_replaced();
        let (status, headers, body) = ctx.into_parts();
        ResponsePhaseOutcome { status, headers, body, body_replaced }
    }
}

impl Drop for MiddlewareChain {
    fn drop(&mut self) {
        for module in &self.modules {
            tracing::debug!(module = %module.name, "middleware shutdown");
            match &module.backend {
                Backend::Builtin(builtin) => builtin.shutdown(),
                Backend::Dynamic { shutdown, .. } => {
                    let shutdown = *shutdown;
                    // SAFETY: `shutdown` points into the module's `_lib`,
                    // which is still loaded — the Library handles drop after
                    // this loop, when the struct's fields are dropped.
                    unsafe { shutdown() };
                }
            }
        }
    }
}

/// Call a dlopened module's `invoke` and copy its verdict into owned host
/// memory. Failures are normalised here — a non-zero return code fails
/// closed as a 500 `RESPOND`, an unknown action is treated as a bare
/// `CONTINUE` — so the chain loop handles both backends identically.
fn invoke_dynamic(invoke: abi::InvokeFn, ctx: &RequestCtx, name: &str) -> Verdict {
    // Zero-initialised verdict struct: action = CONTINUE, all pointers
    // null — the documented pre-call state.
    let mut resp = abi::EphpmResponse {
        action: abi::ACTION_CONTINUE,
        status: 0,
        body: std::ptr::null(),
        body_len: 0,
        rewrite_path: std::ptr::null(),
        header_overrides: std::ptr::null(),
        header_overrides_len: 0,
        response_headers: std::ptr::null(),
        response_headers_len: 0,
    };
    // SAFETY: `ctx.as_abi()` is live for the duration of this call, `resp`
    // is a valid zero-initialised out-struct, and `invoke` points into a
    // library kept alive by the owning `Backend::Dynamic`.
    let rc = unsafe { invoke(ctx.as_abi(), &raw mut resp) };
    if rc != 0 {
        tracing::error!(
            module = %name,
            rc,
            "middleware invoke returned an error — failing closed with 500"
        );
        return Verdict::Respond {
            status: 500,
            body: b"Internal Server Error".to_vec(),
            headers: Vec::new(),
        };
    }

    // Copy EVERYTHING the module pointed at before this call returns — the
    // ABI only guarantees the pointers until invoke's caller returns, and
    // the next invoke may reuse the same buffers.
    match resp.action {
        abi::ACTION_RESPOND => {
            // SAFETY: RESPOND pointers are valid until invoke's caller
            // returns — we are still inside that window.
            let body = unsafe { copy_bytes(resp.body, resp.body_len) };
            // SAFETY: same validity window as above.
            let headers = unsafe { copy_headers(resp.header_overrides, resp.header_overrides_len) };
            Verdict::Respond { status: resp.status, body, headers }
        }
        abi::ACTION_REWRITE => Verdict::Rewrite {
            // SAFETY: REWRITE pointers are valid until invoke's caller
            // returns — we are still inside that window.
            path: unsafe { copy_c_str(resp.rewrite_path) },
            // SAFETY: same validity window as above.
            header_overrides: unsafe {
                copy_headers(resp.header_overrides, resp.header_overrides_len)
            },
            // SAFETY: same validity window as above.
            response_headers: unsafe {
                copy_headers(resp.response_headers, resp.response_headers_len)
            },
        },
        abi::ACTION_CONTINUE => Verdict::Continue {
            // SAFETY: CONTINUE pointers are valid until invoke's caller
            // returns — we are still inside that window.
            response_headers: unsafe {
                copy_headers(resp.response_headers, resp.response_headers_len)
            },
        },
        other => {
            tracing::warn!(
                module = %name,
                action = other,
                "middleware returned an unknown action — treating as continue"
            );
            Verdict::Continue { response_headers: Vec::new() }
        }
    }
}

/// Call a dlopened module's `invoke_response`, copy its staged edit into owned
/// host memory, and apply it to `ctx` in the canonical order (remove → set →
/// status → body). Fails **safe**: a non-zero return leaves `ctx` untouched
/// (a broken transform must not corrupt an already-authorized response).
fn invoke_response_dynamic(
    invoke_response: abi::InvokeResponseFn,
    req_ctx: &RequestCtx,
    ctx: &mut ResponseCtx,
    name: &str,
) {
    // Zero-initialised edit: keep status, keep body, no header ops — the
    // documented pre-call state.
    let mut edit = abi::EphpmResponseEdit {
        status: 0,
        body: std::ptr::null(),
        body_len: 0,
        set_headers: std::ptr::null(),
        set_headers_len: 0,
        remove_headers: std::ptr::null(),
        remove_headers_len: 0,
    };
    // SAFETY: `req_ctx.as_abi()` and `ctx.as_ptr()` are live for this call
    // (`ctx` is only read through the const handle here — it is mutated below,
    // strictly after the call returns), `edit` is a valid zero-initialised
    // out-struct, and `invoke_response` points into a library kept alive by
    // the owning `Backend::Dynamic`.
    let rc = unsafe { invoke_response(req_ctx.as_abi(), ctx.as_ptr(), &raw mut edit) };
    if rc != 0 {
        tracing::warn!(
            module = %name,
            rc,
            "response middleware returned an error — leaving the response unchanged"
        );
        return;
    }

    // Copy everything the module pointed at before it can reuse the buffers —
    // the ABI only guarantees the edit pointers until invoke_response's caller
    // returns, and we are still inside that window.
    let status = (edit.status != 0).then_some(edit.status);
    // SAFETY: a non-null `body` is valid for `body_len` bytes in this window.
    let body = (!edit.body.is_null()).then(|| unsafe { copy_bytes(edit.body, edit.body_len) });
    // SAFETY: `set_headers` is null or a live array of `set_headers_len` KVs.
    let set_headers = unsafe { copy_headers(edit.set_headers, edit.set_headers_len) };
    // SAFETY: `remove_headers` is null or a live array of `remove_headers_len`
    // NUL-terminated name pointers.
    let remove_headers =
        unsafe { copy_remove_headers(edit.remove_headers, edit.remove_headers_len) };

    for header in &remove_headers {
        ctx.remove_header(header);
    }
    for (header, value) in &set_headers {
        ctx.set_header(header, value);
    }
    if let Some(status) = status {
        ctx.set_status(status);
    }
    if let Some(body) = body {
        ctx.replace_body(body);
    }
}

/// Copy a nullable module-owned array of NUL-terminated header-name pointers
/// into host memory. Entries with a null pointer are skipped.
///
/// # Safety
///
/// `p` must be null or valid for reads of `len` `*const c_char` entries whose
/// non-null members are NUL-terminated strings.
unsafe fn copy_remove_headers(p: *const *const c_char, len: usize) -> Vec<String> {
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: caller contract — `(p, len)` is a live array of name pointers.
    let names = unsafe { std::slice::from_raw_parts(p, len) };
    names
        .iter()
        // SAFETY: caller contract — each non-null entry is a C string.
        .filter_map(|&name| unsafe { copy_c_str(name) })
        .collect()
}

/// Mounts sorted by ascending `order` (stable: equal orders keep declaration
/// order).
fn sorted_by_order(mounts: &[MiddlewareMount]) -> Vec<&MiddlewareMount> {
    let mut sorted: Vec<&MiddlewareMount> = mounts.iter().collect();
    sorted.sort_by_key(|m| m.order);
    sorted
}

/// `<os>-<arch>` tag used in platform-suffixed library file names
/// (`linux-x86_64`, `linux-aarch64`, `darwin-aarch64`, `windows-x86_64`).
fn platform_tag() -> String {
    let os = if std::env::consts::OS == "macos" { "darwin" } else { std::env::consts::OS };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Resolve a mount's `library` string to a file on disk.
///
/// A value containing a path separator or a file extension is an explicit
/// path, used as-is. Bare names try `<name>.<platform>.<ext>`,
/// `lib<name>.<ext>` and `<name>.<ext>` (cargo-built artifacts) in the
/// current directory, `$EPHPM_MIDDLEWARE_DIR` (when set) and
/// `/usr/local/lib/ephpm/middleware`, in that order.
fn resolve_library(library: &str) -> anyhow::Result<PathBuf> {
    let explicit =
        library.contains('/') || library.contains('\\') || Path::new(library).extension().is_some();
    if explicit {
        let path = PathBuf::from(library);
        anyhow::ensure!(
            path.is_file(),
            "middleware library not found at explicit path {}",
            path.display()
        );
        return Ok(path);
    }

    let ext = std::env::consts::DLL_EXTENSION;
    let platform = platform_tag();
    let file_names = [
        format!("{library}.{platform}.{ext}"),
        format!("lib{library}.{ext}"),
        format!("{library}.{ext}"),
    ];

    let mut dirs = vec![PathBuf::from(".")];
    if let Ok(dir) = std::env::var("EPHPM_MIDDLEWARE_DIR")
        && !dir.is_empty()
    {
        dirs.push(PathBuf::from(dir));
    }
    dirs.push(PathBuf::from("/usr/local/lib/ephpm/middleware"));

    let mut tried = Vec::new();
    for dir in &dirs {
        for name in &file_names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
            tried.push(candidate.display().to_string());
        }
    }
    anyhow::bail!("middleware library \"{library}\" not found; tried: {}", tried.join(", "))
}

/// Initialise a builtin-registry mount in-process (no dlopen anywhere).
fn load_builtin(mount: &MiddlewareMount, build: BuiltinBuilder) -> anyhow::Result<Loaded> {
    let config = mount.config.clone().unwrap_or(serde_json::Value::Null);
    let module = build(&config).map_err(|msg| {
        anyhow::anyhow!("builtin middleware \"{}\" init failed: {msg}", mount.library)
    })?;
    tracing::info!(
        module = %mount.library,
        describe = %module.describe(),
        "middleware initialised (builtin)"
    );
    Ok(Loaded {
        name: mount.library.clone(),
        match_pattern: mount.match_pattern.clone(),
        backend: Backend::Builtin(module),
    })
}

/// dlopen a module, resolve the four ABI symbols, and run `init`.
fn load_module(mount: &MiddlewareMount, path: &Path) -> anyhow::Result<Loaded> {
    // SAFETY: loading a middleware library runs its initialisers with the
    // host's privileges — that is the documented v1 trust model (middleware
    // is as trusted as ePHPm itself; there is no sandbox).
    let lib = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("dlopen failed for {}", path.display()))?;

    // SAFETY: the symbol is declared with the documented ABI signature; a
    // module exporting it with a different shape is UB by ABI contract.
    let init: abi::InitFn = *unsafe { lib.get::<abi::InitFn>(abi::SYM_INIT) }
        .context("missing required symbol ephpm_middleware_init")?;
    // SAFETY: as above.
    let invoke: abi::InvokeFn = *unsafe { lib.get::<abi::InvokeFn>(abi::SYM_INVOKE) }
        .context("missing required symbol ephpm_middleware_invoke")?;
    // SAFETY: as above.
    let shutdown: abi::ShutdownFn = *unsafe { lib.get::<abi::ShutdownFn>(abi::SYM_SHUTDOWN) }
        .context("missing required symbol ephpm_middleware_shutdown")?;
    // SAFETY: as above. `describe` is optional — absence is not an error.
    let describe: Option<abi::DescribeFn> =
        unsafe { lib.get::<abi::DescribeFn>(abi::SYM_DESCRIBE) }.ok().map(|sym| *sym);
    // SAFETY: as above. `invoke_response` is optional (minor 1) — a module
    // that does not export it has no response phase and the host skips it
    // there. This is the primary v1-module compatibility mechanism.
    let invoke_response: Option<abi::InvokeResponseFn> =
        unsafe { lib.get::<abi::InvokeResponseFn>(abi::SYM_INVOKE_RESPONSE) }.ok().map(|sym| *sym);

    // Serialise the mount's config table to JSON for the module's init.
    let config_json: Option<CString> = mount
        .config
        .as_ref()
        .map(|v| {
            let json = serde_json::to_string(v)
                .context("middleware config is not serialisable to JSON")?;
            CString::new(json).context("middleware config JSON contains an interior NUL")
        })
        .transpose()?;
    let config_ptr = config_json.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

    // SAFETY: `config_ptr` is null or NUL-terminated and outlives the call;
    // the host table is 'static; init is called exactly once per load.
    let rc = unsafe {
        init(abi::ABI_V1, config_ptr, std::ptr::from_ref(ephpm_middleware::host::host_table()))
    };
    anyhow::ensure!(
        rc == 0,
        "middleware \"{}\" init returned {rc} (non-zero = module refused to start)",
        mount.library,
    );

    let description = describe.and_then(|describe| {
        // SAFETY: describe takes no arguments and returns a nullable pointer
        // to a static NUL-terminated string, per the ABI contract.
        let ptr = unsafe { describe() };
        if ptr.is_null() {
            None
        } else {
            // SAFETY: non-null describe() results are NUL-terminated strings
            // that live at least as long as the library.
            Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
        }
    });
    match &description {
        Some(desc) => tracing::info!(
            module = %mount.library,
            describe = %desc,
            path = %path.display(),
            "middleware initialised"
        ),
        None => tracing::info!(
            module = %mount.library,
            path = %path.display(),
            "middleware initialised"
        ),
    }

    Ok(Loaded {
        name: mount.library.clone(),
        match_pattern: mount.match_pattern.clone(),
        backend: Backend::Dynamic { invoke, invoke_response, shutdown, _lib: lib },
    })
}

/// Copy a nullable module-owned C string into host memory.
///
/// # Safety
///
/// `p` must be null or a NUL-terminated string valid for the read.
unsafe fn copy_c_str(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract — non-null `p` is NUL-terminated and live.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Copy a nullable module-owned byte buffer into host memory.
///
/// # Safety
///
/// `p` must be null or valid for reads of `len` bytes.
unsafe fn copy_bytes(p: *const u8, len: usize) -> Vec<u8> {
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: caller contract — `(p, len)` is a live byte slice.
    unsafe { std::slice::from_raw_parts(p, len) }.to_vec()
}

/// Copy a nullable module-owned header array into host memory. Entries with
/// null name or value pointers are skipped.
///
/// # Safety
///
/// `p` must be null or valid for reads of `len` [`abi::EphpmHeaderKv`]
/// entries whose non-null name/value pointers are NUL-terminated strings.
unsafe fn copy_headers(p: *const abi::EphpmHeaderKv, len: usize) -> Vec<(String, String)> {
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: caller contract — `(p, len)` is a live array of header KVs.
    let kvs = unsafe { std::slice::from_raw_parts(p, len) };
    kvs.iter()
        .filter_map(|kv| {
            // SAFETY: caller contract — each entry follows the C-string rule.
            let name = unsafe { copy_c_str(kv.name) }?;
            // SAFETY: as above.
            let value = unsafe { copy_c_str(kv.value) }?;
            Some((name, value))
        })
        .collect()
}

/// Match `path` against a glob `pattern` where `*` matches any character
/// sequence (including `/`) and every other character is literal.
fn path_matches(pattern: &str, path: &str) -> bool {
    let pat = pattern.as_bytes();
    let txt = path.as_bytes();
    let (mut p, mut t) = (0usize, 0usize);
    // Most recent `*`: (pattern index after it, text index it consumed up to).
    let mut backtrack: Option<(usize, usize)> = None;

    while t < txt.len() {
        if p < pat.len() && pat[p] == b'*' {
            backtrack = Some((p + 1, t));
            p += 1;
        } else if p < pat.len() && pat[p] == txt[t] {
            p += 1;
            t += 1;
        } else if let Some((after_star, consumed)) = backtrack {
            // Let the last `*` swallow one more character and retry.
            p = after_star;
            t = consumed + 1;
            backtrack = Some((after_star, consumed + 1));
        } else {
            return false;
        }
    }
    // Trailing `*`s match the empty sequence.
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(library: &str, order: u32) -> MiddlewareMount {
        MiddlewareMount { library: library.to_string(), match_pattern: None, order, config: None }
    }

    #[test]
    fn glob_prefix_wildcard() {
        assert!(path_matches("/api/*", "/api/x"));
        assert!(path_matches("/api/*", "/api/"));
        assert!(path_matches("/api/*", "/api/v1/users")); // `*` crosses `/`
        assert!(!path_matches("/api/*", "/apix"));
        assert!(!path_matches("/api/*", "/api")); // literal `/api/` prefix required
    }

    #[test]
    fn glob_bare_star_matches_everything() {
        assert!(path_matches("*", "/"));
        assert!(path_matches("*", ""));
        assert!(path_matches("*", "/deeply/nested/path.php"));
    }

    #[test]
    fn glob_exact_match() {
        assert!(path_matches("/health", "/health"));
        assert!(!path_matches("/health", "/healthz"));
        assert!(!path_matches("/health", "/heal"));
    }

    #[test]
    fn glob_suffix_wildcard() {
        assert!(path_matches("*.php", "/index.php"));
        assert!(path_matches("*.php", "/admin/login.php"));
        assert!(!path_matches("*.php", "/index.php.bak"));
        assert!(!path_matches("*.php", "/style.css"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(path_matches("/api/*/admin/*", "/api/v1/admin/users"));
        assert!(!path_matches("/api/*/admin/*", "/api/v1/users"));
    }

    #[test]
    fn sort_is_stable_by_order() {
        let mounts = vec![mount("b", 20), mount("a", 10), mount("c", 10), mount("d", 5)];
        let names: Vec<&str> =
            sorted_by_order(&mounts).iter().map(|m| m.library.as_str()).collect();
        // Equal orders (a, c) keep declaration order.
        assert_eq!(names, ["d", "a", "c", "b"]);
    }

    #[test]
    fn resolve_bare_name_error_names_every_candidate() {
        let err = resolve_library("no-such-middleware-xyz").unwrap_err().to_string();
        assert!(err.contains("no-such-middleware-xyz"), "{err}");
        // Three file-name forms per search directory, at least two dirs.
        assert!(err.matches("no-such-middleware-xyz.").count() >= 4, "{err}");
        assert!(err.contains("lib"), "{err}");
        assert!(err.contains(std::env::consts::DLL_EXTENSION), "{err}");
        assert!(err.contains(&platform_tag()), "{err}");
    }

    #[test]
    fn resolve_explicit_missing_path_fails_clearly() {
        let err = resolve_library("some/dir/mod.so").unwrap_err().to_string();
        assert!(err.contains("explicit path"), "{err}");
        assert!(err.contains("mod.so"), "{err}");
    }

    #[test]
    fn resolve_explicit_existing_path_is_used_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mod.so");
        std::fs::write(&file, b"not a real library").unwrap();
        let resolved = resolve_library(file.to_str().unwrap()).unwrap();
        assert_eq!(resolved, file);
    }

    // ── Builtin registry (static, no dlopen) ─────────────────────────────

    /// Wire a real in-memory KV store into the process-global host table
    /// (first call wins; every test in this binary shares it).
    fn wire_kv() {
        ephpm_middleware::host::set_kv_store(&ephpm_kv::store::Store::new(
            ephpm_kv::store::StoreConfig::default(),
        ));
    }

    fn builtin_mount(
        library: &str,
        pattern: Option<&str>,
        order: u32,
        config: serde_json::Value,
    ) -> MiddlewareMount {
        MiddlewareMount {
            library: library.to_string(),
            match_pattern: pattern.map(str::to_owned),
            order,
            config: Some(config),
        }
    }

    fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    #[test]
    fn builtin_registry_resolves_every_documented_spelling() {
        for name in [
            "jwt",
            "cors",
            "ratelimit",
            "rate-limit",
            "rate_limit",
            "security-headers",
            "security_headers",
            "ephpm-middleware-jwt",
            "ephpm_middleware_jwt",
            "ephpm-middleware-cors",
            "ephpm_middleware_cors",
            "ephpm-middleware-ratelimit",
            "ephpm_middleware_ratelimit",
            "ephpm-middleware-security-headers",
            "ephpm_middleware_security_headers",
        ] {
            assert!(builtin(name).is_some(), "\"{name}\" should resolve as builtin");
        }
        // Unknown names and anything path-like fall through to the dlopen lane.
        for name in ["", "jwtx", "my-auth", "jwt.so", "./jwt", "middleware/jwt", "JWT"] {
            assert!(builtin(name).is_none(), "\"{name}\" should NOT resolve as builtin");
        }
    }

    /// The full four-module chain, loaded purely from the static registry —
    /// no cdylib on disk, no dlopen. This is exactly what a fully static
    /// release binary executes.
    #[test]
    fn builtin_chain_all_four_modules_end_to_end() {
        wire_kv();
        let mounts = vec![
            builtin_mount(
                "security-headers",
                None,
                10,
                serde_json::json!({ "csp": "default-src 'self'" }),
            ),
            builtin_mount(
                "cors",
                None,
                20,
                serde_json::json!({ "allow_origins": ["https://app.example"] }),
            ),
            builtin_mount("jwt", Some("/api/*"), 30, serde_json::json!({ "secret": "s3cret" })),
            builtin_mount(
                "ratelimit",
                Some("/api/*"),
                40,
                serde_json::json!({ "per_ip_rps": 1, "burst": 0 }),
            ),
        ];
        let chain = MiddlewareChain::load(&mounts).expect("builtin chain loads without dlopen");
        assert_eq!(chain.len(), 4);
        assert_eq!(chain.module_names(), ["security-headers", "cors", "jwt", "ratelimit"]);

        // Non-API path: jwt/ratelimit are skipped by their globs; the chain
        // continues with security + CORS headers accumulated in order.
        let ctx = RequestCtx::new(
            "GET",
            "/index.php",
            "",
            "203.0.113.7",
            "vhost-builtin-chain",
            &[("Origin".to_owned(), "https://app.example".to_owned())],
        );
        match chain.evaluate(&ctx, "/index.php") {
            ChainVerdict::Continue { rewrite_path, header_overrides, response_headers } => {
                assert!(rewrite_path.is_none());
                assert!(header_overrides.is_empty());
                assert_eq!(
                    find_header(&response_headers, "Content-Security-Policy"),
                    Some("default-src 'self'")
                );
                assert_eq!(find_header(&response_headers, "X-Frame-Options"), Some("DENY"));
                assert_eq!(
                    find_header(&response_headers, "Access-Control-Allow-Origin"),
                    Some("https://app.example")
                );
                assert_eq!(find_header(&response_headers, "Vary"), Some("Origin"));
            }
            ChainVerdict::Respond { status, .. } => {
                panic!("expected CONTINUE for the non-API path, got RESPOND {status}")
            }
        }

        // API path without a token: jwt short-circuits — ratelimit never runs.
        let ctx =
            RequestCtx::new("GET", "/api/x.php", "", "203.0.113.7", "vhost-builtin-chain", &[]);
        match chain.evaluate(&ctx, "/api/x.php") {
            ChainVerdict::Respond { status, body, .. } => {
                assert_eq!(status, 401);
                assert_eq!(body, b"missing bearer token");
            }
            ChainVerdict::Continue { .. } => panic!("jwt must reject a token-less API request"),
        }
    }

    #[test]
    fn builtin_cors_preflight_short_circuits() {
        let mounts = vec![builtin_mount(
            "cors",
            None,
            10,
            serde_json::json!({ "allow_origins": ["*"], "max_age": 600 }),
        )];
        let chain = MiddlewareChain::load(&mounts).expect("load builtin cors");
        let ctx = RequestCtx::new(
            "OPTIONS",
            "/api/x.php",
            "",
            "203.0.113.7",
            "vhost-builtin-cors",
            &[
                ("Origin".to_owned(), "https://any.example".to_owned()),
                ("Access-Control-Request-Method".to_owned(), "PUT".to_owned()),
            ],
        );
        match chain.evaluate(&ctx, "/api/x.php") {
            ChainVerdict::Respond { status, body, headers } => {
                assert_eq!(status, 204);
                assert!(body.is_empty());
                assert_eq!(find_header(&headers, "Access-Control-Allow-Origin"), Some("*"));
                assert_eq!(find_header(&headers, "Access-Control-Max-Age"), Some("600"));
            }
            ChainVerdict::Continue { .. } => panic!("preflight must short-circuit"),
        }
    }

    #[test]
    fn builtin_ratelimit_trips_429_via_embedded_kv() {
        wire_kv();
        let mounts = vec![builtin_mount(
            "ratelimit",
            None,
            10,
            serde_json::json!({ "per_ip_rps": 1, "burst": 0 }),
        )];
        let chain = MiddlewareChain::load(&mounts).expect("load builtin ratelimit");
        // Allowance is 10/window; even if a window boundary lands mid-loop,
        // 3x the allowance must trip the limit.
        let mut limited = None;
        for _ in 0..30 {
            let ctx =
                RequestCtx::new("GET", "/x.php", "", "198.51.100.77", "vhost-builtin-rl", &[]);
            if let ChainVerdict::Respond { status, headers, .. } = chain.evaluate(&ctx, "/x.php") {
                limited = Some((status, headers));
                break;
            }
        }
        let (status, headers) = limited.expect("rate limit never tripped within 3x allowance");
        assert_eq!(status, 429);
        let retry: u64 = find_header(&headers, "Retry-After")
            .expect("Retry-After present")
            .parse()
            .expect("numeric Retry-After");
        assert!((1..=10).contains(&retry), "retry_after = {retry}");
    }

    #[test]
    fn builtin_init_error_aborts_startup() {
        // jwt without the required `secret` — fail-fast, naming the mount.
        let mounts = vec![MiddlewareMount {
            library: "jwt".to_owned(),
            match_pattern: None,
            order: 10,
            config: None,
        }];
        let err = match MiddlewareChain::load(&mounts) {
            Ok(_) => panic!("jwt without a secret must fail startup"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("\"jwt\""), "{err}");
        assert!(err.contains("secret"), "{err}");
    }

    /// Locate a built middleware cdylib in the workspace target directory.
    ///
    /// These are the *shipped* module artifacts, produced only by an explicit
    /// `cargo build` of the crate — `cargo test --workspace` does not emit a
    /// library crate's `cdylib` output, so on a fresh tree they are absent.
    fn built_module_path(stem: &str) -> Option<PathBuf> {
        let target_root = std::env::var("CARGO_TARGET_DIR").map_or_else(
            |_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
            PathBuf::from,
        );
        let (prefix, ext) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_EXTENSION);
        ["debug", "release"]
            .iter()
            .map(|profile| target_root.join(profile).join(format!("{prefix}{stem}.{ext}")))
            .find(|candidate| candidate.is_file())
    }

    /// Resolve a shipped cdylib, or decide what an absent artifact means.
    ///
    /// Locally, absent means "not built yet" and the test skips. In CI it
    /// means the build wiring regressed, and skipping is exactly the failure
    /// being closed here: this lane was *never* exercised, because nothing
    /// built the artifact and the skip fired silently on every run. CI sets
    /// `MIDDLEWARE_CDYLIB_REQUIRED=1` alongside the build step that
    /// produces them, which turns the skip into a hard failure.
    ///
    /// Deliberately NOT `EPHPM_`-prefixed: that prefix is figment's config
    /// namespace, so an `EPHPM_*` variable exported for the whole
    /// `cargo nextest run --workspace` invocation is also visible to
    /// `ephpm-config`'s own tests as a config key.
    fn require_module_path(stem: &str) -> Option<PathBuf> {
        if let Some(path) = built_module_path(stem) {
            return Some(path);
        }
        assert!(
            std::env::var_os("MIDDLEWARE_CDYLIB_REQUIRED").is_none(),
            "shipped middleware cdylib `{stem}` is missing but \
             MIDDLEWARE_CDYLIB_REQUIRED is set — build it with `cargo build --workspace`"
        );
        eprintln!("skipping: {stem} cdylib not built (run `cargo build --workspace`)");
        None
    }

    /// Full dlopen → init → invoke → shutdown lifecycle against the real
    /// `ephpm-middleware-security-headers` cdylib — the artifact operators
    /// actually deploy, as opposed to the purpose-built fixtures in
    /// `tests/middleware_dlopen.rs`. Runs on every CI platform, which is what
    /// gives the dlopen lane macOS `dyld` coverage (the E2E suite is
    /// Linux-only).
    #[test]
    fn load_real_module_end_to_end() {
        let Some(path) = require_module_path("ephpm_middleware_security_headers") else {
            return;
        };
        let mounts = vec![MiddlewareMount {
            library: path.to_string_lossy().into_owned(),
            match_pattern: None,
            order: 10,
            config: Some(serde_json::json!({ "csp": "default-src 'self'" })),
        }];
        let chain = MiddlewareChain::load(&mounts).expect("load real security-headers module");
        assert_eq!(chain.len(), 1);

        let ctx = RequestCtx::new("GET", "/index.php", "", "127.0.0.1", "test", &[]);
        match chain.evaluate(&ctx, "/index.php") {
            ChainVerdict::Continue { rewrite_path, header_overrides, response_headers } => {
                assert!(rewrite_path.is_none());
                assert!(header_overrides.is_empty());
                let find = |name: &str| {
                    response_headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(name))
                        .map(|(_, v)| v.as_str())
                };
                assert_eq!(find("Content-Security-Policy"), Some("default-src 'self'"));
                assert_eq!(find("X-Frame-Options"), Some("DENY"));
                assert_eq!(find("X-Content-Type-Options"), Some("nosniff"));
            }
            ChainVerdict::Respond { status, .. } => {
                panic!("expected CONTINUE from security-headers, got RESPOND {status}")
            }
        }
        // Dropping the chain exercises shutdown + dlclose.
        drop(chain);
    }

    /// The shipped `ephpm-middleware-cors` cdylib, dlopened and driven through
    /// a preflight — the `RESPOND` short-circuit marshaled back across the C
    /// ABI (status, body, headers), which the `CONTINUE`-only security-headers
    /// test above never touches.
    ///
    /// This is the same module and the same verdict the E2E suite asserts over
    /// real HTTP, so a divergence between "the chain decided 204" and "the
    /// client saw 204" localises to the router rather than to the loader.
    #[test]
    fn load_real_cors_module_preflight_short_circuits() {
        let Some(path) = require_module_path("ephpm_middleware_cors") else {
            return;
        };
        let mounts = vec![MiddlewareMount {
            library: path.to_string_lossy().into_owned(),
            match_pattern: None,
            order: 10,
            config: Some(serde_json::json!({ "allow_origins": ["*"], "max_age": 600 })),
        }];
        let chain = MiddlewareChain::load(&mounts).expect("load real cors module");
        assert_eq!(chain.len(), 1);

        let ctx = RequestCtx::new(
            "OPTIONS",
            "/api/x.php",
            "",
            "203.0.113.7",
            "vhost-dynamic-cors",
            &[
                ("Origin".to_owned(), "https://any.example".to_owned()),
                ("Access-Control-Request-Method".to_owned(), "PUT".to_owned()),
            ],
        );
        match chain.evaluate(&ctx, "/api/x.php") {
            ChainVerdict::Respond { status, body, headers } => {
                assert_eq!(status, 204);
                assert!(body.is_empty());
                assert_eq!(find_header(&headers, "Access-Control-Allow-Origin"), Some("*"));
                assert_eq!(find_header(&headers, "Access-Control-Max-Age"), Some("600"));
            }
            ChainVerdict::Continue { .. } => panic!("preflight must short-circuit through dlopen"),
        }
        drop(chain);
    }

    // ── Response phase (builtin lane) ─────────────────────────────────────

    use ephpm_middleware::{Middleware, Request, Response, ResponseMiddleware, ResponseView};

    /// Appends its `id` to the running `X-Seq` header, so the final value
    /// records the order the response phase visited modules in.
    struct SeqTag {
        id: String,
    }
    impl Middleware for SeqTag {
        fn init(config: &serde_json::Value) -> Result<Self, String> {
            Ok(Self { id: config["id"].as_str().unwrap_or("?").to_owned() })
        }
        fn invoke(&self, _req: &Request<'_>) -> Response {
            Response::cont()
        }
    }
    impl ResponseMiddleware for SeqTag {
        fn invoke_response(&self, _req: &Request<'_>, resp: &mut ResponseView<'_>) {
            let next = match resp.header("X-Seq") {
                Some(cur) if !cur.is_empty() => format!("{cur},{}", self.id),
                _ => self.id.clone(),
            };
            resp.set_header("X-Seq", next);
        }
    }

    /// Rewrites status, body, and headers — the buffered transform surface.
    struct Transformer;
    impl Middleware for Transformer {
        fn init(_config: &serde_json::Value) -> Result<Self, String> {
            Ok(Self)
        }
        fn invoke(&self, _req: &Request<'_>) -> Response {
            Response::cont()
        }
    }
    impl ResponseMiddleware for Transformer {
        fn invoke_response(&self, _req: &Request<'_>, resp: &mut ResponseView<'_>) {
            resp.set_status(201);
            let upper = resp.body().to_ascii_uppercase();
            resp.set_body(upper);
            resp.set_header("X-Transformed", "1");
            resp.remove_header("X-Remove-Me");
        }
    }

    /// Panics in the response phase — must be contained (fail-safe).
    struct Panicker;
    impl Middleware for Panicker {
        fn init(_config: &serde_json::Value) -> Result<Self, String> {
            Ok(Self)
        }
        fn invoke(&self, _req: &Request<'_>) -> Response {
            Response::cont()
        }
    }
    impl ResponseMiddleware for Panicker {
        fn invoke_response(&self, _req: &Request<'_>, _resp: &mut ResponseView<'_>) {
            panic!("boom in the response phase");
        }
    }

    /// Request-phase only — no `ResponseMiddleware`, so no response phase.
    struct RequestOnly;
    impl Middleware for RequestOnly {
        fn init(_config: &serde_json::Value) -> Result<Self, String> {
            Ok(Self)
        }
        fn invoke(&self, _req: &Request<'_>) -> Response {
            Response::cont()
        }
    }

    fn loaded_response<T: ResponseMiddleware>(
        name: &str,
        pattern: Option<&str>,
        config: serde_json::Value,
    ) -> Loaded {
        Loaded {
            name: name.to_owned(),
            match_pattern: pattern.map(str::to_owned),
            backend: Backend::Builtin(BuiltinModule::init_response::<T>(&config).expect("init")),
        }
    }

    fn loaded_request<T: Middleware>(name: &str) -> Loaded {
        Loaded {
            name: name.to_owned(),
            match_pattern: None,
            backend: Backend::Builtin(
                BuiltinModule::init::<T>(&serde_json::Value::Null).expect("init"),
            ),
        }
    }

    fn resp_ctx() -> RequestCtx {
        RequestCtx::new("GET", "/x", "", "203.0.113.9", "resp.test", &[])
    }

    #[test]
    fn response_phase_runs_in_reverse_chain_order() {
        // Chain order a, b -> response phase visits b then a (onion unwind).
        let chain = MiddlewareChain {
            modules: vec![
                loaded_response::<SeqTag>("a", None, serde_json::json!({ "id": "a" })),
                loaded_response::<SeqTag>("b", None, serde_json::json!({ "id": "b" })),
            ],
        };
        let out = chain.run_response_phase(&resp_ctx(), "/x", 200, Vec::new(), Vec::new());
        assert_eq!(
            out.headers.iter().find(|(n, _)| n == "X-Seq").map(|(_, v)| v.as_str()),
            Some("b,a"),
            "last request-phase module (b) unwinds first"
        );
    }

    #[test]
    fn response_phase_transforms_status_body_and_headers() {
        let chain = MiddlewareChain {
            modules: vec![loaded_response::<Transformer>("t", None, serde_json::Value::Null)],
        };
        let headers = vec![
            ("Content-Type".to_owned(), "text/plain".to_owned()),
            ("X-Remove-Me".to_owned(), "gone".to_owned()),
        ];
        let out = chain.run_response_phase(&resp_ctx(), "/x", 200, headers, b"hello".to_vec());
        assert_eq!(out.status, 201);
        assert_eq!(out.body, b"HELLO");
        assert!(out.body_replaced);
        assert!(out.headers.iter().any(|(n, v)| n == "X-Transformed" && v == "1"));
        assert!(!out.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("X-Remove-Me")));
        // Untouched headers survive.
        assert!(out.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("Content-Type")));
    }

    #[test]
    fn response_phase_fails_safe_on_module_panic() {
        let chain = MiddlewareChain {
            modules: vec![loaded_response::<Panicker>("p", None, serde_json::Value::Null)],
        };
        let headers = vec![("X-Keep".to_owned(), "yes".to_owned())];
        let out = chain.run_response_phase(&resp_ctx(), "/x", 200, headers, b"body".to_vec());
        // A panicking transform leaves the response exactly as it was.
        assert_eq!(out.status, 200);
        assert_eq!(out.body, b"body");
        assert!(!out.body_replaced);
        assert_eq!(out.headers, vec![("X-Keep".to_owned(), "yes".to_owned())]);
    }

    #[test]
    fn modules_without_a_response_phase_are_skipped() {
        // A request-only module reports no response phase.
        let request_only = MiddlewareChain { modules: vec![loaded_request::<RequestOnly>("r")] };
        assert!(!request_only.has_response_phase());

        // Mixed: the request-only module is skipped, the transformer still runs.
        let mixed = MiddlewareChain {
            modules: vec![
                loaded_request::<RequestOnly>("r"),
                loaded_response::<Transformer>("t", None, serde_json::Value::Null),
            ],
        };
        assert!(mixed.has_response_phase());
        let out = mixed.run_response_phase(&resp_ctx(), "/x", 200, Vec::new(), b"go".to_vec());
        assert_eq!(out.body, b"GO");
        assert!(out.body_replaced);
    }

    #[test]
    fn response_phase_respects_the_match_glob() {
        let chain = MiddlewareChain {
            modules: vec![loaded_response::<Transformer>(
                "t",
                Some("/api/*"),
                serde_json::Value::Null,
            )],
        };
        // Off-glob: untouched.
        let out =
            chain.run_response_phase(&resp_ctx(), "/index.php", 200, Vec::new(), b"x".to_vec());
        assert!(!out.body_replaced);
        assert_eq!(out.body, b"x");
        // On-glob: transformed.
        let out = chain.run_response_phase(&resp_ctx(), "/api/v1", 200, Vec::new(), b"x".to_vec());
        assert!(out.body_replaced);
        assert_eq!(out.body, b"X");
    }
}

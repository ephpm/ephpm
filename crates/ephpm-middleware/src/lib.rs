//! Native middleware for ePHPm: the C ABI plus a safe Rust authoring kit.
//!
//! ePHPm runs middleware per request **before PHP dispatch** — reject,
//! rewrite, or annotate requests at native speed, with direct access to the
//! embedded (cluster-replicated) KV store. See `site/content/` middleware
//! docs for the operator view.
//!
//! There are two execution lanes for the same [`Middleware`] trait:
//!
//! - **Builtin (static registry)** — modules compiled into the ePHPm binary
//!   and invoked in-process via [`builtin::BuiltinModule`] (feature `host`).
//!   Works in every binary, including custom fully static builds where
//!   `dlopen` does not exist.
//! - **Dynamic (C ABI)** — shared libraries (`.so`/`.dylib`/`.dll`) loaded
//!   at startup, for out-of-tree modules. Works with the stock release
//!   binaries on every platform (the Linux release is glibc-dynamic).
//!
//! Authoring a module in Rust:
//!
//! ```ignore
//! use ephpm_middleware::{declare, Middleware, Request, Response};
//!
//! struct SecurityHeaders { csp: Option<String> }
//!
//! impl Middleware for SecurityHeaders {
//!     fn init(config: &serde_json::Value) -> Result<Self, String> {
//!         Ok(Self { csp: config["csp"].as_str().map(str::to_owned) })
//!     }
//!     fn invoke(&self, _req: &Request) -> Response {
//!         let mut r = Response::rewrite();
//!         if let Some(csp) = &self.csp {
//!             r = r.header("Content-Security-Policy", csp);
//!         }
//!         r
//!     }
//! }
//!
//! declare!(SecurityHeaders);
//! ```
//!
//! Build with `crate-type = ["cdylib"]`; the produced library is a drop-in
//! module for any ePHPm built against the same ABI major version.
#![allow(unsafe_code)] // FFI crate — every unsafe block carries a SAFETY note.

pub mod abi;
#[cfg(feature = "host")]
pub mod builtin;
#[cfg(feature = "host")]
pub mod host;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use abi::{EphpmHostV1, EphpmRequest};

/// The trait a Rust-authored middleware implements. [`declare!`] generates
/// the C ABI exports around it.
pub trait Middleware: Sized + Send + Sync + 'static {
    /// Construct from the mount's `config` block (JSON-serialised from
    /// `ephpm.toml`). Returning `Err` aborts server startup with the message.
    ///
    /// # Errors
    ///
    /// Return a human-readable message when the configuration is unusable.
    fn init(config: &serde_json::Value) -> Result<Self, String>;

    /// Called once per request. Must not panic (panics are caught and turn
    /// into a 500 — fail-closed — but cost the module its credibility).
    fn invoke(&self, req: &Request<'_>) -> Response;

    /// Called once at process shutdown.
    fn shutdown(&self) {}

    /// Name/version string for logs. Return `""` (the default) to let
    /// [`declare!`] substitute the module crate's own name — a trait default
    /// body expands `env!` in THIS crate, which would report every module as
    /// `ephpm-middleware`.
    #[must_use]
    fn describe() -> &'static str {
        ""
    }
}

// ── Safe request view ─────────────────────────────────────────────────────

/// Borrowed view of the request being decided. Valid only inside
/// [`Middleware::invoke`].
pub struct Request<'a> {
    raw: *const EphpmRequest,
    host: &'a EphpmHostV1,
}

impl Request<'_> {
    /// Wrap the raw FFI pair. Used by [`declare!`]-generated glue.
    ///
    /// # Safety
    ///
    /// `raw` must be the live request pointer passed to `invoke` and `host`
    /// the table given at init; both outlive the returned view.
    #[must_use]
    pub unsafe fn from_raw(raw: *const EphpmRequest, host: &EphpmHostV1) -> Request<'_> {
        Request { raw, host }
    }

    fn str_of(&self, p: *const c_char) -> &str {
        if p.is_null() {
            return "";
        }
        // SAFETY: host accessors return NUL-terminated UTF-8 strings that
        // live as long as the request.
        unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
    }

    /// HTTP method.
    #[must_use]
    pub fn method(&self) -> &str {
        // SAFETY: contract of `from_raw`.
        self.str_of(unsafe { (self.host.request_method)(self.raw) })
    }

    /// URL path (no query string).
    #[must_use]
    pub fn path(&self) -> &str {
        // SAFETY: contract of `from_raw`.
        self.str_of(unsafe { (self.host.request_path)(self.raw) })
    }

    /// Raw query string (empty when absent).
    #[must_use]
    pub fn query(&self) -> &str {
        // SAFETY: contract of `from_raw`.
        self.str_of(unsafe { (self.host.request_query)(self.raw) })
    }

    /// Client IP after trusted-proxy resolution.
    #[must_use]
    pub fn remote_ip(&self) -> &str {
        // SAFETY: contract of `from_raw`.
        self.str_of(unsafe { (self.host.request_remote_ip)(self.raw) })
    }

    /// Vhost / server-name identity for this request.
    #[must_use]
    pub fn vhost_id(&self) -> &str {
        // SAFETY: contract of `from_raw`.
        self.str_of(unsafe { (self.host.request_vhost_id)(self.raw) })
    }

    /// Host services (KV store, logging) scoped to the table this request
    /// was invoked with. Equivalent to `Host::new(__ephpm_mw_host())` but
    /// also works in unit tests that build a [`Request`] by hand.
    #[must_use]
    pub fn host(&self) -> Host<'_> {
        Host::new(self.host)
    }

    /// Case-insensitive header lookup. Duplicate request headers arrive
    /// pre-joined per HTTP list semantics.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let Ok(cname) = CString::new(name) else {
            return None;
        };
        // SAFETY: contract of `from_raw`; `cname` outlives the call.
        let p = unsafe { (self.host.request_header)(self.raw, cname.as_ptr()) };
        if p.is_null() {
            None
        } else {
            // SAFETY: non-null accessor results are NUL-terminated and live
            // as long as the request.
            Some(unsafe { CStr::from_ptr(p) }.to_str().unwrap_or(""))
        }
    }
}

// ── Verdict builder ───────────────────────────────────────────────────────

/// Owned verdict returned by [`Middleware::invoke`]. The [`declare!`] glue
/// marshals it into the C [`abi::EphpmResponse`], keeping the backing
/// buffers alive until the host has copied them.
// `response_headers` deliberately mirrors the ABI field name.
#[allow(clippy::struct_field_names)]
pub struct Response {
    pub(crate) action: i32,
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    pub(crate) rewrite_path: Option<String>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) response_headers: Vec<(String, String)>,
}

impl Response {
    /// Proceed to the next middleware / PHP dispatch.
    #[must_use]
    pub fn cont() -> Self {
        Self {
            action: abi::ACTION_CONTINUE,
            status: 0,
            body: Vec::new(),
            rewrite_path: None,
            headers: Vec::new(),
            response_headers: Vec::new(),
        }
    }

    /// Short-circuit with `status` and `body`; PHP never runs.
    #[must_use]
    pub fn respond(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            action: abi::ACTION_RESPOND,
            status,
            body: body.into(),
            rewrite_path: None,
            headers: Vec::new(),
            response_headers: Vec::new(),
        }
    }

    /// Mutate the request, then continue the chain.
    #[must_use]
    pub fn rewrite() -> Self {
        Self {
            action: abi::ACTION_REWRITE,
            status: 0,
            body: Vec::new(),
            rewrite_path: None,
            headers: Vec::new(),
            response_headers: Vec::new(),
        }
    }

    /// Override the request path (`rewrite()`) — no effect on `respond()`.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.rewrite_path = Some(path.into());
        self
    }

    /// Add a header: a request-header override for `rewrite()`, a response
    /// header for `respond()`.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Add a header to the eventual client response for `cont()` /
    /// `rewrite()` verdicts (CORS, security headers, ...). Appended after
    /// PHP produced the response, allowing duplicates (e.g. `Set-Cookie`).
    /// For `respond()` use [`Response::header`] instead.
    #[must_use]
    pub fn response_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.response_headers.push((name.into(), value.into()));
        self
    }
}

// ── Host services for authoring code ──────────────────────────────────────

/// Access to host services (KV store, logging) from middleware code.
/// Obtained via [`Request::host`] inside `invoke`, or from the table stashed
/// by [`declare!`]'s generated `init`.
pub struct Host<'a> {
    table: &'a EphpmHostV1,
}

impl<'a> Host<'a> {
    /// Wrap the host table (used by [`declare!`] glue and tests).
    #[must_use]
    pub fn new(table: &'a EphpmHostV1) -> Self {
        Self { table }
    }

    /// Get a key from the embedded KV store (`None` = absent or error).
    #[must_use]
    pub fn kv_get(&self, key: &str) -> Option<Vec<u8>> {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len: usize = 0;
        // SAFETY: valid key slice for the call; out-params point at locals.
        let rc =
            unsafe { (self.table.kv_get)(key.as_ptr(), key.len(), &raw mut ptr, &raw mut len) };
        if rc != 0 || ptr.is_null() {
            return None;
        }
        // SAFETY: rc==0 means the host allocated `len` bytes at `ptr`; we
        // copy then hand the buffer back to the host allocator.
        let out = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        // SAFETY: `ptr`/`len` came from kv_get above.
        unsafe { (self.table.kv_free)(ptr, len) };
        Some(out)
    }

    /// Set a key (TTL in seconds; `0` = no expiry). Returns false on error.
    #[must_use]
    pub fn kv_set(&self, key: &str, value: &[u8], ttl_secs: i64) -> bool {
        // SAFETY: valid slices for the duration of the call.
        let rc = unsafe {
            (self.table.kv_set)(key.as_ptr(), key.len(), value.as_ptr(), value.len(), ttl_secs)
        };
        rc == 0
    }

    /// Set-if-absent. Returns true when this call created the key.
    #[must_use]
    pub fn kv_set_nx(&self, key: &str, value: &[u8], ttl_secs: i64) -> bool {
        // SAFETY: valid slices for the duration of the call.
        let rc = unsafe {
            (self.table.kv_set_nx)(key.as_ptr(), key.len(), value.as_ptr(), value.len(), ttl_secs)
        };
        rc == 0
    }

    /// Atomic increment; `None` on error (e.g. non-numeric value).
    #[must_use]
    pub fn kv_incr(&self, key: &str, by: i64) -> Option<i64> {
        let mut out: i64 = 0;
        // SAFETY: valid key slice; out-param points at a local.
        let rc = unsafe { (self.table.kv_incr)(key.as_ptr(), key.len(), by, &raw mut out) };
        (rc == 0).then_some(out)
    }

    /// Log through the host's `tracing` subscriber.
    pub fn log(&self, level: i32, msg: &str) {
        // SAFETY: valid slice for the duration of the call.
        unsafe { (self.table.log)(level, msg.as_ptr(), msg.len()) };
    }
}

// ── declare! ──────────────────────────────────────────────────────────────

/// Generate the four C ABI exports around a [`Middleware`] implementation.
///
/// Handles: ABI major-version check, config JSON parsing, host-table
/// stashing (retrievable via `__ephpm_mw_host()` for [`Host`]), response
/// marshaling with buffers kept alive until the next call on the same
/// thread, and panic containment (a panicking `invoke` returns a 500 —
/// fail-closed).
#[macro_export]
macro_rules! declare {
    ($ty:ty) => {
        mod __ephpm_mw_glue {
            #![allow(unsafe_code)]
            use super::*;
            use std::ffi::{CStr, CString};
            use std::os::raw::{c_char, c_int};
            use $crate::abi;

            static INSTANCE: std::sync::OnceLock<$ty> = std::sync::OnceLock::new();
            static HOST: std::sync::OnceLock<&'static abi::EphpmHostV1> =
                std::sync::OnceLock::new();
            static DESCRIBE: std::sync::OnceLock<CString> = std::sync::OnceLock::new();

            /// Host table access for authoring code (`Host::new(__ephpm_mw_host())`).
            pub fn __ephpm_mw_host() -> &'static abi::EphpmHostV1 {
                HOST.get().expect("middleware not initialised")
            }

            thread_local! {
                // Backing storage for the pointers handed to the host in
                // EphpmResponse; alive until the next invoke on this thread
                // (the host copies before returning).
                static OUT: std::cell::RefCell<(
                    Vec<u8>,
                    Option<CString>,
                    Vec<CString>,
                    Vec<abi::EphpmHeaderKv>,
                    Vec<abi::EphpmHeaderKv>,
                )> = std::cell::RefCell::new((Vec::new(), None, Vec::new(), Vec::new(), Vec::new()));
            }

            #[unsafe(no_mangle)]
            unsafe extern "C" fn ephpm_middleware_init(
                abi_version: u32,
                config_json: *const c_char,
                host: *const abi::EphpmHostV1,
            ) -> c_int {
                if (abi_version >> 24) != (abi::ABI_V1 >> 24) || host.is_null() {
                    return -1;
                }
                // SAFETY: the host guarantees the table lives for the process.
                let host_ref: &'static abi::EphpmHostV1 = unsafe { &*host };
                let _ = HOST.set(host_ref);

                let config: $crate::serde_json::Value = if config_json.is_null() {
                    $crate::serde_json::Value::Null
                } else {
                    // SAFETY: host passes a NUL-terminated JSON string.
                    let raw = unsafe { CStr::from_ptr(config_json) };
                    match $crate::serde_json::from_slice(raw.to_bytes()) {
                        Ok(v) => v,
                        Err(_) => return -2,
                    }
                };

                let built = std::panic::catch_unwind(|| <$ty as $crate::Middleware>::init(&config));
                match built {
                    Ok(Ok(instance)) => {
                        let _ = INSTANCE.set(instance);
                        0
                    }
                    Ok(Err(msg)) => {
                        let m = format!("middleware init failed: {msg}");
                        // SAFETY: valid slice for the call.
                        unsafe { (host_ref.log)(abi::LOG_ERROR, m.as_ptr(), m.len()) };
                        -3
                    }
                    Err(_) => -4,
                }
            }

            #[unsafe(no_mangle)]
            unsafe extern "C" fn ephpm_middleware_invoke(
                request: *const abi::EphpmRequest,
                response_out: *mut abi::EphpmResponse,
            ) -> c_int {
                if response_out.is_null() {
                    return -1;
                }
                let (Some(instance), Some(host)) = (INSTANCE.get(), HOST.get()) else {
                    return -1;
                };

                let verdict = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // SAFETY: `request` is live for this call; `host` is the
                    // init-time table.
                    let req = unsafe { $crate::Request::from_raw(request, host) };
                    <$ty as $crate::Middleware>::invoke(instance, &req)
                }));

                let verdict = match verdict {
                    Ok(v) => v,
                    // Fail-closed: a broken auth middleware must not fail-open.
                    Err(_) => $crate::Response::respond(500, "middleware panic"),
                };

                OUT.with(|cell| {
                    let mut out = cell.borrow_mut();
                    // Reborrow through the RefMut once so the disjoint field
                    // borrows below don't each re-enter deref_mut().
                    let out = &mut *out;
                    let (parts, kvs, resp_kvs) =
                        $crate::__marshal(&verdict, &mut out.0, &mut out.1, &mut out.2);
                    out.3 = kvs;
                    out.4 = resp_kvs;
                    // SAFETY: response_out is a valid, host-owned struct.
                    unsafe {
                        (*response_out).action = verdict.__action();
                        (*response_out).status = verdict.__status();
                        (*response_out).body = parts.0;
                        (*response_out).body_len = parts.1;
                        (*response_out).rewrite_path = parts.2;
                        (*response_out).header_overrides = if out.3.is_empty() {
                            std::ptr::null()
                        } else {
                            out.3.as_ptr()
                        };
                        (*response_out).header_overrides_len = out.3.len();
                        (*response_out).response_headers = if out.4.is_empty() {
                            std::ptr::null()
                        } else {
                            out.4.as_ptr()
                        };
                        (*response_out).response_headers_len = out.4.len();
                    }
                });
                0
            }

            #[unsafe(no_mangle)]
            unsafe extern "C" fn ephpm_middleware_shutdown() {
                if let Some(instance) = INSTANCE.get() {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        <$ty as $crate::Middleware>::shutdown(instance);
                    }));
                }
            }

            #[unsafe(no_mangle)]
            unsafe extern "C" fn ephpm_middleware_describe() -> *const c_char {
                DESCRIBE
                    .get_or_init(|| {
                        let d = <$ty as $crate::Middleware>::describe();
                        // env! expands HERE (the module crate), so an empty
                        // describe() yields the module's own crate name.
                        let d = if d.is_empty() { env!("CARGO_PKG_NAME") } else { d };
                        CString::new(d).unwrap_or_default()
                    })
                    .as_ptr()
            }
        }
        pub use __ephpm_mw_glue::__ephpm_mw_host;
    };
}

// Re-export for the macro body.
pub use serde_json;

impl Response {
    /// Internal: raw action code (used by [`declare!`] glue).
    #[doc(hidden)]
    #[must_use]
    pub fn __action(&self) -> i32 {
        self.action
    }

    /// Internal: status code (used by [`declare!`] glue).
    #[doc(hidden)]
    #[must_use]
    pub fn __status(&self) -> u16 {
        self.status
    }

    /// Internal: body bytes (used by module unit tests).
    #[doc(hidden)]
    #[must_use]
    pub fn __body(&self) -> &[u8] {
        &self.body
    }

    /// Internal: rewrite path (used by module unit tests).
    #[doc(hidden)]
    #[must_use]
    pub fn __rewrite_path(&self) -> Option<&str> {
        self.rewrite_path.as_deref()
    }

    /// Internal: request-header overrides / RESPOND headers (used by module
    /// unit tests).
    #[doc(hidden)]
    #[must_use]
    pub fn __headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Internal: appended client-response headers (used by module unit
    /// tests).
    #[doc(hidden)]
    #[must_use]
    pub fn __response_headers(&self) -> &[(String, String)] {
        &self.response_headers
    }
}

/// Internal marshaling helper for [`declare!`]: copies the verdict's buffers
/// into thread-local storage and returns the raw pointers for the C struct.
/// The second element is `header_overrides`, the third `response_headers`.
#[doc(hidden)]
pub fn __marshal(
    verdict: &Response,
    body_buf: &mut Vec<u8>,
    path_buf: &mut Option<CString>,
    header_strs: &mut Vec<CString>,
) -> ((*const u8, usize, *const c_char), Vec<abi::EphpmHeaderKv>, Vec<abi::EphpmHeaderKv>) {
    body_buf.clear();
    body_buf.extend_from_slice(&verdict.body);
    let body_ptr = if body_buf.is_empty() { std::ptr::null() } else { body_buf.as_ptr() };

    *path_buf = verdict.rewrite_path.as_deref().and_then(|p| CString::new(p).ok());
    let path_ptr = path_buf.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

    header_strs.clear();
    let kvs = marshal_headers(&verdict.headers, header_strs);
    let resp_kvs = marshal_headers(&verdict.response_headers, header_strs);
    ((body_ptr, body_buf.len(), path_ptr), kvs, resp_kvs)
}

/// Copy one header list into `header_strs`-backed C strings and return the
/// KV entries pointing at them. Moving a `CString` (Vec growth) does not
/// move its heap buffer, so earlier `as_ptr()` results stay valid.
fn marshal_headers(
    headers: &[(String, String)],
    header_strs: &mut Vec<CString>,
) -> Vec<abi::EphpmHeaderKv> {
    let mut kvs = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let (Ok(n), Ok(v)) = (CString::new(name.as_str()), CString::new(value.as_str())) else {
            continue;
        };
        header_strs.push(n);
        header_strs.push(v);
        let len = header_strs.len();
        kvs.push(abi::EphpmHeaderKv {
            name: header_strs[len - 2].as_ptr(),
            value: header_strs[len - 1].as_ptr(),
        });
    }
    kvs
}

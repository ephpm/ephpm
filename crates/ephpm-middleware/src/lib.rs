//! Native middleware for ePHPm: the C ABI plus a safe Rust authoring kit.
//!
//! ePHPm middleware runs in two phases. The **request phase**
//! ([`Middleware::invoke`]) runs *before* a request is served — reject,
//! rewrite, or annotate requests at native speed, with direct access to the
//! embedded (cluster-replicated) KV store; it runs on the PHP path and the
//! static-file path, and fails closed. The optional **response phase**
//! ([`ResponseMiddleware::invoke_response`], opted into with
//! `declare!(Type, response)`) runs *after* the response is generated, in
//! reverse chain order, to transform it (compression, ETag, header
//! injection); it fails safe and is not a security gate. See `site/content/`
//! middleware docs for the operator view.
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

use abi::{EphpmHeaderKv, EphpmHostV1, EphpmRequest, EphpmResponseCtx};

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

/// The optional **response phase** of a middleware. A module implements this
/// *in addition* to [`Middleware`] and opts in with `declare!(Type, response)`
/// — the plain `declare!(Type)` does not export the response symbol, so the
/// host skips such a module during the response phase.
///
/// The response phase runs **after** the response is generated (PHP output,
/// a static file, an error page, or a request-phase `RESPOND` short-circuit),
/// in **reverse** chain order, and — unlike the request phase — it runs on
/// **static** responses too. Its purpose is to *transform* a response:
/// compression, ETag / conditional-GET, response-header injection.
///
/// Two hard contracts, both consequences of where and how it runs:
///
/// - **It may run without a preceding [`Middleware::invoke`].** On the static
///   path no request phase runs at all, and an upstream request-phase
///   `RESPOND` can short-circuit before this module's `invoke`. A response
///   handler must therefore not assume any per-request state its own `invoke`
///   would have set.
/// - **It is not a security gate.** The phase fails *safe*: a panic or an
///   error leaves the response unchanged rather than failing closed. Gate
///   requests in [`Middleware::invoke`] (which fails closed); use this only to
///   transform an already-authorized response.
///
/// The phase applies to **buffered** responses only. A streamed response
/// (worker-mode `send_response_stream`, large files served straight from
/// disk) bypasses it entirely in this ABI version.
pub trait ResponseMiddleware: Middleware {
    /// Inspect and transform the generated response via `resp`. Staged edits
    /// (status / body / header changes) are applied by the host after this
    /// returns, in the documented order. Must not panic (a panic is caught
    /// and the response is left unchanged).
    fn invoke_response(&self, req: &Request<'_>, resp: &mut ResponseView<'_>);
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

    /// The host's advertised ABI minor (low three bytes of `abi_version`).
    /// Trailing accessors added in a later minor must be gated on this so a
    /// module built against a newer ABI never reads past an older host's
    /// (shorter) table.
    fn host_minor(&self) -> u32 {
        self.host.abi_version & 0x00FF_FFFF
    }

    /// Request URL scheme — `"https"` when TLS terminated at ePHPm for this
    /// connection, `"http"` otherwise. Authoritative from the connection, not
    /// from a forwarded header: the correct basis for a `force_https` redirect.
    ///
    /// Falls back to `"http"` on a host older than
    /// [`abi::ABI_MINOR_REQUEST_ACCESSORS`] (the accessor is absent there).
    #[must_use]
    pub fn scheme(&self) -> &str {
        if self.host_minor() < abi::ABI_MINOR_REQUEST_ACCESSORS {
            return "http";
        }
        // SAFETY: contract of `from_raw`; the minor gate above guarantees the
        // `request_scheme` field is present on this host's table.
        self.str_of(unsafe { (self.host.request_scheme)(self.raw) })
    }

    /// Whether the request arrived over a secure (HTTPS/TLS) connection — the
    /// boolean form of [`scheme`](Self::scheme).
    ///
    /// Falls back to `false` on a host older than
    /// [`abi::ABI_MINOR_REQUEST_ACCESSORS`].
    #[must_use]
    pub fn is_secure(&self) -> bool {
        if self.host_minor() < abi::ABI_MINOR_REQUEST_ACCESSORS {
            return false;
        }
        // SAFETY: contract of `from_raw`; minor-gated as above.
        (unsafe { (self.host.request_is_secure)(self.raw) }) != 0
    }

    /// Normalized request `Host` — port stripped, trailing dot stripped,
    /// lowercased (the router's own vhost-key normalization). Distinct from
    /// [`vhost_id`](Self::vhost_id) (the un-normalized server name); empty when
    /// the request carried no usable `Host`.
    ///
    /// Falls back to `""` on a host older than
    /// [`abi::ABI_MINOR_REQUEST_ACCESSORS`].
    #[must_use]
    pub fn http_host(&self) -> &str {
        if self.host_minor() < abi::ABI_MINOR_REQUEST_ACCESSORS {
            return "";
        }
        // SAFETY: contract of `from_raw`; minor-gated as above.
        self.str_of(unsafe { (self.host.request_host)(self.raw) })
    }

    /// Bounded, read-only view of the buffered request body (borrowed; valid
    /// only inside `invoke`).
    ///
    /// Non-empty only when the operator opted in with `[server.request]
    /// middleware_body_limit > 0`, in which case up to that many body bytes are
    /// exposed here (a longer body is truncated to the limit for this view —
    /// PHP still receives the full body). Empty otherwise, and always empty on
    /// the static-file path. Use it for webhook/HMAC signature checks,
    /// CSRF-with-body, and payload validation.
    ///
    /// The `request_body` slot has existed since minor 0, so no minor gate is
    /// needed.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        let mut ptr: *const u8 = std::ptr::null();
        // SAFETY: contract of `from_raw`; `ptr` points at a local.
        let len = unsafe { (self.host.request_body)(self.raw, &raw mut ptr) };
        if ptr.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: the host returned a live `(ptr, len)` byte slice valid for
        // the duration of this call (the request).
        unsafe { std::slice::from_raw_parts(ptr, len) }
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

    /// Atomic increment that applies `ttl_secs` (`0` = no expiry) ONLY when
    /// this call creates the key; existing keys keep their current TTL
    /// (fixed-window semantics). `None` on error (e.g. non-numeric value).
    ///
    /// This is the correct primitive for fixed-window rate limiting: the
    /// window key always gets its TTL atomically on creation, so a counter
    /// can never be created without an expiry and leak.
    #[must_use]
    pub fn kv_incr_ttl(&self, key: &str, by: i64, ttl_secs: i64) -> Option<i64> {
        let mut out: i64 = 0;
        // SAFETY: valid key slice; out-param points at a local.
        let rc = unsafe {
            (self.table.kv_incr_ttl)(key.as_ptr(), key.len(), by, ttl_secs, &raw mut out)
        };
        (rc == 0).then_some(out)
    }

    /// Log through the host's `tracing` subscriber.
    pub fn log(&self, level: i32, msg: &str) {
        // SAFETY: valid slice for the duration of the call.
        unsafe { (self.table.log)(level, msg.as_ptr(), msg.len()) };
    }
}

// ── Response view (response phase) ────────────────────────────────────────

/// The edits a [`ResponseView`] staged, drained for the host to apply:
/// `(new status, replacement body, set-or-replace headers, remove headers)`.
#[doc(hidden)]
pub type StagedEdit = (Option<u16>, Option<Vec<u8>>, Vec<(String, String)>, Vec<String>);

/// Mutable view of the generated response, handed to
/// [`ResponseMiddleware::invoke_response`]. Reads reflect the response as it
/// stands on entry (already carrying any edits from later-in-reverse modules);
/// writes are *staged* and applied by the host after `invoke_response`
/// returns, in the order: remove headers, set headers, status, body.
///
/// Valid only inside `invoke_response`. Borrowed slices returned by
/// [`body`](Self::body) / [`headers`](Self::headers) point into host memory
/// live only for that call — never store them.
pub struct ResponseView<'a> {
    ctx: *const EphpmResponseCtx,
    host: &'a EphpmHostV1,
    new_status: Option<u16>,
    new_body: Option<Vec<u8>>,
    set_headers: Vec<(String, String)>,
    remove_headers: Vec<String>,
}

impl ResponseView<'_> {
    /// Wrap the raw response handle + host table. Used by [`declare!`] glue
    /// and by unit tests that drive a module directly.
    ///
    /// # Safety
    ///
    /// `ctx` must be the live response handle passed to `invoke_response` and
    /// `host` the table given at init, both outliving the returned view. The
    /// host must advertise minor >= [`abi::ABI_MINOR_RESPONSE_PHASE`] so the
    /// `response_*` accessors are present.
    #[must_use]
    pub unsafe fn from_raw(ctx: *const EphpmResponseCtx, host: &EphpmHostV1) -> ResponseView<'_> {
        ResponseView {
            ctx,
            host,
            new_status: None,
            new_body: None,
            set_headers: Vec::new(),
            remove_headers: Vec::new(),
        }
    }

    /// Current HTTP status of the response.
    #[must_use]
    pub fn status(&self) -> u16 {
        // SAFETY: contract of `from_raw` — `ctx` is live for this call.
        unsafe { (self.host.response_status)(self.ctx) }
    }

    /// Current response body bytes (borrowed; valid only for this call).
    #[must_use]
    pub fn body(&self) -> &[u8] {
        let mut ptr: *const u8 = std::ptr::null();
        // SAFETY: contract of `from_raw`; `ptr` points at a local.
        let len = unsafe { (self.host.response_body)(self.ctx, &raw mut ptr) };
        if ptr.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: the host returned a live `(ptr, len)` byte slice valid for
        // the duration of this call.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    /// Iterate the response headers as `(name, value)` (borrowed; valid only
    /// for this call). Duplicate headers appear as separate items.
    #[must_use]
    pub fn headers(&self) -> Vec<(&str, &str)> {
        let mut ptr: *const EphpmHeaderKv = std::ptr::null();
        // SAFETY: contract of `from_raw`; `ptr` points at a local.
        let len = unsafe { (self.host.response_headers)(self.ctx, &raw mut ptr) };
        if ptr.is_null() || len == 0 {
            return Vec::new();
        }
        // SAFETY: the host returned a live array of `len` header KVs valid for
        // the duration of this call.
        let kvs = unsafe { std::slice::from_raw_parts(ptr, len) };
        kvs.iter()
            .filter_map(|kv| {
                if kv.name.is_null() || kv.value.is_null() {
                    return None;
                }
                // SAFETY: non-null name/value are NUL-terminated per the ABI.
                let name = unsafe { CStr::from_ptr(kv.name) }.to_str().ok()?;
                // SAFETY: as above.
                let value = unsafe { CStr::from_ptr(kv.value) }.to_str().ok()?;
                Some((name, value))
            })
            .collect()
    }

    /// First value of `name` (case-insensitive), if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers()
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.to_owned())
    }

    /// Stage a new HTTP status.
    pub fn set_status(&mut self, status: u16) {
        self.new_status = Some(status);
    }

    /// Stage a replacement body.
    pub fn set_body(&mut self, body: impl Into<Vec<u8>>) {
        self.new_body = Some(body.into());
    }

    /// Stage a header, replacing any existing occurrences (case-insensitive)
    /// or adding it when absent.
    pub fn set_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.set_headers.push((name.into(), value.into()));
    }

    /// Stage removal of every header named `name` (case-insensitive).
    pub fn remove_header(&mut self, name: impl Into<String>) {
        self.remove_headers.push(name.into());
    }

    /// Internal: drain the staged edits for the [`declare!`] glue to marshal.
    #[doc(hidden)]
    #[must_use]
    pub fn __into_parts(self) -> StagedEdit {
        (self.new_status, self.new_body, self.set_headers, self.remove_headers)
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
    // Request phase only — no response symbol is exported, so the host skips
    // this module during the response phase.
    ($ty:ty) => {
        $crate::declare!(@inner $ty,);
    };
    // Request + response phase — also exports `ephpm_middleware_invoke_response`
    // (requires `$ty: ResponseMiddleware`).
    ($ty:ty, response) => {
        $crate::declare!(@inner $ty, response);
    };
    (@inner $ty:ty, $($response:ident)?) => {
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

            // Optional response-phase export, emitted only for
            // `declare!($ty, response)`. `$response` is the literal token
            // `response`; referencing it in the doc attribute keeps the `?`
            // repetition well-formed while gating the whole block.
            $(
                thread_local! {
                    // Backing storage for the pointers handed to the host in
                    // EphpmResponseEdit; alive until the next invoke_response
                    // on this thread (the host applies the edit before then).
                    static RESP_OUT: std::cell::RefCell<(
                        Option<Vec<u8>>,
                        Vec<CString>,
                        Vec<abi::EphpmHeaderKv>,
                        Vec<*const c_char>,
                    )> = std::cell::RefCell::new((None, Vec::new(), Vec::new(), Vec::new()));
                }

                #[doc = concat!("Response phase (`", stringify!($response), "`) enabled.")]
                #[unsafe(no_mangle)]
                unsafe extern "C" fn ephpm_middleware_invoke_response(
                    request: *const abi::EphpmRequest,
                    response: *const abi::EphpmResponseCtx,
                    edit_out: *mut abi::EphpmResponseEdit,
                ) -> c_int {
                    if edit_out.is_null() {
                        return -1;
                    }
                    let (Some(instance), Some(host)) = (INSTANCE.get(), HOST.get()) else {
                        return -1;
                    };

                    let parts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // SAFETY: `request`/`response` are live for this call;
                        // `host` is the init-time table.
                        let req = unsafe { $crate::Request::from_raw(request, host) };
                        let mut view =
                            unsafe { $crate::ResponseView::from_raw(response, host) };
                        <$ty as $crate::ResponseMiddleware>::invoke_response(
                            instance, &req, &mut view,
                        );
                        view.__into_parts()
                    }));

                    // Fail-safe: a panicking transform leaves the response
                    // unchanged (rc != 0 => host applies no edit).
                    let (status, body, set_headers, remove_headers) = match parts {
                        Ok(p) => p,
                        Err(_) => return -1,
                    };

                    RESP_OUT.with(|cell| {
                        let mut out = cell.borrow_mut();
                        let out = &mut *out;
                        let ((body_ptr, body_len), set_kvs, remove_ptrs) =
                            $crate::__marshal_edit(
                                body,
                                &set_headers,
                                &remove_headers,
                                &mut out.0,
                                &mut out.1,
                            );
                        out.2 = set_kvs;
                        out.3 = remove_ptrs;
                        // SAFETY: edit_out is a valid, host-owned,
                        // zero-initialised out-struct.
                        unsafe {
                            (*edit_out).status = status.unwrap_or(0);
                            (*edit_out).body = body_ptr;
                            (*edit_out).body_len = body_len;
                            (*edit_out).set_headers = if out.2.is_empty() {
                                std::ptr::null()
                            } else {
                                out.2.as_ptr()
                            };
                            (*edit_out).set_headers_len = out.2.len();
                            (*edit_out).remove_headers = if out.3.is_empty() {
                                std::ptr::null()
                            } else {
                                out.3.as_ptr()
                            };
                            (*edit_out).remove_headers_len = out.3.len();
                        }
                    });
                    0
                }
            )?
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

/// Internal marshaling helper for the [`declare!`] response-phase glue: stages
/// the response edit into thread-local storage and returns the raw pointers
/// for [`abi::EphpmResponseEdit`]. The body pointer is null when the module
/// did not replace the body (`None`); a `Some(empty)` replacement yields a
/// non-null pointer with length 0 (the host's length-0 guard never
/// dereferences it), so "replace with empty body" is distinguishable from
/// "keep body".
///
/// Every returned pointer borrows `body_buf` / `str_buf`, which the caller
/// keeps alive in thread-local storage until the next `invoke_response` on the
/// same thread — long enough for the host to apply the edit.
#[doc(hidden)]
#[must_use]
pub fn __marshal_edit(
    body: Option<Vec<u8>>,
    set_headers: &[(String, String)],
    remove_headers: &[String],
    body_buf: &mut Option<Vec<u8>>,
    str_buf: &mut Vec<CString>,
) -> ((*const u8, usize), Vec<abi::EphpmHeaderKv>, Vec<*const c_char>) {
    *body_buf = body;
    let body_parts = match body_buf.as_ref() {
        Some(b) => (b.as_ptr(), b.len()),
        None => (std::ptr::null(), 0),
    };

    str_buf.clear();
    let set_kvs = marshal_headers(set_headers, str_buf);

    let mut remove_ptrs = Vec::with_capacity(remove_headers.len());
    for name in remove_headers {
        let Ok(n) = CString::new(name.as_str()) else {
            continue;
        };
        str_buf.push(n);
        remove_ptrs.push(str_buf[str_buf.len() - 1].as_ptr());
    }

    (body_parts, set_kvs, remove_ptrs)
}

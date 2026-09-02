//! Host-side pieces: the per-request context handed to modules and the
//! process-wide [`abi::EphpmHostV1`] callback table. Used by `ephpm-server`
//! (feature `host`); middleware authors never touch this module.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::abi::{self, EphpmHeaderKv, EphpmHostV1, EphpmRequest, EphpmResponseCtx};

/// Owned, C-string-backed request context. Built by the router per request;
/// the opaque `EphpmRequest*` handed to modules is a pointer to this.
pub struct RequestCtx {
    method: CString,
    path: CString,
    query: CString,
    remote_ip: CString,
    /// The request's **canonical site key** — the value `Router::resolve_site`
    /// matched, which also selects the per-site database, the KV keyspace and
    /// the OPcache vhost. Empty when the request matched no known virtual host;
    /// the `request_vhost_id` accessor turns that into a NULL return so a
    /// module can tell "tenant `foo`" from "someone sent `Host: foo`".
    site_key: CString,
    /// Normalized request host (port/trailing-dot stripped, lowercased) — the
    /// `request_host` accessor. Empty when the request had no usable `Host`.
    host: CString,
    /// Whether the connection was secure (HTTPS/TLS) — drives `request_scheme`
    /// and `request_is_secure`. Authoritative from the connection.
    is_secure: bool,
    /// Bounded, buffered request body exposed by `request_body`. Empty unless
    /// the operator opted into body buffering (`[server.request]
    /// middleware_body_limit`); already truncated to that limit by the caller.
    body: Vec<u8>,
    /// Lower-cased name → value (values pre-joined per HTTP list semantics).
    headers: Vec<(CString, CString)>,
}

/// Convert a string to a C string, stripping interior NULs (invalid in HTTP
/// metadata anyway) rather than failing.
fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

impl RequestCtx {
    /// Build the context. Interior NULs are stripped (invalid in HTTP
    /// metadata anyway) rather than failing the request.
    ///
    /// `site_key` is the request's **canonical site key** — pass exactly what
    /// `Router::resolve_site` returned, and the **empty string** when it
    /// returned `None` (the host matched no known virtual host). Never pass a
    /// raw `Host` header here: that was issue #390, and modules read this value
    /// as a tenant identity in authorization decisions. The normalized request
    /// host stays available separately through
    /// [`with_host`](Self::with_host) / the `request_host` accessor.
    ///
    /// The connection-derived extras — scheme/secure, normalized host, and the
    /// buffered body — default to "insecure / empty / no body" and are set by
    /// the router via [`with_scheme`](Self::with_scheme),
    /// [`with_host`](Self::with_host), and [`with_body`](Self::with_body).
    #[must_use]
    pub fn new(
        method: &str,
        path: &str,
        query: &str,
        remote_ip: &str,
        site_key: &str,
        headers: &[(String, String)],
    ) -> Self {
        Self {
            method: cstr(method),
            path: cstr(path),
            query: cstr(query),
            remote_ip: cstr(remote_ip),
            site_key: cstr(site_key),
            host: CString::default(),
            is_secure: false,
            body: Vec::new(),
            headers: headers
                .iter()
                .map(|(n, v)| (cstr(&n.to_ascii_lowercase()), cstr(v)))
                .collect(),
        }
    }

    /// Set the connection security (drives `request_scheme` /
    /// `request_is_secure`). `true` = the request arrived over HTTPS/TLS.
    #[must_use]
    pub fn with_scheme(mut self, is_secure: bool) -> Self {
        self.is_secure = is_secure;
        self
    }

    /// Set the normalized request host (`request_host`). Pass the router's
    /// canonical, already-normalized host key.
    #[must_use]
    pub fn with_host(mut self, host: &str) -> Self {
        self.host = cstr(host);
        self
    }

    /// Set the buffered request body exposed by `request_body`. The caller is
    /// responsible for truncating `body` to the configured limit first.
    #[must_use]
    pub fn with_body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }

    /// The opaque pointer to pass through the ABI. Valid while `self` lives.
    #[must_use]
    pub fn as_abi(&self) -> *const EphpmRequest {
        std::ptr::from_ref(self).cast::<EphpmRequest>()
    }
}

// SAFETY: the opaque pointer is only dereferenced back into &RequestCtx by
// the accessors below, on the thread running the chain, while the ctx lives.
unsafe fn ctx<'a>(req: *const EphpmRequest) -> Option<&'a RequestCtx> {
    // SAFETY: see above — `req` originates from RequestCtx::as_abi.
    unsafe { req.cast::<RequestCtx>().as_ref() }
}

unsafe extern "C" fn request_method(req: *const EphpmRequest) -> *const c_char {
    // SAFETY: ABI contract (pointer from as_abi, live during invoke).
    unsafe { ctx(req) }.map_or(std::ptr::null(), |c| c.method.as_ptr())
}
unsafe extern "C" fn request_path(req: *const EphpmRequest) -> *const c_char {
    // SAFETY: ABI contract.
    unsafe { ctx(req) }.map_or(std::ptr::null(), |c| c.path.as_ptr())
}
unsafe extern "C" fn request_query(req: *const EphpmRequest) -> *const c_char {
    // SAFETY: ABI contract.
    unsafe { ctx(req) }.map_or(std::ptr::null(), |c| c.query.as_ptr())
}
unsafe extern "C" fn request_remote_ip(req: *const EphpmRequest) -> *const c_char {
    // SAFETY: ABI contract.
    unsafe { ctx(req) }.map_or(std::ptr::null(), |c| c.remote_ip.as_ptr())
}
unsafe extern "C" fn request_vhost_id(req: *const EphpmRequest) -> *const c_char {
    // NULL — not `""` — for a request that matched no known vhost. An empty
    // C string is still a tenant-shaped answer a module would happily use as a
    // lookup key; NULL is the one value that cannot be mistaken for a tenant,
    // so a module can fail closed on it (issue #390).
    // SAFETY: ABI contract.
    unsafe { ctx(req) }.map_or(std::ptr::null(), |c| {
        if c.site_key.is_empty() { std::ptr::null() } else { c.site_key.as_ptr() }
    })
}
unsafe extern "C" fn request_header(
    req: *const EphpmRequest,
    name: *const c_char,
) -> *const c_char {
    if name.is_null() {
        return std::ptr::null();
    }
    // SAFETY: ABI contract; `name` is a NUL-terminated string from the module.
    let (Some(c), needle) = (unsafe { ctx(req) }, unsafe { CStr::from_ptr(name) }) else {
        return std::ptr::null();
    };
    let needle = needle.to_bytes().to_ascii_lowercase();
    for (n, v) in &c.headers {
        if n.as_bytes() == needle.as_slice() {
            return v.as_ptr();
        }
    }
    std::ptr::null()
}
unsafe extern "C" fn request_body(req: *const EphpmRequest, out_ptr: *mut *const u8) -> usize {
    // The body is non-empty only when the operator opted into buffering
    // (`[server.request] middleware_body_limit`); it is already bounded to
    // that limit by the router. Empty otherwise (the chain ran before the body
    // was read — rejecting before the transfer is the point) and on the
    // static/response-phase paths, which carry no request body.
    // SAFETY: ABI contract (pointer from as_abi, live during invoke).
    let body = unsafe { ctx(req) }.map(|c| c.body.as_slice()).unwrap_or_default();
    if !out_ptr.is_null() {
        let base = if body.is_empty() { std::ptr::null() } else { body.as_ptr() };
        // SAFETY: module passes a valid out-pointer.
        unsafe { *out_ptr = base };
    }
    body.len()
}
unsafe extern "C" fn request_scheme(req: *const EphpmRequest) -> *const c_char {
    // SAFETY: ABI contract. Both branches point at 'static C-string literals.
    unsafe { ctx(req) }.map_or(c"http".as_ptr(), |c| {
        if c.is_secure { c"https".as_ptr() } else { c"http".as_ptr() }
    })
}
unsafe extern "C" fn request_is_secure(req: *const EphpmRequest) -> c_int {
    // SAFETY: ABI contract.
    c_int::from(unsafe { ctx(req) }.is_some_and(|c| c.is_secure))
}
unsafe extern "C" fn request_host(req: *const EphpmRequest) -> *const c_char {
    // SAFETY: ABI contract.
    unsafe { ctx(req) }.map_or(std::ptr::null(), |c| c.host.as_ptr())
}

// ── Response context (response phase) ─────────────────────────────────────

/// Host-owned, mutable representation of the response being transformed by the
/// response phase. The opaque `EphpmResponseCtx*` handed to modules is a
/// pointer to this; the host drives it across the reverse chain, applying each
/// module's staged edit before handing it to the next module.
///
/// Headers are kept as owned Rust strings (easy case-insensitive mutation) and
/// mirrored into a `CString`-backed [`EphpmHeaderKv`] array rebuilt on every
/// mutation, so the `response_headers` accessor can hand back a stable
/// borrowed pointer for the duration of a call.
pub struct ResponseCtx {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    body_replaced: bool,
    kv_strs: Vec<CString>,
    kvs: Vec<EphpmHeaderKv>,
}

impl ResponseCtx {
    /// Build from the response the host just generated.
    #[must_use]
    pub fn new(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        let mut ctx = Self {
            status,
            headers,
            body,
            body_replaced: false,
            kv_strs: Vec::new(),
            kvs: Vec::new(),
        };
        ctx.rebuild_kvs();
        ctx
    }

    /// Rebuild the C-string mirror of `headers`. Interior-NUL header names or
    /// values are dropped (invalid in HTTP anyway).
    fn rebuild_kvs(&mut self) {
        self.kv_strs.clear();
        self.kvs.clear();
        for (name, value) in &self.headers {
            let (Ok(n), Ok(v)) = (CString::new(name.as_str()), CString::new(value.as_str())) else {
                continue;
            };
            self.kv_strs.push(n);
            self.kv_strs.push(v);
            let len = self.kv_strs.len();
            self.kvs.push(EphpmHeaderKv {
                name: self.kv_strs[len - 2].as_ptr(),
                value: self.kv_strs[len - 1].as_ptr(),
            });
        }
    }

    /// The opaque pointer to pass through the ABI. Valid while `self` lives.
    #[must_use]
    pub fn as_ptr(&self) -> *const EphpmResponseCtx {
        std::ptr::from_ref(self).cast::<EphpmResponseCtx>()
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Current headers.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Current body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Replace the status.
    pub fn set_status(&mut self, status: u16) {
        self.status = status;
    }

    /// Replace the body. Marks the response body as replaced so the host can
    /// recompute `Content-Length`.
    pub fn replace_body(&mut self, body: Vec<u8>) {
        self.body = body;
        self.body_replaced = true;
    }

    /// Whether any module replaced the body during the phase.
    #[must_use]
    pub fn body_replaced(&self) -> bool {
        self.body_replaced
    }

    /// Replace-or-add a header (removes every case-insensitive occurrence,
    /// then appends the new one).
    pub fn set_header(&mut self, name: &str, value: &str) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value.to_owned()));
        self.rebuild_kvs();
    }

    /// Remove every case-insensitive occurrence of `name`.
    pub fn remove_header(&mut self, name: &str) {
        let before = self.headers.len();
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        if self.headers.len() != before {
            self.rebuild_kvs();
        }
    }

    /// Consume the context back into owned response parts.
    #[must_use]
    pub fn into_parts(self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        (self.status, self.headers, self.body)
    }
}

// SAFETY: the opaque pointer is only turned back into &ResponseCtx by the
// accessors below, on the thread running the response phase, while it lives.
unsafe fn resp_ctx<'a>(p: *const EphpmResponseCtx) -> Option<&'a ResponseCtx> {
    // SAFETY: see above — `p` originates from ResponseCtx::as_ptr.
    unsafe { p.cast::<ResponseCtx>().as_ref() }
}

unsafe extern "C" fn response_status(p: *const EphpmResponseCtx) -> u16 {
    // SAFETY: ABI contract (pointer from as_ptr, live during invoke_response).
    unsafe { resp_ctx(p) }.map_or(0, ResponseCtx::status)
}

unsafe extern "C" fn response_headers(
    p: *const EphpmResponseCtx,
    out_ptr: *mut *const EphpmHeaderKv,
) -> usize {
    // SAFETY: ABI contract.
    let Some(ctx) = (unsafe { resp_ctx(p) }) else {
        if !out_ptr.is_null() {
            // SAFETY: module passes a valid out-pointer.
            unsafe { *out_ptr = std::ptr::null() };
        }
        return 0;
    };
    if !out_ptr.is_null() {
        let base = if ctx.kvs.is_empty() { std::ptr::null() } else { ctx.kvs.as_ptr() };
        // SAFETY: module passes a valid out-pointer.
        unsafe { *out_ptr = base };
    }
    ctx.kvs.len()
}

unsafe extern "C" fn response_body(p: *const EphpmResponseCtx, out_ptr: *mut *const u8) -> usize {
    // SAFETY: ABI contract.
    let Some(ctx) = (unsafe { resp_ctx(p) }) else {
        if !out_ptr.is_null() {
            // SAFETY: module passes a valid out-pointer.
            unsafe { *out_ptr = std::ptr::null() };
        }
        return 0;
    };
    if !out_ptr.is_null() {
        let base = if ctx.body.is_empty() { std::ptr::null() } else { ctx.body.as_ptr() };
        // SAFETY: module passes a valid out-pointer.
        unsafe { *out_ptr = base };
    }
    ctx.body.len()
}

// ── KV callbacks ─────────────────────────────────────────────────────────
//
// Two scopes, deliberately (issue #376):
//
// * `kv_*` resolves **this request's** keyspace — the per-vhost store on a
//   multi-tenant node, which is the same physically-separate `DashMap` PHP's
//   `ephpm_kv_*` reaches for that vhost. So a rate limiter is per-tenant by
//   default and a module can read a key PHP wrote, without either side having
//   to hand-prefix anything.
// * `kv_*_global` resolves the process-wide store, which is where
//   *operator*-owned state belongs precisely because no tenant's PHP can
//   reach it (see #384: a per-site credential map must not be writable by the
//   site it authorizes).
//
// On a single-site node there is one store and the two are the same thing.

/// The process-wide store. Also the fallback for a request that matched no
/// vhost: such a request has no tenant, so there is no per-site keyspace it
/// could legitimately be given. (PHP's `kv` identity instead falls back to the
/// normalized `Host`, which suits a catch-all document root serving many
/// names. The middleware chain runs on *every* request — static files
/// included — so minting a store per `Host` value here would be a cheap way to
/// grow the site map without bound.)
static KV_STORE: OnceLock<Arc<ephpm_kv::store::Store>> = OnceLock::new();

thread_local! {
    /// The store the `kv_*` callbacks resolve for the request currently being
    /// evaluated on this thread. `None` = fall back to [`KV_STORE`].
    ///
    /// Thread-local rather than an argument because the KV slots on
    /// [`EphpmHostV1`] take no `EphpmRequest*` — changing that would break the
    /// ABI, and modules built against major 1 must keep loading. The chain is
    /// invoked synchronously and never across an `.await`, so "the request on
    /// this thread" is well defined for the duration of a scope. Mirrors
    /// `ephpm_php::kv_bridge`'s per-thread site store, which is what gives the
    /// two lanes the same keyspace.
    static KV_SITE_STORE: RefCell<Option<Arc<ephpm_kv::store::Store>>> =
        const { RefCell::new(None) };
}

/// RAII scope binding the `kv_*` callbacks to one request's site store.
///
/// Restoring the **previous** value on drop rather than clearing is what makes
/// the guard safe to nest, and clearing on drop at all is what stops a pooled
/// tokio worker from carrying one tenant's scope into the next request it
/// picks up — the same stale-per-thread-state hazard the PHP lane's eBPF tag
/// guard exists for.
pub struct SiteKvScope {
    previous: Option<Arc<ephpm_kv::store::Store>>,
}

impl Drop for SiteKvScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        // `try_with`: during thread teardown the slot may already be gone, and
        // failing to restore it then is harmless (nothing will read it again).
        let _ = KV_SITE_STORE.try_with(|s| {
            *s.borrow_mut() = previous;
        });
    }
}

/// Bind the `kv_*` callbacks to `store` for as long as the returned guard
/// lives. Pass `None` to leave them on the process-global store.
///
/// The host calls this immediately around a synchronous chain invocation. It
/// must not be held across an `.await`: the guard is thread-local state and a
/// suspended task can resume on a different worker.
#[must_use]
pub fn enter_site_kv(store: Option<Arc<ephpm_kv::store::Store>>) -> SiteKvScope {
    let previous = KV_SITE_STORE.with(|s| s.replace(store));
    SiteKvScope { previous }
}

/// The store for the request being evaluated: this thread's site store when a
/// scope is active, the process-global store otherwise.
fn kv() -> Option<Arc<ephpm_kv::store::Store>> {
    KV_SITE_STORE
        .try_with(|s| s.borrow().clone())
        .ok()
        .flatten()
        .or_else(|| KV_STORE.get().cloned())
}

/// The process-global store, whatever request is in flight.
fn kv_global_store() -> Option<Arc<ephpm_kv::store::Store>> {
    KV_STORE.get().cloned()
}

unsafe fn key_str<'a>(key: *const u8, key_len: usize) -> Option<&'a str> {
    if key.is_null() {
        return None;
    }
    // SAFETY: module passes a valid (ptr, len) slice for the call duration.
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(key, key_len) }).ok()
}

/// Shared body of `kv_get` / `kv_get_global`; `store` picks the scope.
///
/// # Safety
///
/// ABI contract: `(key, key_len)` is a valid slice for the call, and
/// `out`/`out_len` are writable out-params.
unsafe fn kv_get_in(
    store: Option<Arc<ephpm_kv::store::Store>>,
    key: *const u8,
    key_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    if out.is_null() || out_len.is_null() {
        return -1;
    }
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (store, unsafe { key_str(key, key_len) }) else {
        return -1;
    };
    match store.get(k) {
        Some(v) => {
            // `Store::get` now returns `bytes::Bytes` (Arc-shared,
            // cheap to obtain). The middleware ABI hands PHP a
            // heap-owned `(ptr, len)` freed by `kv_free` via
            // `Box::from_raw` — that requires an owned allocation,
            // so the one memcpy stays at this FFI boundary and only
            // this boundary (previously the `.clone()` inside
            // `Store::get` did the same copy on every read too).
            let boxed: Box<[u8]> = v.as_ref().to_vec().into_boxed_slice();
            let len = boxed.len();
            // SAFETY: out/out_len checked non-null above.
            unsafe {
                *out = Box::into_raw(boxed).cast::<u8>();
                *out_len = len;
            }
            0
        }
        None => 1,
    }
}

unsafe extern "C" fn kv_get(
    key: *const u8,
    key_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_get_in(kv(), key, key_len, out, out_len) }
}

unsafe extern "C" fn kv_get_global(
    key: *const u8,
    key_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_get_in(kv_global_store(), key, key_len, out, out_len) }
}

unsafe extern "C" fn kv_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: (ptr, len) came from kv_get's Box::into_raw above.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

unsafe fn kv_value<'a>(value: *const u8, value_len: usize) -> &'a [u8] {
    if value.is_null() {
        &[]
    } else {
        // SAFETY: module passes a valid (ptr, len) slice for the call.
        unsafe { std::slice::from_raw_parts(value, value_len) }
    }
}

fn ttl_of(ttl_secs: i64) -> Option<Duration> {
    (ttl_secs > 0).then(|| Duration::from_secs(ttl_secs.unsigned_abs()))
}

/// Shared body of `kv_set` / `kv_set_global`.
///
/// # Safety
///
/// ABI contract: `(key, key_len)` and `(value, value_len)` are valid slices
/// for the duration of the call.
unsafe fn kv_set_in(
    store: Option<Arc<ephpm_kv::store::Store>>,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (store, unsafe { key_str(key, key_len) }) else {
        return -1;
    };
    // SAFETY: ABI contract.
    let v = unsafe { kv_value(value, value_len) };
    if store.set(k.to_string(), v.to_vec(), ttl_of(ttl_secs)) { 0 } else { -2 }
}

unsafe extern "C" fn kv_set(
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_set_in(kv(), key, key_len, value, value_len, ttl_secs) }
}

unsafe extern "C" fn kv_set_global(
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_set_in(kv_global_store(), key, key_len, value, value_len, ttl_secs) }
}

/// Shared body of `kv_set_nx` / `kv_set_nx_global`.
///
/// # Safety
///
/// As [`kv_set_in`].
unsafe fn kv_set_nx_in(
    store: Option<Arc<ephpm_kv::store::Store>>,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (store, unsafe { key_str(key, key_len) }) else {
        return -1;
    };
    // SAFETY: ABI contract.
    let v = unsafe { kv_value(value, value_len) };
    i32::from(!store.set_nx(k.to_string(), v.to_vec(), ttl_of(ttl_secs)))
}

unsafe extern "C" fn kv_set_nx(
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_set_nx_in(kv(), key, key_len, value, value_len, ttl_secs) }
}

unsafe extern "C" fn kv_set_nx_global(
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_set_nx_in(kv_global_store(), key, key_len, value, value_len, ttl_secs) }
}

/// Shared body of `kv_incr` / `kv_incr_global`.
///
/// # Safety
///
/// ABI contract: `(key, key_len)` is a valid slice and `out` is writable.
unsafe fn kv_incr_in(
    store: Option<Arc<ephpm_kv::store::Store>>,
    key: *const u8,
    key_len: usize,
    by: i64,
    out: *mut i64,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (store, unsafe { key_str(key, key_len) }) else {
        return -1;
    };
    match store.incr_by(k, by) {
        Ok(v) => {
            // SAFETY: out checked non-null above.
            unsafe { *out = v };
            0
        }
        Err(_) => -2,
    }
}

unsafe extern "C" fn kv_incr(key: *const u8, key_len: usize, by: i64, out: *mut i64) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_incr_in(kv(), key, key_len, by, out) }
}

unsafe extern "C" fn kv_incr_global(
    key: *const u8,
    key_len: usize,
    by: i64,
    out: *mut i64,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_incr_in(kv_global_store(), key, key_len, by, out) }
}

/// Shared body of `kv_incr_ttl` / `kv_incr_ttl_global`.
///
/// # Safety
///
/// As [`kv_incr_in`].
unsafe fn kv_incr_ttl_in(
    store: Option<Arc<ephpm_kv::store::Store>>,
    key: *const u8,
    key_len: usize,
    by: i64,
    ttl_secs: i64,
    out: *mut i64,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (store, unsafe { key_str(key, key_len) }) else {
        return -1;
    };
    match store.incr_by_with_ttl(k, by, ttl_of(ttl_secs)) {
        Ok(v) => {
            // SAFETY: out checked non-null above.
            unsafe { *out = v };
            0
        }
        Err(_) => -2,
    }
}

unsafe extern "C" fn kv_incr_ttl(
    key: *const u8,
    key_len: usize,
    by: i64,
    ttl_secs: i64,
    out: *mut i64,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_incr_ttl_in(kv(), key, key_len, by, ttl_secs, out) }
}

unsafe extern "C" fn kv_incr_ttl_global(
    key: *const u8,
    key_len: usize,
    by: i64,
    ttl_secs: i64,
    out: *mut i64,
) -> c_int {
    // SAFETY: ABI contract, forwarded unchanged.
    unsafe { kv_incr_ttl_in(kv_global_store(), key, key_len, by, ttl_secs, out) }
}

unsafe extern "C" fn host_log(level: c_int, msg: *const u8, msg_len: usize) {
    if msg.is_null() {
        return;
    }
    // SAFETY: module passes a valid (ptr, len) slice for the call.
    let bytes = unsafe { std::slice::from_raw_parts(msg, msg_len) };
    let text = String::from_utf8_lossy(bytes);
    match level {
        abi::LOG_ERROR => tracing::error!(target: "ephpm_middleware", "{text}"),
        abi::LOG_WARN => tracing::warn!(target: "ephpm_middleware", "{text}"),
        abi::LOG_DEBUG => tracing::debug!(target: "ephpm_middleware", "{text}"),
        _ => tracing::info!(target: "ephpm_middleware", "{text}"),
    }
}

static HOST_TABLE: EphpmHostV1 = EphpmHostV1 {
    abi_version: abi::ABI_V1,
    request_method,
    request_path,
    request_query,
    request_remote_ip,
    request_header,
    request_body,
    request_vhost_id,
    kv_get,
    kv_set,
    kv_set_nx,
    kv_incr,
    kv_free,
    log: host_log,
    kv_incr_ttl,
    response_status,
    response_headers,
    response_body,
    request_scheme,
    request_is_secure,
    request_host,
    kv_get_global,
    kv_set_global,
    kv_set_nx_global,
    kv_incr_global,
    kv_incr_ttl_global,
};

/// Wire the process-global KV store into the host table. Call once at startup,
/// before loading any middleware. Subsequent calls are ignored.
///
/// This is the store the `kv_*_global` callbacks always reach, and the one the
/// request-scoped `kv_*` callbacks fall back to outside a
/// [`enter_site_kv`] scope (single-site mode, and any request that matched no
/// vhost).
pub fn set_kv_store(store: &Arc<ephpm_kv::store::Store>) {
    let _ = KV_STORE.set(Arc::clone(store));
}

/// The process-wide v1 host table passed to every module's `init`.
#[must_use]
pub fn host_table() -> &'static EphpmHostV1 {
    &HOST_TABLE
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ephpm_kv::store::{Store, StoreConfig};

    use crate::abi::{self, EphpmHostV1};
    use crate::host::{KV_STORE, RequestCtx, enter_site_kv, host_table, set_kv_store};
    use crate::{Host, Request};

    fn ctx() -> RequestCtx {
        RequestCtx::new("GET", "/hook", "", "203.0.113.4", "srv", &[])
    }

    /// The process-global store as the callbacks see it.
    ///
    /// `KV_STORE` is a `OnceLock`, so whichever test in this binary calls
    /// [`set_kv_store`] first wins; every test must therefore read back what
    /// the table actually resolved rather than assuming its own handle won.
    fn global_store() -> Arc<Store> {
        set_kv_store(&Store::new(StoreConfig::default()));
        KV_STORE.get().expect("global store is set by now").clone()
    }

    fn site_store() -> Arc<Store> {
        Store::new(StoreConfig::default())
    }

    #[test]
    fn defaults_are_insecure_empty_host_no_body() {
        let ctx = ctx();
        // SAFETY: ctx and the real host table outlive the view.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert_eq!(req.scheme(), "http");
        assert!(!req.is_secure());
        assert_eq!(req.http_host(), "");
        assert!(req.body().is_empty());
    }

    /// Issue #390: the vhost accessor is a *tenant identity*, so "no tenant"
    /// must be distinguishable from a tenant. An empty site key crosses the
    /// ABI as NULL and surfaces as `None`, never as `Some("")` — otherwise a
    /// module keys its policy on a string an unauthenticated client chose.
    #[test]
    fn unmatched_host_has_no_vhost_identity() {
        let matched = RequestCtx::new("GET", "/x", "", "203.0.113.4", "blog", &[]);
        // SAFETY: ctx and the real host table outlive the view.
        let req = unsafe { Request::from_raw(matched.as_abi(), host_table()) };
        assert_eq!(req.vhost_id(), Some("blog"));

        let unmatched = RequestCtx::new("GET", "/x", "", "203.0.113.4", "", &[]);
        // SAFETY: as above.
        let req = unsafe { Request::from_raw(unmatched.as_abi(), host_table()) };
        assert_eq!(req.vhost_id(), None, "no matched vhost must read as no tenant");
        // SAFETY: the raw accessor is the C-ABI surface modules see; NULL is
        // the contract, and an empty non-null string would defeat the point.
        assert!(unsafe { (host_table().request_vhost_id)(unmatched.as_abi()) }.is_null());
    }

    /// The tenant identity and the request host are deliberately two different
    /// values: one is what the router resolved, the other is what the client
    /// sent. A module must be able to reach both without confusing them.
    #[test]
    fn vhost_id_and_http_host_stay_distinct() {
        // A suffixed request: `Host: blog.localhost` resolves to site `blog`.
        let ctx = RequestCtx::new("GET", "/x", "", "203.0.113.4", "blog", &[])
            .with_host("blog.localhost");
        // SAFETY: ctx and the real host table outlive the view.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert_eq!(req.vhost_id(), Some("blog"));
        assert_eq!(req.http_host(), "blog.localhost");
    }

    #[test]
    fn builders_set_scheme_host_and_body() {
        let ctx = ctx().with_scheme(true).with_host("blog.example").with_body(b"payload");
        // SAFETY: as above.
        let req = unsafe { Request::from_raw(ctx.as_abi(), host_table()) };
        assert_eq!(req.scheme(), "https");
        assert!(req.is_secure());
        assert_eq!(req.http_host(), "blog.example");
        assert_eq!(req.body(), b"payload");
    }

    /// A module built against minor 2 talking to a host advertising an OLDER
    /// minor must NOT read the appended fields — the safe wrapper falls back.
    /// (The `request_body` slot predates the gate, so it still works.)
    #[test]
    fn minor_gate_hides_appended_accessors_on_older_host() {
        // A downgraded copy of the real table: every field identical, only the
        // advertised minor rolled back below ABI_MINOR_REQUEST_ACCESSORS. All
        // fields are Copy (fn pointers + u32), so struct-update copies them.
        let old = EphpmHostV1 { abi_version: 0x0100_0001, ..*host_table() };
        assert!((old.abi_version & 0x00FF_FFFF) < abi::ABI_MINOR_REQUEST_ACCESSORS);

        let ctx = ctx().with_scheme(true).with_host("blog.example").with_body(b"payload");
        // SAFETY: ctx and `old` outlive the view.
        let req = unsafe { Request::from_raw(ctx.as_abi(), &old) };
        // Appended (minor-2) accessors fall back rather than read a short table.
        assert_eq!(req.scheme(), "http");
        assert!(!req.is_secure());
        assert_eq!(req.http_host(), "");
        // request_body has existed since minor 0 — not gated, still readable.
        assert_eq!(req.body(), b"payload");
    }

    // ── KV scoping (issue #376) ──────────────────────────────────────────

    /// THE isolation guard: two sites, one key name, no bleed. Middleware
    /// running for `tenant-a` must not see, overwrite, or be counted against
    /// `tenant-b`'s value — the same physical-store separation PHP's
    /// `ephpm_kv_*` already has.
    #[test]
    fn two_sites_never_share_a_middleware_kv_key() {
        let global = global_store();
        let a = site_store();
        let b = site_store();
        let host = Host::new(host_table());

        {
            let _scope = enter_site_kv(Some(Arc::clone(&a)));
            assert!(host.kv_set("shared-name", b"from-a", 0));
        }
        {
            let _scope = enter_site_kv(Some(Arc::clone(&b)));
            // B does not see A's write...
            assert_eq!(host.kv_get("shared-name"), None, "tenant B must not read tenant A's key");
            assert!(host.kv_set("shared-name", b"from-b", 0));
        }
        {
            let _scope = enter_site_kv(Some(Arc::clone(&a)));
            // ...and B's write did not clobber A's.
            assert_eq!(host.kv_get("shared-name").as_deref(), Some(&b"from-a"[..]));
        }

        // Neither tenant's write leaked into the process-global store, which is
        // where operator-owned state lives.
        assert_eq!(global.get("shared-name"), None);

        // A counter is per-tenant too — the rate-limiter case from #376.
        for store in [&a, &b] {
            let _scope = enter_site_kv(Some(Arc::clone(store)));
            assert_eq!(host.kv_incr_ttl("counter", 1, 60), Some(1), "each site counts from zero");
        }
    }

    /// The scope is per-request state on a pooled thread, so it must not
    /// outlive the request. Dropping the guard restores whatever was there
    /// before — nothing, for a top-level scope — or the next request served on
    /// this thread would inherit the previous tenant's keyspace.
    #[test]
    fn the_site_scope_does_not_outlive_its_guard() {
        let global = global_store();
        let site = site_store();
        let host = Host::new(host_table());

        {
            let _scope = enter_site_kv(Some(Arc::clone(&site)));
            assert!(host.kv_set("scoped-key", b"tenant", 0));
        }
        // Out of scope: writes land in the global store again.
        assert!(host.kv_set("unscoped-key", b"node", 0));
        assert_eq!(global.get("unscoped-key").as_deref(), Some(&b"node"[..]));
        assert_eq!(global.get("scoped-key"), None, "the tenant write must not have leaked");
        assert_eq!(host.kv_get("scoped-key"), None, "the scope must not persist past its guard");
    }

    /// Nesting restores the outer scope rather than clearing it, so a scope
    /// entered around an inner call cannot silently unscope the outer one.
    #[test]
    fn nested_scopes_restore_the_outer_store() {
        let _global = global_store();
        let outer = site_store();
        let inner = site_store();
        let host = Host::new(host_table());

        let _outer_scope = enter_site_kv(Some(Arc::clone(&outer)));
        {
            let _inner_scope = enter_site_kv(Some(Arc::clone(&inner)));
            assert!(host.kv_set("k", b"inner", 0));
        }
        assert!(host.kv_set("k", b"outer", 0));
        assert_eq!(inner.get("k").as_deref(), Some(&b"inner"[..]));
        assert_eq!(outer.get("k").as_deref(), Some(&b"outer"[..]));
    }

    /// The deliberate escape hatch (#384): operator-level state stays reachable
    /// on the process-global store from inside a tenant's scope, and a tenant's
    /// own keyspace can never shadow it. This is what keeps "readable by
    /// middleware, unwritable by any tenant's PHP" expressible.
    #[test]
    fn global_kv_reaches_the_process_store_from_inside_a_site_scope() {
        let global = global_store();
        let site = site_store();
        let host = Host::new(host_table());
        global.set("operator:flag".into(), b"on".to_vec(), None);

        let _scope = enter_site_kv(Some(Arc::clone(&site)));
        // Site-scoped read misses; global read hits.
        assert_eq!(host.kv_get("operator:flag"), None);
        assert_eq!(host.kv_get_global("operator:flag").as_deref(), Some(&b"on"[..]));

        // A tenant writing the same name shadows nothing.
        assert!(host.kv_set("operator:flag", b"tenant-says-off", 0));
        assert_eq!(host.kv_get_global("operator:flag").as_deref(), Some(&b"on"[..]));

        // The global write path targets the process store, not the scope.
        assert!(host.kv_set_global("operator:written", b"1", 0));
        assert_eq!(global.get("operator:written").as_deref(), Some(&b"1"[..]));
        assert_eq!(site.get("operator:written"), None);
    }

    /// A module built against minor 3 talking to a minor-2 host must not read
    /// the appended `kv_*_global` slots — they are past the end of that host's
    /// table. The safe wrapper degrades instead.
    #[test]
    fn minor_gate_hides_global_kv_on_an_older_host() {
        let _global = global_store();
        let old = EphpmHostV1 { abi_version: 0x0100_0002, ..*host_table() };
        assert!((old.abi_version & 0x00FF_FFFF) < abi::ABI_MINOR_GLOBAL_KV);

        let host = Host::new(&old);
        assert_eq!(host.kv_get_global("anything"), None);
        assert!(!host.kv_set_global("anything", b"v", 0));
        assert!(!host.kv_set_nx_global("anything", b"v", 0));
        assert_eq!(host.kv_incr_global("anything", 1), None);
        assert_eq!(host.kv_incr_ttl_global("anything", 1, 60), None);
        // The un-suffixed slots predate minor 3 and still work.
        assert!(host.kv_set("anything", b"v", 0));
    }
}

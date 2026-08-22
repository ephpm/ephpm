//! Host-side pieces: the per-request context handed to modules and the
//! process-wide [`abi::EphpmHostV1`] callback table. Used by `ephpm-server`
//! (feature `host`); middleware authors never touch this module.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::abi::{self, EphpmHostV1, EphpmRequest};

/// Owned, C-string-backed request context. Built by the router per request;
/// the opaque `EphpmRequest*` handed to modules is a pointer to this.
pub struct RequestCtx {
    method: CString,
    path: CString,
    query: CString,
    remote_ip: CString,
    vhost: CString,
    /// Lower-cased name → value (values pre-joined per HTTP list semantics).
    headers: Vec<(CString, CString)>,
    /// Secure-transport verdict for this request; see
    /// [`abi::EphpmHostV1::request_is_https`].
    https: bool,
}

impl RequestCtx {
    /// Build the context. Interior NULs are stripped (invalid in HTTP
    /// metadata anyway) rather than failing the request.
    ///
    /// Repeated header lines are joined into one value, as the ABI promises
    /// modules: `"; "` for `Cookie` (HTTP/2 may split it across field lines —
    /// RFC 9113 §8.2.3 — and the reassembled value must use the cookie-string
    /// separator), `", "` for everything else (RFC 9110 §5.3). Without this a
    /// module doing `req.header("cookie")` would silently see only the first
    /// line and could miss the very cookie it authenticates on.
    ///
    /// Defaults to `https = false`; the host sets the real verdict with
    /// [`RequestCtx::with_https`].
    #[must_use]
    pub fn new(
        method: &str,
        path: &str,
        query: &str,
        remote_ip: &str,
        vhost: &str,
        headers: &[(String, String)],
    ) -> Self {
        fn c(s: &str) -> CString {
            CString::new(s.replace('\0', "")).unwrap_or_default()
        }
        let mut joined: Vec<(String, String)> = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if let Some((_, existing)) = joined.iter_mut().find(|(n, _)| *n == lower) {
                existing.push_str(if lower == "cookie" { "; " } else { ", " });
                existing.push_str(value);
            } else {
                joined.push((lower, value.clone()));
            }
        }
        Self {
            method: c(method),
            path: c(path),
            query: c(query),
            remote_ip: c(remote_ip),
            vhost: c(vhost),
            headers: joined.iter().map(|(n, v)| (c(n), c(v))).collect(),
            https: false,
        }
    }

    /// Record whether this request arrived over a secure transport. The host
    /// passes its own trusted-proxy-aware verdict (the value PHP sees as
    /// `$_SERVER['HTTPS']`), never a raw client header.
    #[must_use]
    pub fn with_https(mut self, https: bool) -> Self {
        self.https = https;
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
    // SAFETY: ABI contract.
    unsafe { ctx(req) }.map_or(std::ptr::null(), |c| c.vhost.as_ptr())
}
unsafe extern "C" fn request_is_https(req: *const EphpmRequest) -> c_int {
    // SAFETY: ABI contract.
    c_int::from(unsafe { ctx(req) }.is_some_and(|c| c.https))
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
unsafe extern "C" fn request_body(_req: *const EphpmRequest, out_ptr: *mut *const u8) -> usize {
    // v1: the chain runs BEFORE the body is read (rejecting before the
    // transfer is the point) — no body view yet.
    if !out_ptr.is_null() {
        // SAFETY: module passes a valid out-pointer.
        unsafe { *out_ptr = std::ptr::null() };
    }
    0
}

// ── KV callbacks ─────────────────────────────────────────────────────────

static KV_STORE: OnceLock<Arc<ephpm_kv::store::Store>> = OnceLock::new();

fn kv() -> Option<&'static Arc<ephpm_kv::store::Store>> {
    KV_STORE.get()
}

unsafe fn key_str<'a>(key: *const u8, key_len: usize) -> Option<&'a str> {
    if key.is_null() {
        return None;
    }
    // SAFETY: module passes a valid (ptr, len) slice for the call duration.
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(key, key_len) }).ok()
}

unsafe extern "C" fn kv_get(
    key: *const u8,
    key_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    if out.is_null() || out_len.is_null() {
        return -1;
    }
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (kv(), unsafe { key_str(key, key_len) }) else {
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

unsafe extern "C" fn kv_set(
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (kv(), unsafe { key_str(key, key_len) }) else {
        return -1;
    };
    // SAFETY: ABI contract.
    let v = unsafe { kv_value(value, value_len) };
    if store.set(k.to_string(), v.to_vec(), ttl_of(ttl_secs)) { 0 } else { -2 }
}

unsafe extern "C" fn kv_set_nx(
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_secs: i64,
) -> c_int {
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (kv(), unsafe { key_str(key, key_len) }) else {
        return -1;
    };
    // SAFETY: ABI contract.
    let v = unsafe { kv_value(value, value_len) };
    i32::from(!store.set_nx(k.to_string(), v.to_vec(), ttl_of(ttl_secs)))
}

unsafe extern "C" fn kv_incr(key: *const u8, key_len: usize, by: i64, out: *mut i64) -> c_int {
    if out.is_null() {
        return -1;
    }
    // SAFETY: ABI contract.
    let (Some(store), Some(k)) = (kv(), unsafe { key_str(key, key_len) }) else {
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

unsafe extern "C" fn kv_incr_ttl(
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
    let (Some(store), Some(k)) = (kv(), unsafe { key_str(key, key_len) }) else {
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
    request_is_https,
};

/// Wire the embedded KV store into the host table. Call once at startup,
/// before loading any middleware. Subsequent calls are ignored.
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
    use super::*;
    use crate::Request;

    fn view(ctx: &RequestCtx) -> Request<'_> {
        // SAFETY: `ctx` outlives the returned view; host_table() is 'static.
        unsafe { Request::from_raw(ctx.as_abi(), host_table()) }
    }

    fn ctx(headers: &[(String, String)]) -> RequestCtx {
        RequestCtx::new("GET", "/x.php", "", "203.0.113.5", "t.example", headers)
    }

    fn h(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    #[test]
    fn repeated_cookie_lines_are_joined_with_the_cookie_separator() {
        // HTTP/2 may split `Cookie` across field lines (RFC 9113 §8.2.3) and
        // hyper surfaces them separately. A module reading `cookie` must see
        // ALL of them, or it can miss the very cookie it authenticates on.
        let ctx = ctx(&[h("cookie", "a=1"), h("Cookie", "b=2"), h("COOKIE", "c=3")]);
        assert_eq!(view(&ctx).header("cookie"), Some("a=1; b=2; c=3"));
    }

    #[test]
    fn repeated_ordinary_headers_are_joined_as_a_comma_list() {
        let ctx = ctx(&[h("accept", "text/html"), h("Accept", "application/json")]);
        assert_eq!(view(&ctx).header("Accept"), Some("text/html, application/json"));
    }

    #[test]
    fn a_single_header_is_unchanged_and_lookup_is_case_insensitive() {
        let ctx = ctx(&[h("X-Api-Key", "k1")]);
        assert_eq!(view(&ctx).header("x-api-key"), Some("k1"));
        assert_eq!(view(&ctx).header("X-API-KEY"), Some("k1"));
        assert_eq!(view(&ctx).header("absent"), None);
    }

    #[test]
    fn https_defaults_off_and_is_set_only_by_the_host() {
        assert!(!view(&ctx(&[])).is_https(), "default must be the cleartext (safe) verdict");
        let secure = ctx(&[]).with_https(true);
        assert!(view(&secure).is_https());
    }
}

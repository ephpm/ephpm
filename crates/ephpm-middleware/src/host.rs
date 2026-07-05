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
}

impl RequestCtx {
    /// Build the context. Interior NULs are stripped (invalid in HTTP
    /// metadata anyway) rather than failing the request.
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
        Self {
            method: c(method),
            path: c(path),
            query: c(query),
            remote_ip: c(remote_ip),
            vhost: c(vhost),
            headers: headers.iter().map(|(n, v)| (c(&n.to_ascii_lowercase()), c(v))).collect(),
        }
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
            let boxed = v.into_boxed_slice();
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

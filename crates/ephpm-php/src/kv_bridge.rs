//! Bridge between the C KV PHP functions and the Rust [`ephpm_kv::store::Store`].
//!
//! Provides a C-compatible function pointer table ([`EphpmKvOps`]) that the
//! PHP `ephpm_kv_*` native functions call into. Each callback delegates to the
//! global [`Store`] instance set via [`set_store`].
//!
//! The `get` result is stored in a thread-local buffer to avoid malloc/free
//! across the FFI boundary. The C side copies the data via `RETURN_STRINGL`.
//!
//! # Thread teardown (issue #269)
//!
//! `ThreadPhpGuard`'s destructor runs `php_request_shutdown()` on a retiring
//! worker thread, which executes userland `register_shutdown_function()`
//! callbacks and object destructors — and those can call `ephpm_kv_*`. By then
//! [`KV_GET_BUF`] is already destroyed ( it is first touched *inside* a
//! request, i.e. registered after the guard, and destructors run in reverse
//! registration order), so every access made from a PHP native uses
//! `LocalKey::try_with`: `with` would panic inside a TLS destructor, which is
//! an unconditional `SIGABRT` for the whole process.
//!
//! The degraded answer is always the store's own "nothing here" answer — `get`
//! reports a miss, `wait` reports a timeout, the site store resolves to
//! `None`, which makes every op return failure. No silent success and no
//! cross-tenant fallback. See `db_bridge`'s module docs for the full
//! mechanism.

#[cfg(php_linked)]
use std::cell::RefCell;
#[cfg(php_linked)]
use std::ffi::CStr;
#[cfg(php_linked)]
use std::sync::{Arc, OnceLock};
#[cfg(php_linked)]
use std::time::Duration;

#[cfg(php_linked)]
use ephpm_kv::store::Store;

// ── Global store handle ─────────────────────────────────────────────────

#[cfg(php_linked)]
static KV_STORE: OnceLock<Arc<Store>> = OnceLock::new();

// ── Thread-local state ──────────────────────────────────────────────────

#[cfg(php_linked)]
thread_local! {
    static KV_GET_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    /// Per-request site store for vhost KV isolation.
    /// Points to the site-specific Store for the current request's hostname.
    /// When None, falls back to the global store (single-site mode).
    static KV_SITE_STORE: RefCell<Option<Arc<Store>>> = const { RefCell::new(None) };
}

/// Set the KV store for the current thread/request.
///
/// Called by the request handler before PHP execution. In multi-tenant
/// mode, this points to the site-specific store. In single-site mode,
/// pass `None` to use the global store.
#[cfg(php_linked)]
pub fn set_site_store(store: Option<Arc<Store>>) {
    // Plain `with` on purpose (issue #269): Rust-side per-request setup on a
    // live thread, never reachable from a PHP shutdown function. The *read*
    // side ([`effective_store`]) is reachable and does use `try_with`.
    KV_SITE_STORE.with(|s| {
        *s.borrow_mut() = store;
    });
}

/// Stub when PHP is not linked.
#[cfg(not(php_linked))]
pub fn set_site_store(_store: Option<std::sync::Arc<ephpm_kv::store::Store>>) {}

/// Get the effective store for the current request.
/// Returns the site-specific store if set, otherwise the global store.
///
/// Returns `None` when the per-thread site slot has already been destroyed
/// (issue #269) — **fail closed**, not "fall back to the global store". In
/// multi-tenant mode that fallback would let a tenant's shutdown function
/// write into a keyspace that is not its own; a KV miss is the safe answer.
#[cfg(php_linked)]
fn effective_store() -> Option<Arc<Store>> {
    KV_SITE_STORE
        .try_with(|s| {
            let site = s.borrow();
            if let Some(ref store) = *site {
                Some(Arc::clone(store))
            } else {
                KV_STORE.get().cloned()
            }
        })
        .unwrap_or(None)
}

// ── C-compatible ops struct ─────────────────────────────────────────────

/// Function pointer table passed to C so PHP native functions can call
/// into the Rust KV store without knowing about Rust types.
#[cfg(php_linked)]
#[repr(C)]
pub struct EphpmKvOps {
    /// Get a value by key. Returns 1 if found, 0 if not.
    /// The result is stored in a thread-local buffer and retrieved
    /// via `get_result`.
    pub get: Option<unsafe extern "C" fn(key: *const std::os::raw::c_char) -> std::os::raw::c_int>,

    /// Retrieve the pointer and length of the last `get` result.
    pub get_result:
        Option<unsafe extern "C" fn(ptr: *mut *const std::os::raw::c_char, len: *mut usize)>,

    /// Set a key to a value. `ttl_ms` of 0 means no expiry.
    /// Returns 1 on success, 0 on failure (e.g. OOM with noeviction).
    pub set: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            val: *const std::os::raw::c_char,
            val_len: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Atomically set a key only if it doesn't already exist (SETNX).
    /// Returns 1 if the value was inserted, 0 if a live entry was
    /// already present at this key (or the write was refused under
    /// `NoEviction`). The check-and-set is performed under the same
    /// per-key shard lock, so concurrent callers see exactly one winner.
    pub set_nx: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            val: *const std::os::raw::c_char,
            val_len: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Delete a key. Returns 1 if it existed, 0 if not.
    pub del: Option<unsafe extern "C" fn(key: *const std::os::raw::c_char) -> std::os::raw::c_long>,

    /// Check if a key exists. Returns 1 if yes, 0 if no.
    pub exists:
        Option<unsafe extern "C" fn(key: *const std::os::raw::c_char) -> std::os::raw::c_int>,

    /// Increment value by delta. Stores result in `*result`.
    /// Returns 1 on success, 0 on error (value not an integer).
    pub incr_by: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            delta: std::os::raw::c_longlong,
            result: *mut std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Set TTL on a key. `ttl_ms` in milliseconds. Returns 1 if key
    /// exists, 0 if not.
    pub expire: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Get remaining TTL in milliseconds. Returns -1 for no expiry,
    /// -2 for missing key.
    pub pttl:
        Option<unsafe extern "C" fn(key: *const std::os::raw::c_char) -> std::os::raw::c_longlong>,

    /// Remove all keys from the effective store (per-site if set, else
    /// global). Backs Redis-style `FLUSHDB` / `FLUSHALL` from PHP userland.
    /// Returns 1 on success, 0 if no store is registered.
    pub flush_all: Option<unsafe extern "C" fn() -> std::os::raw::c_int>,

    /// Block until the key's watch version exceeds `last_version` or
    /// `timeout_ms` elapses (see `Store::wait_for_change`). Returns:
    /// - `0` — timeout (or no store registered); `*new_version` untouched.
    /// - `1` — version advanced and the key holds a value: `*new_version`
    ///   receives the version and the value is in the thread-local get
    ///   buffer (retrieve via `get_result`, same contract as `get`).
    /// - `2` — version advanced but the key is absent (deleted/expired):
    ///   `*new_version` receives the version; the get buffer is untouched.
    ///
    /// Blocking is safe here: callers are PHP worker OS threads or the
    /// tokio `spawn_blocking` pool, never async tasks.
    pub wait: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            last_version: std::os::raw::c_longlong,
            timeout_ms: std::os::raw::c_longlong,
            new_version: *mut std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,
}

// ── Callback implementations ────────────────────────────────────────────

#[cfg(php_linked)]
unsafe extern "C" fn kv_get(key: *const std::os::raw::c_char) -> std::os::raw::c_int {
    // Safety: `key` is a null-terminated C string provided by PHP's
    // zend_parse_parameters. Valid for the duration of this call.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    match store.get(key_str) {
        Some(val) => {
            // `try_with` (issue #269): reachable from a shutdown function on
            // an exiting thread. Without the buffer there is nowhere to hand
            // the value to C, so report a miss rather than returning 1 with a
            // stale/empty buffer behind it.
            let staged = KV_GET_BUF.try_with(|buf| {
                let mut buf = buf.borrow_mut();
                buf.clear();
                buf.extend_from_slice(&val);
            });
            i32::from(staged.is_ok())
        }
        None => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_get_result(ptr: *mut *const std::os::raw::c_char, len: *mut usize) {
    // Safety: `ptr` and `len` are valid pointers provided by our own C code
    // in `PHP_FUNCTION(ephpm_kv_get)`. The buffer remains valid because this
    // is called on the same thread immediately after `kv_get`, and the
    // thread-local buffer is not modified until the next `kv_get` call.
    // `try_with` (issue #269). A destroyed buffer reports an empty result;
    // the C side sees len 0 and returns an empty string rather than reading a
    // pointer that no longer has a buffer behind it. Unreachable in practice
    // because `kv_get` would have returned 0, but the pointer contract must
    // hold on its own.
    let staged = KV_GET_BUF.try_with(|buf| {
        let buf = buf.borrow();
        unsafe {
            *ptr = buf.as_ptr().cast();
            *len = buf.len();
        }
    });
    if staged.is_err() {
        unsafe {
            *ptr = std::ptr::null();
            *len = 0;
        }
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_set(
    key: *const std::os::raw::c_char,
    val: *const std::os::raw::c_char,
    val_len: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` is a null-terminated C string from PHP. `val` is a
    // pointer to `val_len` bytes from PHP's string parameter.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };
    // Safety: `val` points to `val_len` bytes of valid memory from PHP.
    let val_bytes = unsafe { std::slice::from_raw_parts(val.cast::<u8>(), val_len) };

    let ttl = if ttl_ms > 0 {
        #[allow(clippy::cast_sign_loss)]
        Some(Duration::from_millis(ttl_ms as u64))
    } else {
        None
    };

    i32::from(store.set(key_str.to_string(), val_bytes.to_vec(), ttl))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_set_nx(
    key: *const std::os::raw::c_char,
    val: *const std::os::raw::c_char,
    val_len: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` is a null-terminated C string from PHP. `val` is a
    // pointer to `val_len` bytes from PHP's string parameter.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };
    // Safety: `val` points to `val_len` bytes of valid memory from PHP.
    let val_bytes = unsafe { std::slice::from_raw_parts(val.cast::<u8>(), val_len) };

    let ttl = if ttl_ms > 0 {
        #[allow(clippy::cast_sign_loss)]
        Some(Duration::from_millis(ttl_ms as u64))
    } else {
        None
    };

    i32::from(store.set_nx(key_str.to_string(), val_bytes.to_vec(), ttl))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_del(key: *const std::os::raw::c_char) -> std::os::raw::c_long {
    // Safety: `key` is a null-terminated C string from PHP.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    std::os::raw::c_long::from(store.remove(&key_str))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_exists(key: *const std::os::raw::c_char) -> std::os::raw::c_int {
    // Safety: `key` is a null-terminated C string from PHP.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    i32::from(store.exists(&key_str))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_incr_by(
    key: *const std::os::raw::c_char,
    delta: std::os::raw::c_longlong,
    result: *mut std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` is a null-terminated C string from PHP. `result` is
    // a valid pointer to a local variable in our C wrapper code.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    match store.incr_by(&key_str, delta) {
        Ok(val) => {
            // Safety: `result` points to a valid `long long` in our C code.
            unsafe { *result = val };
            1
        }
        Err(_) => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_expire(
    key: *const std::os::raw::c_char,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` is a null-terminated C string from PHP.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    if ttl_ms <= 0 {
        return 0;
    }

    #[allow(clippy::cast_sign_loss)]
    let ttl = Duration::from_millis(ttl_ms as u64);
    i32::from(store.expire(&key_str, ttl))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_pttl(key: *const std::os::raw::c_char) -> std::os::raw::c_longlong {
    // Safety: `key` is a null-terminated C string from PHP.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return -2;
    };
    let Some(store) = effective_store() else {
        return -2;
    };

    match store.pttl(&key_str) {
        Some(ms) => ms,
        None => -2, // key does not exist
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_flush_all() -> std::os::raw::c_int {
    let Some(store) = effective_store() else {
        return 0;
    };
    store.flush();
    1
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_wait(
    key: *const std::os::raw::c_char,
    last_version: std::os::raw::c_longlong,
    timeout_ms: std::os::raw::c_longlong,
    new_version: *mut std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` is a null-terminated C string provided by PHP's
    // zend_parse_parameters. Valid for the duration of this call.
    let key_str = unsafe { CStr::from_ptr(key) };
    let Ok(key_str) = key_str.to_str() else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    // Negative inputs clamp to 0: last_version 0 = "register + snapshot",
    // timeout 0 = non-blocking poll.
    let last = u64::try_from(last_version).unwrap_or(0);
    let timeout = Duration::from_millis(u64::try_from(timeout_ms).unwrap_or(0));

    let Some((version, value)) = store.wait_for_change(key_str, last, timeout) else {
        return 0;
    };

    // Stage the value BEFORE reporting success. `try_with` (issue #269): a
    // shutdown function on an exiting thread has no get buffer left, and the
    // documented contract says `*new_version` is untouched when this returns
    // 0 — so the write below must not happen on the degraded path.
    let rc = match value {
        Some(val) => {
            let staged = KV_GET_BUF.try_with(|buf| {
                let mut buf = buf.borrow_mut();
                buf.clear();
                buf.extend_from_slice(&val);
            });
            if staged.is_ok() { 1 } else { 0 }
        }
        None => 2,
    };
    if rc == 0 {
        return 0;
    }
    // Safety: `new_version` points to a valid `long long` local in
    // our C wrapper code (PHP_FUNCTION(ephpm_kv_wait)).
    unsafe { *new_version = std::os::raw::c_longlong::try_from(version).unwrap_or(i64::MAX) };
    rc
}

// ── Static ops table ────────────────────────────────────────────────────

/// The C-compatible function pointer table, ready to pass to
/// `ephpm_set_kv_ops()`.
#[cfg(php_linked)]
pub static KV_OPS: EphpmKvOps = EphpmKvOps {
    get: Some(kv_get),
    get_result: Some(kv_get_result),
    set: Some(kv_set),
    set_nx: Some(kv_set_nx),
    del: Some(kv_del),
    exists: Some(kv_exists),
    incr_by: Some(kv_incr_by),
    expire: Some(kv_expire),
    pttl: Some(kv_pttl),
    flush_all: Some(kv_flush_all),
    wait: Some(kv_wait),
};

// ── Public API ──────────────────────────────────────────────────────────

/// Register the KV store instance so PHP native functions can access it.
///
/// Must be called before any PHP requests execute. Safe to call from any
/// thread. Subsequent calls are no-ops (the first store wins).
#[cfg(php_linked)]
pub fn set_store(store: Arc<Store>) {
    let _ = KV_STORE.set(store);
    tracing::debug!("KV store registered for PHP native functions");
}

/// Stub `set_store` when PHP is not linked — compiles to nothing.
#[cfg(not(php_linked))]
pub fn set_store(_store: std::sync::Arc<ephpm_kv::store::Store>) {
    // No-op in stub mode.
}

// ── Tests ───────────────────────────────────────────────────────────────
//
// These tests exercise the Rust callback layer (`kv_get`, `kv_set`, etc.)
// directly, bypassing the PHP function registration layer. They require
// a real libphp link (`php_linked`) because the callbacks and `KV_STORE`
// only exist in that configuration.
//
// Run with: cargo nextest run -p ephpm-php --run-ignored all
//   (or `cargo test` after `cargo xtask release`)

#[cfg(all(test, php_linked))]
mod tests {
    use std::ffi::CString;
    use std::sync::{Arc, OnceLock};
    use std::thread;
    use std::time::Duration;

    use ephpm_kv::store::{Store, StoreConfig};
    use serial_test::serial;

    use super::*;

    // All bridge tests share one store (OnceLock can only be set once per
    // process). Keys are namespaced per test to avoid cross-test interference.
    static BRIDGE_STORE: OnceLock<Arc<Store>> = OnceLock::new();

    fn init_store() -> Arc<Store> {
        BRIDGE_STORE
            .get_or_init(|| {
                let s = Store::new(StoreConfig::default());
                set_store(Arc::clone(&s));
                s
            })
            .clone()
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    // ── get / get_result ────────────────────────────────────────────────

    #[test]
    #[serial]
    fn get_missing_returns_zero() {
        init_store();
        let key = cstr("bridge_get_missing");
        // Safety: key is a valid C string.
        let found = unsafe { kv_get(key.as_ptr()) };
        assert_eq!(found, 0);
    }

    #[test]
    #[serial]
    fn set_and_get_round_trip() {
        let store = init_store();
        store.set("bridge_rtrip".into(), b"hello".to_vec(), None);

        let key = cstr("bridge_rtrip");
        // Safety: key is a valid C string.
        let found = unsafe { kv_get(key.as_ptr()) };
        assert_eq!(found, 1);

        let mut ptr: *const std::os::raw::c_char = std::ptr::null();
        let mut len: usize = 0;
        // Safety: ptr and len are valid stack variables.
        unsafe { kv_get_result(&mut ptr, &mut len) };
        let got = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        assert_eq!(got, b"hello");
    }

    #[test]
    #[serial]
    fn get_result_reflects_thread_local_after_get() {
        let store = init_store();
        store.set("bridge_tl_a".into(), b"aaa".to_vec(), None);
        store.set("bridge_tl_b".into(), b"bbb".to_vec(), None);

        // Get "a", then "b" — result buffer should reflect the last call.
        unsafe { kv_get(cstr("bridge_tl_a").as_ptr()) };
        unsafe { kv_get(cstr("bridge_tl_b").as_ptr()) };

        let mut ptr: *const std::os::raw::c_char = std::ptr::null();
        let mut len: usize = 0;
        unsafe { kv_get_result(&mut ptr, &mut len) };
        let got = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        assert_eq!(got, b"bbb");
    }

    // ── kv_set ──────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn set_stores_value() {
        let store = init_store();
        let key = cstr("bridge_set_k");
        let val = b"value";
        // Safety: key and val are valid for the duration of the call.
        let ok = unsafe { kv_set(key.as_ptr(), val.as_ptr().cast(), val.len(), 0) };
        assert_eq!(ok, 1);
        assert_eq!(store.get("bridge_set_k").as_deref(), Some(&b"value"[..]));
    }

    #[test]
    #[serial]
    fn set_with_ttl_stores_value_with_expiry() {
        let store = init_store();
        let key = cstr("bridge_set_ttl");
        let val = b"expiring";
        // 60 seconds in milliseconds.
        // Safety: key and val are valid pointers.
        unsafe { kv_set(key.as_ptr(), val.as_ptr().cast(), val.len(), 60_000) };
        let pttl = store.pttl("bridge_set_ttl").unwrap();
        assert!(pttl > 0 && pttl <= 60_000, "expected TTL in (0, 60000], got {pttl}");
    }

    #[test]
    #[serial]
    fn set_with_zero_ttl_stores_without_expiry() {
        let store = init_store();
        let key = cstr("bridge_set_nottl");
        let val = b"forever";
        // Safety: key and val are valid pointers.
        unsafe { kv_set(key.as_ptr(), val.as_ptr().cast(), val.len(), 0) };
        assert_eq!(store.pttl("bridge_set_nottl"), Some(-1));
    }

    #[test]
    #[serial]
    fn set_handles_binary_value() {
        let store = init_store();
        let key = cstr("bridge_set_bin");
        let val: &[u8] = &[0x00, 0x01, 0xFF, 0xFE];
        // Safety: key is a valid C string; val is valid for val.len() bytes.
        unsafe { kv_set(key.as_ptr(), val.as_ptr().cast(), val.len(), 0) };
        assert_eq!(store.get("bridge_set_bin").as_deref(), Some(&val[..]));
    }

    // ── kv_del ──────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn del_existing_returns_one() {
        let store = init_store();
        store.set("bridge_del_k".into(), b"v".to_vec(), None);
        let key = cstr("bridge_del_k");
        // Safety: key is a valid C string.
        let removed = unsafe { kv_del(key.as_ptr()) };
        assert_eq!(removed, 1);
        assert_eq!(store.get("bridge_del_k"), None);
    }

    #[test]
    #[serial]
    fn del_missing_returns_zero() {
        init_store();
        let key = cstr("bridge_del_missing");
        // Safety: key is a valid C string.
        let removed = unsafe { kv_del(key.as_ptr()) };
        assert_eq!(removed, 0);
    }

    // ── kv_exists ───────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn exists_present_returns_one() {
        let store = init_store();
        store.set("bridge_ex_k".into(), b"v".to_vec(), None);
        let key = cstr("bridge_ex_k");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_exists(key.as_ptr()) }, 1);
    }

    #[test]
    #[serial]
    fn exists_absent_returns_zero() {
        init_store();
        let key = cstr("bridge_ex_missing");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_exists(key.as_ptr()) }, 0);
    }

    // ── kv_incr_by ──────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn incr_by_creates_key() {
        let store = init_store();
        let key = cstr("bridge_incr_new");
        let mut result: std::os::raw::c_longlong = 0;
        // Safety: key and result are valid.
        let ok = unsafe { kv_incr_by(key.as_ptr(), 1, &mut result) };
        assert_eq!(ok, 1);
        assert_eq!(result, 1);
        assert_eq!(store.get("bridge_incr_new").as_deref(), Some(&b"1"[..]));
    }

    #[test]
    #[serial]
    fn incr_by_delta_accumulates() {
        let store = init_store();
        store.set("bridge_incr_acc".into(), b"10".to_vec(), None);
        let key = cstr("bridge_incr_acc");
        let mut result: std::os::raw::c_longlong = 0;
        // Safety: key and result are valid.
        unsafe { kv_incr_by(key.as_ptr(), 5, &mut result) };
        assert_eq!(result, 15);
    }

    #[test]
    #[serial]
    fn incr_by_negative_decrements() {
        let store = init_store();
        store.set("bridge_incr_neg".into(), b"10".to_vec(), None);
        let key = cstr("bridge_incr_neg");
        let mut result: std::os::raw::c_longlong = 0;
        // Safety: key and result are valid.
        unsafe { kv_incr_by(key.as_ptr(), -3, &mut result) };
        assert_eq!(result, 7);
    }

    #[test]
    #[serial]
    fn incr_by_non_integer_returns_zero() {
        let store = init_store();
        store.set("bridge_incr_str".into(), b"not_a_number".to_vec(), None);
        let key = cstr("bridge_incr_str");
        let mut result: std::os::raw::c_longlong = 0;
        // Safety: key and result are valid.
        let ok = unsafe { kv_incr_by(key.as_ptr(), 1, &mut result) };
        assert_eq!(ok, 0); // error — value is not an integer
    }

    // ── kv_expire / kv_pttl ─────────────────────────────────────────────

    #[test]
    #[serial]
    fn expire_sets_ttl_on_existing_key() {
        let store = init_store();
        store.set("bridge_exp_k".into(), b"v".to_vec(), None);
        let key = cstr("bridge_exp_k");
        // Safety: key is a valid C string.
        let ok = unsafe { kv_expire(key.as_ptr(), 30_000) };
        assert_eq!(ok, 1);
        let pttl = store.pttl("bridge_exp_k").unwrap();
        assert!(pttl > 0 && pttl <= 30_000, "expected PTTL in (0, 30000], got {pttl}");
    }

    #[test]
    #[serial]
    fn expire_on_missing_key_returns_zero() {
        init_store();
        let key = cstr("bridge_exp_missing");
        // Safety: key is a valid C string.
        let ok = unsafe { kv_expire(key.as_ptr(), 10_000) };
        assert_eq!(ok, 0);
    }

    #[test]
    #[serial]
    fn expire_zero_or_negative_returns_zero() {
        let store = init_store();
        store.set("bridge_exp_neg".into(), b"v".to_vec(), None);
        let key = cstr("bridge_exp_neg");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_expire(key.as_ptr(), 0) }, 0);
        assert_eq!(unsafe { kv_expire(key.as_ptr(), -1) }, 0);
        // Key should be unaffected.
        assert_eq!(store.pttl("bridge_exp_neg"), Some(-1));
    }

    #[test]
    #[serial]
    fn pttl_no_expiry_returns_minus_one() {
        let store = init_store();
        store.set("bridge_pttl_noexp".into(), b"v".to_vec(), None);
        let key = cstr("bridge_pttl_noexp");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_pttl(key.as_ptr()) }, -1);
    }

    #[test]
    #[serial]
    fn pttl_missing_key_returns_minus_two() {
        init_store();
        let key = cstr("bridge_pttl_missing");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_pttl(key.as_ptr()) }, -2);
    }

    #[test]
    #[serial]
    fn pttl_with_expiry_returns_positive() {
        let store = init_store();
        store.set("bridge_pttl_exp".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        let key = cstr("bridge_pttl_exp");
        // Safety: key is a valid C string.
        let ms = unsafe { kv_pttl(key.as_ptr()) };
        assert!(ms > 0 && ms <= 60_000, "expected PTTL in (0, 60000], got {ms}");
    }

    // ── kv_flush_all ────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn flush_all_removes_all_keys_from_global_store() {
        let store = init_store();
        store.set("bridge_flush_a".into(), b"v1".to_vec(), None);
        store.set("bridge_flush_b".into(), b"v2".to_vec(), Some(Duration::from_secs(60)));
        store.hset("bridge_flush_h", "f", b"v".to_vec());

        // Safety: no arguments; effective_store() falls back to global.
        let ok = unsafe { kv_flush_all() };
        assert_eq!(ok, 1);

        assert_eq!(store.len(), 0);
        assert_eq!(store.mem_used(), 0);
        assert!(!store.exists("bridge_flush_a"));
        assert!(!store.exists("bridge_flush_b"));
        assert!(!store.exists("bridge_flush_h"));
    }

    #[test]
    #[serial]
    fn flush_all_only_affects_site_store_when_set() {
        // Make sure the global store has data, then point this thread at a
        // separate site store and flush; the global store must be untouched.
        let global = init_store();
        global.set("bridge_flush_global".into(), b"keep_me".to_vec(), None);

        let site = Store::new(StoreConfig::default());
        site.set("bridge_flush_site".into(), b"goodbye".to_vec(), None);
        set_site_store(Some(Arc::clone(&site)));

        // Safety: no arguments.
        let ok = unsafe { kv_flush_all() };
        assert_eq!(ok, 1);

        assert_eq!(site.len(), 0, "site store should be empty");
        assert_eq!(site.mem_used(), 0, "site mem counter should be reset");
        assert_eq!(
            global.get("bridge_flush_global").as_deref(),
            Some(&b"keep_me"[..]),
            "global store must be untouched when a site store is active"
        );

        // Reset thread-local state so it doesn't leak into other serial tests.
        set_site_store(None);
    }

    // ── kv_wait ─────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn wait_with_zero_last_version_snapshots_immediately() {
        let store = init_store();
        store.set("bridge_wait_snap".into(), b"current".to_vec(), None);

        let key = cstr("bridge_wait_snap");
        let mut new_version: std::os::raw::c_longlong = 0;
        // Safety: key and new_version are valid for the duration of the call.
        let rc = unsafe { kv_wait(key.as_ptr(), 0, 5_000, &mut new_version) };
        assert_eq!(rc, 1, "snapshot must report a present value");
        assert!(new_version >= 1);

        let mut ptr: *const std::os::raw::c_char = std::ptr::null();
        let mut len: usize = 0;
        // Safety: ptr and len are valid stack variables.
        unsafe { kv_get_result(&mut ptr, &mut len) };
        let got = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        assert_eq!(got, b"current");
    }

    #[test]
    #[serial]
    fn wait_snapshot_on_missing_key_returns_absent() {
        init_store();
        let key = cstr("bridge_wait_missing");
        let mut new_version: std::os::raw::c_longlong = 0;
        // Safety: key and new_version are valid.
        let rc = unsafe { kv_wait(key.as_ptr(), 0, 1_000, &mut new_version) };
        assert_eq!(rc, 2, "missing key must report changed-but-absent");
        assert!(new_version >= 1);
    }

    #[test]
    #[serial]
    fn wait_times_out_when_nothing_changes() {
        let store = init_store();
        store.set("bridge_wait_to".into(), b"v".to_vec(), None);
        let key = cstr("bridge_wait_to");

        // Snapshot to learn the current version.
        let mut ver: std::os::raw::c_longlong = 0;
        // Safety: key and ver are valid.
        assert_eq!(unsafe { kv_wait(key.as_ptr(), 0, 1_000, &mut ver) }, 1);

        // Waiting past the current version with no writes must time out.
        let start = std::time::Instant::now();
        let mut unused: std::os::raw::c_longlong = 0;
        // Safety: key and unused are valid.
        let rc = unsafe { kv_wait(key.as_ptr(), ver, 80, &mut unused) };
        assert_eq!(rc, 0, "must time out");
        assert!(start.elapsed() >= Duration::from_millis(80));
    }

    #[test]
    #[serial]
    fn wait_wakes_on_concurrent_set() {
        let store = init_store();
        store.set("bridge_wait_wake".into(), b"old".to_vec(), None);
        let key = cstr("bridge_wait_wake");

        let mut ver: std::os::raw::c_longlong = 0;
        // Safety: key and ver are valid.
        assert_eq!(unsafe { kv_wait(key.as_ptr(), 0, 1_000, &mut ver) }, 1);

        let writer = {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                store.set("bridge_wait_wake".into(), b"new".to_vec(), None);
            })
        };

        let mut new_ver: std::os::raw::c_longlong = 0;
        // Safety: key and new_ver are valid.
        let rc = unsafe { kv_wait(key.as_ptr(), ver, 10_000, &mut new_ver) };
        writer.join().unwrap();

        assert_eq!(rc, 1, "waiter must wake with a value");
        assert!(new_ver > ver);
        let mut ptr: *const std::os::raw::c_char = std::ptr::null();
        let mut len: usize = 0;
        // Safety: ptr and len are valid stack variables.
        unsafe { kv_get_result(&mut ptr, &mut len) };
        let got = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        assert_eq!(got, b"new");
    }

    // ── Thread safety of the get buffer ─────────────────────────────────
    //
    // The thread-local buffer means two threads each see their own buffer.

    #[test]
    #[serial]
    fn get_buffer_is_thread_local() {
        let store = init_store();
        store.set("bridge_tl_t1".into(), b"thread1".to_vec(), None);
        store.set("bridge_tl_t2".into(), b"thread2".to_vec(), None);

        let t1 = thread::spawn(|| {
            let key = cstr("bridge_tl_t1");
            // Safety: key is valid for this thread.
            unsafe { kv_get(key.as_ptr()) };
            let mut ptr: *const std::os::raw::c_char = std::ptr::null();
            let mut len: usize = 0;
            unsafe { kv_get_result(&mut ptr, &mut len) };
            unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec()
        });

        let t2 = thread::spawn(|| {
            let key = cstr("bridge_tl_t2");
            // Safety: key is valid for this thread.
            unsafe { kv_get(key.as_ptr()) };
            let mut ptr: *const std::os::raw::c_char = std::ptr::null();
            let mut len: usize = 0;
            unsafe { kv_get_result(&mut ptr, &mut len) };
            unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec()
        });

        assert_eq!(t1.join().unwrap(), b"thread1");
        assert_eq!(t2.join().unwrap(), b"thread2");
    }

    // ── #269: PHP userland running during thread teardown ───────────────

    thread_local! {
        /// Stands in for `ThreadPhpGuard`: touched before [`KV_GET_BUF`] and
        /// [`KV_SITE_STORE`], so its destructor runs last and both are already
        /// destroyed when [`KvShutdownHook::drop`] calls back into the bridge.
        static KV_SHUTDOWN_HOOK: RefCell<Option<KvShutdownHook>> = const { RefCell::new(None) };
    }

    static KV_HOOK_RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static KV_HOOK_GET: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    static KV_HOOK_SET: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    static KV_HOOK_RESULT_LEN: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(usize::MAX);

    /// A `register_shutdown_function()` callback calling `ephpm_kv_*` from
    /// inside a thread-local destructor — the issue #269 seam.
    struct KvShutdownHook;

    impl Drop for KvShutdownHook {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;

            let key = cstr("bridge_hook_teardown");
            let val = b"written-by-a-dying-thread";
            // Safety: `key` and `val` are valid for the duration of the calls.
            unsafe {
                KV_HOOK_GET.store(kv_get(key.as_ptr()), Ordering::Release);
                KV_HOOK_SET.store(
                    kv_set(key.as_ptr(), val.as_ptr().cast(), val.len(), 0),
                    Ordering::Release,
                );
                let mut ptr: *const std::os::raw::c_char = std::ptr::null();
                let mut len: usize = usize::MAX;
                kv_get_result(&mut ptr, &mut len);
                KV_HOOK_RESULT_LEN.store(len, Ordering::Release);
            }
            KV_HOOK_RAN.store(true, Ordering::Release);
        }
    }

    /// A shutdown function touching the KV store while its thread retires gets
    /// a clean miss, not a process abort (issue #269).
    ///
    /// `effective_store()` fails **closed** on a destroyed site slot rather
    /// than falling back to the process-global store: in multi-tenant mode
    /// that fallback would let a tenant's shutdown code write into a keyspace
    /// that is not its own. So the write below must not land anywhere.
    ///
    /// Without `try_with` this does not fail — the test binary dies with
    /// `fatal runtime error: thread local panicked on drop`.
    #[test]
    #[serial]
    fn shutdown_hook_during_thread_teardown_misses_cleanly() {
        use std::sync::atomic::Ordering;

        let store = init_store();
        store.set("bridge_hook_teardown".into(), b"seed".to_vec(), None);

        thread::spawn(|| {
            // 1. The guard-equivalent, before the bridge cells exist.
            KV_SHUTDOWN_HOOK.with(|c| *c.borrow_mut() = Some(KvShutdownHook));
            // 2. Then the bridge cells — registered later, destroyed earlier.
            set_site_store(None);
            let key = cstr("bridge_hook_teardown");
            // Safety: `key` is valid for the duration of the call.
            assert_eq!(unsafe { kv_get(key.as_ptr()) }, 1);
        })
        .join()
        .expect("worker thread must exit cleanly, not abort");

        assert!(KV_HOOK_RAN.load(Ordering::Acquire), "the shutdown hook must have run");
        assert_eq!(
            KV_HOOK_GET.load(Ordering::Acquire),
            0,
            "a get from a retiring thread must report a miss — there is no buffer to hand \
             the value back through"
        );
        assert_eq!(KV_HOOK_SET.load(Ordering::Acquire), 0, "a set must fail closed");
        assert_eq!(
            KV_HOOK_RESULT_LEN.load(Ordering::Acquire),
            0,
            "get_result must report an empty buffer, not a stale pointer"
        );
        assert_eq!(
            store.get("bridge_hook_teardown").as_deref(),
            Some(&b"seed"[..]),
            "the refused write must not have reached the store"
        );
    }
}

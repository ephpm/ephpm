//! Bridge between the C KV PHP functions and the Rust [`ephpm_kv::store::Store`].
//!
//! Provides a C-compatible function pointer table ([`EphpmKvOps`]) that the
//! PHP `ephpm_kv_*` native functions call into. Each callback delegates to the
//! global [`Store`] instance set via [`set_store`].
//!
//! The `get` result is stored in a thread-local buffer to avoid malloc/free
//! across the FFI boundary. The C side copies the data via `RETURN_STRINGL`.

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
    KV_SITE_STORE.with(|s| {
        *s.borrow_mut() = store;
    });
}

/// Stub when PHP is not linked.
#[cfg(not(php_linked))]
pub fn set_site_store(_store: Option<std::sync::Arc<ephpm_kv::store::Store>>) {}

/// Get the effective store for the current request.
/// Returns the site-specific store if set, otherwise the global store.
#[cfg(php_linked)]
fn effective_store() -> Option<Arc<Store>> {
    KV_SITE_STORE.with(|s| {
        let site = s.borrow();
        if let Some(ref store) = *site { Some(Arc::clone(store)) } else { KV_STORE.get().cloned() }
    })
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
            KV_GET_BUF.with(|buf| {
                let mut buf = buf.borrow_mut();
                buf.clear();
                buf.extend_from_slice(&val);
            });
            1
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
    KV_GET_BUF.with(|buf| {
        let buf = buf.borrow();
        unsafe {
            *ptr = buf.as_ptr().cast();
            *len = buf.len();
        }
    });
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
        assert_eq!(store.get("bridge_set_k"), Some(b"value".to_vec()));
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
        assert_eq!(store.get("bridge_set_bin"), Some(val.to_vec()));
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
        assert_eq!(store.get("bridge_incr_new"), Some(b"1".to_vec()));
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
}

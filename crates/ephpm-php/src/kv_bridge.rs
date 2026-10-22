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
use std::sync::{Arc, OnceLock};
#[cfg(php_linked)]
use std::time::Duration;

#[cfg(php_linked)]
use ephpm_kv::store::Store;

// ── Global store handle ─────────────────────────────────────────────────

#[cfg(php_linked)]
static KV_STORE: OnceLock<Arc<Store>> = OnceLock::new();

// ── Thread-local state ──────────────────────────────────────────────────

/// Which KV keyspace `ephpm_kv_*` reaches for the current request.
pub enum KvSiteScope {
    /// The process-global store: single-site mode, where there is one store
    /// and nothing to isolate.
    Global,
    /// This vhost's own store, resolved from its canonical site key.
    Site(std::sync::Arc<ephpm_kv::store::Store>),
    /// A multi-tenant node serving a `Host` that matched no vhost. There is no
    /// tenant to scope to, so there is **no** store: every `ephpm_kv_*` call
    /// fails with its documented failure value and a warning (once per
    /// request). Neither a per-`Host` store (a client-controlled, never-freed
    /// allocation per header value) nor the global store (operator state,
    /// shared by every unmatched request) is acceptable here — the same
    /// fail-closed rule `ephpm_db_*` applies ("no per-site database context").
    Unmatched,
}

/// Per-thread state behind [`KvSiteScope`]. `Unmatched` carries whether this
/// request has already been warned, so a script calling `ephpm_kv_get()` in a
/// loop logs one line, not thousands. It resets on every [`set_site_scope`],
/// i.e. once per request.
#[cfg(php_linked)]
enum ThreadScope {
    Global,
    Site(Arc<Store>),
    Unmatched { warned: bool },
}

#[cfg(php_linked)]
thread_local! {
    static KV_GET_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    /// Per-request KV scope for vhost isolation. Starts (and, for a
    /// single-site request, stays) on the global store.
    static KV_SITE_STORE: RefCell<ThreadScope> = const { RefCell::new(ThreadScope::Global) };
}

/// Set the KV scope for the current thread/request.
///
/// Called by the request handler before PHP execution, for **every** request,
/// so one request's scope can never leak into the next on a reused thread.
#[cfg(php_linked)]
pub fn set_site_scope(scope: KvSiteScope) {
    let scope = match scope {
        KvSiteScope::Global => ThreadScope::Global,
        KvSiteScope::Site(store) => ThreadScope::Site(store),
        KvSiteScope::Unmatched => ThreadScope::Unmatched { warned: false },
    };
    // Plain `with` on purpose (issue #269): Rust-side per-request setup on a
    // live thread, never reachable from a PHP shutdown function. The *read*
    // side ([`effective_store`]) is reachable and does use `try_with`.
    KV_SITE_STORE.with(|s| {
        *s.borrow_mut() = scope;
    });
}

/// Stub when PHP is not linked.
#[cfg(not(php_linked))]
pub fn set_site_scope(_scope: KvSiteScope) {}

/// Set the KV store for the current thread/request: `Some` scopes to that
/// site's store, `None` to the global store. Shorthand for
/// [`set_site_scope`] with [`KvSiteScope::Site`] / [`KvSiteScope::Global`].
pub fn set_site_store(store: Option<std::sync::Arc<ephpm_kv::store::Store>>) {
    set_site_scope(store.map_or(KvSiteScope::Global, KvSiteScope::Site));
}

/// Get the effective store for the current request: the site store, the
/// global store in single-site mode, or `None` for a request that matched no
/// vhost on a multi-tenant node.
///
/// Also returns `None` when the per-thread site slot has already been
/// destroyed (issue #269) — **fail closed**, not "fall back to the global
/// store". In multi-tenant mode that fallback would let a tenant's shutdown
/// function write into a keyspace that is not its own; a KV miss is the safe
/// answer.
#[cfg(php_linked)]
fn effective_store() -> Option<Arc<Store>> {
    KV_SITE_STORE
        .try_with(|s| match &*s.borrow() {
            ThreadScope::Site(store) => Some(Arc::clone(store)),
            ThreadScope::Global => KV_STORE.get().cloned(),
            ThreadScope::Unmatched { .. } => None,
        })
        .unwrap_or(None)
}

/// `site_context` op: whether this request has a KV keyspace at all.
///
/// `0` = it does (site or global store). `1` = it matched no vhost on a
/// multi-tenant node and has not been warned yet — the C side warns and fails
/// the call; this call marks the request warned. `2` = no keyspace, already
/// warned: fail the call silently. A destroyed slot (issue #269) reports `2`:
/// the call must fail, and a warning from a dying thread has nowhere useful to
/// go.
#[cfg(php_linked)]
unsafe extern "C" fn kv_site_context() -> std::os::raw::c_int {
    KV_SITE_STORE
        .try_with(|s| match &mut *s.borrow_mut() {
            ThreadScope::Global | ThreadScope::Site(_) => 0,
            ThreadScope::Unmatched { warned } if !*warned => {
                *warned = true;
                1
            }
            ThreadScope::Unmatched { .. } => 2,
        })
        .unwrap_or(2)
}

/// Borrow a key that PHP passed as `(pointer, length)`.
///
/// Keys cross the FFI boundary with their length, never as NUL-terminated
/// strings: `CStr::from_ptr` truncated `"nul\0A"` and `"nul\0B"` to the same
/// key `"nul"` (KV stress report F-18). The store keys entries by UTF-8
/// `String`, so a non-UTF-8 key is refused (`None`) rather than converted
/// lossily — the C side has already warned about it, this is the backstop.
///
/// # Safety
///
/// `key` must point to `key_len` readable bytes that stay valid for `'a`.
#[cfg(php_linked)]
unsafe fn key_arg<'a>(key: *const std::os::raw::c_char, key_len: usize) -> Option<&'a str> {
    if key.is_null() {
        return None;
    }
    // Safety: the caller guarantees `key_len` readable bytes at `key`.
    let bytes = unsafe { std::slice::from_raw_parts(key.cast::<u8>(), key_len) };
    std::str::from_utf8(bytes).ok()
}

/// `ttl_ms` from PHP as an optional TTL: `<= 0` is "no expiry" (`Ok(None)`),
/// and a value too large to represent is `Err(())` — the store would panic on
/// `Instant + Duration` overflow (KV stress report F-14). The C side already
/// bounds it; this is the backstop.
#[cfg(php_linked)]
fn optional_ttl(ttl_ms: std::os::raw::c_longlong) -> Result<Option<Duration>, ()> {
    if ttl_ms <= 0 {
        return Ok(None);
    }
    ephpm_kv::command::checked_ttl(ttl_ms, ephpm_kv::command::TtlUnit::Millis).map(Some).ok_or(())
}

// ── C-compatible ops struct ─────────────────────────────────────────────

/// Function pointer table passed to C so PHP native functions can call
/// into the Rust KV store without knowing about Rust types.
///
/// Every key is passed as `(key, key_len)` — PHP strings are binary and may
/// contain NUL — and must be UTF-8 (see [`key_arg`]). The layout mirrors
/// `ephpm_wrapper.c`'s `EphpmKvOps` field for field.
#[cfg(php_linked)]
#[repr(C)]
pub struct EphpmKvOps {
    /// Get a value by key. Returns 1 if found, 0 if not.
    /// The result is stored in a thread-local buffer and retrieved
    /// via `get_result`.
    pub get: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_int,
    >,

    /// Retrieve the pointer and length of the last `get` result.
    pub get_result:
        Option<unsafe extern "C" fn(ptr: *mut *const std::os::raw::c_char, len: *mut usize)>,

    /// Set a key to a value. `ttl_ms` of 0 means no expiry.
    /// Returns 1 on success, 0 on failure (e.g. OOM with noeviction).
    pub set: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            val: *const std::os::raw::c_char,
            val_len: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Atomically set a key only if it doesn't already exist (SETNX).
    /// Returns 1 if the value was inserted, 0 if a live entry was
    /// already present at this key (or the write was refused under
    /// `NoEviction`), and -1 if a clustered owner was unreachable (outcome
    /// unknown). The check-and-set is performed under the same per-key
    /// shard lock, so concurrent callers see exactly one winner.
    pub set_nx: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            val: *const std::os::raw::c_char,
            val_len: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Delete a key. Returns 1 if it existed, 0 if not.
    pub del: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_long,
    >,

    /// Check if a key exists. Returns 1 if yes, 0 if no.
    pub exists: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_int,
    >,

    /// Increment value by delta. Stores result in `*result`.
    /// Returns 1 on success, 0 on error (value not an integer).
    pub incr_by: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            delta: std::os::raw::c_longlong,
            result: *mut std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Set TTL on a key. `ttl_ms` in milliseconds. Returns 1 if key
    /// exists, 0 if not.
    pub expire: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Get remaining TTL in milliseconds. Returns -1 for no expiry,
    /// -2 for missing key.
    pub pttl: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_longlong,
    >,

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
    /// dedicated PHP execution pools, never async tasks.
    pub wait: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            last_version: std::os::raw::c_longlong,
            timeout_ms: std::os::raw::c_longlong,
            new_version: *mut std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,

    /// Atomically delete `key` only if its current value equals the
    /// `token`/`token_len` bytes (compare-and-delete). Returns 1 if the key
    /// was deleted by this call, 0 otherwise (value mismatch, absent, expired,
    /// or no store registered). The compare and delete happen under the same
    /// per-key shard lock, so a concurrent overwrite makes this a no-op rather
    /// than deleting a value it never observed — this is the safe-release
    /// primitive for KV locks (the delete-side companion to `set_nx`).
    pub del_if_eq: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            token: *const std::os::raw::c_char,
            token_len: usize,
        ) -> std::os::raw::c_int,
    >,

    /// Whether the current request has a KV keyspace: `0` yes, `1` no (a
    /// multi-tenant request whose `Host` matched no vhost) and this is the
    /// first call this request — warn, `2` no and already warned. The C side
    /// checks it before every `ephpm_kv_*` op and fails the call on non-zero.
    /// See `kv_site_context`.
    ///
    /// The layout mirrors `ephpm_wrapper.c`'s `EphpmKvOps`: fields are only
    /// ever appended, below this one.
    pub site_context: Option<unsafe extern "C" fn() -> std::os::raw::c_int>,

    // ── v0.13: list results, hashes, multi-key and key-space ops ───────
    //
    // Ops that return several values stage them in a thread-local list
    // buffer (see `stage_list`) and return how many items they staged, or
    // -1 on failure; the C side reads each item with `list_item` right after,
    // on the same thread. Arrays of keys/fields/values cross as parallel
    // `(pointer, length)` arrays of `n` entries.
    /// Item `index` of the last staged list: `1` with `*ptr`/`*len` set, `0`
    /// for a null item, `-1` past the end.
    pub list_item: Option<
        unsafe extern "C" fn(
            index: usize,
            ptr: *mut *const std::os::raw::c_char,
            len: *mut usize,
        ) -> std::os::raw::c_int,
    >,
    /// `HGET`: `1` found (value in the get buffer), `0` missing/failure.
    pub hget: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            field: *const std::os::raw::c_char,
            field_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// `HSET` of `n` field/value pairs: `1` with `*added` = fields created,
    /// `0` failure (refused for memory, clustered value too large, no store).
    pub hset: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            fields: *const *const std::os::raw::c_char,
            field_lens: *const usize,
            vals: *const *const std::os::raw::c_char,
            val_lens: *const usize,
            n: usize,
            added: *mut std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,
    /// `HSETNX`: `1` set, `0` existed or failure.
    pub hsetnx: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            field: *const std::os::raw::c_char,
            field_len: usize,
            val: *const std::os::raw::c_char,
            val_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// `HMGET`: stages `n` items (null for a missing field); `-1` failure.
    pub hmget: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            fields: *const *const std::os::raw::c_char,
            field_lens: *const usize,
            n: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `HGETALL`: stages field, value, field, value, ...; returns the item
    /// count (twice the field count); `-1` failure.
    pub hgetall: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `HKEYS` (`which` = 0) or `HVALS` (`which` = 1): stages the items;
    /// `-1` failure.
    pub hkeys_vals: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            which: std::os::raw::c_int,
        ) -> std::os::raw::c_longlong,
    >,
    /// `HDEL` of `n` fields: the number removed (`0` on failure).
    pub hdel: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            fields: *const *const std::os::raw::c_char,
            field_lens: *const usize,
            n: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `HEXISTS`: `1` / `0`.
    pub hexists: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            field: *const std::os::raw::c_char,
            field_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// `HLEN`: live fields (`0` for missing / failure).
    pub hlen: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `HINCRBY`: `1` with `*result` set, `0` failure (non-integer field,
    /// overflow, refused write).
    pub hincr_by: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            field: *const std::os::raw::c_char,
            field_len: usize,
            delta: std::os::raw::c_longlong,
            result: *mut std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,
    /// `HEXPIRE` of `n` fields to `ttl_ms` (> 0): the number of fields given
    /// the TTL; `-1` for an invalid TTL or failure.
    pub hexpire: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            fields: *const *const std::os::raw::c_char,
            field_lens: *const usize,
            n: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_longlong,
    >,
    /// `HPTTL` of one field: milliseconds, `-1` no TTL, `-2` no field.
    pub hpttl: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            field: *const std::os::raw::c_char,
            field_len: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `HPERSIST` of one field: `1` a TTL was removed, `0` otherwise.
    pub hpersist: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            field: *const std::os::raw::c_char,
            field_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// `MGET` of `n` keys: stages `n` items (null for a missing key); `-1`
    /// failure.
    pub mget: Option<
        unsafe extern "C" fn(
            keys: *const *const std::os::raw::c_char,
            key_lens: *const usize,
            n: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `MSET` of `n` pairs with one TTL (`0` = none): `1` all stored, `0`
    /// failure (a refusal stops the write; earlier pairs stay).
    pub mset: Option<
        unsafe extern "C" fn(
            keys: *const *const std::os::raw::c_char,
            key_lens: *const usize,
            vals: *const *const std::os::raw::c_char,
            val_lens: *const usize,
            n: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,
    /// `GETDEL`: `1` found (value in the get buffer), `0` missing/failure.
    pub getdel: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// `GETEX`: `ttl_ms` > 0 sets the TTL, `0` removes it. `1` found (value
    /// in the get buffer), `0` missing/failure.
    pub getex: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,
    /// `PERSIST` (strings and hashes): `1` a TTL was removed.
    pub persist: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// Compare-and-set (`SET ... IFEQ`). `ttl_ms` > 0 new TTL, `0` none,
    /// `-1` keep. `1` set, `0` not set (mismatch, absent, refused), `-1`
    /// clustered owner unreachable.
    pub set_if_eq: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            expected: *const std::os::raw::c_char,
            expected_len: usize,
            val: *const std::os::raw::c_char,
            val_len: usize,
            ttl_ms: std::os::raw::c_longlong,
        ) -> std::os::raw::c_int,
    >,
    /// `APPEND`: the new length, `-1` failure.
    pub append: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
            val: *const std::os::raw::c_char,
            val_len: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `STRLEN`: `0` for missing / failure.
    pub str_len: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_longlong,
    >,
    /// `TYPE`: `0` none, `1` string, `2` hash.
    pub key_type: Option<
        unsafe extern "C" fn(
            key: *const std::os::raw::c_char,
            key_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// One `SCAN` step from `cursor`; `pattern` may be NULL. Stages the keys
    /// and returns their count, writing the next cursor (`0` = done) to
    /// `*next`; `-1` failure.
    pub scan: Option<
        unsafe extern "C" fn(
            cursor: std::os::raw::c_ulonglong,
            pattern: *const std::os::raw::c_char,
            pattern_len: usize,
            count: std::os::raw::c_longlong,
            next: *mut std::os::raw::c_ulonglong,
        ) -> std::os::raw::c_longlong,
    >,
}

// ── Callback implementations ────────────────────────────────────────────

#[cfg(php_linked)]
unsafe extern "C" fn kv_get(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    // Through the cluster: a large value held by other nodes only is fetched
    // from them (bounded) rather than reported missing.
    match store.get_through(key_str) {
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
    key_len: usize,
    val: *const std::os::raw::c_char,
    val_len: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };
    // Safety: `val` points to `val_len` bytes of valid memory from PHP.
    let val_bytes = unsafe { std::slice::from_raw_parts(val.cast::<u8>(), val_len) };

    let Ok(ttl) = optional_ttl(ttl_ms) else {
        return 0;
    };

    i32::from(store.set(key_str.to_string(), val_bytes.to_vec(), ttl))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_set_nx(
    key: *const std::os::raw::c_char,
    key_len: usize,
    val: *const std::os::raw::c_char,
    val_len: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };
    // Safety: `val` points to `val_len` bytes of valid memory from PHP.
    let val_bytes = unsafe { std::slice::from_raw_parts(val.cast::<u8>(), val_len) };

    let Ok(ttl) = optional_ttl(ttl_ms) else {
        return 0;
    };

    set_nx_rc(store.set_nx_outcome(key_str.to_string(), val_bytes.to_vec(), ttl))
}

/// C return code for `set_nx`: `1` inserted, `0` not inserted (key exists,
/// or refused for memory), `-1` the clustered owner could not be reached —
/// the outcome is **unknown** (a timed-out request may still have been
/// applied), so the session lock path treats it differently from contention.
/// `ephpm_kv_setnx()` maps only `1` to `true`.
#[cfg_attr(not(php_linked), allow(dead_code))]
fn set_nx_rc(outcome: ephpm_kv::store::SetNxOutcome) -> std::os::raw::c_int {
    use ephpm_kv::store::SetNxOutcome;
    match outcome {
        SetNxOutcome::Inserted => 1,
        SetNxOutcome::OwnerUnreachable => -1,
        SetNxOutcome::Exists | SetNxOutcome::OutOfMemory => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_del(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_long {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    std::os::raw::c_long::from(store.remove(&key_str))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_del_if_eq(
    key: *const std::os::raw::c_char,
    key_len: usize,
    token: *const std::os::raw::c_char,
    token_len: usize,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };
    // Safety: `token` points to `token_len` bytes of valid memory from PHP.
    let token_bytes = unsafe { std::slice::from_raw_parts(token.cast::<u8>(), token_len) };

    i32::from(store.del_if_eq(key_str, token_bytes))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_exists(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
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
    key_len: usize,
    delta: std::os::raw::c_longlong,
    result: *mut std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
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
    key_len: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
        return 0;
    };
    let Some(store) = effective_store() else {
        return 0;
    };

    // Non-positive or out-of-range: refuse (the C side warns about the
    // latter). `checked_ttl` keeps `Instant + ttl` from overflowing.
    let Some(ttl) = ephpm_kv::command::checked_ttl(ttl_ms, ephpm_kv::command::TtlUnit::Millis)
    else {
        return 0;
    };
    i32::from(store.expire(&key_str, ttl))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_pttl(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_longlong {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
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
    key_len: usize,
    last_version: std::os::raw::c_longlong,
    timeout_ms: std::os::raw::c_longlong,
    new_version: *mut std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    // Safety: `key` points to `key_len` bytes of PHP's (binary-safe) string
    // parameter, valid for the duration of this call.
    let Some(key_str) = (unsafe { key_arg(key, key_len) }) else {
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

// ── v0.13 ops: list buffer, hashes, multi-key, key space ───────────────

/// A staged list result: every item's bytes back to back in `data`, and per
/// item its `(offset, len)` — `None` for a null item.
#[cfg(php_linked)]
#[derive(Default)]
struct ListBuf {
    data: Vec<u8>,
    items: Vec<Option<(usize, usize)>>,
}

#[cfg(php_linked)]
thread_local! {
    /// The last list result of this thread (see [`EphpmKvOps::list_item`]).
    static KV_LIST_BUF: RefCell<ListBuf> = RefCell::new(ListBuf::default());
}

/// Stage `items` as this thread's list result and return how many there
/// are, or `-1` when the buffer is gone (thread teardown, issue #269) — the
/// caller must then report failure rather than a list C cannot read.
#[cfg(php_linked)]
fn stage_list<I, B>(items: I) -> std::os::raw::c_longlong
where
    I: IntoIterator<Item = Option<B>>,
    B: AsRef<[u8]>,
{
    KV_LIST_BUF
        .try_with(|buf| {
            let mut buf = buf.borrow_mut();
            buf.data.clear();
            buf.items.clear();
            for item in items {
                let staged = item.map(|bytes| {
                    let bytes = bytes.as_ref();
                    let at = buf.data.len();
                    buf.data.extend_from_slice(bytes);
                    (at, bytes.len())
                });
                buf.items.push(staged);
            }
            std::os::raw::c_longlong::try_from(buf.items.len()).unwrap_or(-1)
        })
        .unwrap_or(-1)
}

/// Stage one value in the get buffer (the `get_result` contract). `false`
/// when the buffer is gone.
#[cfg(php_linked)]
fn stage_value(value: &[u8]) -> bool {
    KV_GET_BUF
        .try_with(|buf| {
            let mut buf = buf.borrow_mut();
            buf.clear();
            buf.extend_from_slice(value);
        })
        .is_ok()
}

/// Borrow `n` UTF-8 strings passed as parallel `(pointer, length)` arrays;
/// `None` if any is not UTF-8 or the arrays are NULL with `n > 0`.
///
/// # Safety
///
/// `ptrs` and `lens` must each point to `n` readable entries, and every
/// `ptrs[i]` to `lens[i]` readable bytes, all valid for `'a`.
#[cfg(php_linked)]
unsafe fn str_list<'a>(
    ptrs: *const *const std::os::raw::c_char,
    lens: *const usize,
    n: usize,
) -> Option<Vec<&'a str>> {
    if n == 0 {
        return Some(Vec::new());
    }
    if ptrs.is_null() || lens.is_null() {
        return None;
    }
    // Safety: the caller guarantees `n` entries in each array.
    let (ptrs, lens) =
        unsafe { (std::slice::from_raw_parts(ptrs, n), std::slice::from_raw_parts(lens, n)) };
    ptrs.iter()
        .zip(lens)
        // Safety: each pointer covers its length, per the caller.
        .map(|(p, l)| unsafe { key_arg(*p, *l) })
        .collect()
}

/// Borrow `n` byte strings passed as parallel `(pointer, length)` arrays.
///
/// # Safety
///
/// As [`str_list`].
#[cfg(php_linked)]
unsafe fn bytes_list<'a>(
    ptrs: *const *const std::os::raw::c_char,
    lens: *const usize,
    n: usize,
) -> Option<Vec<&'a [u8]>> {
    if n == 0 {
        return Some(Vec::new());
    }
    if ptrs.is_null() || lens.is_null() {
        return None;
    }
    // Safety: the caller guarantees `n` entries in each array.
    let (ptrs, lens) =
        unsafe { (std::slice::from_raw_parts(ptrs, n), std::slice::from_raw_parts(lens, n)) };
    ptrs.iter()
        .zip(lens)
        .map(|(p, l)| {
            if p.is_null() && *l > 0 {
                return None;
            }
            if *l == 0 {
                return Some(&[][..]);
            }
            // Safety: `p` covers `l` readable bytes, per the caller.
            Some(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), *l) })
        })
        .collect()
}

/// Borrow one value passed as `(pointer, length)`.
///
/// # Safety
///
/// `ptr` must point to `len` readable bytes valid for `'a` (or be NULL with
/// `len == 0`).
#[cfg(php_linked)]
unsafe fn bytes_arg<'a>(ptr: *const std::os::raw::c_char, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        return &[];
    }
    // Safety: per the caller.
    unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }
}

/// Resolve the store and a key, or return `$fail` from the enclosing op.
#[cfg(php_linked)]
macro_rules! store_and_key {
    ($key:expr, $len:expr, $fail:expr) => {{
        // Safety: every op's `key` points to `key_len` bytes of a PHP string
        // parameter, valid for the duration of the call.
        let Some(key) = (unsafe { key_arg($key, $len) }) else {
            return $fail;
        };
        let Some(store) = effective_store() else {
            return $fail;
        };
        (store, key)
    }};
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_list_item(
    index: usize,
    ptr: *mut *const std::os::raw::c_char,
    len: *mut usize,
) -> std::os::raw::c_int {
    KV_LIST_BUF
        .try_with(|buf| {
            let buf = buf.borrow();
            match buf.items.get(index) {
                Some(Some((at, n))) => {
                    // Safety: `ptr`/`len` are valid out-pointers from our C
                    // code; the buffer is not touched until the next staging
                    // call on this thread, which C makes only after it has
                    // copied every item.
                    unsafe {
                        *ptr = buf.data.as_ptr().add(*at).cast();
                        *len = *n;
                    }
                    1
                }
                Some(None) => 0,
                None => -1,
            }
        })
        .unwrap_or(-1)
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hget(
    key: *const std::os::raw::c_char,
    key_len: usize,
    field: *const std::os::raw::c_char,
    field_len: usize,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    // Safety: `field` points to `field_len` bytes of a PHP string parameter.
    let Some(field) = (unsafe { key_arg(field, field_len) }) else {
        return 0;
    };
    match store.hget(key, field) {
        Some(v) => i32::from(stage_value(&v)),
        None => 0,
    }
}

#[cfg(php_linked)]
#[allow(clippy::too_many_arguments, reason = "a C ABI: parallel arrays plus an out-pointer")]
unsafe extern "C" fn kv_hset(
    key: *const std::os::raw::c_char,
    key_len: usize,
    fields: *const *const std::os::raw::c_char,
    field_lens: *const usize,
    vals: *const *const std::os::raw::c_char,
    val_lens: *const usize,
    n: usize,
    added: *mut std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    // Safety: the C side passes `n` entries in each array, each covering
    // its length, valid for this call.
    let (Some(fields), Some(vals)) =
        (unsafe { str_list(fields, field_lens, n) }, unsafe { bytes_list(vals, val_lens, n) })
    else {
        return 0;
    };
    let pairs: Vec<(&str, &[u8])> = fields.into_iter().zip(vals).collect();
    match store.hset_many(key, &pairs) {
        Ok(count) => {
            // Safety: `added` points to a `long long` local in our C code.
            unsafe { *added = std::os::raw::c_longlong::try_from(count).unwrap_or(i64::MAX) };
            1
        }
        Err(_) => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hsetnx(
    key: *const std::os::raw::c_char,
    key_len: usize,
    field: *const std::os::raw::c_char,
    field_len: usize,
    val: *const std::os::raw::c_char,
    val_len: usize,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    // Safety: `field`/`val` point to their lengths of PHP string data.
    let Some(field) = (unsafe { key_arg(field, field_len) }) else {
        return 0;
    };
    let val = unsafe { bytes_arg(val, val_len) };
    i32::from(store.hsetnx(key, field, val) == Ok(true))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hmget(
    key: *const std::os::raw::c_char,
    key_len: usize,
    fields: *const *const std::os::raw::c_char,
    field_lens: *const usize,
    n: usize,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, -1);
    // Safety: `n` entries per array, each covering its length.
    let Some(fields) = (unsafe { str_list(fields, field_lens, n) }) else {
        return -1;
    };
    stage_list(store.hmget(key, &fields))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hgetall(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, -1);
    let pairs = store.hgetall(key);
    stage_list(pairs.iter().flat_map(|(f, v)| [Some(f.as_bytes()), Some(v.as_ref())]))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hkeys_vals(
    key: *const std::os::raw::c_char,
    key_len: usize,
    which: std::os::raw::c_int,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, -1);
    if which == 0 {
        stage_list(store.hkeys(key).iter().map(|k| Some(k.as_bytes())))
    } else {
        stage_list(store.hvals(key).iter().map(|v| Some(v.as_ref())))
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hdel(
    key: *const std::os::raw::c_char,
    key_len: usize,
    fields: *const *const std::os::raw::c_char,
    field_lens: *const usize,
    n: usize,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, 0);
    // Safety: `n` entries per array, each covering its length.
    let Some(fields) = (unsafe { str_list(fields, field_lens, n) }) else {
        return 0;
    };
    std::os::raw::c_longlong::try_from(store.hdel_many(key, &fields)).unwrap_or(i64::MAX)
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hexists(
    key: *const std::os::raw::c_char,
    key_len: usize,
    field: *const std::os::raw::c_char,
    field_len: usize,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    // Safety: `field` points to `field_len` bytes of PHP string data.
    let Some(field) = (unsafe { key_arg(field, field_len) }) else {
        return 0;
    };
    i32::from(store.hexists(key, field))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hlen(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, 0);
    std::os::raw::c_longlong::try_from(store.hlen(key)).unwrap_or(i64::MAX)
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hincr_by(
    key: *const std::os::raw::c_char,
    key_len: usize,
    field: *const std::os::raw::c_char,
    field_len: usize,
    delta: std::os::raw::c_longlong,
    result: *mut std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    // Safety: `field` points to `field_len` bytes of PHP string data.
    let Some(field) = (unsafe { key_arg(field, field_len) }) else {
        return 0;
    };
    match store.hincr_by(key, field, delta) {
        Ok(v) => {
            // Safety: `result` points to a `long long` local in our C code.
            unsafe { *result = v };
            1
        }
        Err(_) => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hexpire(
    key: *const std::os::raw::c_char,
    key_len: usize,
    fields: *const *const std::os::raw::c_char,
    field_lens: *const usize,
    n: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, -1);
    // Positive and representable only; `checked_ttl` keeps `Instant + ttl`
    // from overflowing.
    let Some(ttl) = ephpm_kv::command::checked_ttl(ttl_ms, ephpm_kv::command::TtlUnit::Millis)
    else {
        return -1;
    };
    // Safety: `n` entries per array, each covering its length.
    let Some(fields) = (unsafe { str_list(fields, field_lens, n) }) else {
        return -1;
    };
    let codes = store.hexpire(key, &fields, ttl, ephpm_kv::store::ExpireCondition::Always);
    let set = codes.iter().filter(|c| **c == ephpm_kv::store::field_code::UPDATED).count();
    std::os::raw::c_longlong::try_from(set).unwrap_or(i64::MAX)
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hpttl(
    key: *const std::os::raw::c_char,
    key_len: usize,
    field: *const std::os::raw::c_char,
    field_len: usize,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, -2);
    // Safety: `field` points to `field_len` bytes of PHP string data.
    let Some(field) = (unsafe { key_arg(field, field_len) }) else {
        return -2;
    };
    store.hpttl(key, &[field]).first().copied().unwrap_or(-2)
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_hpersist(
    key: *const std::os::raw::c_char,
    key_len: usize,
    field: *const std::os::raw::c_char,
    field_len: usize,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    // Safety: `field` points to `field_len` bytes of PHP string data.
    let Some(field) = (unsafe { key_arg(field, field_len) }) else {
        return 0;
    };
    let codes = store.hpersist(key, &[field]);
    i32::from(codes.first() == Some(&ephpm_kv::store::field_code::UPDATED))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_mget(
    keys: *const *const std::os::raw::c_char,
    key_lens: *const usize,
    n: usize,
) -> std::os::raw::c_longlong {
    let Some(store) = effective_store() else {
        return -1;
    };
    // Safety: `n` entries per array, each covering its length.
    let Some(keys) = (unsafe { str_list(keys, key_lens, n) }) else {
        return -1;
    };
    // Through the cluster, like `get`: a large value held by other nodes is
    // fetched rather than reported missing.
    stage_list(keys.iter().map(|k| store.get_through(k)))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_mset(
    keys: *const *const std::os::raw::c_char,
    key_lens: *const usize,
    vals: *const *const std::os::raw::c_char,
    val_lens: *const usize,
    n: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    let Some(store) = effective_store() else {
        return 0;
    };
    let Ok(ttl) = optional_ttl(ttl_ms) else {
        return 0;
    };
    // Safety: `n` entries per array, each covering its length. Every key is
    // checked before anything is written.
    let (Some(keys), Some(vals)) =
        (unsafe { str_list(keys, key_lens, n) }, unsafe { bytes_list(vals, val_lens, n) })
    else {
        return 0;
    };
    for (key, val) in keys.into_iter().zip(vals) {
        if !store.set(key.to_string(), val.to_vec(), ttl) {
            return 0;
        }
    }
    1
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_getdel(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    match store.getdel(key) {
        Some(v) => i32::from(stage_value(&v)),
        None => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_getex(
    key: *const std::os::raw::c_char,
    key_len: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    let expiry = match optional_ttl(ttl_ms) {
        Ok(Some(ttl)) => ephpm_kv::store::GetExpiry::After(ttl),
        Ok(None) if ttl_ms == 0 => ephpm_kv::store::GetExpiry::Persist,
        _ => return 0,
    };
    match store.getex(key, expiry) {
        Some(v) => i32::from(stage_value(&v)),
        None => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_persist(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    i32::from(store.persist(key))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_set_if_eq(
    key: *const std::os::raw::c_char,
    key_len: usize,
    expected: *const std::os::raw::c_char,
    expected_len: usize,
    val: *const std::os::raw::c_char,
    val_len: usize,
    ttl_ms: std::os::raw::c_longlong,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    let ttl = match ttl_ms {
        -1 => ephpm_kv::store::SetTtl::Keep,
        0 => ephpm_kv::store::SetTtl::Persist,
        ms => match ephpm_kv::command::checked_ttl(ms, ephpm_kv::command::TtlUnit::Millis) {
            Some(d) => ephpm_kv::store::SetTtl::After(d),
            None => return 0,
        },
    };
    // Safety: both point to their lengths of PHP string data.
    let (expected, val) = unsafe { (bytes_arg(expected, expected_len), bytes_arg(val, val_len)) };
    set_if_eq_rc(store.set_if_eq(key.to_string(), expected, val.to_vec(), ttl))
}

/// C return code for `set_if_eq`: `1` set, `0` not set (mismatch, absent,
/// refused for memory), `-1` clustered owner unreachable (outcome unknown).
#[cfg_attr(not(php_linked), allow(dead_code))]
fn set_if_eq_rc(outcome: ephpm_kv::store::SetIfEqOutcome) -> std::os::raw::c_int {
    use ephpm_kv::store::SetIfEqOutcome;
    match outcome {
        SetIfEqOutcome::Set => 1,
        SetIfEqOutcome::OwnerUnreachable => -1,
        SetIfEqOutcome::NotSet | SetIfEqOutcome::OutOfMemory => 0,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_append(
    key: *const std::os::raw::c_char,
    key_len: usize,
    val: *const std::os::raw::c_char,
    val_len: usize,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, -1);
    // Safety: `val` points to `val_len` bytes of PHP string data.
    let val = unsafe { bytes_arg(val, val_len) };
    store.append(key, val).map_or(-1, |n| std::os::raw::c_longlong::try_from(n).unwrap_or(-1))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_strlen(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_longlong {
    let (store, key) = store_and_key!(key, key_len, 0);
    store.get_through(key).map_or(0, |v| std::os::raw::c_longlong::try_from(v.len()).unwrap_or(0))
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_key_type(
    key: *const std::os::raw::c_char,
    key_len: usize,
) -> std::os::raw::c_int {
    let (store, key) = store_and_key!(key, key_len, 0);
    if store.is_hash(key) { 2 } else { i32::from(store.exists(key)) }
}

#[cfg(php_linked)]
unsafe extern "C" fn kv_scan(
    cursor: std::os::raw::c_ulonglong,
    pattern: *const std::os::raw::c_char,
    pattern_len: usize,
    count: std::os::raw::c_longlong,
    next: *mut std::os::raw::c_ulonglong,
) -> std::os::raw::c_longlong {
    let Some(store) = effective_store() else {
        return -1;
    };
    let pattern = if pattern.is_null() {
        None
    } else {
        // Safety: `pattern` points to `pattern_len` bytes of PHP string data.
        match unsafe { key_arg(pattern, pattern_len) } {
            Some(p) => Some(p),
            None => return -1,
        }
    };
    let count = usize::try_from(count.max(1)).unwrap_or(1);
    let page = store.scan(cursor, pattern, count, None);
    let staged = stage_list(page.keys.iter().map(|k| Some(k.as_bytes())));
    if staged >= 0 {
        // Safety: `next` points to an `unsigned long long` local in our C
        // code.
        unsafe { *next = page.cursor };
    }
    staged
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
    del_if_eq: Some(kv_del_if_eq),
    site_context: Some(kv_site_context),
    list_item: Some(kv_list_item),
    hget: Some(kv_hget),
    hset: Some(kv_hset),
    hsetnx: Some(kv_hsetnx),
    hmget: Some(kv_hmget),
    hgetall: Some(kv_hgetall),
    hkeys_vals: Some(kv_hkeys_vals),
    hdel: Some(kv_hdel),
    hexists: Some(kv_hexists),
    hlen: Some(kv_hlen),
    hincr_by: Some(kv_hincr_by),
    hexpire: Some(kv_hexpire),
    hpttl: Some(kv_hpttl),
    hpersist: Some(kv_hpersist),
    mget: Some(kv_mget),
    mset: Some(kv_mset),
    getdel: Some(kv_getdel),
    getex: Some(kv_getex),
    persist: Some(kv_persist),
    set_if_eq: Some(kv_set_if_eq),
    append: Some(kv_append),
    str_len: Some(kv_strlen),
    key_type: Some(kv_key_type),
    scan: Some(kv_scan),
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
        let found = unsafe { kv_get(key.as_ptr(), key.as_bytes().len()) };
        assert_eq!(found, 0);
    }

    #[test]
    #[serial]
    fn set_and_get_round_trip() {
        let store = init_store();
        store.set("bridge_rtrip".into(), b"hello".to_vec(), None);

        let key = cstr("bridge_rtrip");
        // Safety: key is a valid C string.
        let found = unsafe { kv_get(key.as_ptr(), key.as_bytes().len()) };
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
        unsafe { kv_get(cstr("bridge_tl_a").as_ptr(), "bridge_tl_a".len()) };
        unsafe { kv_get(cstr("bridge_tl_b").as_ptr(), "bridge_tl_b".len()) };

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
        let ok = unsafe {
            kv_set(key.as_ptr(), key.as_bytes().len(), val.as_ptr().cast(), val.len(), 0)
        };
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
        unsafe {
            kv_set(key.as_ptr(), key.as_bytes().len(), val.as_ptr().cast(), val.len(), 60_000)
        };
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
        unsafe { kv_set(key.as_ptr(), key.as_bytes().len(), val.as_ptr().cast(), val.len(), 0) };
        assert_eq!(store.pttl("bridge_set_nottl"), Some(-1));
    }

    #[test]
    #[serial]
    fn set_handles_binary_value() {
        let store = init_store();
        let key = cstr("bridge_set_bin");
        let val: &[u8] = &[0x00, 0x01, 0xFF, 0xFE];
        // Safety: key is a valid C string; val is valid for val.len() bytes.
        unsafe { kv_set(key.as_ptr(), key.as_bytes().len(), val.as_ptr().cast(), val.len(), 0) };
        assert_eq!(store.get("bridge_set_bin").as_deref(), Some(&val[..]));
    }

    // ── Binary keys and TTL bounds (KV stress report F-14/F-18) ─────────

    /// `"nul\0A"` and `"nul\0B"` used to be read with `CStr::from_ptr`, which
    /// stopped at the NUL: both wrote the key `"nul"`, so the second write
    /// replaced the first. With `(ptr, len)` keys they are distinct entries.
    #[test]
    #[serial]
    fn keys_containing_nul_are_distinct() {
        let store = init_store();
        let (a, b): (&[u8], &[u8]) = (b"bridge_nul\0A", b"bridge_nul\0B");
        // Safety: keys and values are valid for their lengths.
        unsafe {
            assert_eq!(kv_set(a.as_ptr().cast(), a.len(), b"va".as_ptr().cast(), 2, 0), 1);
            assert_eq!(kv_set(b.as_ptr().cast(), b.len(), b"vb".as_ptr().cast(), 2, 0), 1);
        }
        assert_eq!(store.get("bridge_nul\0A").as_deref(), Some(&b"va"[..]));
        assert_eq!(store.get("bridge_nul\0B").as_deref(), Some(&b"vb"[..]));
        assert_eq!(store.get("bridge_nul"), None, "the key must not be truncated at the NUL");

        // Safety: `a` is valid for its length.
        assert_eq!(unsafe { kv_get(a.as_ptr().cast(), a.len()) }, 1);
        let mut ptr: *const std::os::raw::c_char = std::ptr::null();
        let mut len: usize = 0;
        // Safety: ptr and len are valid stack variables.
        unsafe { kv_get_result(&mut ptr, &mut len) };
        let got = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        assert_eq!(got, b"va");
        // Safety: `b` is valid for its length.
        assert_eq!(unsafe { kv_del(b.as_ptr().cast(), b.len()) }, 1);
        assert_eq!(store.get("bridge_nul\0A").as_deref(), Some(&b"va"[..]));
    }

    /// A non-UTF-8 key (raw `md5($x, true)` output) cannot be held by the
    /// `String`-keyed store, so every op refuses it rather than storing it
    /// under a lossy, colliding name.
    #[test]
    #[serial]
    fn non_utf8_keys_are_refused() {
        let store = init_store();
        let before = store.len();
        let key: &[u8] = b"bridge_bin\xff\xfe";
        let k = key.as_ptr().cast();
        let mut out: std::os::raw::c_longlong = 0;
        // Safety: `key` and the values are valid for their lengths.
        unsafe {
            assert_eq!(kv_set(k, key.len(), b"v".as_ptr().cast(), 1, 0), 0);
            assert_eq!(kv_set_nx(k, key.len(), b"v".as_ptr().cast(), 1, 0), 0);
            assert_eq!(kv_incr_by(k, key.len(), 1, &mut out), 0);
            assert_eq!(kv_get(k, key.len()), 0);
            assert_eq!(kv_exists(k, key.len()), 0);
            assert_eq!(kv_pttl(k, key.len()), -2);
        }
        assert_eq!(store.len(), before, "nothing may have been written");
    }

    /// A TTL too large for `Instant + Duration` is refused instead of
    /// panicking inside the store (a panic in an `extern "C"` fn aborts).
    #[test]
    #[serial]
    fn out_of_range_ttl_is_refused_not_a_panic() {
        let store = init_store();
        let key = cstr("bridge_huge_ttl");
        let max = std::os::raw::c_longlong::MAX;
        // Safety: key and value are valid for their lengths.
        unsafe {
            assert_eq!(kv_set(key.as_ptr(), key.as_bytes().len(), b"v".as_ptr().cast(), 1, max), 0);
            assert_eq!(store.get("bridge_huge_ttl"), None);
            assert_eq!(kv_set(key.as_ptr(), key.as_bytes().len(), b"v".as_ptr().cast(), 1, 0), 1);
            assert_eq!(kv_expire(key.as_ptr(), key.as_bytes().len(), max), 0);
        }
        assert_eq!(store.pttl("bridge_huge_ttl"), Some(-1), "the refused expire changed nothing");
    }

    // ── kv_del ──────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn del_existing_returns_one() {
        let store = init_store();
        store.set("bridge_del_k".into(), b"v".to_vec(), None);
        let key = cstr("bridge_del_k");
        // Safety: key is a valid C string.
        let removed = unsafe { kv_del(key.as_ptr(), key.as_bytes().len()) };
        assert_eq!(removed, 1);
        assert_eq!(store.get("bridge_del_k"), None);
    }

    #[test]
    #[serial]
    fn del_missing_returns_zero() {
        init_store();
        let key = cstr("bridge_del_missing");
        // Safety: key is a valid C string.
        let removed = unsafe { kv_del(key.as_ptr(), key.as_bytes().len()) };
        assert_eq!(removed, 0);
    }

    // ── kv_del_if_eq (compare-and-delete) ───────────────────────────────

    #[test]
    #[serial]
    fn del_if_eq_matching_token_deletes() {
        let store = init_store();
        store.set("bridge_cad_match".into(), b"tok".to_vec(), None);
        let key = cstr("bridge_cad_match");
        let token = b"tok";
        // Safety: key is a valid C string; token is valid for token.len() bytes.
        let rc = unsafe {
            kv_del_if_eq(key.as_ptr(), key.as_bytes().len(), token.as_ptr().cast(), token.len())
        };
        assert_eq!(rc, 1, "a matching token must delete");
        assert_eq!(store.get("bridge_cad_match"), None);
    }

    #[test]
    #[serial]
    fn del_if_eq_mismatched_token_is_noop() {
        let store = init_store();
        store.set("bridge_cad_mismatch".into(), b"tok".to_vec(), None);
        let key = cstr("bridge_cad_mismatch");
        let token = b"other";
        // Safety: key is a valid C string; token is valid for token.len() bytes.
        let rc = unsafe {
            kv_del_if_eq(key.as_ptr(), key.as_bytes().len(), token.as_ptr().cast(), token.len())
        };
        assert_eq!(rc, 0, "a different token must not delete");
        assert_eq!(store.get("bridge_cad_mismatch").as_deref(), Some(&b"tok"[..]));
    }

    #[test]
    #[serial]
    fn del_if_eq_absent_key_is_noop() {
        init_store();
        let key = cstr("bridge_cad_absent");
        let token = b"tok";
        // Safety: key is a valid C string; token is valid for token.len() bytes.
        let rc = unsafe {
            kv_del_if_eq(key.as_ptr(), key.as_bytes().len(), token.as_ptr().cast(), token.len())
        };
        assert_eq!(rc, 0);
    }

    #[test]
    #[serial]
    fn del_if_eq_matches_binary_token() {
        let store = init_store();
        let token: &[u8] = &[0x00, 0x01, 0xFF, 0xFE];
        store.set("bridge_cad_bin".into(), token.to_vec(), None);
        let key = cstr("bridge_cad_bin");
        // Safety: key is a valid C string; token is valid for token.len() bytes.
        let rc = unsafe {
            kv_del_if_eq(key.as_ptr(), key.as_bytes().len(), token.as_ptr().cast(), token.len())
        };
        assert_eq!(rc, 1, "a binary token must compare byte-exactly and delete");
        assert_eq!(store.get("bridge_cad_bin"), None);
    }

    // ── kv_exists ───────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn exists_present_returns_one() {
        let store = init_store();
        store.set("bridge_ex_k".into(), b"v".to_vec(), None);
        let key = cstr("bridge_ex_k");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_exists(key.as_ptr(), key.as_bytes().len()) }, 1);
    }

    #[test]
    #[serial]
    fn exists_absent_returns_zero() {
        init_store();
        let key = cstr("bridge_ex_missing");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_exists(key.as_ptr(), key.as_bytes().len()) }, 0);
    }

    // ── kv_incr_by ──────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn incr_by_creates_key() {
        let store = init_store();
        let key = cstr("bridge_incr_new");
        let mut result: std::os::raw::c_longlong = 0;
        // Safety: key and result are valid.
        let ok = unsafe { kv_incr_by(key.as_ptr(), key.as_bytes().len(), 1, &mut result) };
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
        unsafe { kv_incr_by(key.as_ptr(), key.as_bytes().len(), 5, &mut result) };
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
        unsafe { kv_incr_by(key.as_ptr(), key.as_bytes().len(), -3, &mut result) };
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
        let ok = unsafe { kv_incr_by(key.as_ptr(), key.as_bytes().len(), 1, &mut result) };
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
        let ok = unsafe { kv_expire(key.as_ptr(), key.as_bytes().len(), 30_000) };
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
        let ok = unsafe { kv_expire(key.as_ptr(), key.as_bytes().len(), 10_000) };
        assert_eq!(ok, 0);
    }

    #[test]
    #[serial]
    fn expire_zero_or_negative_returns_zero() {
        let store = init_store();
        store.set("bridge_exp_neg".into(), b"v".to_vec(), None);
        let key = cstr("bridge_exp_neg");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_expire(key.as_ptr(), key.as_bytes().len(), 0) }, 0);
        assert_eq!(unsafe { kv_expire(key.as_ptr(), key.as_bytes().len(), -1) }, 0);
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
        assert_eq!(unsafe { kv_pttl(key.as_ptr(), key.as_bytes().len()) }, -1);
    }

    #[test]
    #[serial]
    fn pttl_missing_key_returns_minus_two() {
        init_store();
        let key = cstr("bridge_pttl_missing");
        // Safety: key is a valid C string.
        assert_eq!(unsafe { kv_pttl(key.as_ptr(), key.as_bytes().len()) }, -2);
    }

    #[test]
    #[serial]
    fn pttl_with_expiry_returns_positive() {
        let store = init_store();
        store.set("bridge_pttl_exp".into(), b"v".to_vec(), Some(Duration::from_secs(60)));
        let key = cstr("bridge_pttl_exp");
        // Safety: key is a valid C string.
        let ms = unsafe { kv_pttl(key.as_ptr(), key.as_bytes().len()) };
        assert!(ms > 0 && ms <= 60_000, "expected PTTL in (0, 60000], got {ms}");
    }

    // ── kv_flush_all ────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn flush_all_removes_all_keys_from_global_store() {
        let store = init_store();
        store.set("bridge_flush_a".into(), b"v1".to_vec(), None);
        store.set("bridge_flush_b".into(), b"v2".to_vec(), Some(Duration::from_secs(60)));
        store.hset("bridge_flush_h", "f", b"v".to_vec()).unwrap();

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

    // ── Unmatched host (KV stress report F-2) ───────────────────────────

    /// A multi-tenant request whose `Host` matched no vhost has no keyspace:
    /// every op fails, nothing falls back to the global store, and the
    /// `site_context` op asks the C side to warn exactly once per request.
    #[test]
    #[serial]
    fn unmatched_scope_fails_closed_and_warns_once_per_request() {
        let global = init_store();
        global.set("bridge_unmatched".into(), b"global".to_vec(), None);
        let key = cstr("bridge_unmatched");
        let val = b"from-an-unknown-host";

        set_site_scope(KvSiteScope::Unmatched);
        // Safety: key and val are valid for the duration of each call.
        unsafe {
            assert_eq!(
                kv_get(key.as_ptr(), key.as_bytes().len()),
                0,
                "must not read the global store"
            );
            assert_eq!(
                kv_set(key.as_ptr(), key.as_bytes().len(), val.as_ptr().cast(), val.len(), 0),
                0
            );
            assert_eq!(
                kv_set_nx(key.as_ptr(), key.as_bytes().len(), val.as_ptr().cast(), val.len(), 0),
                0
            );
            assert_eq!(kv_exists(key.as_ptr(), key.as_bytes().len()), 0);
            assert_eq!(kv_del(key.as_ptr(), key.as_bytes().len()), 0);
            assert_eq!(kv_pttl(key.as_ptr(), key.as_bytes().len()), -2);
            assert_eq!(kv_flush_all(), 0);
        }
        assert_eq!(
            global.get("bridge_unmatched").as_deref(),
            Some(&b"global"[..]),
            "an unmatched request must not touch the global store"
        );

        // Safety: no arguments.
        unsafe {
            assert_eq!(kv_site_context(), 1, "first call this request: warn");
            assert_eq!(kv_site_context(), 2, "then fail quietly");
        }
        // The next request re-arms the warning...
        set_site_scope(KvSiteScope::Unmatched);
        assert_eq!(unsafe { kv_site_context() }, 1);
        // ...and a request with a keyspace reports one.
        set_site_scope(KvSiteScope::Global);
        assert_eq!(unsafe { kv_site_context() }, 0);
        set_site_store(Some(Store::new(StoreConfig::default())));
        assert_eq!(unsafe { kv_site_context() }, 0);

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
        let rc = unsafe { kv_wait(key.as_ptr(), key.as_bytes().len(), 0, 5_000, &mut new_version) };
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
        let rc = unsafe { kv_wait(key.as_ptr(), key.as_bytes().len(), 0, 1_000, &mut new_version) };
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
        assert_eq!(unsafe { kv_wait(key.as_ptr(), key.as_bytes().len(), 0, 1_000, &mut ver) }, 1);

        // Waiting past the current version with no writes must time out.
        let start = std::time::Instant::now();
        let mut unused: std::os::raw::c_longlong = 0;
        // Safety: key and unused are valid.
        let rc = unsafe { kv_wait(key.as_ptr(), key.as_bytes().len(), ver, 80, &mut unused) };
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
        assert_eq!(unsafe { kv_wait(key.as_ptr(), key.as_bytes().len(), 0, 1_000, &mut ver) }, 1);

        let writer = {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                store.set("bridge_wait_wake".into(), b"new".to_vec(), None);
            })
        };

        let mut new_ver: std::os::raw::c_longlong = 0;
        // Safety: key and new_ver are valid.
        let rc = unsafe { kv_wait(key.as_ptr(), key.as_bytes().len(), ver, 10_000, &mut new_ver) };
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
            unsafe { kv_get(key.as_ptr(), key.as_bytes().len()) };
            let mut ptr: *const std::os::raw::c_char = std::ptr::null();
            let mut len: usize = 0;
            unsafe { kv_get_result(&mut ptr, &mut len) };
            unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec()
        });

        let t2 = thread::spawn(|| {
            let key = cstr("bridge_tl_t2");
            // Safety: key is valid for this thread.
            unsafe { kv_get(key.as_ptr(), key.as_bytes().len()) };
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
                KV_HOOK_GET.store(kv_get(key.as_ptr(), key.as_bytes().len()), Ordering::Release);
                KV_HOOK_SET.store(
                    kv_set(key.as_ptr(), key.as_bytes().len(), val.as_ptr().cast(), val.len(), 0),
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
            assert_eq!(unsafe { kv_get(key.as_ptr(), key.as_bytes().len()) }, 1);
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

#[cfg(test)]
mod set_nx_rc_tests {
    use ephpm_kv::store::SetNxOutcome;

    use super::set_nx_rc;

    #[test]
    fn owner_unreachable_is_distinct_from_contention() {
        // The session lock loop relies on -1 meaning "outcome unknown", not
        // "someone else holds it"; ephpm_kv_setnx() maps only 1 to true.
        assert_eq!(set_nx_rc(SetNxOutcome::Inserted), 1);
        assert_eq!(set_nx_rc(SetNxOutcome::Exists), 0);
        assert_eq!(set_nx_rc(SetNxOutcome::OutOfMemory), 0);
        assert_eq!(set_nx_rc(SetNxOutcome::OwnerUnreachable), -1);
    }
}

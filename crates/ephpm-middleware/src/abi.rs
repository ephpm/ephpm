//! The versioned C ABI shared between the ePHPm host and middleware modules.
//!
//! A middleware module is a shared library exporting four C symbols:
//!
//! ```c
//! int32_t ephpm_middleware_init(uint32_t abi_version,
//!                               const char* config_json,
//!                               const ephpm_host_v1* host);
//! int32_t ephpm_middleware_invoke(const ephpm_request_t* request,
//!                                 ephpm_response_t* response_out);
//! void    ephpm_middleware_shutdown(void);
//! const char* ephpm_middleware_describe(void);   /* nullable */
//! ```
//!
//! The host callback table ([`EphpmHostV1`]) is passed BY POINTER at `init`
//! (valid for the process lifetime) rather than having modules `dlsym` host
//! symbols — exporting symbols from the host executable needs `-rdynamic` on
//! Linux and has no clean Windows analogue, while a table pointer is portable
//! everywhere `dlopen`/`LoadLibrary` is.
//!
//! Request data is exposed through accessor function pointers on the host
//! table (not a flat struct), so fields can be added without breaking the
//! ABI: additions append to the end of the table under the same major
//! version. The major byte of [`ABI_V1`] gates compatibility — modules must
//! refuse to init when the host's major is newer than they were built for.
//!
//! All pointers a module writes into [`EphpmResponse`] must remain valid
//! until its `invoke` returns; the host copies everything before unwinding.

use std::os::raw::{c_char, c_int};

/// ABI version 1 (`0x01_00_00_00` — major byte gates compatibility).
pub const ABI_V1: u32 = 0x0100_0000;

/// Middleware verdicts for one request.
pub const ACTION_CONTINUE: c_int = 0;
/// Short-circuit: return `status`/`body` to the client; PHP never runs.
pub const ACTION_RESPOND: c_int = 1;
/// Mutate the request (path/header overrides), then continue the chain.
pub const ACTION_REWRITE: c_int = 2;

/// Log levels for [`EphpmHostV1::log`] (match `tracing` levels).
pub const LOG_ERROR: c_int = 1;
/// Warning level.
pub const LOG_WARN: c_int = 2;
/// Info level.
pub const LOG_INFO: c_int = 3;
/// Debug level.
pub const LOG_DEBUG: c_int = 4;

/// Opaque per-request context. Only valid for the duration of one
/// `ephpm_middleware_invoke` call; never store it.
#[repr(C)]
pub struct EphpmRequest {
    _opaque: [u8; 0],
}

/// One header name/value pair (NUL-terminated UTF-8).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EphpmHeaderKv {
    /// Header name.
    pub name: *const c_char,
    /// Header value.
    pub value: *const c_char,
}

/// The module's verdict, filled during `invoke`. Zero-initialized by the
/// host before the call (`action = ACTION_CONTINUE`, everything else null).
#[repr(C)]
pub struct EphpmResponse {
    /// One of `ACTION_CONTINUE` / `ACTION_RESPOND` / `ACTION_REWRITE`.
    pub action: c_int,
    /// HTTP status for `ACTION_RESPOND`.
    pub status: u16,
    /// Response body bytes for `ACTION_RESPOND` (nullable).
    pub body: *const u8,
    /// Length of `body`.
    pub body_len: usize,
    /// New request path for `ACTION_REWRITE` (nullable = keep).
    pub rewrite_path: *const c_char,
    /// Header overrides for `ACTION_REWRITE` **and** extra response headers
    /// for `ACTION_RESPOND` (nullable).
    pub header_overrides: *const EphpmHeaderKv,
    /// Number of entries in `header_overrides`.
    pub header_overrides_len: usize,
    /// Headers appended to the eventual client response for
    /// `ACTION_CONTINUE` / `ACTION_REWRITE` (nullable). For `ACTION_RESPOND`
    /// use `header_overrides`.
    pub response_headers: *const EphpmHeaderKv,
    /// Number of entries in `response_headers`.
    pub response_headers_len: usize,
}

/// Version-1 host callback table, passed to `ephpm_middleware_init` and valid
/// for the process lifetime.
///
/// KV operations hit the embedded, gossip-replicated store — the same data
/// PHP sees through `ephpm_kv_*` — which is what makes a cluster-wide rate
/// limiter a single `kv_incr` call.
#[repr(C)]
pub struct EphpmHostV1 {
    /// Host ABI version (== [`ABI_V1`] for this table).
    pub abi_version: u32,

    // ── Request accessors (valid only during `invoke`) ───────────────────
    /// HTTP method (`"GET"`, ...).
    pub request_method: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// URL path (no query string).
    pub request_path: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// Raw query string (no leading `?`; empty when absent).
    pub request_query: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// Client IP after trusted-proxy resolution.
    pub request_remote_ip: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,
    /// Header lookup by case-insensitive name; NULL when absent. Multi-value
    /// headers return the values pre-joined per HTTP list semantics.
    pub request_header:
        unsafe extern "C" fn(*const EphpmRequest, name: *const c_char) -> *const c_char,
    /// Request body view. v1: bodies are not read before the middleware chain
    /// runs (rejecting BEFORE the body transfer is the point), so this
    /// returns length 0. Reserved for a future buffered-body option.
    pub request_body: unsafe extern "C" fn(*const EphpmRequest, out_ptr: *mut *const u8) -> usize,
    /// Identity of the vhost/site serving this request (server name).
    pub request_vhost_id: unsafe extern "C" fn(*const EphpmRequest) -> *const c_char,

    // ── KV store ─────────────────────────────────────────────────────────
    /// Get. Returns 0 with `out`/`out_len` set (free with `kv_free`),
    /// 1 when the key is absent, negative on error.
    pub kv_get: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int,
    /// Set with optional TTL (`ttl_secs <= 0` = no expiry). Returns 0 on
    /// success, negative on error.
    pub kv_set: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
        ttl_secs: i64,
    ) -> c_int,
    /// Set-if-absent. Returns 0 when set, 1 when the key already existed,
    /// negative on error.
    pub kv_set_nx: unsafe extern "C" fn(
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
        ttl_secs: i64,
    ) -> c_int,
    /// Atomic increment. Returns 0 with the new value in `out`, negative on
    /// error (e.g. non-numeric existing value).
    pub kv_incr:
        unsafe extern "C" fn(key: *const u8, key_len: usize, by: i64, out: *mut i64) -> c_int,
    /// Free a buffer returned by `kv_get`.
    pub kv_free: unsafe extern "C" fn(ptr: *mut u8, len: usize),

    // ── Logging ──────────────────────────────────────────────────────────
    /// Log through the host's `tracing` subscriber (`LOG_*` levels).
    pub log: unsafe extern "C" fn(level: c_int, msg: *const u8, msg_len: usize),
}

/// Symbol names the loader looks up.
pub const SYM_INIT: &[u8] = b"ephpm_middleware_init";
/// Per-request entrypoint symbol.
pub const SYM_INVOKE: &[u8] = b"ephpm_middleware_invoke";
/// Shutdown symbol.
pub const SYM_SHUTDOWN: &[u8] = b"ephpm_middleware_shutdown";
/// Optional metadata symbol.
pub const SYM_DESCRIBE: &[u8] = b"ephpm_middleware_describe";

/// `ephpm_middleware_init` signature.
pub type InitFn = unsafe extern "C" fn(u32, *const c_char, *const EphpmHostV1) -> c_int;
/// `ephpm_middleware_invoke` signature.
pub type InvokeFn = unsafe extern "C" fn(*const EphpmRequest, *mut EphpmResponse) -> c_int;
/// `ephpm_middleware_shutdown` signature.
pub type ShutdownFn = unsafe extern "C" fn();
/// `ephpm_middleware_describe` signature.
pub type DescribeFn = unsafe extern "C" fn() -> *const c_char;

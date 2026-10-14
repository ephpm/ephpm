//! Bridge between the C worker-mode PHP functions and the Rust dispatch
//! channel (persistent-worker engine, `worker-mode-design.md`).
//!
//! Mirrors the [`kv_bridge`](crate::kv_bridge) ops-table pattern. The C
//! `\Ephpm\Worker\take_request()` / `send_response()` native functions call
//! into the [`EphpmWorkerOps`] table set via [`ephpm_set_worker_ops`]:
//!
//! - `take_request` blocks on the thread's dispatch source (its
//!   [`handoff::Worker`](crate::handoff::Worker) mailbox in the pool, a
//!   one-job channel on the debug lane), stashes the
//!   job's `oneshot::Sender` in a [`thread_local!`] cell, and fills a borrowed
//!   [`EphpmWorkerRequest`] view for C to marshal into the `Envelope` object.
//! - `send_response` takes the stashed sender back out and fulfils it.
//!
//! The stashed sender is the only Rust value live across a PHP call that can
//! `longjmp` (design §5.3). `oneshot::Sender`'s `Drop` is longjmp-safe (it just
//! signals the receiver — no files, no locks), so a fatal that unwinds past
//! `send_response` still lets the HTTP handler resolve: either the pool's
//! supervisor finds the sender still stashed and sends
//! [`WorkerResponse::internal_error`] (500), or the sender is dropped and the
//! receiver sees `RecvError` (also 500).
//!
//! ## Phase 3: streaming bodies
//!
//! The ops table also carries `body_read` (incremental request body) and
//! `response_begin`/`response_chunk`/`response_end` (incremental response). A
//! request body is either [`WorkerBody::Buffered`] (small, Phase-1 path) or
//! [`WorkerBody::Streaming`] (large — chunks arrive on a bounded channel the
//! worker `blocking_recv`s via `body_read`, so ePHPm never holds the whole
//! body). `send_response_stream` on the PHP side drives `response_begin` (which
//! delivers status+headers on the oneshot immediately, as
//! [`WorkerResponse::Streaming`]) then `response_chunk` per chunk (bounded
//! `blocking_send` = download backpressure) then `response_end`. The worker
//! threads are plain OS threads (not on the tokio runtime), so `blocking_recv`
//! / `blocking_send` are valid there.
//!
//! Everything real is `#[cfg(php_linked)]`; stub builds get inert fallbacks.

#[cfg(php_linked)]
use std::cell::RefCell;
#[cfg(php_linked)]
use std::ffi::CString;
#[cfg(php_linked)]
use std::os::raw::{c_char, c_int};

use crate::request::CServerVar;

/// Chunk size / channel bounds shared with the streaming-body plumbing.
///
/// A bounded channel of `BODY_CHANNEL_DEPTH` chunks caps the in-flight buffered
/// bytes at roughly `depth * chunk` regardless of upload size, which is the
/// flat-memory guarantee (design §9 exit criterion).
pub const BODY_CHANNEL_DEPTH: usize = 8;

// ── Shared message types (used by ephpm-server's worker pool) ────────────

/// The request body handed to a worker: either fully buffered (Phase 1
/// back-compat, small bodies) or streamed incrementally from the hyper task
/// (Phase 3, large uploads with flat worker memory).
pub enum WorkerBody {
    /// The whole body, already in memory.
    Buffered(Vec<u8>),
    /// Incremental body: the worker thread `blocking_recv`s `Bytes` chunks off
    /// this bounded receiver (a hyper task sends frames); the sender closing
    /// signals clean EOF. `declared_len` is the `Content-Length` (so PHP's POST
    /// reader knows how much to expect); it may be `0` for chunked bodies.
    Streaming {
        /// Bounded receiver of body chunks; sender close = clean EOF. Bounded,
        /// so a slow worker applies backpressure to the hyper reader.
        rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
        /// Declared `Content-Length`, or `0` when unknown.
        declared_len: usize,
    },
}

impl std::fmt::Debug for WorkerBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buffered(b) => f.debug_tuple("Buffered").field(&b.len()).finish(),
            Self::Streaming { declared_len, .. } => {
                f.debug_struct("Streaming").field("declared_len", declared_len).finish()
            }
        }
    }
}

/// An owned HTTP request handed to a worker thread. Owns every string buffer so
/// the borrowed [`EphpmWorkerRequest`] view stays valid for the whole iteration
/// (until `send_response`).
pub struct WorkerRequestOwned {
    /// HTTP method (`GET`, `POST`, ...).
    pub method: String,
    /// `REQUEST_URI` — path plus query string.
    pub uri: String,
    /// Query string without the leading `?`.
    pub query_string: String,
    /// Raw `Cookie` header value (empty if none).
    pub cookie_data: String,
    /// `Content-Type` header value, if any.
    pub content_type: Option<String>,
    /// The request body (buffered or streaming).
    pub body: WorkerBody,
    /// `$_SERVER`-shaped variables, already in FFI-ready form (built once by
    /// `build_server_variables_c` — issue #133; the borrowed
    /// [`EphpmWorkerRequest`] view points straight into these).
    pub server_vars: Vec<CServerVar>,
    /// HTTP headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
}

/// Hand-written for the same reason as [`PhpRequest`](crate::request::PhpRequest)'s:
/// `server_vars` holds the request's whole `$_SERVER`, which since #386 includes
/// `PHP_AUTH_PW` alongside the per-site `DB_PASSWORD`, and `headers` holds the
/// `Authorization` header they came from. Values for secret-bearing names are
/// replaced with a placeholder — see `request::is_secret_name`.
impl std::fmt::Debug for WorkerRequestOwned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let server_vars = crate::request::redacted_pairs(
            self.server_vars
                .iter()
                .map(|(k, v)| (k.to_str().unwrap_or_default(), v.to_str().unwrap_or_default())),
        );
        let headers = crate::request::redacted_pairs(
            self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        );
        f.debug_struct("WorkerRequestOwned")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("query_string", &self.query_string)
            .field("cookie_data", &"<redacted>")
            .field("content_type", &self.content_type)
            .field("body", &self.body)
            .field("server_vars", &server_vars)
            .field("headers", &headers)
            .finish()
    }
}

/// A response produced by a worker for one request: either fully buffered
/// (`send_response`) or streamed (`send_response_stream`).
#[derive(Debug)]
pub enum WorkerResponse {
    /// Complete response with a fully-materialized body.
    Buffered {
        /// HTTP status code.
        status: u16,
        /// Response headers as `(name, value)` pairs.
        headers: Vec<(String, String)>,
        /// Response body bytes.
        body: Vec<u8>,
    },
    /// Streamed response: status + headers now, body chunks arrive on `body_rx`
    /// as the worker produces them (`send_response_stream`). The hyper handler
    /// turns `body_rx` into a `StreamBody` so bytes flush before PHP finishes.
    Streaming {
        /// HTTP status code.
        status: u16,
        /// Response headers as `(name, value)` pairs.
        headers: Vec<(String, String)>,
        /// Bounded receiver of body chunks; sender close = end of body.
        body_rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
        /// Set when the worker died before finishing this body — see
        /// [`StreamAbortFlag`].
        aborted: StreamAbortFlag,
    },
}

/// Marks a streamed response whose producer died before it was finished.
///
/// Once `response_begin` has delivered status + headers, they are on the wire
/// and cannot be retracted. If the worker then bails out, simply dropping the
/// chunk sender closes the body channel — and a closed channel is
/// indistinguishable from a normal end of body, so hyper writes the
/// terminating chunk and the client reads a well-formed, silently truncated
/// **200**. That is the failure this flag exists to prevent.
///
/// [`clear_in_flight_streams`] sets it before dropping the sender; the body
/// stream checks it once the channel is exhausted and, if set, ends the body
/// with an `io::Error` instead of a clean EOF. hyper then abandons the
/// response — no terminating chunk under `Transfer-Encoding: chunked`, a
/// short read against a declared `Content-Length` — so the client's transfer
/// fails rather than succeeding with half a document.
pub type StreamAbortFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

impl WorkerResponse {
    /// Build the canonical 500 response used when a worker bailed out before
    /// sending a real response (design §5.3).
    #[must_use]
    pub fn internal_error() -> Self {
        Self::Buffered {
            status: 500,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: b"Internal Server Error".to_vec(),
        }
    }
}

/// A unit of work dispatched to the worker pool: the owned request plus the
/// `oneshot` channel the worker fulfils.
pub struct WorkerJob {
    /// The owned request marshaled to PHP.
    pub request: WorkerRequestOwned,
    /// Where the worker's response (or the supervisor's 500) is delivered.
    pub respond_to: tokio::sync::oneshot::Sender<WorkerResponse>,
    /// Fair-admission permit held while this job occupies the dispatch queue
    /// (issue #442). Granted by the pool's FIFO admission semaphore before the
    /// job is enqueued and dropped by the worker the moment it pulls the job —
    /// releasing the queue slot to the *longest-waiting* dispatcher instead of
    /// whichever sender happens to race a bounded-channel `send()` first.
    /// `None` only for jobs that never passed admission (tests).
    pub admission: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl WorkerBody {
    /// The length PHP should expect: the actual size of a buffered body, or
    /// the declared `Content-Length` (possibly `0`) of a streaming one.
    #[must_use]
    pub fn declared_len(&self) -> usize {
        match self {
            Self::Buffered(data) => data.len(),
            Self::Streaming { declared_len, .. } => *declared_len,
        }
    }
}

// ── Debug lane: pre-seeded request startup ───────────────────────────────

/// The request a worker-mode **debug lane** thread pre-seeds into
/// `SG(request_info)` (and its `$_SERVER` registration list) *before* its
/// `php_request_startup()`, so request-startup consumers — Xdebug's
/// step-debug trigger check, and its `discover_client_host` lookup of
/// `REMOTE_ADDR` / forwarding headers — see the triggering HTTP request.
/// Owned copies, taken from the [`WorkerRequestOwned`] before it is queued for
/// `take_request()`: the job keeps its own copy of everything, including the
/// body.
#[derive(Clone)]
pub struct DebugSeed {
    /// HTTP method.
    pub method: String,
    /// `REQUEST_URI`.
    pub uri: String,
    /// Query string without the leading `?` (`XDEBUG_TRIGGER=1` lives here).
    pub query_string: String,
    /// Raw `Cookie` header (`XDEBUG_SESSION=...` lives here).
    pub cookie_data: String,
    /// `Content-Type`, if any.
    pub content_type: Option<String>,
    /// Declared body length (see [`WorkerBody::declared_len`]).
    pub content_length: usize,
    /// The request's `$_SERVER` variables, in FFI-ready form.
    pub server_vars: Vec<CServerVar>,
    /// A private copy of the request body for PHP to parse into `$_POST`
    /// during request startup, so a trigger in a form body is visible to
    /// Xdebug. Only ever set for a fully **buffered**
    /// `application/x-www-form-urlencoded` `POST` (see
    /// [`DebugSeed::from_request`]); `None` means no body is read at startup.
    /// Freed as soon as request startup returns.
    pub post_body: Option<Vec<u8>>,
}

impl std::fmt::Debug for DebugSeed {
    // Hand-written so `$_SERVER` values (which carry `PHP_AUTH_PW` since
    // #386) and cookies never reach a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugSeed")
            .field("method", &self.method)
            .field("content_type", &self.content_type)
            .field("content_length", &self.content_length)
            .field("server_vars", &self.server_vars.len())
            .field("post_body", &self.post_body.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

impl DebugSeed {
    /// Copy the seed out of `req`. The body is copied only when PHP itself
    /// would parse it into `$_POST` *and* it is already in memory: a `POST`
    /// whose content type is `application/x-www-form-urlencoded` (PHP's
    /// matching: case-insensitive, parameters after `;` ignored) with a
    /// [`WorkerBody::Buffered`] body. A streaming body is never touched —
    /// reading it here would consume it before the application could.
    #[must_use]
    pub fn from_request(req: &WorkerRequestOwned) -> Self {
        let post_body = match &req.body {
            WorkerBody::Buffered(bytes)
                if req.method == "POST"
                    && req.content_type.as_deref().is_some_and(is_form_urlencoded) =>
            {
                Some(bytes.clone())
            }
            _ => None,
        };
        Self {
            method: req.method.clone(),
            uri: req.uri.clone(),
            query_string: req.query_string.clone(),
            cookie_data: req.cookie_data.clone(),
            content_type: req.content_type.clone(),
            content_length: req.body.declared_len(),
            server_vars: req.server_vars.clone(),
            post_body,
        }
    }

    /// `REQUEST_URI` without its query string — what the lane logs. The
    /// query can carry the `xdebug.trigger_value` secret.
    #[must_use]
    pub fn path(&self) -> &str {
        self.uri.split('?').next().unwrap_or_default()
    }
}

/// Whether a `Content-Type` selects PHP's `application/x-www-form-urlencoded`
/// POST handler. Mirrors `sapi_read_post_data()`: the type is lower-cased and
/// cut at the first `;`, `,` or space before the lookup.
#[must_use]
pub fn is_form_urlencoded(content_type: &str) -> bool {
    let end = content_type.find([';', ',', ' ']).unwrap_or(content_type.len());
    content_type[..end].eq_ignore_ascii_case("application/x-www-form-urlencoded")
}

/// Storage for the pointers a debug-lane thread lends to C at request
/// startup. Lives in a thread-local for the whole life of the thread: the C
/// side keeps the `SG(request_info)` pointers until `ephpm_thread_shutdown()`
/// has run, and the thread retires through `worker_thread_shutdown()` as
/// ordinary code — before TLS destructors — so the storage is still alive at
/// that point. (Once `take_request()` has run, `SG(request_info)` points at
/// the job instead.) The server vars stay registered for the request's
/// lifetime because PHP builds `$_SERVER` lazily (`auto_globals_jit`); only
/// the POST copy is released early — see [`thread_init_preseeded`].
#[cfg(php_linked)]
#[allow(dead_code)] // owns memory C borrows; never read from Rust after install
struct PreseededStrings {
    method: CString,
    uri: CString,
    query_string: CString,
    cookie_data: CString,
    content_type: Option<CString>,
    path_translated: CString,
    server_vars: Vec<CServerVar>,
    /// The key/value pointer arrays `ephpm_request_set_server_vars`
    /// borrows (into `server_vars`). Filled once the struct is in its cell.
    server_var_keys: Vec<*const c_char>,
    server_var_vals: Vec<*const c_char>,
    post_body: Option<Vec<u8>>,
}

#[cfg(php_linked)]
thread_local! {
    /// The strings `SG(request_info)` borrows on a debug lane thread.
    static PRESEEDED: RefCell<Option<PreseededStrings>> = const { RefCell::new(None) };
}

/// Raw views into [`PreseededStrings`], collected so the `RefCell` borrow is
/// released before any FFI call.
#[cfg(php_linked)]
struct PreseededPtrs {
    method: *const c_char,
    uri: *const c_char,
    query_string: *const c_char,
    content_type: *const c_char,
    cookie_data: *const c_char,
    path_translated: *const c_char,
    server_var_count: usize,
    server_var_keys: *const *const c_char,
    server_var_vals: *const *const c_char,
    post_data: *const c_char,
    post_len: usize,
}

/// Register this thread with TSRM and start its long-lived request with
/// `SG(request_info)` and `$_SERVER` pre-seeded from `seed`. Returns the C
/// status (`0` ok).
///
/// Only [`crate::PhpRuntime::worker_thread_init_preseeded`] should call this —
/// it owns the registration guard that pairs with `worker_thread_shutdown`.
#[cfg(php_linked)]
pub(crate) fn thread_init_preseeded(seed: DebugSeed, path_translated: &std::path::Path) -> c_int {
    // Lossy on interior NULs, same policy as `build_current_request`.
    let cstr = |s: String| CString::new(s).unwrap_or_default();
    let content_length = seed.content_length;
    let strings = PreseededStrings {
        method: cstr(seed.method),
        uri: cstr(seed.uri),
        query_string: cstr(seed.query_string),
        cookie_data: cstr(seed.cookie_data),
        content_type: seed.content_type.map(cstr),
        path_translated: cstr(path_translated.to_string_lossy().into_owned()),
        server_vars: seed.server_vars,
        server_var_keys: Vec::new(),
        server_var_vals: Vec::new(),
        post_body: seed.post_body,
    };
    // Install the storage, then take raw views with a short-lived shared
    // borrow: no RefCell guard (or any other Rust value with a destructor)
    // is live across the PHP calls below. The heap buffers the pointers refer
    // to do not move when the struct is moved into the cell.
    let ptrs = PRESEEDED.with(|cell| {
        *cell.borrow_mut() = Some(strings);
        if let Some(s) = cell.borrow_mut().as_mut() {
            s.server_var_keys = s.server_vars.iter().map(|(k, _)| k.as_ptr()).collect();
            s.server_var_vals = s.server_vars.iter().map(|(_, v)| v.as_ptr()).collect();
        }
        let slot = cell.borrow();
        let s = slot.as_ref().expect("PRESEEDED was set on the line above");
        PreseededPtrs {
            method: s.method.as_ptr(),
            uri: s.uri.as_ptr(),
            query_string: s.query_string.as_ptr(),
            content_type: s.content_type.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            cookie_data: s.cookie_data.as_ptr(),
            path_translated: s.path_translated.as_ptr(),
            server_var_count: s.server_var_keys.len(),
            server_var_keys: s.server_var_keys.as_ptr(),
            server_var_vals: s.server_var_vals.as_ptr(),
            post_data: s.post_body.as_ref().map_or(std::ptr::null(), |b| b.as_ptr().cast()),
            post_len: s.post_body.as_ref().map_or(0, Vec::len),
        }
    });
    // SAFETY: both arrays, and the NUL-terminated strings they point to, are
    // owned by this thread's PRESEEDED cell (see below for its lifetime).
    // The C side only stores the array pointers (no PHP call, cannot bail
    // out); PHP reads them when it builds `$_SERVER`.
    unsafe {
        crate::ffi::ephpm_request_set_server_vars(
            ptrs.server_var_count,
            ptrs.server_var_keys,
            ptrs.server_var_vals,
        );
    }
    // SAFETY: every pointer borrows from the `PreseededStrings` now owned by
    // this thread's PRESEEDED cell, which is not written again until the POST
    // copy is released below (after C has stopped using it) and otherwise
    // lives until the thread exits — after `ephpm_thread_shutdown` has run
    // (worker_thread_shutdown is called explicitly on the thread body, before
    // TLS teardown). `post_data`/`post_len` describe the same Vec (or NULL/0).
    // The C function registers the calling thread with TSRM and runs
    // php_request_startup(), which wraps activation in PHP's own zend_try, so
    // no longjmp escapes into this frame.
    let status = unsafe {
        crate::ffi::ephpm_thread_init_preseeded(
            ptrs.method,
            ptrs.uri,
            ptrs.query_string,
            ptrs.content_type,
            ptrs.cookie_data,
            content_length,
            ptrs.path_translated,
            ptrs.post_data,
            ptrs.post_len,
        )
    };
    // The C side drops its read_post pointers before returning and PHP keeps
    // its own copy of the body (SG(request_info).request_body), so the POST
    // copy is no longer borrowed: free it now rather than hold it for the
    // thread's life (which, for a session parked in a debugger, is unbounded).
    PRESEEDED.with(|cell| {
        if let Some(s) = cell.borrow_mut().as_mut() {
            s.post_body = None;
        }
    });
    status
}

// ── C-compatible request view ────────────────────────────────────────────

/// Borrowed view of the next request, filled by [`take_request`] and read by
/// the C `take_request` shim. **Layout must match `EphpmWorkerRequest` in
/// `ephpm_wrapper.c`.** All pointers borrow from the thread-local
/// [`CURRENT_REQUEST`] and stay valid until the next `take_request` call.
///
/// # SAFETY (Phase 1 lazy Envelope contract)
///
/// The C wrapper's Phase 1 fast path (`ephpm_wrapper.c` — lazy Envelope) also
/// depends on this exact lifetime: `take_request` stashes the header /
/// server-var pointer arrays into thread-local C state and returns without
/// materializing PHP arrays, so those pointers must stay valid until the
/// matching `send_response` completes on this worker thread. The invariant
/// holds because `CURRENT_REQUEST` is written on entry to `worker_take_request`
/// (`crates/ephpm-php/src/worker_bridge.rs::worker_take_request`, line ~530),
/// is only cleared / overwritten by the NEXT `worker_take_request` call on the
/// same thread (see `ephpm_worker_reset_request` in the C wrapper — the reset
/// runs synchronously at the top of the next take_request), and the
/// `CurrentRequest` fields (Vec / CString) are `#[allow(dead_code)]`
/// specifically to keep the compiler from freeing them while borrowed by C.
/// The `req_generation` counter provides a cross-iteration isolation check
/// for userland-stashed Envelopes.
#[cfg(php_linked)]
#[repr(C)]
pub struct EphpmWorkerRequest {
    /// HTTP method, null-terminated.
    pub method: *const c_char,
    /// `REQUEST_URI`, null-terminated.
    pub uri: *const c_char,
    /// Query string (no leading `?`), null-terminated.
    pub query_string: *const c_char,
    /// Raw `Cookie` header, null-terminated.
    pub cookie_data: *const c_char,
    /// `Content-Type`, null-terminated, or null.
    pub content_type: *const c_char,
    /// Raw body bytes, or null when empty (unset for streaming requests).
    pub body: *const c_char,
    /// Body length in bytes. For streaming requests this carries the declared
    /// `Content-Length` (so PHP's POST reader knows how much to expect); the
    /// bytes themselves arrive through `body_read`.
    pub body_len: usize,
    /// Non-zero when the body streams via the `body_read` op instead of `body`.
    pub body_streaming: c_int,

    /// Number of `$_SERVER` entries.
    pub server_var_count: usize,
    /// Array of `server_var_count` key pointers.
    pub server_var_keys: *const *const c_char,
    /// Array of `server_var_count` value pointers.
    pub server_var_vals: *const *const c_char,

    /// Number of HTTP header entries.
    pub header_count: usize,
    /// Array of `header_count` name pointers.
    pub header_keys: *const *const c_char,
    /// Array of `header_count` value pointers.
    pub header_vals: *const *const c_char,
}

/// Function pointer table handed to C so the native `\Ephpm\Worker\*`
/// functions can call into Rust. **Layout must match `EphpmWorkerOps` in
/// `ephpm_wrapper.c`.**
#[cfg(php_linked)]
#[repr(C)]
pub struct EphpmWorkerOps {
    /// Block for the next request. Returns 1 with `req` filled, or 0 for
    /// graceful shutdown.
    pub take_request: Option<unsafe extern "C" fn(req: *mut EphpmWorkerRequest) -> c_int>,
    /// Deliver the response. Headers pass as parallel (ptr, len) arrays of
    /// `header_count` entries, valid for the duration of the call (issue
    /// #134 — replaces the `"Name: Value\n"` pack + re-parse round-trip).
    pub send_response: Option<
        unsafe extern "C" fn(
            status: c_int,
            header_names: *const *const c_char,
            header_name_lens: *const usize,
            header_values: *const *const c_char,
            header_value_lens: *const usize,
            header_count: usize,
            body: *const c_char,
            body_len: usize,
        ),
    >,

    // ── Phase 3: streaming bodies ────────────────────────────────────────
    /// Read up to `cap` bytes of the incremental request body into `buf`.
    /// Returns bytes written (0 = EOF, negative = error). Blocks until data or
    /// EOF. Serves the in-memory body when the request was buffered.
    pub body_read: Option<unsafe extern "C" fn(buf: *mut c_char, cap: usize) -> isize>,
    /// Begin a streaming response (status + headers in the same
    /// parallel-array shape as `send_response`; body chunks follow).
    pub response_begin: Option<
        unsafe extern "C" fn(
            status: c_int,
            header_names: *const *const c_char,
            header_name_lens: *const usize,
            header_values: *const *const c_char,
            header_value_lens: *const usize,
            header_count: usize,
        ),
    >,
    /// Push one response body chunk (blocks on backpressure). Returns 0, or
    /// negative if the receiver/client is gone.
    pub response_chunk: Option<unsafe extern "C" fn(buf: *const c_char, len: usize) -> isize>,
    /// Finish the streaming response (close the body channel).
    pub response_end: Option<unsafe extern "C" fn()>,
}

// ── Thread-local per-worker state ────────────────────────────────────────

/// Owned, C-string-backed storage for the request currently borrowed by C.
/// Held for the whole iteration so the pointers in [`EphpmWorkerRequest`] stay
/// valid until the next `take_request` replaces it.
///
/// Several fields exist purely to own the backing memory that the `*_ptrs`
/// vectors (and the C `EphpmWorkerRequest`) borrow — they must not be dropped
/// while borrowed, hence `#[allow(dead_code)]`.
#[cfg(php_linked)]
#[allow(dead_code)]
struct CurrentRequest {
    // Owning CStrings — the pointer arrays below borrow from these.
    _method: CString,
    _uri: CString,
    _query: CString,
    _cookie: CString,
    _content_type: Option<CString>,
    // For buffered requests, `body` points into here; for streaming requests
    // this is empty and `body_len` carries the declared Content-Length.
    _body: Vec<u8>,
    body_len: usize,
    streaming: bool,
    _server_vars: Vec<CServerVar>,
    _header_keys: Vec<CString>,
    _header_vals: Vec<CString>,
    // Pointer arrays into the CString vecs (stable: the vecs are not mutated
    // while borrowed).
    server_key_ptrs: Vec<*const c_char>,
    server_val_ptrs: Vec<*const c_char>,
    header_key_ptrs: Vec<*const c_char>,
    header_val_ptrs: Vec<*const c_char>,
}

/// The request-body reader for the in-flight request. Serves `body_read` from
/// either an in-memory buffer (buffered dispatch) or a blocking chunk receiver
/// (streaming dispatch). Never crosses a longjmp with a live lock — it lives in
/// a thread-local `RefCell` borrowed only for the duration of one `body_read`.
#[cfg(php_linked)]
enum BodyReader {
    /// No body / already drained.
    Empty,
    /// Buffered body with a read cursor.
    Buffered { data: Vec<u8>, offset: usize },
    /// Streaming body: a leftover partial chunk plus the bounded receiver. The
    /// worker thread is a plain OS thread (not on the tokio runtime), so
    /// `blocking_recv()` is valid here.
    Streaming { pending: bytes::Bytes, rx: tokio::sync::mpsc::Receiver<bytes::Bytes> },
}

/// The in-flight streaming-response sender (`send_response_stream`). Chunks the
/// worker produces are pushed here; the hyper handler drains it into a
/// `StreamBody`. Bounded — `response_chunk` uses `blocking_send`, giving
/// download backpressure. Longjmp-safe: the `Sender`'s `Drop` just signals the
/// receiver (no files/locks).
#[cfg(php_linked)]
type ResponseChunkTx = tokio::sync::mpsc::Sender<bytes::Bytes>;

// Per-worker-thread state for the `ephpm_worker_*` natives.
//
// These deliberately keep plain `LocalKey::with`, unlike the `db_bridge` /
// `kv_bridge` / `ws_bridge` cells that issue #269 converted to `try_with`.
// The #269 hazard is `ThreadPhpGuard`'s TLS destructor running
// `php_request_shutdown()` — and therefore userland shutdown functions —
// after these cells are gone. A worker thread never retires that way: it
// retires through `PhpRuntime::worker_thread_shutdown`, which disarms the
// guard and calls `ephpm_thread_shutdown()` itself, as ordinary code on a live
// thread where every cell below is still alive. `worker_pool` and `fpm_pool`
// are the only callers and both do exactly that.
#[cfg(php_linked)]
thread_local! {
    /// Where this worker thread's jobs come from. Installed by the pool
    /// ([`set_dispatch_worker`]) or the debug lane ([`set_dispatch_receiver`])
    /// before the worker boots.
    static DISPATCH: RefCell<Option<DispatchSource>> = const { RefCell::new(None) };

    /// The parked `oneshot::Sender` for the in-flight request. Stashed by
    /// `take_request`, taken by `send_response` (or by the supervisor on
    /// bailout). This is the only Rust value live across the PHP call.
    static PENDING_SENDER: RefCell<Option<tokio::sync::oneshot::Sender<WorkerResponse>>> =
        const { RefCell::new(None) };

    /// Backing storage for the request C currently borrows.
    static CURRENT_REQUEST: RefCell<Option<CurrentRequest>> = const { RefCell::new(None) };

    /// The incremental request-body reader for the in-flight request. Consumed
    /// by `body_read` (drives both read_post and bodyStream()).
    static BODY_READER: RefCell<BodyReader> = const { RefCell::new(BodyReader::Empty) };

    /// The in-flight streaming-response chunk sender (`send_response_stream`).
    static RESPONSE_TX: RefCell<Option<ResponseChunkTx>> = const { RefCell::new(None) };

    /// The in-flight streaming response's [`StreamAbortFlag`]. Present only
    /// between `response_begin` and `response_end`; cleared on a clean finish
    /// so a later bailout cannot retroactively abort a completed stream.
    static RESPONSE_ABORT: RefCell<Option<StreamAbortFlag>> = const { RefCell::new(None) };

    /// Requests handled by this worker since boot (drives `[php.worker] max_requests`).
    static REQUESTS_HANDLED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// The recycle threshold for this worker (`0` = never recycle).
    static MAX_REQUESTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// Fired exactly once, on this worker's first `take_request()` — the
    /// framework-finished-booting signal. The pool uses it to mark readiness
    /// and record the true boot duration (`run_worker` itself blocks for the
    /// worker's entire life, so it can't distinguish boot from serving).
    static BOOT_NOTIFIER: RefCell<Option<Box<dyn FnOnce() + Send>>> =
        const { RefCell::new(None) };

    /// Ceiling on how long `response_chunk` waits for the client to drain the
    /// streaming-response channel. Without it, a client that stops reading
    /// mid-download pins this worker thread in `blocking_send` forever — N
    /// such clients wedge the whole pool undetected.
    static STREAM_SEND_TIMEOUT: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(std::time::Duration::from_secs(60)) };
}

/// How long an idle worker polls its mailbox before parking.
///
/// Under load the next job usually arrives within microseconds of a worker
/// finishing the previous one. Parking straight away costs, per idle gap, a
/// futex sleep, the dispatcher's futex wake, and then the scheduler latency
/// until the woken thread runs again, which on a busy host is the dominant
/// term. A short bounded poll catches the next job without any of that; an
/// idle server pays at most this much CPU per request before the worker parks.
/// The poll reads only this worker's own mailbox
/// ([`handoff::Worker::recv`](crate::handoff::Worker::recv)), so polling
/// workers do not contend with each other or with the dispatcher.
///
/// 200 µs was chosen by measurement (Yii3 `/`, 20 workers, 32 vCPUs): it
/// raised saturated throughput ~4% over parking immediately, more than 50 µs
/// did. Most of the poll yields the CPU rather than burning it — but on an
/// idle host `sched_yield` returns at once, so each request that leaves its
/// worker idle costs up to this much CPU before the park. Under a tight cgroup
/// CPU quota at low request rates that is the price of the lower wake latency
/// under load.
#[cfg_attr(not(php_linked), allow(dead_code))]
const DISPATCH_SPIN: std::time::Duration = std::time::Duration::from_micros(200);

/// A worker thread's job source.
#[cfg(php_linked)]
enum DispatchSource {
    /// A pool worker's mailbox on the pool's per-worker [`handoff`](crate::handoff)
    /// dispatcher.
    Handoff(crate::handoff::Worker<WorkerJob>),
    /// The debug lane's one-job channel (closed right after its job is sent).
    Channel(async_channel::Receiver<WorkerJob>),
}

#[cfg(php_linked)]
impl DispatchSource {
    fn recv(&self) -> Option<WorkerJob> {
        match self {
            Self::Handoff(worker) => worker.recv(DISPATCH_SPIN).ok(),
            Self::Channel(rx) => recv_spin_then_block(rx, DISPATCH_SPIN).ok(),
        }
    }
}

/// Receive the next dispatched job: poll `rx` for up to `spin`, then park in
/// `recv_blocking`.
///
/// The first few polls are pure CPU spins; after that each poll yields the
/// CPU (`sched_yield`), so on an oversubscribed host the wait hands the core
/// to whichever thread (a tokio worker about to enqueue the next job,
/// typically) is runnable. A closed channel returns immediately, exactly as
/// `recv_blocking` does — drain is not delayed.
///
/// # Errors
///
/// [`async_channel::RecvError`] once the channel is closed and empty.
#[cfg_attr(not(php_linked), allow(dead_code))]
pub(crate) fn recv_spin_then_block<T>(
    rx: &async_channel::Receiver<T>,
    spin: std::time::Duration,
) -> Result<T, async_channel::RecvError> {
    const PURE_SPINS: u32 = 32;
    let mut started: Option<std::time::Instant> = None;
    let mut polls: u32 = 0;
    loop {
        match rx.try_recv() {
            Ok(job) => return Ok(job),
            Err(async_channel::TryRecvError::Closed) => return Err(async_channel::RecvError),
            Err(async_channel::TryRecvError::Empty) => {}
        }
        // Only consult the clock once the fast path has missed.
        let start = *started.get_or_insert_with(std::time::Instant::now);
        if start.elapsed() >= spin {
            return rx.recv_blocking();
        }
        if polls < PURE_SPINS {
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
        polls = polls.saturating_add(1);
    }
}

/// Install a one-job dispatch channel as the current worker thread's job
/// source (the debug lane). Called immediately before booting the worker.
#[cfg(php_linked)]
pub fn set_dispatch_receiver(rx: async_channel::Receiver<WorkerJob>) {
    DISPATCH.with(|cell| *cell.borrow_mut() = Some(DispatchSource::Channel(rx)));
}

/// Stub: no dispatch channel without PHP linked.
#[cfg(not(php_linked))]
pub fn set_dispatch_receiver(_rx: async_channel::Receiver<WorkerJob>) {}

/// Install the current worker thread's mailbox on the pool's dispatcher as its
/// job source. `worker` must have been registered on this thread
/// ([`Dispatcher::register_worker`](crate::handoff::Dispatcher::register_worker)).
/// Called by the pool immediately before booting the worker.
#[cfg(php_linked)]
pub fn set_dispatch_worker(worker: crate::handoff::Worker<WorkerJob>) {
    DISPATCH.with(|cell| *cell.borrow_mut() = Some(DispatchSource::Handoff(worker)));
}

/// Stub: no dispatch without PHP linked.
#[cfg(not(php_linked))]
pub fn set_dispatch_worker(_worker: crate::handoff::Worker<WorkerJob>) {}

/// Drop the current worker thread's job source. The pool calls this when the
/// worker leaves its loop, so the thread's mailbox is unregistered (and
/// anything left in it re-routed — see [`crate::handoff`]) before the thread
/// spends time in PHP teardown.
#[cfg(php_linked)]
pub fn clear_dispatch_source() {
    let source = DISPATCH.with(|cell| cell.borrow_mut().take());
    drop(source);
}

/// Stub.
#[cfg(not(php_linked))]
pub fn clear_dispatch_source() {}

/// Set the per-worker recycle threshold (`[php.worker] max_requests`; `0` disables).
#[cfg(php_linked)]
pub fn set_max_requests(max: u64) {
    MAX_REQUESTS.with(|c| c.set(max));
    REQUESTS_HANDLED.with(|c| c.set(0));
}

/// Stub.
#[cfg(not(php_linked))]
pub fn set_max_requests(_max: u64) {}

/// Requests handled by this worker thread since boot. `0` on a fresh worker
/// (or in stub mode). Used by the pool supervisor to log recycle events with
/// the number of requests actually served.
#[cfg(php_linked)]
#[must_use]
pub fn requests_handled() -> u64 {
    REQUESTS_HANDLED.with(std::cell::Cell::get)
}

/// Stub: no PHP linked, no worker requests.
#[cfg(not(php_linked))]
#[must_use]
pub fn requests_handled() -> u64 {
    0
}

/// Install the boot-completion callback for the current worker thread. Fired
/// exactly once, on the worker's first `take_request()` — i.e. after the
/// framework finished booting and is ready to serve. Install before
/// `run_worker`.
#[cfg(php_linked)]
pub fn set_boot_notifier(f: Box<dyn FnOnce() + Send>) {
    BOOT_NOTIFIER.with(|cell| *cell.borrow_mut() = Some(f));
}

/// Stub: never fires (stub `run_worker` errors before any `take_request`).
#[cfg(not(php_linked))]
pub fn set_boot_notifier(_f: Box<dyn FnOnce() + Send>) {}

/// Set this worker thread's streaming-response send timeout: how long
/// `response_chunk` waits for a slow/stalled client before aborting the
/// stream (the worker sees "client gone" and stops producing). Install
/// before `run_worker`.
#[cfg(php_linked)]
pub fn set_stream_send_timeout(timeout: std::time::Duration) {
    STREAM_SEND_TIMEOUT.with(|c| c.set(timeout));
}

/// Stub.
#[cfg(not(php_linked))]
pub fn set_stream_send_timeout(_timeout: std::time::Duration) {}

/// Take the parked `oneshot::Sender` for the in-flight request, if one is
/// still stashed. Used by the pool's crash-recovery path: after
/// `ephpm_worker_run` returns following a bailout, a still-present sender means
/// the response was never sent, so the pool fulfils it with a 500.
#[cfg(php_linked)]
#[must_use]
pub fn take_pending_sender() -> Option<tokio::sync::oneshot::Sender<WorkerResponse>> {
    PENDING_SENDER.with(|cell| cell.borrow_mut().take())
}

/// Stub.
#[cfg(not(php_linked))]
#[must_use]
pub fn take_pending_sender() -> Option<tokio::sync::oneshot::Sender<WorkerResponse>> {
    None
}

/// Tear down any in-flight streaming state left over after a bailout, marking
/// an unfinished response as aborted so it cannot be mistaken for a complete
/// one. Call from the pool's crash-recovery path before the worker
/// resumes/respawns. Idempotent.
///
/// Order matters: the [`StreamAbortFlag`] is set **before** the chunk sender is
/// dropped, because dropping the sender is what closes the channel, and the
/// body stream only reads the flag after it observes that close. Setting it
/// first is what makes the flag visible in time.
///
/// Returns `true` if a still-open streamed body was aborted — i.e. the response
/// headers were already on the wire, so the client is receiving a deliberately
/// broken transfer rather than a 500.
///
/// `CURRENT_REQUEST` is NOT cleared: `worker_thread_shutdown()` runs after
/// this and its `php_request_shutdown` may still read the `SG(request_info)`
/// pointers that borrow from it. The thread-local drops at thread exit.
#[cfg(php_linked)]
#[must_use]
pub fn clear_in_flight_streams() -> bool {
    let aborted = RESPONSE_ABORT.with(|cell| cell.borrow_mut().take());
    if let Some(flag) = &aborted {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let had_stream = RESPONSE_TX.with(|cell| cell.borrow_mut().take()).is_some();
    BODY_READER.with(|cell| *cell.borrow_mut() = BodyReader::Empty);
    had_stream && aborted.is_some()
}

/// Stub: no worker streams exist without PHP linked.
#[cfg(not(php_linked))]
#[must_use]
pub fn clear_in_flight_streams() -> bool {
    false
}

// ── Callback implementations ─────────────────────────────────────────────

/// Build the C-borrowed request backing storage from an owned job, consuming
/// its body into a [`BodyReader`] (returned separately so the caller can stash
/// it in [`BODY_READER`]). The `_body` `Vec` in the returned struct is empty
/// for streaming requests — their bytes flow through `body_read`.
#[cfg(php_linked)]
fn build_current_request(job: WorkerRequestOwned) -> (CurrentRequest, BodyReader) {
    // Lossy on interior NULs (extremely rare in HTTP metadata) — replace with
    // an empty string rather than fail the request.
    let cstr = |s: &str| CString::new(s).unwrap_or_default();

    let method = cstr(&job.method);
    let uri = cstr(&job.uri);
    let query = cstr(&job.query_string);
    let cookie = cstr(&job.cookie_data);
    let content_type = job.content_type.as_deref().map(cstr);

    // $_SERVER arrives already FFI-ready (issue #133) — no re-allocation, just
    // pointer arrays into the moved-in storage.
    let server_vars = job.server_vars;
    let header_keys: Vec<CString> = job.headers.iter().map(|(k, _)| cstr(k)).collect();
    let header_vals: Vec<CString> = job.headers.iter().map(|(_, v)| cstr(v)).collect();

    let server_key_ptrs: Vec<*const c_char> = server_vars.iter().map(|(k, _)| k.as_ptr()).collect();
    let server_val_ptrs: Vec<*const c_char> = server_vars.iter().map(|(_, v)| v.as_ptr()).collect();
    let header_key_ptrs: Vec<*const c_char> = header_keys.iter().map(|c| c.as_ptr()).collect();
    let header_val_ptrs: Vec<*const c_char> = header_vals.iter().map(|c| c.as_ptr()).collect();

    let (body_vec, body_len, streaming, reader) = match job.body {
        WorkerBody::Buffered(data) => {
            let len = data.len();
            // The C borrow reads from the CurrentRequest._body copy; the reader
            // owns its own copy so read_post/bodyStream stay consistent.
            let reader = if len == 0 {
                BodyReader::Empty
            } else {
                BodyReader::Buffered { data: data.clone(), offset: 0 }
            };
            (data, len, false, reader)
        }
        WorkerBody::Streaming { rx, declared_len } => (
            Vec::new(),
            declared_len,
            true,
            BodyReader::Streaming { pending: bytes::Bytes::new(), rx },
        ),
    };

    let current = CurrentRequest {
        _method: method,
        _uri: uri,
        _query: query,
        _cookie: cookie,
        _content_type: content_type,
        _body: body_vec,
        body_len,
        streaming,
        _server_vars: server_vars,
        _header_keys: header_keys,
        _header_vals: header_vals,
        server_key_ptrs,
        server_val_ptrs,
        header_key_ptrs,
        header_val_ptrs,
    };
    (current, reader)
}

/// C-callable `take_request`: block for the next job, stash its sender, and
/// fill the borrowed request view.
#[cfg(php_linked)]
unsafe extern "C" fn worker_take_request(req: *mut EphpmWorkerRequest) -> c_int {
    // First call on this thread = the framework finished booting. Fire the
    // pool's boot notifier (readiness, boot-duration metric, backoff reset).
    if let Some(notify) = BOOT_NOTIFIER.with(|cell| cell.borrow_mut().take()) {
        notify();
    }

    // Cooperative recycle: once this worker has handled its quota, return
    // shutdown so the framework loop exits and the pool respawns a fresh boot.
    let max = MAX_REQUESTS.with(std::cell::Cell::get);
    if max > 0 && REQUESTS_HANDLED.with(std::cell::Cell::get) >= max {
        return 0;
    }

    if DISPATCH.with(|cell| cell.borrow().is_none()) {
        // No job source installed — nothing can ever arrive; treat as shutdown.
        return 0;
    }

    // Framework terminate hooks may run PHP (including ephpm_db_*) between
    // send_response and this call — after finish_iteration's per-request
    // sweep. Sweep again BEFORE parking in recv so a transaction a
    // terminate hook left open is rolled back now rather than held (with
    // its SQLite write lock) for however long this worker sits idle.
    // Idempotent and a cheap flag check when there is nothing to do.
    crate::db_bridge::on_request_end();

    // JIT buffer gauges: this thread sits inside its long-lived worker
    // request, which is exactly the context `opcache_get_status` needs, and
    // worker requests never pass through the router's fpm dispatch closure —
    // so worker mode samples here. Rate-limited process-wide; one relaxed
    // atomic load when it is not due.
    crate::jit_metrics::maybe_sample();

    // The worker is idle exactly while it waits in this recv. The source is
    // borrowed for the wait, not cloned or moved.
    metrics::gauge!("ephpm_worker_idle").increment(1.0);
    let recv = DISPATCH.with(|cell| {
        let source = cell.borrow();
        source.as_ref().expect("checked above; only replaced at thread start").recv()
    });
    metrics::gauge!("ephpm_worker_idle").decrement(1.0);
    let Some(job) = recv else {
        // Dispatch closed (graceful drain) — end the loop.
        return 0;
    };
    // Release the job's admission permit so the pool's FIFO admission
    // semaphore hands the freed queue slot to the longest-waiting dispatcher
    // (#442).
    drop(job.admission);

    // Stash the sender for send_response / crash recovery.
    PENDING_SENDER.with(|cell| *cell.borrow_mut() = Some(job.respond_to));

    // Any stale streaming-response sender from a prior iteration is dropped
    // here (closing its channel) so it can never bleed into this request. Its
    // abort flag goes with it, unset — that iteration is over either way.
    RESPONSE_TX.with(|cell| *cell.borrow_mut() = None);
    RESPONSE_ABORT.with(|cell| *cell.borrow_mut() = None);

    // Build owned backing storage + the body reader, and publish the pointers.
    let (current, reader) = build_current_request(job.request);
    BODY_READER.with(|cell| *cell.borrow_mut() = reader);
    CURRENT_REQUEST.with(|cell| *cell.borrow_mut() = Some(current));

    CURRENT_REQUEST.with(|cell| {
        let borrow = cell.borrow();
        let cur = borrow.as_ref().expect("just set");
        // SAFETY: `req` is a valid, writable EphpmWorkerRequest provided by the
        // C shim. Every pointer we write borrows from `cur`, which lives in the
        // thread-local until the next take_request replaces it — outliving the
        // C use of these pointers (through send_response).
        unsafe {
            (*req).method = cur._method.as_ptr();
            (*req).uri = cur._uri.as_ptr();
            (*req).query_string = cur._query.as_ptr();
            (*req).cookie_data = cur._cookie.as_ptr();
            (*req).content_type =
                cur._content_type.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
            (*req).body = if cur.streaming || cur._body.is_empty() {
                std::ptr::null()
            } else {
                cur._body.as_ptr().cast::<c_char>()
            };
            (*req).body_len = cur.body_len;
            (*req).body_streaming = c_int::from(cur.streaming);
            (*req).server_var_count = cur.server_key_ptrs.len();
            (*req).server_var_keys = cur.server_key_ptrs.as_ptr();
            (*req).server_var_vals = cur.server_val_ptrs.as_ptr();
            (*req).header_count = cur.header_key_ptrs.len();
            (*req).header_keys = cur.header_key_ptrs.as_ptr();
            (*req).header_vals = cur.header_val_ptrs.as_ptr();
        }
    });

    1
}

/// C-callable `send_response`: fulfil the parked oneshot with the PHP response.
#[cfg(php_linked)]
unsafe extern "C" fn worker_send_response(
    status: c_int,
    header_names: *const *const c_char,
    header_name_lens: *const usize,
    header_values: *const *const c_char,
    header_value_lens: *const usize,
    header_count: usize,
    body: *const c_char,
    body_len: usize,
) {
    // SAFETY: C passes parallel arrays of `header_count` valid (ptr, len)
    // pairs, live for the duration of this call (null arrays mean zero
    // headers); same for the body buffer.
    let parsed_headers = unsafe {
        headers_from_raw(
            header_names,
            header_name_lens,
            header_values,
            header_value_lens,
            header_count,
        )
    };
    let body_bytes: Vec<u8> = if body.is_null() || body_len == 0 {
        Vec::new()
    } else {
        // SAFETY: see above — valid for body_len bytes during this call.
        unsafe { std::slice::from_raw_parts(body.cast::<u8>(), body_len) }.to_vec()
    };

    let status = u16::try_from(status).unwrap_or(200);
    let response = WorkerResponse::Buffered { status, headers: parsed_headers, body: body_bytes };

    // Deliver to the parked receiver. If it was already taken (shouldn't happen
    // on the normal path) or the receiver dropped, the response is discarded.
    if let Some(sender) = PENDING_SENDER.with(|cell| cell.borrow_mut().take()) {
        let _ = sender.send(response);
    }

    finish_iteration();
}

/// Common per-iteration teardown after a response is delivered (buffered or
/// streaming): bump the recycle counter and release the body reader so nothing
/// leaks into the next `take_request`.
///
/// `CURRENT_REQUEST` is deliberately NOT cleared here: the C statics
/// (`req_method`, ...) and `SG(request_info)` still point into its `CString`s,
/// and PHP code can run between `send_response()` and the next `take_request()`
/// (framework terminate hooks that call `header()`, which reads
/// `request_method`). The backing storage is dropped only when the next
/// `take_request` replaces it — while PHP is blocked inside that call — or at
/// thread exit.
#[cfg(php_linked)]
fn finish_iteration() {
    // Per-request DB-bridge teardown (worker-mode seam): runs on the
    // worker thread, inside send_response / response_end — i.e. at every
    // normal request end — so a transaction the script left open is rolled
    // back before the thread parks for its next request, and the staged
    // result/error memory is released. The crash path (fatal before
    // send_response) is covered by worker_thread_shutdown, and terminate
    // hooks that touch the DB after the response are swept at the top of
    // the next take_request.
    crate::db_bridge::on_request_end();

    REQUESTS_HANDLED.with(|c| c.set(c.get().saturating_add(1)));
    BODY_READER.with(|cell| *cell.borrow_mut() = BodyReader::Empty);
    // Dropping the streaming-response sender (if any) closes the body channel,
    // signalling end-of-body to the hyper handler.
    RESPONSE_TX.with(|cell| *cell.borrow_mut() = None);
    // The response finished on its own terms, so release the abort flag
    // WITHOUT setting it: a bailout later in this worker's life must not
    // retroactively break a stream that already completed.
    RESPONSE_ABORT.with(|cell| *cell.borrow_mut() = None);
}

/// C-callable `body_read`: fill `buf` (capacity `cap`) with the next bytes of
/// the incremental request body. Returns bytes written, 0 on EOF, negative on
/// error. Blocks on the streaming channel until data or EOF.
#[cfg(php_linked)]
unsafe extern "C" fn worker_body_read(buf: *mut c_char, cap: usize) -> isize {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    // SAFETY: C guarantees `buf` points to at least `cap` writable bytes for
    // the duration of this call. We only write `n <= cap` bytes into it.
    let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), cap) };

    BODY_READER.with(|cell| {
        let mut reader = cell.borrow_mut();
        match &mut *reader {
            BodyReader::Empty => 0,
            BodyReader::Buffered { data, offset } => {
                let remaining = data.len().saturating_sub(*offset);
                if remaining == 0 {
                    return 0;
                }
                let n = remaining.min(cap);
                out[..n].copy_from_slice(&data[*offset..*offset + n]);
                *offset += n;
                isize::try_from(n).unwrap_or(isize::MAX)
            }
            BodyReader::Streaming { pending, rx } => {
                // Refill from the channel when the leftover partial chunk is
                // empty. blocking_recv() blocks until a chunk arrives or the
                // sender closes (EOF). This is the hyper->worker backpressure
                // point (the worker is a plain OS thread, so blocking_recv is
                // valid).
                if pending.is_empty() {
                    match rx.blocking_recv() {
                        Some(chunk) => *pending = chunk,
                        None => return 0, // sender closed => clean EOF
                    }
                }
                let n = pending.len().min(cap);
                out[..n].copy_from_slice(&pending[..n]);
                // Advance past the copied bytes without reallocating.
                let _ = pending.split_to(n);
                isize::try_from(n).unwrap_or(isize::MAX)
            }
        }
    })
}

/// C-callable `response_begin`: open a streaming response. Builds a bounded
/// chunk channel, sends the [`WorkerResponse::Streaming`] header to the parked
/// oneshot immediately (so hyper can start the response), and stashes the
/// sender for the `response_chunk` calls that follow.
#[cfg(php_linked)]
unsafe extern "C" fn worker_response_begin(
    status: c_int,
    header_names: *const *const c_char,
    header_name_lens: *const usize,
    header_values: *const *const c_char,
    header_value_lens: *const usize,
    header_count: usize,
) {
    // SAFETY: C passes parallel arrays of `header_count` valid (ptr, len)
    // pairs, live for the duration of this call (null arrays => none).
    let parsed_headers = unsafe {
        headers_from_raw(
            header_names,
            header_name_lens,
            header_values,
            header_value_lens,
            header_count,
        )
    };
    let status = u16::try_from(status).unwrap_or(200);

    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(BODY_CHANNEL_DEPTH);
    RESPONSE_TX.with(|cell| *cell.borrow_mut() = Some(tx));

    // From here on the status line and headers are unretractable, so the only
    // way to signal a mid-stream death is to break the body framing. Both ends
    // of that signal are wired up now: the flag goes to the hyper body stream,
    // and a clone stays on this thread for clear_in_flight_streams().
    let aborted: StreamAbortFlag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    RESPONSE_ABORT.with(|cell| *cell.borrow_mut() = Some(std::sync::Arc::clone(&aborted)));

    if let Some(sender) = PENDING_SENDER.with(|cell| cell.borrow_mut().take()) {
        let _ = sender.send(WorkerResponse::Streaming {
            status,
            headers: parsed_headers,
            body_rx: rx,
            aborted,
        });
    }
}

/// C-callable `response_chunk`: push one body chunk. Blocks on the bounded
/// channel (download backpressure), but only up to the per-worker
/// [`STREAM_SEND_TIMEOUT`]. Returns 0, or -1 if the receiver is gone or the
/// client made no progress within the timeout.
///
/// The timeout matters: once a streaming response's headers are delivered,
/// the router's hung-worker net (oneshot timeout -> `note_hung`) can never
/// fire again for this request — an unbounded `blocking_send` here would let
/// a client that stops reading pin this worker OS thread forever.
#[cfg(php_linked)]
unsafe extern "C" fn worker_response_chunk(buf: *const c_char, len: usize) -> isize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    // SAFETY: C passes a valid (ptr, len) for the chunk, live for this call.
    let chunk = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) };
    let mut bytes = bytes::Bytes::copy_from_slice(chunk);

    let timeout = STREAM_SEND_TIMEOUT.with(std::cell::Cell::get);
    let deadline = std::time::Instant::now() + timeout;

    RESPONSE_TX.with(|cell| {
        let borrow = cell.borrow();
        let Some(tx) = borrow.as_ref() else {
            return -1;
        };
        // try_send + bounded sleep instead of blocking_send: same backpressure
        // (the channel depth still caps in-flight bytes), but with an escape
        // hatch for a stalled client. The sleep only runs while the channel is
        // full — the normal path is a single successful try_send.
        loop {
            match tx.try_send(bytes) {
                Ok(()) => return 0,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return -1; // receiver dropped (client gone)
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(b)) => {
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            timeout_secs = timeout.as_secs(),
                            "streaming response stalled (client not reading) — aborting stream"
                        );
                        metrics::counter!("ephpm_worker_stream_stalls_total").increment(1);
                        return -1;
                    }
                    bytes = b;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    })
}

/// C-callable `response_end`: finish the streaming response and complete the
/// iteration. Dropping the sender closes the channel (end-of-body).
#[cfg(php_linked)]
unsafe extern "C" fn worker_response_end() {
    finish_iteration();
}

/// Copy the parallel (ptr, len) header arrays C hands to `send_response` /
/// `response_begin` into owned `(name, value)` pairs (issue #134 — replaces
/// parsing a `"Name: Value\n"` pack).
///
/// Names and values are trimmed of ASCII whitespace, matching what the old
/// pack-then-parse round trip produced, so downstream header handling sees
/// identical strings.
///
/// # Safety
///
/// Either all four arrays are non-null with at least `count` valid entries,
/// each entry a pointer valid for its recorded length for the duration of
/// the call — or `count` is 0 (null arrays allowed then).
#[cfg(php_linked)]
unsafe fn headers_from_raw(
    names: *const *const c_char,
    name_lens: *const usize,
    values: *const *const c_char,
    value_lens: *const usize,
    count: usize,
) -> Vec<(String, String)> {
    if count == 0
        || names.is_null()
        || name_lens.is_null()
        || values.is_null()
        || value_lens.is_null()
    {
        return Vec::new();
    }
    // SAFETY: per the function contract, each array has `count` entries.
    let (names, name_lens, values, value_lens) = unsafe {
        (
            std::slice::from_raw_parts(names, count),
            std::slice::from_raw_parts(name_lens, count),
            std::slice::from_raw_parts(values, count),
            std::slice::from_raw_parts(value_lens, count),
        )
    };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        if names[i].is_null() || values[i].is_null() {
            continue;
        }
        // SAFETY: per the function contract, each pointer is valid for its
        // recorded length.
        let (name, value) = unsafe {
            (
                std::slice::from_raw_parts(names[i].cast::<u8>(), name_lens[i]),
                std::slice::from_raw_parts(values[i].cast::<u8>(), value_lens[i]),
            )
        };
        out.push((
            String::from_utf8_lossy(name).trim().to_string(),
            String::from_utf8_lossy(value).trim().to_string(),
        ));
    }
    out
}

// ── Static ops table ─────────────────────────────────────────────────────

/// The C-compatible ops table, ready to pass to `ephpm_set_worker_ops()`.
#[cfg(php_linked)]
pub static WORKER_OPS: EphpmWorkerOps = EphpmWorkerOps {
    take_request: Some(worker_take_request),
    send_response: Some(worker_send_response),
    body_read: Some(worker_body_read),
    response_begin: Some(worker_response_begin),
    response_chunk: Some(worker_response_chunk),
    response_end: Some(worker_response_end),
};

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(all(test, php_linked))]
mod tests {
    use super::*;

    /// Build the parallel arrays from Rust-owned strings and round-trip them
    /// through [`headers_from_raw`] — the same shapes the C side produces.
    fn from_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let names: Vec<*const c_char> = pairs.iter().map(|(n, _)| n.as_ptr().cast()).collect();
        let name_lens: Vec<usize> = pairs.iter().map(|(n, _)| n.len()).collect();
        let values: Vec<*const c_char> = pairs.iter().map(|(_, v)| v.as_ptr().cast()).collect();
        let value_lens: Vec<usize> = pairs.iter().map(|(_, v)| v.len()).collect();
        // SAFETY: the arrays all have `pairs.len()` entries pointing into
        // `pairs`' string data, which outlives the call.
        unsafe {
            headers_from_raw(
                names.as_ptr(),
                name_lens.as_ptr(),
                values.as_ptr(),
                value_lens.as_ptr(),
                pairs.len(),
            )
        }
    }

    #[test]
    fn headers_from_raw_basic() {
        let parsed = from_pairs(&[("Content-Type", "text/plain"), ("X-Foo", "bar")]);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ("Content-Type".to_string(), "text/plain".to_string()));
        assert_eq!(parsed[1], ("X-Foo".to_string(), "bar".to_string()));
    }

    #[test]
    fn headers_from_raw_trims_like_the_old_parse() {
        // The old "Name: Value\n" round trip trimmed both halves; header
        // handling downstream must keep seeing identical strings.
        let parsed = from_pairs(&[(" X-Padded ", "  spaced out  ")]);
        assert_eq!(parsed, vec![("X-Padded".to_string(), "spaced out".to_string())]);
    }

    #[test]
    fn headers_from_raw_null_arrays_mean_no_headers() {
        // SAFETY: count 0 permits null arrays per the contract.
        let parsed = unsafe {
            headers_from_raw(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        };
        assert!(parsed.is_empty());
    }

    #[test]
    fn headers_from_raw_preserves_value_with_newline() {
        // The packed form could not carry a value containing '\n' (it broke
        // the line framing and the remainder was dropped or mis-parsed);
        // structured passing keeps the bytes intact.
        let parsed = from_pairs(&[("X-Odd", "line1\nline2")]);
        assert_eq!(parsed, vec![("X-Odd".to_string(), "line1\nline2".to_string())]);
    }

    #[test]
    fn internal_error_is_500() {
        match WorkerResponse::internal_error() {
            WorkerResponse::Buffered { status, body, .. } => {
                assert_eq!(status, 500);
                assert_eq!(body, b"Internal Server Error");
            }
            WorkerResponse::Streaming { .. } => panic!("internal_error must be buffered"),
        }
    }
}

/// [`recv_spin_then_block`] — no PHP needed, so these run in stub mode too.
#[cfg(test)]
mod dispatch_recv_tests {
    use std::time::{Duration, Instant};

    use super::recv_spin_then_block;

    #[test]
    fn returns_a_queued_job_immediately() {
        let (tx, rx) = async_channel::bounded(4);
        tx.try_send(7u32).unwrap();
        assert_eq!(recv_spin_then_block(&rx, Duration::from_secs(10)), Ok(7));
    }

    #[test]
    fn picks_up_a_job_that_arrives_while_spinning() {
        let (tx, rx) = async_channel::bounded(4);
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            tx.send_blocking(1u32).unwrap();
            tx // keep the channel open until the receiver is done
        });
        assert_eq!(recv_spin_then_block(&rx, Duration::from_secs(10)), Ok(1));
        drop(sender.join().unwrap());
    }

    #[test]
    fn parks_after_the_spin_and_still_receives() {
        let (tx, rx) = async_channel::bounded(4);
        let sender = std::thread::spawn(move || {
            // Well past the 1 ms spin: the receiver is parked in recv_blocking.
            std::thread::sleep(Duration::from_millis(50));
            tx.send_blocking(2u32).unwrap();
            tx
        });
        assert_eq!(recv_spin_then_block(&rx, Duration::from_millis(1)), Ok(2));
        drop(sender.join().unwrap());
    }

    #[test]
    fn a_closed_channel_ends_the_wait_without_finishing_the_spin() {
        let (tx, rx) = async_channel::bounded::<u32>(4);
        tx.close();
        let t = Instant::now();
        assert_eq!(
            recv_spin_then_block(&rx, Duration::from_secs(30)),
            Err(async_channel::RecvError)
        );
        assert!(t.elapsed() < Duration::from_secs(5), "drain must not wait out the spin");

        // Closed while already parked (past the spin) — recv_blocking's own
        // close handling.
        let (tx, rx) = async_channel::bounded::<u32>(4);
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            tx.close();
        });
        assert_eq!(
            recv_spin_then_block(&rx, Duration::from_millis(1)),
            Err(async_channel::RecvError)
        );
        closer.join().unwrap();
    }

    #[test]
    fn queued_jobs_still_drain_after_close() {
        let (tx, rx) = async_channel::bounded(4);
        tx.try_send(3u32).unwrap();
        tx.close();
        assert_eq!(recv_spin_then_block(&rx, Duration::from_millis(1)), Ok(3));
        assert_eq!(
            recv_spin_then_block(&rx, Duration::from_millis(1)),
            Err(async_channel::RecvError)
        );
    }
}

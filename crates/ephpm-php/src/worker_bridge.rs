//! Bridge between the C worker-mode PHP functions and the Rust dispatch
//! channel (persistent-worker engine, `worker-mode-design.md`).
//!
//! Mirrors the [`kv_bridge`](crate::kv_bridge) ops-table pattern. The C
//! `\Ephpm\Worker\take_request()` / `send_response()` native functions call
//! into the [`EphpmWorkerOps`] table set via [`ephpm_set_worker_ops`]:
//!
//! - `take_request` blocks on the per-thread dispatch [`Receiver`], stashes the
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
#[derive(Debug)]
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
    /// `$_SERVER`-shaped variables as `(key, value)` pairs.
    pub server_vars: Vec<(String, String)>,
    /// HTTP headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
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
    },
}

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
}

// ── C-compatible request view ────────────────────────────────────────────

/// Borrowed view of the next request, filled by [`take_request`] and read by
/// the C `take_request` shim. **Layout must match `EphpmWorkerRequest` in
/// `ephpm_wrapper.c`.** All pointers borrow from the thread-local
/// [`CURRENT_REQUEST`] and stay valid until the next `take_request` call.
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
    /// Deliver the response. `headers` is `"Name: Value\n"` packed.
    pub send_response: Option<
        unsafe extern "C" fn(
            status: c_int,
            headers: *const c_char,
            headers_len: usize,
            body: *const c_char,
            body_len: usize,
        ),
    >,

    // ── Phase 3: streaming bodies ────────────────────────────────────────
    /// Read up to `cap` bytes of the incremental request body into `buf`.
    /// Returns bytes written (0 = EOF, negative = error). Blocks until data or
    /// EOF. Serves the in-memory body when the request was buffered.
    pub body_read: Option<unsafe extern "C" fn(buf: *mut c_char, cap: usize) -> isize>,
    /// Begin a streaming response (status + packed headers, body chunks follow).
    pub response_begin:
        Option<unsafe extern "C" fn(status: c_int, headers: *const c_char, headers_len: usize)>,
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
    _server_keys: Vec<CString>,
    _server_vals: Vec<CString>,
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

#[cfg(php_linked)]
thread_local! {
    /// The dispatch receiver for this worker thread. Installed by the pool via
    /// [`set_dispatch_receiver`] before the worker boots.
    static DISPATCH_RX: RefCell<Option<async_channel::Receiver<WorkerJob>>> =
        const { RefCell::new(None) };

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

    /// Requests handled by this worker since boot (drives `worker_max_requests`).
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

/// Install the dispatch receiver for the current worker thread. Called by the
/// pool immediately before booting the worker.
#[cfg(php_linked)]
pub fn set_dispatch_receiver(rx: async_channel::Receiver<WorkerJob>) {
    DISPATCH_RX.with(|cell| *cell.borrow_mut() = Some(rx));
}

/// Stub: no dispatch channel without PHP linked.
#[cfg(not(php_linked))]
pub fn set_dispatch_receiver(_rx: async_channel::Receiver<WorkerJob>) {}

/// Set the per-worker recycle threshold (`worker_max_requests`; `0` disables).
#[cfg(php_linked)]
pub fn set_max_requests(max: u64) {
    MAX_REQUESTS.with(|c| c.set(max));
    REQUESTS_HANDLED.with(|c| c.set(0));
}

/// Stub.
#[cfg(not(php_linked))]
pub fn set_max_requests(_max: u64) {}

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

/// Drop any in-flight streaming state left over after a bailout: the response
/// chunk sender (closing its channel signals end-of-body to the hyper handler,
/// so a client mid-download gets a truncated-but-not-hung response) and the
/// body reader. Call from the pool's crash-recovery path before the worker
/// resumes/respawns. Idempotent.
///
/// `CURRENT_REQUEST` is NOT cleared: `worker_thread_shutdown()` runs after
/// this and its `php_request_shutdown` may still read the `SG(request_info)`
/// pointers that borrow from it. The thread-local drops at thread exit.
#[cfg(php_linked)]
pub fn clear_in_flight_streams() {
    RESPONSE_TX.with(|cell| *cell.borrow_mut() = None);
    BODY_READER.with(|cell| *cell.borrow_mut() = BodyReader::Empty);
}

/// Stub.
#[cfg(not(php_linked))]
pub fn clear_in_flight_streams() {}

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

    let server_keys: Vec<CString> = job.server_vars.iter().map(|(k, _)| cstr(k)).collect();
    let server_vals: Vec<CString> = job.server_vars.iter().map(|(_, v)| cstr(v)).collect();
    let header_keys: Vec<CString> = job.headers.iter().map(|(k, _)| cstr(k)).collect();
    let header_vals: Vec<CString> = job.headers.iter().map(|(_, v)| cstr(v)).collect();

    let server_key_ptrs: Vec<*const c_char> = server_keys.iter().map(|c| c.as_ptr()).collect();
    let server_val_ptrs: Vec<*const c_char> = server_vals.iter().map(|c| c.as_ptr()).collect();
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
        _server_keys: server_keys,
        _server_vals: server_vals,
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

    let Some(rx) = DISPATCH_RX.with(|cell| cell.borrow().clone()) else {
        // No receiver installed — nothing can ever arrive; treat as shutdown.
        return 0;
    };

    // The worker is idle exactly while it is parked in this recv.
    metrics::gauge!("ephpm_worker_idle").increment(1.0);
    let recv = rx.recv_blocking();
    metrics::gauge!("ephpm_worker_idle").decrement(1.0);
    let job = match recv {
        Ok(job) => job,
        // Sender side closed (graceful drain) — end the loop.
        Err(_) => return 0,
    };

    // Stash the sender for send_response / crash recovery.
    PENDING_SENDER.with(|cell| *cell.borrow_mut() = Some(job.respond_to));

    // Any stale streaming-response sender from a prior iteration is dropped
    // here (closing its channel) so it can never bleed into this request.
    RESPONSE_TX.with(|cell| *cell.borrow_mut() = None);

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
    headers: *const c_char,
    headers_len: usize,
    body: *const c_char,
    body_len: usize,
) {
    // SAFETY: C passes valid (ptr, len) pairs for headers and body; the buffers
    // live for the duration of this call. A null pointer means zero length.
    let header_bytes: &[u8] = if headers.is_null() || headers_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(headers.cast::<u8>(), headers_len) }
    };
    let body_bytes: Vec<u8> = if body.is_null() || body_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(body.cast::<u8>(), body_len) }.to_vec()
    };

    let parsed_headers = parse_packed_headers(header_bytes);

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
    REQUESTS_HANDLED.with(|c| c.set(c.get().saturating_add(1)));
    BODY_READER.with(|cell| *cell.borrow_mut() = BodyReader::Empty);
    // Dropping the streaming-response sender (if any) closes the body channel,
    // signalling end-of-body to the hyper handler.
    RESPONSE_TX.with(|cell| *cell.borrow_mut() = None);
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
    headers: *const c_char,
    headers_len: usize,
) {
    // SAFETY: C passes a valid (ptr, len) for the packed headers; null => none.
    let header_bytes: &[u8] = if headers.is_null() || headers_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(headers.cast::<u8>(), headers_len) }
    };
    let parsed_headers = parse_packed_headers(header_bytes);
    let status = u16::try_from(status).unwrap_or(200);

    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(BODY_CHANNEL_DEPTH);
    RESPONSE_TX.with(|cell| *cell.borrow_mut() = Some(tx));

    if let Some(sender) = PENDING_SENDER.with(|cell| cell.borrow_mut().take()) {
        let _ =
            sender.send(WorkerResponse::Streaming { status, headers: parsed_headers, body_rx: rx });
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

/// Parse `"Name: Value\n"` packed header lines into `(name, value)` pairs.
#[cfg(php_linked)]
fn parse_packed_headers(bytes: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                return None;
            }
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
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

    #[test]
    fn parse_packed_headers_basic() {
        let packed = b"Content-Type: text/plain\nX-Foo: bar\n";
        let parsed = parse_packed_headers(packed);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ("Content-Type".to_string(), "text/plain".to_string()));
        assert_eq!(parsed[1], ("X-Foo".to_string(), "bar".to_string()));
    }

    #[test]
    fn parse_packed_headers_skips_blank() {
        let packed = b"A: 1\n\nB: 2\n";
        let parsed = parse_packed_headers(packed);
        assert_eq!(parsed.len(), 2);
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

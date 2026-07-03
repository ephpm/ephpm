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
//! Everything real is `#[cfg(php_linked)]`; stub builds get inert fallbacks.

#[cfg(php_linked)]
use std::cell::RefCell;
#[cfg(php_linked)]
use std::ffi::CString;
#[cfg(php_linked)]
use std::os::raw::{c_char, c_int};

// ── Shared message types (used by ephpm-server's worker pool) ────────────

/// An owned HTTP request handed to a worker thread. Owns every string/byte
/// buffer so the borrowed [`EphpmWorkerRequest`] view stays valid for the
/// whole iteration (until `send_response`).
#[derive(Debug, Clone)]
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
    /// Raw request body bytes.
    pub body: Vec<u8>,
    /// `$_SERVER`-shaped variables as `(key, value)` pairs.
    pub server_vars: Vec<(String, String)>,
    /// HTTP headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
}

/// A response produced by a worker for one request.
#[derive(Debug, Clone)]
pub struct WorkerResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

impl WorkerResponse {
    /// Build the canonical 500 response used when a worker bailed out before
    /// sending a real response (design §5.3).
    #[must_use]
    pub fn internal_error() -> Self {
        Self {
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
    /// Raw body bytes, or null when empty.
    pub body: *const c_char,
    /// Body length in bytes.
    pub body_len: usize,

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
    _body: Vec<u8>,
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

    /// Requests handled by this worker since boot (drives `worker_max_requests`).
    static REQUESTS_HANDLED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// The recycle threshold for this worker (`0` = never recycle).
    static MAX_REQUESTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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

// ── Callback implementations ─────────────────────────────────────────────

#[cfg(php_linked)]
fn build_current_request(job: &WorkerRequestOwned) -> CurrentRequest {
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

    CurrentRequest {
        _method: method,
        _uri: uri,
        _query: query,
        _cookie: cookie,
        _content_type: content_type,
        _body: job.body.clone(),
        _server_keys: server_keys,
        _server_vals: server_vals,
        _header_keys: header_keys,
        _header_vals: header_vals,
        server_key_ptrs,
        server_val_ptrs,
        header_key_ptrs,
        header_val_ptrs,
    }
}

/// C-callable `take_request`: block for the next job, stash its sender, and
/// fill the borrowed request view.
#[cfg(php_linked)]
unsafe extern "C" fn worker_take_request(req: *mut EphpmWorkerRequest) -> c_int {
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

    let job = match rx.recv_blocking() {
        Ok(job) => job,
        // Sender side closed (graceful drain) — end the loop.
        Err(_) => return 0,
    };

    // Stash the sender for send_response / crash recovery.
    PENDING_SENDER.with(|cell| *cell.borrow_mut() = Some(job.respond_to));

    // Build owned backing storage and publish the borrowed pointers.
    let current = build_current_request(&job.request);
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
            (*req).body = if cur._body.is_empty() {
                std::ptr::null()
            } else {
                cur._body.as_ptr().cast::<c_char>()
            };
            (*req).body_len = cur._body.len();
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
    let response = WorkerResponse { status, headers: parsed_headers, body: body_bytes };

    // Deliver to the parked receiver. If it was already taken (shouldn't happen
    // on the normal path) or the receiver dropped, the response is discarded.
    if let Some(sender) = PENDING_SENDER.with(|cell| cell.borrow_mut().take()) {
        let _ = sender.send(response);
    }

    // Count this completed request toward the recycle quota.
    REQUESTS_HANDLED.with(|c| c.set(c.get().saturating_add(1)));

    // Release the borrowed request backing storage.
    CURRENT_REQUEST.with(|cell| *cell.borrow_mut() = None);
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
        let e = WorkerResponse::internal_error();
        assert_eq!(e.status, 500);
        assert_eq!(e.body, b"Internal Server Error");
    }
}

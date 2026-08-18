//! `ephpm_ws_*` native PHP functions → the site-scoped WebSocket registry.
//!
//! Mirrors [`db_bridge`](crate::db_bridge)'s ops-table shape: the C wrapper
//! calls into an [`EphpmWsOps`] table installed once at startup, and every
//! entry point resolves its context from thread-locals the router installs
//! before PHP runs.
//!
//! # Two thread-locals, two different jobs
//!
//! **[`WS_CURRENT_SITE`] — which connections may be reached at all.** These
//! functions are callable from *any* PHP execution: a WebSocket event, an
//! ordinary HTTP request, a cron script. Pushing to a socket from a normal
//! request handler is the point of the design, so there is no "current
//! connection" to key security off. What there always is, is the request
//! executing on this thread, and the router has already derived its canonical
//! site key exactly once (`Router::resolve_site`, issue #293).
//!
//! So the scope is read from the thread-local, never from a parameter. A script
//! cannot name another tenant's site any more than it can name another tenant's
//! database, and a request with no tenant identity gets no capability at all.
//!
//! **[`WS_CURRENT_CONN`] — which connection the short forms mean.** During a
//! WebSocket event dispatch the router also installs the id of the connection
//! that fired it, so `ephpm_ws_send($payload)` needs no id. Outside an event
//! there is no current connection and the implicit form reports
//! [`Status::NoConnection`], which the C wrapper turns into an exception. It
//! must never silently succeed: a no-op that looks like a delivered frame is
//! the worst possible failure mode for a push API.
//!
//! Both are set on **every** PHP execution — to a value or to `None` — so
//! neither can leak into the next request that lands on a reused thread. This
//! is the same discipline as [`db_bridge::set_current_site`].
//!
//! # Split for stub mode
//!
//! Everything except the `unsafe extern "C"` shims and the `#[repr(C)]` table
//! is ungated, so the scoping logic — the security-relevant half — is unit
//! tested by a plain `cargo test` with no PHP SDK present.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use ephpm_ws::{OutFrame, Registry};

/// The process-wide registry, installed once by
/// [`PhpRuntime::set_ws_registry`](crate::PhpRuntime::set_ws_registry).
///
/// Absent when `[server.websocket] enabled = false`.
static WS_REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

/// Site scope for single-site deployments.
///
/// Empty string, matching [`db_bridge`](crate::db_bridge)'s `SINGLE_SITE_KEY`
/// sentinel and safe for the same reason: a real site key is a validated
/// vhost-directory name and can never be empty, so the sentinel cannot collide
/// with a tenant.
pub const SINGLE_SITE_SCOPE: &str = "";

thread_local! {
    /// The site scope for the request executing on this thread.
    static WS_CURRENT_SITE: RefCell<Option<Box<str>>> = const { RefCell::new(None) };
    /// The connection that fired the WebSocket event executing on this thread,
    /// or `None` for an ordinary HTTP request.
    static WS_CURRENT_CONN: RefCell<Option<Box<str>>> = const { RefCell::new(None) };
}

/// Outcome of a bridge call, in the encoding the C wrapper expects.
///
/// Non-negative values are results; negative values are conditions the script
/// must not be able to ignore, and the wrapper converts each into a distinct
/// exception message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Boolean `false`, or a broadcast that reached nobody.
    Ok(i64),
    /// No current connection — the implicit form was used outside a WebSocket
    /// event.
    NoConnection,
    /// This request has no tenant identity, so it has no WebSocket capability.
    NoSite,
    /// `[server.websocket]` is disabled; no registry was ever installed.
    NoRegistry,
}

impl Status {
    /// The wire value handed back to C.
    #[must_use]
    pub fn code(self) -> i64 {
        match self {
            Self::Ok(v) => v,
            Self::NoConnection => -1,
            Self::NoSite => -2,
            Self::NoRegistry => -3,
        }
    }

    /// The wire value narrowed to the C ABI's `long`, as the ops table
    /// declares it (`long (*send)(...)` etc. in `ephpm_wrapper.c`).
    ///
    /// `long` is 64-bit on LP64 (Linux, macOS) but **32-bit on Windows**
    /// (LLP64), so [`Status::code`]'s `i64` has to narrow at the boundary —
    /// without this the Windows build fails to compile (and it did: the
    /// v0.7.0 release's `build-windows` leg was the first thing to ever
    /// type-check this code, because `php_linked` is only set when
    /// `PHP_SDK_PATH` is present, which PR CI never does).
    ///
    /// The narrowing cannot lose information: the error variants are
    /// `-1..=-3`, and `Ok(v)` carries either a boolean or a count of
    /// connections a frame was queued to, which the registry's per-site and
    /// global connection caps bound far below `i32::MAX`.
    #[cfg(php_linked)]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "every reachable value fits in 32 bits; see doc comment"
    )]
    fn code_c(self) -> std::os::raw::c_long {
        self.code() as std::os::raw::c_long
    }

    /// Shorthand for a boolean result.
    fn boolean(ok: bool) -> Self {
        Self::Ok(i64::from(ok))
    }
}

/// Install the process-wide registry. Returns `false` if one was already set.
///
/// Called once at startup, before any PHP thread runs.
pub fn set_registry(registry: Arc<Registry>) -> bool {
    WS_REGISTRY.set(registry).is_ok()
}

/// Whether a registry is installed (i.e. `[server.websocket]` is enabled).
#[must_use]
pub fn is_configured() -> bool {
    WS_REGISTRY.get().is_some()
}

/// Set (or clear) the site scope for the request about to run on this thread.
///
/// Called by the request handler before PHP execution, alongside
/// [`crate::db_bridge::set_current_site`]. Passing `None` clears any previous
/// scope so a request with no tenant identity gets **no** WebSocket capability
/// rather than inheriting the last request's.
pub fn set_current_site(scope: Option<&str>) {
    WS_CURRENT_SITE.with(|s| {
        *s.borrow_mut() = scope.map(Box::from);
    });
}

/// Set (or clear) the connection the implicit `ephpm_ws_*` forms act on.
///
/// Called with `Some(id)` immediately before a WebSocket event's PHP execution
/// and with `None` before every ordinary request, so the id cannot outlive its
/// event on a thread the pool later reuses. Clearing is not an optimisation:
/// a stale id would make an unrelated HTTP request silently push frames to
/// whichever socket happened to run last on that thread.
pub fn set_current_connection(connection_id: Option<&str>) {
    WS_CURRENT_CONN.with(|c| {
        *c.borrow_mut() = connection_id.map(Box::from);
    });
}

/// The site scope for this thread's request, or `None`.
fn site_scope() -> Option<Box<str>> {
    WS_CURRENT_SITE.with(|s| s.borrow().clone())
}

/// The current event's connection id, or `None` outside an event.
fn current_connection() -> Option<Box<str>> {
    WS_CURRENT_CONN.with(|c| c.borrow().clone())
}

/// Resolve the registry and this request's site scope.
fn context() -> Result<(&'static Arc<Registry>, Box<str>), Status> {
    let registry = WS_REGISTRY.get().ok_or(Status::NoRegistry)?;
    let scope = site_scope().ok_or_else(|| {
        tracing::debug!(
            "ephpm_ws_* called with no site context — this request's Host matched no virtual \
             host, so it has no websocket capability"
        );
        Status::NoSite
    })?;
    Ok((registry, scope))
}

/// Resolve the target connection: the explicit id when given, otherwise the
/// current event's connection.
fn target(explicit: Option<&[u8]>) -> Result<Box<str>, Status> {
    match explicit {
        Some(bytes) => {
            // Non-UTF-8 is refused rather than lossily converted: a connection
            // id is a hex capability token and two different byte strings must
            // never normalize onto one.
            let id = std::str::from_utf8(bytes).map_err(|_| Status::Ok(0))?;
            Ok(Box::from(id))
        }
        None => current_connection().ok_or(Status::NoConnection),
    }
}

/// Decode a channel name. Channel names are map keys, so the same
/// no-lossy-conversion rule applies.
fn channel_name(bytes: &[u8]) -> Result<&str, Status> {
    std::str::from_utf8(bytes).map_err(|_| Status::Ok(0))
}

/// Collapse `Result<Status, Status>` — both arms are already a status.
fn settle(result: Result<Status, Status>) -> Status {
    match result {
        Ok(status) | Err(status) => status,
    }
}

// ── Core operations (ungated — unit tested in stub mode) ─────────────────

/// `ephpm_ws_send()` / `ephpm_ws_connection_send()`.
///
/// `conn_id` is `None` for the implicit form.
#[must_use]
pub fn send(conn_id: Option<&[u8]>, payload: &[u8], binary: bool) -> Status {
    settle((|| {
        let (registry, scope) = context()?;
        let id = target(conn_id)?;
        let frame = OutFrame::new(bytes::Bytes::copy_from_slice(payload), binary);
        Ok(Status::boolean(registry.send(&scope, &id, frame)))
    })())
}

/// `ephpm_ws_subscribe()` / `ephpm_ws_connection_subscribe()`.
#[must_use]
pub fn subscribe(conn_id: Option<&[u8]>, channel: &[u8]) -> Status {
    settle((|| {
        let (registry, scope) = context()?;
        let id = target(conn_id)?;
        let channel = channel_name(channel)?;
        Ok(Status::boolean(registry.subscribe(&scope, &id, channel)))
    })())
}

/// `ephpm_ws_unsubscribe()` / `ephpm_ws_connection_unsubscribe()`.
#[must_use]
pub fn unsubscribe(conn_id: Option<&[u8]>, channel: &[u8]) -> Status {
    settle((|| {
        let (registry, scope) = context()?;
        let id = target(conn_id)?;
        let channel = channel_name(channel)?;
        Ok(Status::boolean(registry.unsubscribe(&scope, &id, channel)))
    })())
}

/// `ephpm_ws_broadcast()`. Needs no current connection.
#[must_use]
pub fn broadcast(channel: &[u8], payload: &[u8], binary: bool) -> Status {
    settle((|| {
        let (registry, scope) = context()?;
        let channel = channel_name(channel)?;
        let delivered =
            registry.broadcast(&scope, channel, bytes::Bytes::copy_from_slice(payload), binary);
        Ok(Status::Ok(i64::try_from(delivered).unwrap_or(i64::MAX)))
    })())
}

/// `ephpm_ws_close()` / `ephpm_ws_connection_close()`.
#[must_use]
pub fn close(conn_id: Option<&[u8]>, code: u16) -> Status {
    settle((|| {
        let (registry, scope) = context()?;
        let id = target(conn_id)?;
        Ok(Status::boolean(registry.close(&scope, &id, code)))
    })())
}

// ── FFI shims ───────────────────────────────────────────────────────────

/// C-ABI mirror of `EphpmWsOps` in `ephpm_wrapper.c`.
///
/// Field order and types must match the C `typedef` exactly. New entries are
/// **appended**, never inserted — the C side documents the same rule.
///
/// Every entry takes the target connection as a `(ptr, len)` pair where a NULL
/// pointer means "the connection that fired the current event", which is what
/// lets one op back both the implicit and the explicit PHP form.
#[cfg(php_linked)]
#[repr(C)]
pub struct EphpmWsOps {
    /// `send(conn_id|NULL, len, payload, len, binary) -> status`
    pub send: Option<
        unsafe extern "C" fn(
            conn_id: *const std::os::raw::c_char,
            conn_id_len: usize,
            payload: *const std::os::raw::c_char,
            payload_len: usize,
            binary: std::os::raw::c_int,
        ) -> std::os::raw::c_long,
    >,
    /// `subscribe(conn_id|NULL, len, channel, len) -> status`
    pub subscribe: Option<
        unsafe extern "C" fn(
            conn_id: *const std::os::raw::c_char,
            conn_id_len: usize,
            channel: *const std::os::raw::c_char,
            channel_len: usize,
        ) -> std::os::raw::c_long,
    >,
    /// `unsubscribe(conn_id|NULL, len, channel, len) -> status`
    pub unsubscribe: Option<
        unsafe extern "C" fn(
            conn_id: *const std::os::raw::c_char,
            conn_id_len: usize,
            channel: *const std::os::raw::c_char,
            channel_len: usize,
        ) -> std::os::raw::c_long,
    >,
    /// `broadcast(channel, len, payload, len, binary) -> status`
    pub broadcast: Option<
        unsafe extern "C" fn(
            channel: *const std::os::raw::c_char,
            channel_len: usize,
            payload: *const std::os::raw::c_char,
            payload_len: usize,
            binary: std::os::raw::c_int,
        ) -> std::os::raw::c_long,
    >,
    /// `close(conn_id|NULL, len, code) -> status`
    pub close: Option<
        unsafe extern "C" fn(
            conn_id: *const std::os::raw::c_char,
            conn_id_len: usize,
            code: std::os::raw::c_int,
        ) -> std::os::raw::c_long,
    >,
}

/// Borrow a `(ptr, len)` pair from C as a byte slice.
///
/// # Safety
///
/// `ptr` must either be NULL or point to at least `len` readable bytes that
/// stay valid for the duration of the call. PHP guarantees this for a
/// `zend_string`'s buffer across a `PHP_FUNCTION` body: the string is a live
/// zval argument, and none of these shims can trigger a GC pass or re-enter
/// PHP.
#[cfg(php_linked)]
unsafe fn opt_slice<'a>(ptr: *const std::os::raw::c_char, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    if len == 0 {
        return Some(&[]);
    }
    // SAFETY: caller contract above — non-NULL `ptr` is a live zend_string
    // buffer of at least `len` bytes, and nothing here re-enters the Zend
    // engine.
    Some(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

/// Borrow a required `(ptr, len)` pair. A NULL payload is treated as empty
/// rather than trusted.
///
/// # Safety
///
/// Same contract as [`opt_slice`].
#[cfg(php_linked)]
unsafe fn slice<'a>(ptr: *const std::os::raw::c_char, len: usize) -> &'a [u8] {
    // SAFETY: caller contract above.
    unsafe { opt_slice(ptr, len) }.unwrap_or(&[])
}

#[cfg(php_linked)]
unsafe extern "C" fn ws_send(
    conn_id: *const std::os::raw::c_char,
    conn_id_len: usize,
    payload: *const std::os::raw::c_char,
    payload_len: usize,
    binary: std::os::raw::c_int,
) -> std::os::raw::c_long {
    // SAFETY: both pairs are live zend_string buffers (or NULL) for this call.
    let (id, body) = unsafe { (opt_slice(conn_id, conn_id_len), slice(payload, payload_len)) };
    send(id, body, binary != 0).code_c()
}

#[cfg(php_linked)]
unsafe extern "C" fn ws_subscribe(
    conn_id: *const std::os::raw::c_char,
    conn_id_len: usize,
    channel: *const std::os::raw::c_char,
    channel_len: usize,
) -> std::os::raw::c_long {
    // SAFETY: both pairs are live zend_string buffers (or NULL) for this call.
    let (id, chan) = unsafe { (opt_slice(conn_id, conn_id_len), slice(channel, channel_len)) };
    subscribe(id, chan).code_c()
}

#[cfg(php_linked)]
unsafe extern "C" fn ws_unsubscribe(
    conn_id: *const std::os::raw::c_char,
    conn_id_len: usize,
    channel: *const std::os::raw::c_char,
    channel_len: usize,
) -> std::os::raw::c_long {
    // SAFETY: both pairs are live zend_string buffers (or NULL) for this call.
    let (id, chan) = unsafe { (opt_slice(conn_id, conn_id_len), slice(channel, channel_len)) };
    unsubscribe(id, chan).code_c()
}

#[cfg(php_linked)]
unsafe extern "C" fn ws_broadcast(
    channel: *const std::os::raw::c_char,
    channel_len: usize,
    payload: *const std::os::raw::c_char,
    payload_len: usize,
    binary: std::os::raw::c_int,
) -> std::os::raw::c_long {
    // SAFETY: both pairs are live zend_string buffers for this call.
    let (chan, body) = unsafe { (slice(channel, channel_len), slice(payload, payload_len)) };
    broadcast(chan, body, binary != 0).code_c()
}

#[cfg(php_linked)]
unsafe extern "C" fn ws_close(
    conn_id: *const std::os::raw::c_char,
    conn_id_len: usize,
    code: std::os::raw::c_int,
) -> std::os::raw::c_long {
    // SAFETY: the pair is a live zend_string buffer (or NULL) for this call.
    let id = unsafe { opt_slice(conn_id, conn_id_len) };
    // PHP `int` is 64-bit; anything outside the WebSocket close-code range is
    // clamped to "normal closure" rather than silently truncated into a
    // different, meaningful code.
    let code = u16::try_from(code).unwrap_or(ephpm_ws::CLOSE_NORMAL);
    close(id, code).code_c()
}

/// The ops table handed to the C wrapper.
#[cfg(php_linked)]
pub static WS_OPS: EphpmWsOps = EphpmWsOps {
    send: Some(ws_send),
    subscribe: Some(ws_subscribe),
    unsubscribe: Some(ws_unsubscribe),
    broadcast: Some(ws_broadcast),
    close: Some(ws_close),
};

#[cfg(test)]
mod tests {
    use ephpm_ws::Limits;

    use super::*;

    /// The scope and connection are thread-local, so every test that touches
    /// them runs on its own thread and cannot be perturbed by a sibling.
    fn on_thread<F: FnOnce() + Send + 'static>(f: F) {
        std::thread::spawn(f).join().expect("test thread");
    }

    /// Install the process-wide registry exactly once for this test binary.
    ///
    /// `WS_REGISTRY` is a `OnceLock`, so no test may assume it is *unset* —
    /// every assertion below holds whether this ran first or last.
    fn shared_registry() -> &'static Arc<Registry> {
        let registry = Arc::new(Registry::new(Limits {
            max_connections: 0,
            max_connections_per_site: 0,
            send_queue: 4,
            max_message_size: 0,
        }));
        let _ = set_registry(registry);
        WS_REGISTRY.get().expect("registry installed")
    }

    #[test]
    fn no_scope_means_no_capability() {
        on_thread(|| {
            let registry = shared_registry();
            // A real, live connection under site `alpha`...
            let conn = registry.register("alpha").expect("register");

            // ...is unreachable from a request that has no tenant identity,
            // even holding its exact id.
            set_current_site(None);
            set_current_connection(None);
            assert_eq!(send(Some(conn.id.as_bytes()), b"hi", false), Status::NoSite);
            assert_eq!(subscribe(Some(conn.id.as_bytes()), b"room"), Status::NoSite);
            assert_eq!(unsubscribe(Some(conn.id.as_bytes()), b"room"), Status::NoSite);
            assert_eq!(close(Some(conn.id.as_bytes()), 1000), Status::NoSite);
            assert_eq!(broadcast(b"room", b"hi", false), Status::NoSite);

            registry.unregister(&conn.id);
        });
    }

    #[test]
    fn the_thread_scope_is_what_gates_a_send_not_the_argument() {
        on_thread(|| {
            let registry = shared_registry();
            let mut conn = registry.register("scoped-alpha").expect("register");

            // Executing as another tenant: the id is valid, the scope is not.
            set_current_site(Some("scoped-beta"));
            assert_eq!(send(Some(conn.id.as_bytes()), b"pwned", false), Status::Ok(0));
            assert!(conn.rx.try_recv().is_err(), "no frame may cross the site boundary");

            // Executing as the owning tenant: the same call succeeds.
            set_current_site(Some("scoped-alpha"));
            assert_eq!(send(Some(conn.id.as_bytes()), b"hello", false), Status::Ok(1));
            assert!(conn.rx.try_recv().is_ok());

            registry.unregister(&conn.id);
        });
    }

    /// The whole point of the implicit form: no id argument, and it must reach
    /// exactly the connection whose event is running.
    #[test]
    fn the_implicit_form_targets_the_current_event_connection() {
        on_thread(|| {
            let registry = shared_registry();
            let mut firing = registry.register("implicit").expect("register");
            let mut bystander = registry.register("implicit").expect("register");

            set_current_site(Some("implicit"));
            set_current_connection(Some(&firing.id));

            assert_eq!(send(None, b"echo", false), Status::Ok(1));
            assert!(firing.rx.try_recv().is_ok(), "the firing connection receives it");
            assert!(bystander.rx.try_recv().is_err(), "no one else does");

            assert_eq!(subscribe(None, b"room"), Status::Ok(1));
            assert_eq!(registry.channel_members("implicit", "room"), 1);
            assert_eq!(unsubscribe(None, b"room"), Status::Ok(1));
            assert_eq!(registry.channel_members("implicit", "room"), 0);

            assert_eq!(close(None, 4000), Status::Ok(1));

            registry.unregister(&firing.id);
            registry.unregister(&bystander.id);
        });
    }

    /// An ordinary HTTP request has no current connection. The implicit form
    /// must say so loudly — a silent no-op looks exactly like a delivered
    /// frame.
    #[test]
    fn the_implicit_form_fails_loudly_outside_an_event() {
        on_thread(|| {
            let _ = shared_registry();
            set_current_site(Some("alpha"));
            set_current_connection(None);

            assert_eq!(send(None, b"hi", false), Status::NoConnection);
            assert_eq!(subscribe(None, b"room"), Status::NoConnection);
            assert_eq!(unsubscribe(None, b"room"), Status::NoConnection);
            assert_eq!(close(None, 1000), Status::NoConnection);

            // Broadcast needs no connection, so it stays usable from HTTP.
            assert_eq!(broadcast(b"room", b"hi", false), Status::Ok(0));
        });
    }

    /// The HTTP-pushes-to-socket pattern the guide documents: no current
    /// connection, an explicit id looked up out of band.
    #[test]
    fn an_http_request_can_push_with_an_explicit_id() {
        on_thread(|| {
            let registry = shared_registry();
            let mut conn = registry.register("push").expect("register");
            let stashed_id = conn.id.clone();

            // Simulate a later, unrelated HTTP request on this thread.
            set_current_site(Some("push"));
            set_current_connection(None);

            assert_eq!(send(Some(stashed_id.as_bytes()), b"new comment", false), Status::Ok(1));
            assert!(conn.rx.try_recv().is_ok());

            registry.unregister(&conn.id);
        });
    }

    #[test]
    fn the_current_connection_does_not_leak_between_dispatches() {
        on_thread(|| {
            set_current_connection(Some("abc"));
            assert_eq!(current_connection().as_deref(), Some("abc"));
            // Next execution on this thread is an ordinary HTTP request.
            set_current_connection(None);
            assert_eq!(
                current_connection(),
                None,
                "a cleared connection must not fall back to the last event's"
            );
        });
    }

    #[test]
    fn a_scope_does_not_leak_between_requests_on_one_thread() {
        on_thread(|| {
            set_current_site(Some("alpha"));
            assert_eq!(site_scope().as_deref(), Some("alpha"));
            set_current_site(None);
            assert_eq!(site_scope(), None, "a cleared scope must not fall back to the last one");
        });
    }

    #[test]
    fn scope_is_replaced_not_merged() {
        on_thread(|| {
            set_current_site(Some("alpha"));
            set_current_site(Some("beta"));
            assert_eq!(site_scope().as_deref(), Some("beta"));
        });
    }

    #[test]
    fn non_utf8_arguments_are_refused_without_panicking() {
        on_thread(|| {
            let _ = shared_registry();
            set_current_site(Some("alpha"));
            set_current_connection(Some("abc"));
            assert_eq!(send(Some(b"\xff\xfe"), b"payload", false), Status::Ok(0));
            assert_eq!(subscribe(None, b"\xff\xfe"), Status::Ok(0));
            assert_eq!(unsubscribe(None, b"\xff\xfe"), Status::Ok(0));
            assert_eq!(broadcast(b"\xff\xfe", b"payload", false), Status::Ok(0));
        });
    }

    #[test]
    fn an_unknown_connection_id_is_a_false_not_an_exception() {
        on_thread(|| {
            let _ = shared_registry();
            set_current_site(Some("alpha"));
            let unknown = b"00000000000000000000000000000000";
            assert_eq!(send(Some(unknown), b"hi", false), Status::Ok(0));
            assert_eq!(close(Some(unknown), 1000), Status::Ok(0));
        });
    }

    #[test]
    fn status_codes_match_the_c_wrapper_constants() {
        // These three values are duplicated in ephpm_wrapper.c as
        // EPHPM_WS_NO_CONN / NO_SITE / NO_REGISTRY. Drift here is a silent
        // wrong-exception bug, so pin them.
        assert_eq!(Status::NoConnection.code(), -1);
        assert_eq!(Status::NoSite.code(), -2);
        assert_eq!(Status::NoRegistry.code(), -3);
        assert_eq!(Status::Ok(0).code(), 0);
        assert_eq!(Status::Ok(7).code(), 7);
    }
}

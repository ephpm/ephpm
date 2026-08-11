//! Bridge between the C `ephpm_db_*` PHP functions and a litewire
//! [`Session`].
//!
//! Modeled on [`crate::kv_bridge`]: a C-compatible function pointer table
//! ([`EphpmDbOps`]) is handed to `ephpm_set_db_ops()` at startup, and the
//! PHP native functions (`ephpm_db_query`, `ephpm_db_execute`) call through
//! it into this module. Results and errors are staged in thread-local
//! buffers so the C side copies data without malloc/free across the FFI
//! boundary (same contract as `KV_GET_BUF`).
//!
//! # Backend sharing
//!
//! [`set_backend`] receives the **same** erased backend instance the wire
//! frontends serve — including ephpm-server's `TrackedBackend` query-stats
//! wrapper — so bridge queries appear in query-stats exactly like wire
//! queries. Registration happens only when a `[db.sqlite]` backend is
//! active (see `share_backend_with_php` in ephpm-server); when nothing is
//! registered, [`run_sql_bytes`] reports [`RunStatus::Unavailable`] and the
//! C side throws a clean PHP exception instead of crashing.
//!
//! # One Session per PHP thread
//!
//! Each OS thread that executes PHP gets its own lazily-created
//! [`Session`] (thread-local), opened via `Backend::connect()` on first
//! use. The session translates MySQL-dialect SQL, runs the metadata
//! emulation for `SHOW`/`DESCRIBE`, returns OK for dialect no-ops like
//! `SET NAMES`, and tracks transaction state — `BEGIN`/`COMMIT`/`ROLLBACK`
//! flow through as plain SQL, exactly as they do on the wire path.
//!
//! # Transactions end with the request
//!
//! The session lives for the thread, but transactions do not. At
//! per-request teardown ([`on_request_end`], wired into both execution
//! modes — see its docs for the exact seams) any explicit transaction the
//! script left open is rolled back with a `tracing::warn!`, and the staged
//! result/error buffers are released. A script that runs `BEGIN` and then
//! fatals (or simply forgets to `COMMIT`) therefore cannot leak its open
//! transaction into the next, unrelated request dispatched to the same
//! worker thread. Scripts should still `COMMIT`/`ROLLBACK` explicitly —
//! the automatic rollback is a safety net, not an API.
//!
//! # Session recycling on connection failure
//!
//! A thread's session holds its `BackendConn` indefinitely. If the
//! connection dies underneath it (sqld restart on clustered failover), the
//! failed call is reported to PHP as an error, and — when the error
//! classifies as connection-shaped (`is_connection_error`) and the
//! session is **not** inside an explicit transaction — the session is
//! dropped so the next call lazily reconnects, mirroring how wire clients
//! recover by reconnecting. SQL-level errors (1062, 1064, ...) never
//! recycle: that would discard live transaction state on every constraint
//! violation. See `is_connection_error` for why the classification is
//! substring-based and deliberately conservative.
//!
//! # Async boundary
//!
//! `Session::query` is async but PHP FFI callbacks are synchronous, so the
//! bridge pins the server's tokio [`Handle`](tokio::runtime::Handle) at
//! registration time and uses `Handle::block_on`. This is legal ONLY
//! because PHP FFI callbacks run on PHP worker OS threads or the tokio
//! `spawn_blocking` pool, never on async tasks — the invariant documented
//! on `EphpmKvOps::wait` in `kv_bridge.rs` ("Blocking is safe here:
//! callers are PHP worker OS threads or the tokio `spawn_blocking` pool,
//! never async tasks"). The same pinned-`Handle` sync bridge is the
//! established precedent in `ephpm-cluster`'s `KvReplicator`
//! (`clustered_store.rs`).

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use litewire::backend::{SharedBackend, Value};
use litewire::session::ER_PARSE_ERROR;
use litewire::session::error_map::ER_UNKNOWN_ERROR;
use litewire::translate::{Dialect, TranslateCache};
use litewire::{Session, SessionError, SessionResult};

// ── Global bridge handle ────────────────────────────────────────────────

/// Everything a thread needs to open and drive a session.
struct DbBridge {
    /// The erased backend shared with the wire frontends (including the
    /// `TrackedBackend` stats wrapper).
    backend: SharedBackend,
    /// The server's tokio runtime, pinned so sync FFI callbacks can
    /// `block_on` session work.
    handle: tokio::runtime::Handle,
    /// Translation cache shared by every bridge session on this process
    /// (the wire frontends keep their own; both are per-frontend by
    /// design in litewire).
    cache: Arc<TranslateCache>,
}

static DB_BRIDGE: OnceLock<DbBridge> = OnceLock::new();

/// Register the backend + runtime handle backing the PHP `ephpm_db_*`
/// functions. First registration wins; later calls are no-ops (mirrors
/// [`crate::kv_bridge::set_store`]).
///
/// Returns `true` if this call performed the registration.
pub fn set_backend(backend: SharedBackend, handle: tokio::runtime::Handle) -> bool {
    let registered = DB_BRIDGE
        .set(DbBridge { backend, handle, cache: Arc::new(TranslateCache::default()) })
        .is_ok();
    if registered {
        tracing::debug!("db backend registered for PHP native functions");
    }
    registered
}

/// Whether a backend has been registered.
#[must_use]
pub fn is_configured() -> bool {
    DB_BRIDGE.get().is_some()
}

// ── Thread-local state ──────────────────────────────────────────────────

thread_local! {
    /// The per-thread session, created lazily on first use.
    static DB_SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// Parameters staged by `param_*` calls for the next `run`.
    static DB_PARAMS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
    /// Result of the last successful `run` on this thread.
    static DB_RESULT: RefCell<Option<SessionResult>> = const { RefCell::new(None) };
    /// Error from the last failed `run` on this thread.
    static DB_ERROR: RefCell<Option<BridgeError>> = const { RefCell::new(None) };
}

/// The MySQL error triple staged for the C side after a failed `run`.
struct BridgeError {
    code: u16,
    sqlstate: [u8; 5],
    message: String,
}

impl From<SessionError> for BridgeError {
    fn from(e: SessionError) -> Self {
        Self { code: e.code(), sqlstate: e.sqlstate(), message: e.to_string() }
    }
}

// ── Core operations (stub-safe: no PHP types involved) ──────────────────

/// Outcome of [`run_sql_bytes`], mapped by the C shim onto the
/// `EphpmDbOps::run` return codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// A result set is staged; read it via the row/col/cell accessors.
    Rows,
    /// An OK result is staged; read it via [`ok_info`].
    Ok,
    /// An error is staged; read it via the error accessor.
    Err,
    /// No backend registered — `[db.sqlite]` is not active.
    Unavailable,
}

/// Clear the staged parameter list for this thread.
pub fn params_reset() {
    DB_PARAMS.with(|p| p.borrow_mut().clear());
}

/// Stage one parameter for the next [`run_sql_bytes`] on this thread.
pub fn param_push(v: Value) {
    DB_PARAMS.with(|p| p.borrow_mut().push(v));
}

/// Convert PHP string bytes to a bind [`Value`]: valid UTF-8 binds as
/// `Text`, anything else as `Blob` — the same decoding the MySQL wire
/// frontend applies to `COM_STMT_EXECUTE` byte parameters.
#[must_use]
pub fn bytes_to_value(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => Value::Text(s.to_string()),
        Err(_) => Value::Blob(bytes.to_vec()),
    }
}

/// Drop the staged result/error, releasing their memory. Also called
/// implicitly at the start of every [`run_sql_bytes`] and at per-request
/// teardown ([`on_request_end`]).
pub fn finish() {
    DB_RESULT.with(|r| *r.borrow_mut() = None);
    DB_ERROR.with(|e| *e.borrow_mut() = None);
}

fn stage_error(err: BridgeError) -> RunStatus {
    DB_ERROR.with(|e| *e.borrow_mut() = Some(err));
    RunStatus::Err
}

/// Message substrings that mark a [`SessionError`] as connection-shaped —
/// the backend connection (not the SQL) is what failed.
///
/// litewire's `BackendError` is stringly typed (`Sqlite(String)` /
/// `Other(String)`) and `SessionError::Db` flattens it further into a
/// `(code, sqlstate, message)` triple, so there is no structured
/// "connection failure" variant to key off — substring matching against
/// the known producer sites is the only classification available. Every
/// marker below is anchored to a concrete error `format!` in
/// litewire-backend's `hrana_client.rs` (the only backend whose
/// connections can die independently of the process; the rusqlite backend
/// is in-process) or to the transport wording reqwest/hyper nest inside
/// those messages.
const CONNECTION_ERROR_MARKERS: &[&str] = &[
    // hrana_client transport failures: the pipeline POST never completed
    // or returned garbage ("HTTP request failed: {reqwest}", "failed to
    // parse response: {e}", "empty pipeline response", "unexpected close
    // response", "health check failed: {e}").
    "http request failed",
    "health check failed",
    "failed to parse response",
    "empty pipeline response",
    "unexpected close response",
    // "sqld returned {status}: {body}" — emitted only for a non-success
    // HTTP status. SQL errors always come back in-band with HTTP 200, so
    // an HTTP-level failure is a stream/transport problem by construction
    // (a restarted sqld rejecting a stale baton answers 4xx here).
    "sqld returned ",
    // Hrana stream loss after a sqld restart, surfaced as an in-band
    // ErrorResponse without a SQLITE code ("Invalid baton", "The stream
    // has expired ...", stream not found).
    "baton",
    "stream has expired",
    "stream not found",
    // Transport wording reqwest/hyper nest inside their Display output.
    "connection refused",
    "connection reset",
    "connection closed",
    "broken pipe",
    "error sending request",
    "channel closed",
];

/// Whether `err` indicates a dead/broken backend connection, as opposed to
/// a SQL-level error the statement earned on its own.
///
/// Conservative on purpose:
///
/// * Anything litewire's `error_map` managed to classify (1062 duplicate
///   key, 1064 parse, 1205 busy, 1290 read-only, 1452 FK, ...) is a SQL
///   error — never connection-shaped. Only the `ER_UNKNOWN_ERROR` (1105)
///   fallback is considered further.
/// * Within 1105, only messages matching [`CONNECTION_ERROR_MARKERS`]
///   qualify.
///
/// Known limitation: the match is substring-based because the underlying
/// error is stringly typed. A false positive (e.g. `no such column:
/// baton`) costs one needless reconnect on an autocommit session — cheap
/// and self-healing. A false negative leaves the session broken until the
/// thread's next connection-shaped error, which the next call will
/// produce. Callers additionally refuse to recycle mid-transaction (see
/// [`run_sql_bytes`]) so a misclassification can never discard live
/// transaction state.
fn is_connection_error(err: &SessionError) -> bool {
    if err.code() != ER_UNKNOWN_ERROR {
        return false;
    }
    let msg = err.to_string().to_ascii_lowercase();
    CONNECTION_ERROR_MARKERS.iter().any(|marker| msg.contains(marker))
}

/// Execute `sql` (raw PHP string bytes) with the staged parameters through
/// this thread's [`Session`], staging the result or error for the
/// accessors below.
///
/// The staged parameter list is consumed (cleared) whether or not
/// execution succeeds.
///
/// On a connection-shaped failure (`is_connection_error`) outside an
/// explicit transaction, the thread's session is dropped so the next call
/// reconnects (see the module docs, `Session recycling`).
pub fn run_sql_bytes(sql: &[u8]) -> RunStatus {
    run_on(DB_BRIDGE.get(), sql)
}

/// [`run_sql_bytes`] against an explicit bridge. Split out so unit tests
/// can drive a locally-constructed [`DbBridge`] (with a mock backend)
/// without going through the process-wide, set-once [`DB_BRIDGE`].
fn run_on(bridge: Option<&DbBridge>, sql: &[u8]) -> RunStatus {
    let params: Vec<Value> = DB_PARAMS.with(|p| std::mem::take(&mut *p.borrow_mut()));
    finish();

    let Some(bridge) = bridge else {
        return RunStatus::Unavailable;
    };

    let Ok(sql) = std::str::from_utf8(sql) else {
        return stage_error(BridgeError {
            code: ER_PARSE_ERROR,
            sqlstate: *b"42000",
            message: "SQL must be valid UTF-8".to_string(),
        });
    };

    let outcome = DB_SESSION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let session = match slot.as_mut() {
            Some(session) => session,
            None => {
                // Lazily open this thread's session against the shared
                // backend. block_on is legal here — see the module docs
                // (`Async boundary`): FFI callbacks never run on async
                // tasks.
                match bridge.handle.block_on(bridge.backend.connect()) {
                    Ok(conn) => slot.insert(Session::with_cache(
                        conn,
                        Dialect::MySQL,
                        Arc::clone(&bridge.cache),
                    )),
                    Err(e) => {
                        return Err(BridgeError {
                            code: ER_UNKNOWN_ERROR,
                            sqlstate: *b"HY000",
                            message: format!("failed to open embedded database session: {e}"),
                        });
                    }
                }
            }
        };
        let result = bridge.handle.block_on(session.query(sql, &params));
        if let Err(e) = &result {
            // Recycle a session whose connection looks dead so the NEXT
            // call reconnects — but never mid-transaction: recycling
            // there would silently discard the transaction state a
            // misclassified SQL error (the match is substring-based, see
            // is_connection_error) may still legitimately own. A
            // mid-transaction connection death is instead cleaned up at
            // request end: on_request_end's ROLLBACK fails over the same
            // dead connection and drops the session then.
            if !session.in_transaction && is_connection_error(e) {
                tracing::warn!(
                    error = %e,
                    "embedded db session hit a connection-class error — dropping it so the \
                     next call reconnects"
                );
                *slot = None;
            }
        }
        result.map_err(BridgeError::from)
    });

    match outcome {
        Ok(result) => {
            let status = match result {
                SessionResult::Rows(_) => RunStatus::Rows,
                SessionResult::Ok(_) => RunStatus::Ok,
            };
            DB_RESULT.with(|r| *r.borrow_mut() = Some(result));
            status
        }
        Err(e) => stage_error(e),
    }
}

/// Per-request teardown for this thread's bridge state.
///
/// Must run at the end of **every** PHP request, on the thread that ran
/// it. The wired seams, per execution mode:
///
/// * **fpm mode** — `PhpRuntime::execute()` calls this right after
///   `execute_php` returns (success, script exit, or bailout alike). That
///   is the single choke point every fpm-style request passes through on
///   its own `spawn_blocking` thread.
/// * **worker mode** — `worker_bridge::finish_iteration()` (runs on the
///   worker thread inside `send_response` / `response_end`, i.e. at every
///   normal request end), again at the top of the next `take_request`
///   (covering framework terminate hooks that touch the DB *after* the
///   response was delivered), and from
///   `PhpRuntime::worker_thread_shutdown()` when the thread recycles
///   after a fatal (the in-flight request never reached `send_response`).
///
/// Two jobs:
///
/// 1. If the script left an explicit transaction open (`BEGIN` without
///    `COMMIT`/`ROLLBACK` — a mid-transaction fatal looks identical),
///    issue a `ROLLBACK` through the session and `tracing::warn!`.
///    Without this the transaction stays open on the worker thread and
///    the next unrelated request's writes silently join it.
/// 2. Release the staged result/error buffers ([`finish`]) so the memory
///    of a request's last query does not sit around until the thread's
///    next request.
///
/// If the rollback itself fails (typically because the connection under
/// the transaction died), the session is dropped entirely: the server
/// side abandons an uncommitted transaction when its connection goes
/// away, and the next request reconnects with clean state.
///
/// Idempotent and cheap when there is nothing to do (no session, or no
/// open transaction). Safe in stub mode — no PHP types are involved.
pub fn on_request_end() {
    finish();
    let Some(bridge) = DB_BRIDGE.get() else {
        return;
    };
    DB_SESSION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(session) = slot.as_mut() else {
            return;
        };
        if !session.in_transaction {
            return;
        }
        tracing::warn!(
            "PHP script left a database transaction open at request end — rolling it back \
             (scripts must COMMIT or ROLLBACK before the request finishes)"
        );
        // block_on is legal here for the same reason as the query path:
        // request teardown runs on a PHP worker OS thread or the tokio
        // spawn_blocking pool, never on an async task (module docs,
        // `Async boundary`). On success Session::query clears
        // `in_transaction` itself.
        if let Err(e) = bridge.handle.block_on(session.query("ROLLBACK", &[])) {
            tracing::warn!(
                error = %e,
                "rollback of the abandoned transaction failed — dropping the session so \
                 the next request reconnects with clean state"
            );
            *slot = None;
        }
    });
}

/// Number of rows in the staged result set (0 when none is staged).
#[must_use]
pub fn row_count() -> usize {
    DB_RESULT.with(|r| match &*r.borrow() {
        Some(SessionResult::Rows(rs)) => rs.rows.len(),
        _ => 0,
    })
}

/// Number of columns in the staged result set (0 when none is staged).
#[must_use]
pub fn col_count() -> usize {
    DB_RESULT.with(|r| match &*r.borrow() {
        Some(SessionResult::Rows(rs)) => rs.columns.len(),
        _ => 0,
    })
}

/// Look at a column name of the staged result set.
pub fn with_col_name<R>(col: usize, f: impl FnOnce(Option<&str>) -> R) -> R {
    DB_RESULT.with(|r| match &*r.borrow() {
        Some(SessionResult::Rows(rs)) => f(rs.columns.get(col).map(|c| c.name.as_str())),
        _ => f(None),
    })
}

/// Look at a cell of the staged result set.
pub fn with_cell<R>(row: usize, col: usize, f: impl FnOnce(Option<&Value>) -> R) -> R {
    DB_RESULT.with(|r| match &*r.borrow() {
        Some(SessionResult::Rows(rs)) => f(rs.rows.get(row).and_then(|cells| cells.get(col))),
        _ => f(None),
    })
}

/// `(affected_rows, last_insert_id)` of the staged result. A staged result
/// set (or nothing staged) reports `(0, 0)` — `ephpm_db_execute` on a
/// SELECT is defined to return zeros rather than error.
#[must_use]
pub fn ok_info() -> (u64, u64) {
    DB_RESULT.with(|r| match &*r.borrow() {
        Some(SessionResult::Ok(ok)) => (ok.affected_rows, ok.last_insert_id),
        _ => (0, 0),
    })
}

/// Look at the staged error, if any.
pub fn with_error<R>(f: impl FnOnce(Option<(u16, &[u8; 5], &str)>) -> R) -> R {
    DB_ERROR.with(|e| match &*e.borrow() {
        Some(err) => f(Some((err.code, &err.sqlstate, err.message.as_str()))),
        None => f(None),
    })
}

// ── C-compatible ops struct (php_linked only) ───────────────────────────

/// Function pointer table passed to C so the PHP native `ephpm_db_*`
/// functions can call into the Rust bridge without knowing Rust types.
///
/// Layout mirrors `EphpmDbOps` in `ephpm_wrapper.c` — keep the two in
/// sync, appending only (same rule as `EphpmKvOps`).
#[cfg(php_linked)]
#[repr(C)]
pub struct EphpmDbOps {
    /// Reset the staged parameter list for this thread.
    pub params_begin: Option<unsafe extern "C" fn()>,
    /// Stage a NULL parameter.
    pub param_null: Option<unsafe extern "C" fn()>,
    /// Stage an integer parameter.
    pub param_int: Option<unsafe extern "C" fn(v: std::os::raw::c_longlong)>,
    /// Stage a float parameter.
    pub param_float: Option<unsafe extern "C" fn(v: f64)>,
    /// Stage a bytes parameter (UTF-8 → TEXT, else BLOB).
    pub param_bytes: Option<unsafe extern "C" fn(p: *const std::os::raw::c_char, len: usize)>,
    /// Execute SQL with the staged parameters. Returns 1 = result set
    /// staged, 2 = OK staged, -1 = error staged, -2 = no backend
    /// registered.
    pub run: Option<
        unsafe extern "C" fn(
            sql: *const std::os::raw::c_char,
            sql_len: usize,
        ) -> std::os::raw::c_int,
    >,
    /// Rows in the staged result set.
    pub row_count: Option<unsafe extern "C" fn() -> usize>,
    /// Columns in the staged result set.
    pub col_count: Option<unsafe extern "C" fn() -> usize>,
    /// Column name; `*p`/`*len` point into the staged result (valid until
    /// the next `run`/`finish` on this thread).
    pub col_name: Option<
        unsafe extern "C" fn(col: usize, p: *mut *const std::os::raw::c_char, len: *mut usize),
    >,
    /// Cell accessor. `*type_` = 0 null, 1 int (`*ival`), 2 float
    /// (`*fval`), 3 text / 4 blob (`*p`/`*len`, valid until the next
    /// `run`/`finish` on this thread).
    pub cell: Option<
        unsafe extern "C" fn(
            row: usize,
            col: usize,
            type_: *mut std::os::raw::c_int,
            ival: *mut std::os::raw::c_longlong,
            fval: *mut f64,
            p: *mut *const std::os::raw::c_char,
            len: *mut usize,
        ),
    >,
    /// `affected_rows` / `last_insert_id` of the staged OK result
    /// (zeros when a result set is staged instead).
    pub ok_info: Option<
        unsafe extern "C" fn(
            affected_rows: *mut std::os::raw::c_ulonglong,
            last_insert_id: *mut std::os::raw::c_ulonglong,
        ),
    >,
    /// Staged error triple. `*sqlstate` points at 5 bytes (NOT
    /// NUL-terminated); `*msg`/`*msg_len` at the message bytes. Valid
    /// until the next `run`/`finish` on this thread.
    pub error_info: Option<
        unsafe extern "C" fn(
            code: *mut std::os::raw::c_uint,
            sqlstate: *mut *const std::os::raw::c_char,
            msg: *mut *const std::os::raw::c_char,
            msg_len: *mut usize,
        ),
    >,
    /// Drop the staged result/error, releasing memory.
    pub finish: Option<unsafe extern "C" fn()>,
}

// ── C shims (php_linked only) ───────────────────────────────────────────

#[cfg(php_linked)]
unsafe extern "C" fn db_params_begin() {
    params_reset();
}

#[cfg(php_linked)]
unsafe extern "C" fn db_param_null() {
    param_push(Value::Null);
}

#[cfg(php_linked)]
unsafe extern "C" fn db_param_int(v: std::os::raw::c_longlong) {
    param_push(Value::Integer(v));
}

#[cfg(php_linked)]
unsafe extern "C" fn db_param_float(v: f64) {
    param_push(Value::Float(v));
}

#[cfg(php_linked)]
unsafe extern "C" fn db_param_bytes(p: *const std::os::raw::c_char, len: usize) {
    // SAFETY: `p` points to `len` bytes of a PHP string obtained via
    // zend_parse_parameters, valid for the duration of this call.
    let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
    param_push(bytes_to_value(bytes));
}

#[cfg(php_linked)]
unsafe extern "C" fn db_run(
    sql: *const std::os::raw::c_char,
    sql_len: usize,
) -> std::os::raw::c_int {
    // SAFETY: `sql` points to `sql_len` bytes of a PHP string obtained via
    // zend_parse_parameters, valid for the duration of this call.
    let bytes = unsafe { std::slice::from_raw_parts(sql.cast::<u8>(), sql_len) };
    match run_sql_bytes(bytes) {
        RunStatus::Rows => 1,
        RunStatus::Ok => 2,
        RunStatus::Err => -1,
        RunStatus::Unavailable => -2,
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn db_row_count() -> usize {
    row_count()
}

#[cfg(php_linked)]
unsafe extern "C" fn db_col_count() -> usize {
    col_count()
}

#[cfg(php_linked)]
unsafe extern "C" fn db_col_name(col: usize, p: *mut *const std::os::raw::c_char, len: *mut usize) {
    with_col_name(col, |name| {
        let (ptr, n) = name.map_or((std::ptr::null(), 0), |s| (s.as_ptr().cast(), s.len()));
        // SAFETY: `p` and `len` are valid pointers to locals in our C
        // wrapper (`PHP_FUNCTION(ephpm_db_query)`). The returned pointer
        // aims into the thread-local staged result, which is not dropped
        // or mutated until the next `run`/`finish` on this same thread —
        // the C side copies the bytes before returning to PHP.
        unsafe {
            *p = ptr;
            *len = n;
        }
    });
}

#[cfg(php_linked)]
unsafe extern "C" fn db_cell(
    row: usize,
    col: usize,
    type_: *mut std::os::raw::c_int,
    ival: *mut std::os::raw::c_longlong,
    fval: *mut f64,
    p: *mut *const std::os::raw::c_char,
    len: *mut usize,
) {
    with_cell(row, col, |cell| {
        // SAFETY: all out-pointers are valid locals in our C wrapper.
        // Byte pointers aim into the thread-local staged result — stable
        // until the next `run`/`finish` on this thread; the C side copies
        // them into zvals before returning to PHP.
        unsafe {
            *p = std::ptr::null();
            *len = 0;
            match cell {
                None | Some(Value::Null) => *type_ = 0,
                Some(Value::Integer(v)) => {
                    *type_ = 1;
                    *ival = *v;
                }
                Some(Value::Float(v)) => {
                    *type_ = 2;
                    *fval = *v;
                }
                Some(Value::Text(s)) => {
                    *type_ = 3;
                    *p = s.as_ptr().cast();
                    *len = s.len();
                }
                Some(Value::Blob(b)) => {
                    *type_ = 4;
                    *p = b.as_ptr().cast();
                    *len = b.len();
                }
            }
        }
    });
}

#[cfg(php_linked)]
unsafe extern "C" fn db_ok_info(
    affected_rows: *mut std::os::raw::c_ulonglong,
    last_insert_id: *mut std::os::raw::c_ulonglong,
) {
    let (affected, last_id) = ok_info();
    // SAFETY: both out-pointers are valid locals in our C wrapper
    // (`PHP_FUNCTION(ephpm_db_execute)`).
    unsafe {
        *affected_rows = affected;
        *last_insert_id = last_id;
    }
}

#[cfg(php_linked)]
unsafe extern "C" fn db_error_info(
    code: *mut std::os::raw::c_uint,
    sqlstate: *mut *const std::os::raw::c_char,
    msg: *mut *const std::os::raw::c_char,
    msg_len: *mut usize,
) {
    with_error(|err| {
        // SAFETY: all out-pointers are valid locals in our C wrapper. The
        // sqlstate/message pointers aim into the thread-local staged
        // error, stable until the next `run`/`finish` on this thread; the
        // C side formats them into the exception before returning.
        unsafe {
            match err {
                Some((c, state, message)) => {
                    *code = u32::from(c);
                    *sqlstate = state.as_ptr().cast();
                    *msg = message.as_ptr().cast();
                    *msg_len = message.len();
                }
                None => {
                    *code = 0;
                    *sqlstate = std::ptr::null();
                    *msg = std::ptr::null();
                    *msg_len = 0;
                }
            }
        }
    });
}

#[cfg(php_linked)]
unsafe extern "C" fn db_finish() {
    finish();
}

// ── Static ops table ────────────────────────────────────────────────────

/// The C-compatible function pointer table, ready to pass to
/// `ephpm_set_db_ops()`.
#[cfg(php_linked)]
pub static DB_OPS: EphpmDbOps = EphpmDbOps {
    params_begin: Some(db_params_begin),
    param_null: Some(db_param_null),
    param_int: Some(db_param_int),
    param_float: Some(db_param_float),
    param_bytes: Some(db_param_bytes),
    run: Some(db_run),
    row_count: Some(db_row_count),
    col_count: Some(db_col_count),
    col_name: Some(db_col_name),
    cell: Some(db_cell),
    ok_info: Some(db_ok_info),
    error_info: Some(db_error_info),
    finish: Some(db_finish),
};

// ── Tests ───────────────────────────────────────────────────────────────
//
// The bridge core is deliberately stub-safe (no PHP types), so its
// semantics — SET NAMES noop, SHOW TABLES metadata emulation, CRUD with
// params, error mapping, transaction flow — are covered by plain
// `cargo test` without a PHP SDK. The `php_linked`-gated module below
// additionally exercises the raw C shims (pointer contracts), mirroring
// kv_bridge's FFI tests; it requires a real libphp link to compile the
// ops table but does not need a PHP runtime.

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use litewire::backend::Rusqlite;
    use serial_test::serial;

    use super::*;

    /// One shared runtime + in-memory backend for every test in this
    /// process (the bridge's `OnceLock` can only be set once). Tables are
    /// namespaced per test to avoid cross-test interference. `pub(super)`
    /// so the sibling `recycle_tests` module can borrow the runtime for
    /// its locally-constructed bridges.
    pub(super) static TEST_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

    pub(super) fn init_bridge() {
        let rt = TEST_RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("test runtime")
        });
        let backend: SharedBackend =
            std::sync::Arc::new(Rusqlite::memory().expect("in-memory backend"));
        let _ = set_backend(backend, rt.handle().clone());
    }

    fn run(sql: &str) -> RunStatus {
        run_sql_bytes(sql.as_bytes())
    }

    fn cell_text(row: usize, col: usize) -> Option<String> {
        with_cell(row, col, |c| match c {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
    }

    // (a) SET NAMES — the Noop path: proves the bridge sits above
    // translate, not on raw SQLite (raw SQLite rejects SET NAMES).

    #[test]
    #[serial]
    fn set_names_returns_ok_noop() {
        init_bridge();
        assert_eq!(run("SET NAMES utf8mb4"), RunStatus::Ok);
        assert_eq!(ok_info(), (0, 0));
        finish();
    }

    // (b) SHOW TABLES — the metadata emulation path.

    #[test]
    #[serial]
    fn show_tables_returns_metadata_emulation() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_show_t (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(run("SHOW TABLES"), RunStatus::Rows);
        assert_eq!(col_count(), 1);
        let names: Vec<String> = (0..row_count()).filter_map(|r| cell_text(r, 0)).collect();
        assert!(
            names.contains(&"bridge_show_t".to_string()),
            "SHOW TABLES must list bridge_show_t, got: {names:?}"
        );
        finish();
    }

    // (c) Basic CRUD with params.

    #[test]
    #[serial]
    fn crud_with_params() {
        init_bridge();
        assert_eq!(
            run("CREATE TABLE bridge_crud (id INTEGER PRIMARY KEY, name TEXT, score REAL)"),
            RunStatus::Ok
        );

        params_reset();
        param_push(Value::Text("alice".into()));
        param_push(Value::Float(2.5));
        assert_eq!(run("INSERT INTO bridge_crud (name, score) VALUES (?, ?)"), RunStatus::Ok);
        let (affected, last_id) = ok_info();
        assert_eq!(affected, 1);
        assert_eq!(last_id, 1);

        params_reset();
        param_push(Value::Integer(1));
        assert_eq!(run("SELECT id, name, score FROM bridge_crud WHERE id = ?"), RunStatus::Rows);
        assert_eq!(row_count(), 1);
        assert_eq!(col_count(), 3);
        with_col_name(1, |n| assert_eq!(n, Some("name")));
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(1))));
        assert_eq!(cell_text(0, 1).as_deref(), Some("alice"));
        with_cell(0, 2, |c| assert_eq!(c, Some(&Value::Float(2.5))));

        params_reset();
        param_push(Value::Text("bob".into()));
        param_push(Value::Integer(1));
        assert_eq!(run("UPDATE bridge_crud SET name = ? WHERE id = ?"), RunStatus::Ok);
        assert_eq!(ok_info().0, 1);

        params_reset();
        param_push(Value::Integer(1));
        assert_eq!(run("DELETE FROM bridge_crud WHERE id = ?"), RunStatus::Ok);
        assert_eq!(run("SELECT COUNT(*) FROM bridge_crud"), RunStatus::Rows);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(0))));
        finish();
    }

    // (d) Errors surface with the mapped MySQL error info.

    #[test]
    #[serial]
    fn bad_sql_stages_parse_error() {
        init_bridge();
        assert_eq!(run("NOT VALID SQL !!! @@@ {{{}}"), RunStatus::Err);
        with_error(|e| {
            let (code, sqlstate, msg) = e.expect("error must be staged");
            assert_eq!(code, ER_PARSE_ERROR);
            assert_eq!(sqlstate, b"42000");
            assert!(!msg.is_empty());
        });
        finish();
    }

    #[test]
    #[serial]
    fn duplicate_key_stages_1062() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_dup (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_dup (id) VALUES (1)"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_dup (id) VALUES (1)"), RunStatus::Err);
        with_error(|e| {
            let (code, sqlstate, msg) = e.expect("error must be staged");
            assert_eq!(code, litewire::session::error_map::ER_DUP_ENTRY);
            assert_eq!(sqlstate, b"23000");
            assert!(msg.to_ascii_lowercase().contains("unique"), "got: {msg}");
        });
        finish();
    }

    #[test]
    #[serial]
    fn invalid_utf8_sql_is_a_clean_error() {
        init_bridge();
        assert_eq!(run_sql_bytes(&[0xFF, 0xFE, 0x00]), RunStatus::Err);
        with_error(|e| {
            let (code, ..) = e.expect("error must be staged");
            assert_eq!(code, ER_PARSE_ERROR);
        });
        finish();
    }

    // Transactions flow through as SQL and the Session tracks state.

    #[test]
    #[serial]
    fn transactions_flow_through_as_sql() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_txn (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(run("BEGIN"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_txn (id) VALUES (1)"), RunStatus::Ok);
        assert_eq!(run("ROLLBACK"), RunStatus::Ok);
        assert_eq!(run("SELECT COUNT(*) FROM bridge_txn"), RunStatus::Rows);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(0))));

        assert_eq!(run("BEGIN"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_txn (id) VALUES (2)"), RunStatus::Ok);
        assert_eq!(run("COMMIT"), RunStatus::Ok);
        assert_eq!(run("SELECT COUNT(*) FROM bridge_txn"), RunStatus::Rows);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(1))));
        finish();
    }

    // Param byte classification mirrors the wire frontend.

    #[test]
    fn bytes_to_value_classifies_utf8_vs_blob() {
        assert_eq!(bytes_to_value(b"hello"), Value::Text("hello".into()));
        assert_eq!(bytes_to_value(&[0xFF, 0xFE]), Value::Blob(vec![0xFF, 0xFE]));
        assert_eq!(bytes_to_value(b""), Value::Text(String::new()));
    }

    // Accessors are inert when nothing is staged.

    #[test]
    #[serial]
    fn accessors_are_inert_without_a_staged_result() {
        finish();
        assert_eq!(row_count(), 0);
        assert_eq!(col_count(), 0);
        assert_eq!(ok_info(), (0, 0));
        with_col_name(0, |n| assert!(n.is_none()));
        with_cell(0, 0, |c| assert!(c.is_none()));
        with_error(|e| assert!(e.is_none()));
    }

    // ── Per-request teardown (on_request_end) ──────────────────────────

    /// Whether this thread's session currently believes it is inside an
    /// explicit transaction (None = no session open).
    fn thread_in_transaction() -> Option<bool> {
        DB_SESSION.with(|s| s.borrow().as_ref().map(|sess| sess.in_transaction))
    }

    #[test]
    #[serial]
    fn request_end_rolls_back_an_abandoned_transaction() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_req_end (id INTEGER PRIMARY KEY)"), RunStatus::Ok);

        // Simulate a script that BEGINs, writes, and never COMMITs.
        assert_eq!(run("BEGIN"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_req_end (id) VALUES (1)"), RunStatus::Ok);
        assert_eq!(thread_in_transaction(), Some(true));

        on_request_end();

        // The transaction is gone and the write with it; the session (and
        // its connection) survive for the next request.
        assert_eq!(thread_in_transaction(), Some(false));
        assert_eq!(run("SELECT COUNT(*) FROM bridge_req_end"), RunStatus::Rows);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(0))));

        // And a committed transaction on the recovered session works.
        assert_eq!(run("BEGIN"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_req_end (id) VALUES (2)"), RunStatus::Ok);
        assert_eq!(run("COMMIT"), RunStatus::Ok);
        assert_eq!(run("SELECT COUNT(*) FROM bridge_req_end"), RunStatus::Rows);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(1))));
        finish();
    }

    #[test]
    #[serial]
    fn request_end_clears_staged_results_and_is_noop_without_a_transaction() {
        init_bridge();
        assert_eq!(run("SELECT 1"), RunStatus::Rows);
        assert_eq!(row_count(), 1);

        on_request_end();

        // Staged result released; autocommit session untouched.
        assert_eq!(row_count(), 0);
        assert_eq!(thread_in_transaction(), Some(false));
        assert_eq!(run("SELECT 1"), RunStatus::Rows);
        finish();
    }

    #[test]
    fn request_end_without_a_session_is_a_noop() {
        // This test's thread never ran a query, so no session exists.
        // (Thread-locals are per-test-thread; no #[serial] needed.)
        assert_eq!(thread_in_transaction(), None);
        on_request_end();
        assert_eq!(thread_in_transaction(), None);
    }
}

// ── Session-recycling tests (mock backend, local bridge) ────────────────
//
// These drive `run_on` with a locally-constructed `DbBridge` wrapping a
// mock backend, so they never touch the process-wide set-once DB_BRIDGE
// and need no #[serial]: every thread-local involved (DB_SESSION,
// DB_PARAMS, DB_RESULT, DB_ERROR) is private to the test's own thread.
#[cfg(test)]
mod recycle_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use litewire::backend::{
        Backend, BackendConn, BackendError, ExecuteResult, ResultSet, Rusqlite,
    };

    use super::tests::init_bridge;
    use super::*;

    /// Counters shared between a test and its [`FlakyBackend`].
    #[derive(Default)]
    struct Flags {
        /// Successful `connect()` calls — proves when a reconnect happened.
        connects: AtomicUsize,
        /// While set, every query/execute on existing connections fails
        /// with a connection-shaped error (the backend is "down").
        down: AtomicBool,
    }

    /// A backend whose connections start failing on demand, with a
    /// counting `connect()` that keeps succeeding — the "counting factory"
    /// proving that dropping the session makes the next call reconnect.
    struct FlakyBackend {
        /// Real in-memory engine serving the non-failing calls.
        inner: Rusqlite,
        flags: Arc<Flags>,
    }

    struct FlakyConn {
        inner: Box<dyn BackendConn>,
        flags: Arc<Flags>,
    }

    impl FlakyConn {
        fn outage() -> BackendError {
            // Verbatim shape of hrana_client's pipeline-POST failure with
            // a reqwest connect error nested inside.
            BackendError::Other(
                "HTTP request failed: error sending request for url \
                 (http://127.0.0.1:8081/v2/pipeline): connection refused"
                    .to_string(),
            )
        }
    }

    #[async_trait::async_trait]
    impl Backend for FlakyBackend {
        async fn connect(&self) -> Result<Box<dyn BackendConn>, BackendError> {
            let inner = self.inner.connect().await?;
            self.flags.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FlakyConn { inner, flags: Arc::clone(&self.flags) }))
        }
    }

    #[async_trait::async_trait]
    impl BackendConn for FlakyConn {
        async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet, BackendError> {
            if self.flags.down.load(Ordering::SeqCst) {
                return Err(Self::outage());
            }
            self.inner.query(sql, params).await
        }

        async fn execute(
            &self,
            sql: &str,
            params: &[Value],
        ) -> Result<ExecuteResult, BackendError> {
            if self.flags.down.load(Ordering::SeqCst) {
                return Err(Self::outage());
            }
            self.inner.execute(sql, params).await
        }
    }

    /// A local bridge over a [`FlakyBackend`] (plus the shared flags).
    fn flaky_bridge() -> (DbBridge, Arc<Flags>) {
        init_bridge(); // ensures the shared test runtime exists
        let flags = Arc::new(Flags::default());
        let backend: SharedBackend = Arc::new(FlakyBackend {
            inner: Rusqlite::memory().expect("in-memory backend"),
            flags: Arc::clone(&flags),
        });
        let handle = super::tests::TEST_RT.get().expect("runtime").handle().clone();
        (DbBridge { backend, handle, cache: Arc::new(TranslateCache::default()) }, flags)
    }

    fn run(bridge: &DbBridge, sql: &str) -> RunStatus {
        run_on(Some(bridge), sql.as_bytes())
    }

    #[test]
    fn connection_error_recycles_and_next_call_reconnects() {
        let (bridge, flags) = flaky_bridge();

        // First call opens the session: one connect.
        assert_eq!(run(&bridge, "SELECT 1"), RunStatus::Rows);
        assert_eq!(flags.connects.load(Ordering::SeqCst), 1);

        // Backend "restarts": the existing connection now fails with a
        // connection-shaped error. The error is surfaced to PHP...
        flags.down.store(true, Ordering::SeqCst);
        assert_eq!(run(&bridge, "SELECT 1"), RunStatus::Err);
        with_error(|e| {
            let (code, sqlstate, msg) = e.expect("error must be staged");
            assert_eq!(code, ER_UNKNOWN_ERROR);
            assert_eq!(sqlstate, b"HY000");
            assert!(msg.contains("connection refused"), "got: {msg}");
        });
        // ...without opening a new connection during the failing call.
        assert_eq!(flags.connects.load(Ordering::SeqCst), 1);

        // Backend is back: the NEXT call reconnects (counting factory
        // shows a second connect) and succeeds.
        flags.down.store(false, Ordering::SeqCst);
        assert_eq!(run(&bridge, "SELECT 1"), RunStatus::Rows);
        assert_eq!(flags.connects.load(Ordering::SeqCst), 2);
        finish();
    }

    #[test]
    fn sql_errors_do_not_recycle_the_session() {
        let (bridge, flags) = flaky_bridge();

        assert_eq!(run(&bridge, "CREATE TABLE rt_keep (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(flags.connects.load(Ordering::SeqCst), 1);

        // 1062 duplicate key: classified, never connection-shaped.
        assert_eq!(run(&bridge, "INSERT INTO rt_keep (id) VALUES (1)"), RunStatus::Ok);
        assert_eq!(run(&bridge, "INSERT INTO rt_keep (id) VALUES (1)"), RunStatus::Err);

        // 1105 fallback with a non-connection message ("no such table").
        assert_eq!(run(&bridge, "SELECT * FROM rt_missing"), RunStatus::Err);

        // 1064 translate error: never reaches the backend at all.
        assert_eq!(run(&bridge, "NOT VALID SQL !!!"), RunStatus::Err);

        // Same session throughout — no reconnect happened, and the
        // session still sees the table it created.
        assert_eq!(run(&bridge, "SELECT COUNT(*) FROM rt_keep"), RunStatus::Rows);
        assert_eq!(flags.connects.load(Ordering::SeqCst), 1);
        finish();
    }

    #[test]
    fn connection_error_inside_a_transaction_does_not_recycle() {
        let (bridge, flags) = flaky_bridge();

        assert_eq!(run(&bridge, "CREATE TABLE rt_txn (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(run(&bridge, "BEGIN"), RunStatus::Ok);
        assert_eq!(run(&bridge, "INSERT INTO rt_txn (id) VALUES (1)"), RunStatus::Ok);

        // A connection-shaped failure mid-transaction must NOT drop the
        // session (that would silently discard transaction state on a
        // possible misclassification).
        flags.down.store(true, Ordering::SeqCst);
        assert_eq!(run(&bridge, "SELECT 1"), RunStatus::Err);
        assert_eq!(flags.connects.load(Ordering::SeqCst), 1);

        // The session survived, still mid-transaction, and recovers once
        // the backend does.
        flags.down.store(false, Ordering::SeqCst);
        assert_eq!(run(&bridge, "ROLLBACK"), RunStatus::Ok);
        assert_eq!(run(&bridge, "SELECT COUNT(*) FROM rt_txn"), RunStatus::Rows);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(0))));
        assert_eq!(flags.connects.load(Ordering::SeqCst), 1);
        finish();
    }

    #[test]
    fn classification_is_conservative() {
        // Connection-shaped: 1105 + a known transport marker.
        let conn_err = SessionError::Db {
            code: ER_UNKNOWN_ERROR,
            sqlstate: *b"HY000",
            message: "sqld returned 400 Bad Request: Invalid baton".to_string(),
        };
        assert!(is_connection_error(&conn_err));

        // 1105 with an ordinary SQL message: not connection-shaped.
        let sql_err = SessionError::Db {
            code: ER_UNKNOWN_ERROR,
            sqlstate: *b"HY000",
            message: "no such table: sprockets".to_string(),
        };
        assert!(!is_connection_error(&sql_err));

        // A classified code never qualifies, whatever the message says.
        let classified = SessionError::Db {
            code: litewire::session::error_map::ER_DUP_ENTRY,
            sqlstate: *b"23000",
            message: "UNIQUE constraint failed: connection refused".to_string(),
        };
        assert!(!is_connection_error(&classified));
    }
}

// FFI shim tests: exercise the raw C-ABI layer (pointer contracts) the
// same way kv_bridge's php_linked tests do. Compiled and run only with a
// real libphp link (`cargo test` after `cargo xtask release`); they need
// no PHP runtime, only the `php_linked` cfg that gates the ops table.
#[cfg(all(test, php_linked))]
mod ffi_tests {
    use serial_test::serial;

    use super::tests::init_bridge;
    use super::*;

    unsafe fn run_c(sql: &str) -> std::os::raw::c_int {
        // SAFETY (caller): sql lives for the duration of the call.
        unsafe { db_run(sql.as_ptr().cast(), sql.len()) }
    }

    #[test]
    #[serial]
    fn ffi_set_names_returns_ok() {
        init_bridge();
        // SAFETY: valid pointer + length.
        let rc = unsafe { run_c("SET NAMES utf8mb4") };
        assert_eq!(rc, 2, "SET NAMES must be an OK (noop) result");
        let mut affected: std::os::raw::c_ulonglong = 9;
        let mut last_id: std::os::raw::c_ulonglong = 9;
        // SAFETY: valid out-pointers.
        unsafe { db_ok_info(&mut affected, &mut last_id) };
        assert_eq!((affected, last_id), (0, 0));
        // SAFETY: no arguments.
        unsafe { db_finish() };
    }

    #[test]
    #[serial]
    fn ffi_show_tables_and_cells() {
        init_bridge();
        // SAFETY: valid pointer + length.
        assert_eq!(
            unsafe { run_c("CREATE TABLE bridge_ffi_t (id INTEGER PRIMARY KEY, name TEXT)") },
            2
        );
        unsafe { db_params_begin() };
        let name = b"ffi";
        // SAFETY: valid pointer + length.
        unsafe { db_param_bytes(name.as_ptr().cast(), name.len()) };
        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("INSERT INTO bridge_ffi_t (name) VALUES (?)") }, 2);

        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("SHOW TABLES") }, 1);
        // SAFETY: no arguments.
        let rows = unsafe { db_row_count() };
        assert!(rows >= 1);

        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("SELECT id, name FROM bridge_ffi_t") }, 1);
        let mut ty: std::os::raw::c_int = -1;
        let mut ival: std::os::raw::c_longlong = 0;
        let mut fval: f64 = 0.0;
        let mut p: *const std::os::raw::c_char = std::ptr::null();
        let mut len: usize = 0;
        // SAFETY: valid out-pointers.
        unsafe { db_cell(0, 0, &mut ty, &mut ival, &mut fval, &mut p, &mut len) };
        assert_eq!((ty, ival), (1, 1));
        // SAFETY: valid out-pointers.
        unsafe { db_cell(0, 1, &mut ty, &mut ival, &mut fval, &mut p, &mut len) };
        assert_eq!(ty, 3);
        // SAFETY: p/len come from the staged result, valid until finish.
        let got = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
        assert_eq!(got, b"ffi");

        let mut np: *const std::os::raw::c_char = std::ptr::null();
        let mut nlen: usize = 0;
        // SAFETY: valid out-pointers.
        unsafe { db_col_name(1, &mut np, &mut nlen) };
        // SAFETY: np/nlen come from the staged result, valid until finish.
        let cname = unsafe { std::slice::from_raw_parts(np.cast::<u8>(), nlen) };
        assert_eq!(cname, b"name");
        // SAFETY: no arguments.
        unsafe { db_finish() };
    }

    #[test]
    #[serial]
    fn ffi_error_info_carries_mapped_triple() {
        init_bridge();
        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("TOTALLY ((( not sql") }, -1);
        let mut code: std::os::raw::c_uint = 0;
        let mut state: *const std::os::raw::c_char = std::ptr::null();
        let mut msg: *const std::os::raw::c_char = std::ptr::null();
        let mut msg_len: usize = 0;
        // SAFETY: valid out-pointers.
        unsafe { db_error_info(&mut code, &mut state, &mut msg, &mut msg_len) };
        assert_eq!(code, u32::from(ER_PARSE_ERROR));
        // SAFETY: state points at 5 staged bytes, valid until finish.
        let sqlstate = unsafe { std::slice::from_raw_parts(state.cast::<u8>(), 5) };
        assert_eq!(sqlstate, b"42000");
        assert!(msg_len > 0);
        // SAFETY: no arguments.
        unsafe { db_finish() };
    }
}

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
//! # Single backend vs. per-site registry
//!
//! The bridge resolves the backend for a query from a [`BackendSource`]:
//!
//! * [`BackendSource::Single`] — one process-global backend (single-site
//!   embedded Turso, or the single-node DB-proxy path). The request's site
//!   is ignored; every query hits the one backend, exactly as before per-site
//!   isolation existed.
//! * [`BackendSource::PerSite`] — a [`SiteBackendResolver`] (implemented by
//!   ephpm-server's site-backend registry) that maps the **current request's
//!   validated site key** to that site's own database, lazily opening and
//!   caching it. This is the secure-multi-tenancy path: tenant A and tenant B
//!   resolve to different database files, so A's SQL cannot reach B's data.
//!   The site key is set per request by the router via [`set_current_site`]
//!   before PHP runs; a query with no site context (per-site mode but no key)
//!   **fails closed** — it never falls back to a shared default database.
//!
//! # One Session per (PHP thread, site)
//!
//! Each OS thread that executes PHP holds at most one live [`Session`]
//! (thread-local), keyed by the site it belongs to. A worker thread that
//! served site A and is then dispatched a request for site B does **not**
//! reuse A's connection: the held session is swapped for a fresh one against
//! B's backend (A's session and its pin on A's database are dropped). Within a
//! single request the site never changes, so the swap only ever happens
//! between requests. The session translates MySQL-dialect SQL, runs the
//! metadata emulation for `SHOW`/`DESCRIBE`, returns OK for dialect no-ops
//! like `SET NAMES`, and tracks transaction state — `BEGIN`/`COMMIT`/`ROLLBACK`
//! flow through as plain SQL, exactly as they do on the wire path.
//!
//! The held session keeps a clone of the registry's backend `Arc`, which
//! *pins that site's database open*. The registry's LRU eviction is therefore
//! refcount-aware: it only closes a site whose backend `Arc` has no live
//! session clone (see ephpm-server's `site_backends`). Swapping away from a
//! site at the next differing request drops that pin, letting an idle site
//! become evictable.
//!
//! # Defense-in-depth SQL screening
//!
//! Every statement on the tenant query path is screened
//! ([`screen_sql`]) and `ATTACH`/`DETACH`/`VACUUM` plus path-bearing
//! `PRAGMA`s are rejected before reaching the backend — independently of
//! Turso already refusing them. `ATTACH` is the exact cross-tenant primitive
//! of issue #274 (read/write another site's file, or plant a PHP shell in it):
//! making the refusal ePHPm's own, not the pinned engine's default, keeps the
//! property if a future engine bump ever flips that default. A leading
//! `EXPLAIN` / `EXPLAIN QUERY PLAN` diagnostic wrapper is seen through before
//! the check, so `EXPLAIN ATTACH …` is refused just like `ATTACH …` rather
//! than sliding under a leading-keyword match.
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
//! # Thread teardown must never drop a session (issue #300)
//!
//! The held session lives in a `thread_local!`. It is **never dropped by that
//! thread-local's destructor**: at thread teardown the session is *parked* in a
//! process-global graveyard ([`park_session`]) and dropped later, from ordinary
//! code, by [`drain_parked_sessions`].
//!
//! This is not defensive tidiness — dropping it at teardown aborted the process.
//! The mechanism, exactly:
//!
//! 1. A thread's first `ephpm_db_*` call enters `DB_HELD.with(…)`, which
//!    initializes the thread-local and **registers its destructor** with the
//!    platform (`__cxa_thread_atexit_impl` on glibc).
//! 2. *Inside that same call*, the query reaches the Turso engine, which
//!    allocates a scratch page buffer through its own `thread_local!`
//!    (`turso_core::io::TEMP_BUFFER_CACHE`) — registering **that** destructor
//!    strictly *after* ours.
//! 3. TLS destructors run in reverse registration order, so on thread exit
//!    `TEMP_BUFFER_CACHE` is destroyed **first** and marked inaccessible. Ours
//!    runs next, drops the [`Session`] → the backend connection → the pager's
//!    live `turso_core::io::Buffer`s — and `Buffer::drop` calls
//!    `TEMP_BUFFER_CACHE.with(…)` to recycle each buffer.
//! 4. `LocalKey::with` on a destroyed value panics
//!    (`cannot access a Thread Local Storage value during or after
//!    destruction`), and a panic inside a TLS destructor is a hard
//!    `fatal runtime error: thread local panicked on drop` → **`SIGABRT`**.
//!
//! Every tenant in the process dies, minutes after the burst that caused it —
//! tokio reaps idle blocking threads long after a load spike, which is when this
//! was observed (three of ten flooded instances, mid-flood, in post-flood
//! cooldown, and at `SIGTERM`).
//!
//! Note what does **not** fix it: using `try_with` in *this* module. The
//! panicking `with` is Turso's, several frames below our drop glue, and we
//! cannot reach it. Nor does field ordering inside [`HeldSession`] — both fields
//! own engine state. The only fix available to an embedder is to not run that
//! drop on a dying thread at all, which is what parking does. It mirrors the
//! `[php] crash_containment` philosophy (see `ephpm-server`'s `fpm_pool`):
//! **when teardown is unsafe, abandon the state rather than tear it down.**
//!
//! The cost is bounded and temporary, not a leak: a parked session holds its
//! site's backend `Arc` (so that database stays open, and un-evictable, a little
//! longer) until the next [`on_request_end`] on any PHP thread drains and drops
//! it. The graveyard therefore holds at most "sessions of threads that retired
//! since the last request", and the drain is a single relaxed atomic load when
//! it is empty — which it is on every request in a steady-state server.
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
//!
//! # What survives `finish()` — the last-call record (issues #259/#262/#263)
//!
//! A statement's *rows* are large and short-lived; its *metadata* is small and
//! wanted afterwards. The two are therefore staged separately:
//!
//! * [`DB_ROWS`] holds `Vec<Vec<Value>>` — released by [`finish`], which the C
//!   side calls as soon as it has copied the cells into PHP zvals.
//! * [`DB_LAST`] holds the [`LastCall`] record — "did the statement produce a
//!   rowset", the column metadata, and `affected_rows`/`last_insert_id`. It is
//!   **not** released by [`finish`]; it lives until the next statement on this
//!   thread or [`on_request_end`].
//! * [`DB_ERROR`] holds the error triple under the same rule.
//!
//! The split is free rather than a copy: a `SessionResult::Rows` is
//! destructured and its `columns` **moved** into the record while its `rows`
//! move into [`DB_ROWS`].
//!
//! Three adapter-facing gaps close as a direct consequence:
//!
//! * **Has-rowset (#263).** Adapters no longer classify SQL by its first
//!   keyword to choose between `ephpm_db_query` and `ephpm_db_execute` — the
//!   record says what actually happened, which is what `ephpm_db_run()`
//!   returns.
//! * **Empty-set metadata (#262).** A zero-row `SELECT` returns `[]` rows but
//!   its columns are still in the record, so `ephpm_db_columns()` can describe
//!   a result that has no rows to describe it with.
//! * **Error classification (#259).** The staged error outliving the throw is
//!   what lets `ephpm_db_errno()` / `ephpm_db_error()` report on a statement
//!   after its exception was caught.
//!
//! # Reserved error codes
//!
//! Every `ephpm_db_*` failure carries a nonzero code. SQL failures carry the
//! MySQL **server** error code litewire's `error_map` assigned (1062, 1064,
//! 1105, ...). Infrastructure failures — the bridge is not there, or this
//! request has no database to reach — carry one of the reserved codes below,
//! which is what makes the two distinguishable without parsing message text.
//!
//! The reserved values sit in MySQL's **client**-error range (2000-2999,
//! `CR_*`), which a server never emits: `error_map` can only ever produce a
//! server-range code, so a reserved code can never collide with a real SQL
//! error. See [`ERR_UNAVAILABLE`], [`ERR_NO_SITE_CONTEXT`], [`ERR_CONNECT`],
//! and (C-side only) `EPHPM_DB_ERR_BAD_PARAM`.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use litewire::backend::{Column, SharedBackend, Value};
use litewire::session::ER_PARSE_ERROR;
use litewire::session::error_map::ER_UNKNOWN_ERROR;
use litewire::translate::{Dialect, TranslateCache};
use litewire::{Session, SessionError, SessionResult};

// ── Reserved infrastructure error codes (issue #259) ────────────────────

/// No embedded database is active at all — `[db.sqlite]` is not configured,
/// so no backend was ever registered with the bridge.
///
/// This is an *infrastructure* condition, not a SQL error: the statement never
/// reached a database. Adapters key on this to fall back to another driver (or
/// to report "not running under ePHPm with an embedded database") instead of
/// reporting a broken query.
pub const ERR_UNAVAILABLE: u16 = 2000;

/// A backend is registered, but this request has no tenant identity, so the
/// bridge cannot decide *which* per-site database to use — and deliberately
/// refuses to guess (see the module docs, `Single backend vs. per-site
/// registry`).
///
/// Reached in per-site mode when the `Host` matched no configured site.
pub const ERR_NO_SITE_CONTEXT: u16 = 2001;

/// The database for this request is known but could not be opened, or the
/// session against it could not be established.
///
/// Distinct from [`ERR_UNAVAILABLE`]: the bridge is configured and the tenant
/// is known — the storage underneath failed.
pub const ERR_CONNECT: u16 = 2002;

/// SQLSTATE reported alongside every reserved infrastructure code. `HY000`
/// ("general error") is what a MySQL client library reports for its own
/// client-side failures, which is exactly what these are.
const INFRA_SQLSTATE: [u8; 5] = *b"HY000";

// ── Global bridge handle ────────────────────────────────────────────────

/// Resolves a request's validated site key to that site's backend.
///
/// Implemented by ephpm-server's per-site backend registry (`site_backends`).
/// The registry lazily opens and caches one database per site and evicts idle
/// ones under an LRU cap. Kept as a trait here so `ephpm-php` stays ignorant of
/// Turso, config, and on-disk layout — it only needs "give me the backend for
/// this site key".
///
/// `resolve` may block (open a database on first use); it is only ever called
/// from PHP worker / `spawn_blocking` threads, never async tasks — the same
/// invariant that licenses the bridge's `block_on` (see the `Async boundary`
/// module docs).
pub trait SiteBackendResolver: Send + Sync {
    /// Return a clone of the shared backend `Arc` for `site_key`, opening and
    /// caching the site's database if this is its first use. The returned
    /// clone pins the database open for as long as the caller (a thread-local
    /// [`Session`]) holds it.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when the key is invalid or the
    /// database cannot be opened. The bridge surfaces it to PHP as an
    /// exception and **never** falls back to another site's database.
    fn resolve(&self, site_key: &str) -> Result<SharedBackend, String>;
}

/// Where the bridge gets the backend for a query.
enum BackendSource {
    /// One process-global backend; the request's site is ignored.
    Single(SharedBackend),
    /// Per-site registry; the backend is chosen by the current request's
    /// site key (fails closed when no key is set).
    PerSite(Arc<dyn SiteBackendResolver>),
}

/// Everything a thread needs to open and drive a session.
struct DbBridge {
    /// How to resolve the backend for a query — one global backend, or a
    /// per-site registry.
    source: BackendSource,
    /// The server's tokio runtime, pinned so sync FFI callbacks can
    /// `block_on` session work.
    handle: tokio::runtime::Handle,
    /// Translation cache shared by every bridge session on this process
    /// (the wire frontends keep their own; both are per-frontend by
    /// design in litewire).
    cache: Arc<TranslateCache>,
}

/// Site key used for the [`BackendSource::Single`] held session. Real
/// per-site keys are validated `[a-z0-9._-]`, so the empty string can never
/// collide with one.
const SINGLE_SITE_KEY: &str = "";

static DB_BRIDGE: OnceLock<DbBridge> = OnceLock::new();

/// Register a single process-global backend backing the PHP `ephpm_db_*`
/// functions. First registration wins; later calls are no-ops (mirrors
/// [`crate::kv_bridge::set_store`]).
///
/// Returns `true` if this call performed the registration.
pub fn set_backend(backend: SharedBackend, handle: tokio::runtime::Handle) -> bool {
    let registered = DB_BRIDGE
        .set(DbBridge {
            source: BackendSource::Single(backend),
            handle,
            cache: Arc::new(TranslateCache::default()),
        })
        .is_ok();
    if registered {
        tracing::debug!("db backend registered for PHP native functions (single-backend mode)");
    }
    registered
}

/// Register a per-site backend resolver backing the PHP `ephpm_db_*`
/// functions (multi-site secure-multi-tenancy mode). First registration wins.
///
/// Returns `true` if this call performed the registration.
pub fn set_resolver(
    resolver: Arc<dyn SiteBackendResolver>,
    handle: tokio::runtime::Handle,
) -> bool {
    let registered = DB_BRIDGE
        .set(DbBridge {
            source: BackendSource::PerSite(resolver),
            handle,
            cache: Arc::new(TranslateCache::default()),
        })
        .is_ok();
    if registered {
        tracing::debug!("db backend registered for PHP native functions (per-site mode)");
    }
    registered
}

/// Whether a backend has been registered.
#[must_use]
pub fn is_configured() -> bool {
    DB_BRIDGE.get().is_some()
}

/// Set the site key for the current request on this thread (per-site mode).
///
/// Called by the request handler before PHP execution, mirroring
/// [`crate::kv_bridge::set_site_store`]. In single-backend mode this is
/// unnecessary (the key is ignored) and typically not called. Passing `None`
/// clears any previous key so a subsequent query in per-site mode fails closed
/// rather than silently reusing a stale site.
pub fn set_current_site(site_key: Option<&str>) {
    DB_CURRENT_SITE.with(|s| {
        *s.borrow_mut() = site_key.map(Box::from);
    });
}

// ── Thread-local state ──────────────────────────────────────────────────

thread_local! {
    /// The per-thread held session, created lazily on first use and swapped
    /// when the request's site changes. `None` until the thread's first query.
    ///
    /// Wrapped in [`HeldCell`] so that thread teardown **parks** the session
    /// instead of dropping it — see the module docs, `Thread teardown must
    /// never drop a session`.
    static DB_HELD: HeldCell = const { HeldCell(RefCell::new(None)) };
    /// The current request's site key on this thread (per-site mode). Set by
    /// [`set_current_site`] before PHP runs; `None` outside per-site mode or
    /// before it is set.
    static DB_CURRENT_SITE: RefCell<Option<Box<str>>> = const { RefCell::new(None) };
    /// Parameters staged by `param_*` calls for the next `run`.
    static DB_PARAMS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
    /// Rows of the last successful `run` on this thread. Released by
    /// [`finish`] as soon as the C side has copied them into PHP zvals — this
    /// is the large, short-lived half of a result.
    static DB_ROWS: RefCell<Vec<Vec<Value>>> = const { RefCell::new(Vec::new()) };
    /// Metadata about the last `run` on this thread. Deliberately **outlives**
    /// [`finish`] — see the module docs, `What survives finish()`.
    static DB_LAST: RefCell<LastCall> = const { RefCell::new(LastCall::EMPTY) };
    /// Error from the last failed `run` on this thread. Outlives [`finish`]
    /// under the same rule as [`DB_LAST`], which is what lets
    /// `ephpm_db_errno()` report on a statement whose exception was caught.
    static DB_ERROR: RefCell<Option<BridgeError>> = const { RefCell::new(None) };
}

/// What is remembered about the last statement executed on this thread once
/// its rows have been released.
///
/// Small by construction — column metadata and two counters — so retaining it
/// across [`finish`] costs a few hundred bytes per PHP thread, not a copy of
/// the result set.
struct LastCall {
    /// Whether the statement produced a result set (as opposed to an OK).
    ///
    /// This is the authoritative has-rowset signal of issue #263: it comes
    /// from the executed [`SessionResult`], not from inspecting the SQL, so
    /// `WITH ... INSERT`, `INSERT ... RETURNING`, `CALL`, and any future
    /// dialect surprise classify correctly for free.
    had_rowset: bool,
    /// Column metadata of the result set, **moved** out of the `ResultSet`
    /// rather than cloned. Present even when the set had zero rows — the
    /// point of issue #262. Empty when `had_rowset` is false.
    columns: Vec<Column>,
    /// `affected_rows` of the OK result (0 for a result set).
    affected_rows: u64,
    /// `last_insert_id` of the OK result (0 for a result set).
    last_insert_id: u64,
}

impl LastCall {
    /// The "nothing has run on this thread yet" value. A `const` rather than
    /// a `Default` impl so the thread-local can be initialized without
    /// allocating (`Vec::new` does not allocate).
    const EMPTY: Self =
        Self { had_rowset: false, columns: Vec::new(), affected_rows: 0, last_insert_id: 0 };
}

/// A thread's live session together with the site it belongs to and a clone
/// of that site's backend `Arc` (which pins the database open — see the
/// module docs, `One Session per (PHP thread, site)`).
struct HeldSession {
    /// The site this session's connection belongs to ([`SINGLE_SITE_KEY`] in
    /// single-backend mode).
    site: Box<str>,
    /// The litewire session driving this thread's queries for `site`.
    ///
    /// **Declared before `_backend` on purpose.** Struct fields drop in
    /// declaration order, so this releases the site's *connection* before the
    /// pin below releases the site's *database*. The registry treats
    /// "`strong_count == 1`" as "idle, safe to close" (see ephpm-server's
    /// `site_backends`, invariant 2); dropping the pin first would open a
    /// window — however narrow — in which the registry could close and re-open
    /// the file while this connection was still alive.
    session: Session,
    /// Clone of the registry backend `Arc`, held so the site's database stays
    /// open for the session's lifetime and the registry's refcount-aware LRU
    /// never evicts a site with a live session. Never read — its `Drop` is the
    /// whole point, as it releases the site's pin. That happens when the session
    /// is swapped out (immediately, in-band) or when a retired thread's parked
    /// session is drained ([`drain_parked_sessions`]) — never at thread teardown
    /// itself, which is the #300 abort.
    #[allow(dead_code)]
    _backend: SharedBackend,
}

/// Thread-local home of [`HeldSession`], whose whole purpose is its [`Drop`]:
/// it hands the session to the graveyard instead of dropping it on a dying
/// thread (issue #300 — see the module docs for why that abort happens).
///
/// Only *thread teardown* runs this destructor. The ordinary in-band paths
/// (`*slot = None` on a site swap or a dead connection) assign through the inner
/// `RefCell` and drop the session immediately, on a live thread, exactly as
/// before — this wrapper does not defer those.
struct HeldCell(RefCell<Option<HeldSession>>);

impl Drop for HeldCell {
    fn drop(&mut self) {
        // Reached only from the thread-local destructor. `take()` moves the
        // session out without touching any of its fields, so no engine drop
        // glue — and therefore no foreign `LocalKey::with` — runs here.
        if let Some(held) = self.0.get_mut().take() {
            park_session(held);
        }
    }
}

/// Sessions rescued from retiring threads, waiting to be dropped from ordinary
/// code. Drained by [`drain_parked_sessions`].
///
/// `Mutex<Vec<_>>` is chosen for what it does *not* do on the park side: locking
/// a `std::sync::Mutex` and pushing to a `Vec` touch no Rust thread-local (the
/// futex/SRWLOCK fast path and the system allocator only), so the whole park
/// operation is safe from inside a TLS destructor — which is the one place it is
/// ever called from.
static PARKED_SESSIONS: Mutex<Vec<HeldSession>> = Mutex::new(Vec::new());

/// Number of sessions in [`PARKED_SESSIONS`], so the per-request drain is a
/// relaxed load rather than a lock acquisition in the (overwhelmingly common)
/// empty case.
static PARKED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Hand a retiring thread's session to the graveyard. Infallible by
/// construction: a poisoned lock is recovered rather than unwrapped, because a
/// panic here is a process abort (this runs inside a TLS destructor).
fn park_session(held: HeldSession) {
    let mut parked = match PARKED_SESSIONS.lock() {
        Ok(guard) => guard,
        // Poisoned only if some other thread panicked while draining. The Vec is
        // still structurally sound and the alternative — unwinding out of a TLS
        // destructor — is the very abort this function exists to prevent.
        Err(poisoned) => poisoned.into_inner(),
    };
    parked.push(held);
    PARKED_COUNT.store(parked.len(), Ordering::Release);
    #[cfg(test)]
    PARKED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Drop every parked session, on the calling (live) thread.
///
/// Called from [`on_request_end`], i.e. from ordinary code on a PHP worker
/// thread whose thread-locals are all alive — so the engine drop glue that
/// aborts at teardown runs harmlessly here. Cheap when there is nothing parked:
/// one relaxed atomic load.
///
/// Public so a future shutdown path can flush the graveyard explicitly; nothing
/// outside this module needs to call it for correctness.
pub fn drain_parked_sessions() {
    if PARKED_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let parked = {
        let mut guard = match PARKED_SESSIONS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        PARKED_COUNT.store(0, Ordering::Release);
        std::mem::take(&mut *guard)
    };
    if parked.is_empty() {
        return;
    }
    #[cfg(test)]
    DRAINED_TOTAL.fetch_add(parked.len(), Ordering::Relaxed);
    tracing::debug!(
        count = parked.len(),
        "dropping database sessions parked by retired PHP threads"
    );
    // Dropped outside the lock: a session's drop reaches into the storage
    // engine, and holding the graveyard mutex across that would serialize
    // retiring threads behind it for no reason.
    drop(parked);
}

/// Sessions ever parked / ever drained. Monotonic, so a test can assert
/// "the park path fired" without racing other tests that share these globals
/// (every libtest thread that ran a query parks its own session on exit).
#[cfg(test)]
static PARKED_TOTAL: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static DRAINED_TOTAL: AtomicUsize = AtomicUsize::new(0);

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

/// Release the staged **rows**, the large half of a result.
///
/// Called by the C side the moment it has copied the cells into PHP zvals, and
/// again at per-request teardown ([`on_request_end`]).
///
/// The last-call record ([`had_rowset`], [`col_count`], [`with_col_name`],
/// [`ok_info`]) and the staged error ([`with_error`]) deliberately **survive**
/// this — see the module docs, `What survives finish()`. They are cleared by
/// the next [`run_sql_bytes`] on this thread, or by [`on_request_end`].
pub fn finish() {
    // `clear()` rather than assigning a fresh Vec: the row buffer's capacity is
    // reused by the next statement on this thread, which is the common case.
    DB_ROWS.with(|r| r.borrow_mut().clear());
}

/// Discard everything staged for this thread — rows, last-call record, and
/// error alike. The full reset that [`finish`] deliberately is not.
fn reset_staged() {
    finish();
    DB_LAST.with(|l| *l.borrow_mut() = LastCall::EMPTY);
    DB_ERROR.with(|e| *e.borrow_mut() = None);
}

fn stage_error(err: BridgeError) -> RunStatus {
    DB_ERROR.with(|e| *e.borrow_mut() = Some(err));
    RunStatus::Err
}

/// Build an infrastructure failure: one of the reserved codes from the module
/// docs, `Reserved error codes`, always with SQLSTATE `HY000`.
fn infra_error(code: u16, message: String) -> BridgeError {
    BridgeError { code, sqlstate: INFRA_SQLSTATE, message }
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

/// The site this thread's session must belong to, compared against the site it
/// *does* belong to (`held_site`, `None` when the thread has no session yet).
///
/// Returns `None` when the held session already matches — the common case, on
/// which nothing is allocated and the registry is never touched. Returns
/// `Some(key)` (a fresh owned copy, needed to re-key the session) only when a
/// swap is required.
///
/// In per-site mode the key comes from the thread-local set by
/// [`set_current_site`] and this **fails closed** if none is present — it never
/// substitutes a shared/default database.
fn site_swap_target(
    source: &BackendSource,
    held_site: Option<&str>,
) -> Result<Option<Box<str>>, BridgeError> {
    match source {
        BackendSource::Single(_) => Ok(if held_site == Some(SINGLE_SITE_KEY) {
            None
        } else {
            Some(Box::from(SINGLE_SITE_KEY))
        }),
        BackendSource::PerSite(_) => DB_CURRENT_SITE.with(|s| {
            let current = s.borrow();
            let Some(current) = current.as_deref() else {
                return Err(infra_error(
                    ERR_NO_SITE_CONTEXT,
                    "no per-site database context for this request — multi-site \
                     database isolation could not determine the tenant"
                        .to_string(),
                ));
            };
            Ok(if held_site == Some(current) { None } else { Some(Box::from(current)) })
        }),
    }
}

/// Resolve the backend for `site` — only called when a session swap is needed
/// (a thread's first query, or a site change), so the registry lookup is off
/// the same-site hot path.
fn backend_for(source: &BackendSource, site: &str) -> Result<SharedBackend, BridgeError> {
    match source {
        BackendSource::Single(backend) => Ok(Arc::clone(backend)),
        BackendSource::PerSite(resolver) => resolver.resolve(site).map_err(|msg| {
            infra_error(ERR_CONNECT, format!("failed to open the database for this site: {msg}"))
        }),
    }
}

/// [`run_sql_bytes`] against an explicit bridge. Split out so unit tests
/// can drive a locally-constructed [`DbBridge`] (with a mock backend)
/// without going through the process-wide, set-once [`DB_BRIDGE`].
fn run_on(bridge: Option<&DbBridge>, sql: &[u8]) -> RunStatus {
    let params: Vec<Value> = DB_PARAMS.with(|p| std::mem::take(&mut *p.borrow_mut()));
    reset_staged();

    let Some(bridge) = bridge else {
        // Staged as well as signalled: the C side throws from the RunStatus,
        // but `ephpm_db_errno()` / `ephpm_db_error()` read the staged triple,
        // and an adapter must see the same reserved code either way (#259).
        stage_error(infra_error(
            ERR_UNAVAILABLE,
            "no embedded database is active (requires [db.sqlite])".to_string(),
        ));
        return RunStatus::Unavailable;
    };

    let Ok(sql) = std::str::from_utf8(sql) else {
        return stage_error(BridgeError {
            code: ER_PARSE_ERROR,
            sqlstate: *b"42000",
            message: "SQL must be valid UTF-8".to_string(),
        });
    };

    // Defense-in-depth: reject cross-database / path primitives on the tenant
    // query path regardless of what the backend would do on its own.
    if let Err(offending) = screen_sql(sql) {
        return stage_error(BridgeError {
            code: ER_UNKNOWN_ERROR,
            sqlstate: *b"HY000",
            message: format!(
                "statement type `{offending}` is not permitted on the tenant database path"
            ),
        });
    }

    let outcome = DB_HELD.with(|cell| {
        let mut slot = cell.0.borrow_mut();

        // Swap the held session if it belongs to a different site (or none):
        // a thread that served site A must never run site B's query on A's
        // connection. Within one request the site is fixed, so this only ever
        // fires between requests. The registry lookup happens only here — a
        // same-site consecutive query reuses the held session untouched, and
        // does not even copy the site key.
        let swap_to = site_swap_target(&bridge.source, slot.as_ref().map(|h| h.site.as_ref()))?;
        if let Some(site) = swap_to {
            let backend = backend_for(&bridge.source, &site)?;
            if let Some(old) = slot.as_mut() {
                // Defensive: the previous site's session should not be
                // mid-transaction here (on_request_end rolls abandoned
                // transactions back between requests, and the site never
                // changes within a request). Roll back before dropping it so
                // a stray open transaction can't linger on a dropped session.
                if old.session.in_transaction {
                    tracing::warn!(
                        old_site = %old.site,
                        new_site = %site,
                        "swapping bridge session to a new site while a transaction was open on \
                         the previous site — rolling it back"
                    );
                    let _ = bridge.handle.block_on(old.session.query("ROLLBACK", &[]));
                }
            }
            // Lazily open this thread's session against the site's backend.
            // block_on is legal here — see the module docs (`Async boundary`):
            // FFI callbacks never run on async tasks.
            match bridge.handle.block_on(backend.connect()) {
                Ok(conn) => {
                    *slot = Some(HeldSession {
                        site,
                        session: Session::with_cache(
                            conn,
                            Dialect::MySQL,
                            Arc::clone(&bridge.cache),
                        ),
                        _backend: backend,
                    });
                }
                Err(e) => {
                    return Err(infra_error(
                        ERR_CONNECT,
                        format!("failed to open embedded database session: {e}"),
                    ));
                }
            }
        }

        // `slot` is Some here: either it already matched, or we just set it.
        let held = slot.as_mut().expect("held session was just ensured");
        let result = bridge.handle.block_on(held.session.query(sql, &params));
        if let Err(e) = &result {
            // Recycle a session whose connection looks dead so the NEXT
            // call reconnects — but never mid-transaction: recycling
            // there would silently discard the transaction state a
            // misclassified SQL error (the match is substring-based, see
            // is_connection_error) may still legitimately own. A
            // mid-transaction connection death is instead cleaned up at
            // request end: on_request_end's ROLLBACK fails over the same
            // dead connection and drops the session then.
            if !held.session.in_transaction && is_connection_error(e) {
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
        Ok(result) => stage_result(result),
        Err(e) => stage_error(e),
    }
}

/// Split a successful [`SessionResult`] into the two staging areas: rows into
/// [`DB_ROWS`] (released by [`finish`]) and everything else into [`DB_LAST`]
/// (which outlives it).
///
/// Nothing is cloned — `columns` and `rows` are moved out of the `ResultSet`.
fn stage_result(result: SessionResult) -> RunStatus {
    match result {
        SessionResult::Rows(rs) => {
            DB_LAST.with(|l| {
                *l.borrow_mut() = LastCall {
                    had_rowset: true,
                    columns: rs.columns,
                    // A result set carries no OK counters. Reporting zeros
                    // (rather than erroring) is the long-standing contract of
                    // `ephpm_db_execute()` on a SELECT — see `ok_info`.
                    affected_rows: 0,
                    last_insert_id: 0,
                };
            });
            DB_ROWS.with(|r| *r.borrow_mut() = rs.rows);
            RunStatus::Rows
        }
        SessionResult::Ok(ok) => {
            DB_LAST.with(|l| {
                *l.borrow_mut() = LastCall {
                    had_rowset: false,
                    columns: Vec::new(),
                    affected_rows: ok.affected_rows,
                    last_insert_id: ok.last_insert_id,
                };
            });
            RunStatus::Ok
        }
    }
}

/// Statement keywords rejected on the tenant query path: cross-database and
/// whole-file primitives. `ATTACH`/`DETACH` are the issue #274 cross-tenant
/// read/write/shell-plant primitive; `VACUUM [INTO]` can write a copy of the
/// database to an arbitrary path.
const FORBIDDEN_KEYWORDS: [&str; 3] = ["ATTACH", "DETACH", "VACUUM"];

/// `PRAGMA` names rejected because they name or move a filesystem path (or
/// re-open the schema for arbitrary edits). Ordinary tuning pragmas
/// (`foreign_keys`, `journal_mode`, ...) are unaffected.
///
/// Matched against every dot-separated component of the pragma's target (see
/// [`pragma_name_parts`]), so the schema-qualified and quoted spellings —
/// `PRAGMA main.data_store_directory`, `PRAGMA "writable_schema"` — are refused
/// the same way as the bare form.
const FORBIDDEN_PRAGMAS: [&str; 3] =
    ["writable_schema", "temp_store_directory", "data_store_directory"];

/// Screen a (possibly multi-statement) SQL string for statements that are
/// forbidden on the tenant path. Quote- and comment-aware so a `;` or keyword
/// hidden inside a string literal or comment is not mistaken for a statement.
///
/// # Errors
///
/// Returns the offending keyword (e.g. `"ATTACH"`, `"PRAGMA data_store_directory"`)
/// when a forbidden statement is found. An unterminated quote or block comment
/// is treated conservatively as forbidden (`"malformed SQL"`) so a truncated
/// `ATTACH` cannot slip through.
///
/// Public because the tenant path is no longer only this bridge: ephpm-server
/// applies the same screen to the multi-tenant MySQL wire listener
/// (`ScreenedBackend`), so both routes to a tenant database refuse the same
/// statements for ePHPm's own reasons rather than the engine's defaults. Pure
/// and stub-safe — a string scan with no PHP or backend involvement.
pub fn screen_sql(sql: &str) -> Result<(), String> {
    let bytes = sql.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                loop {
                    if i >= bytes.len() {
                        return Err("malformed SQL".to_string());
                    }
                    if bytes[i] == quote {
                        // A doubled quote is an escaped quote, not a close.
                        if i + 1 < bytes.len() && bytes[i + 1] == quote {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                loop {
                    if i + 1 >= bytes.len() {
                        return Err("malformed SQL".to_string());
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b';' => {
                check_statement(&sql[start..i])?;
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    check_statement(&sql[start..])
}

/// Reject one statement whose leading keyword is forbidden. Empty /
/// comment-only statements are allowed.
///
/// A leading `EXPLAIN` / `EXPLAIN QUERY PLAN` diagnostic wrapper is seen
/// through first (see [`strip_explain_prefixes`]), so `EXPLAIN ATTACH …` is
/// refused for the same reason and with the same error as `ATTACH …`. The
/// wrapper does not make the inner statement safe, and the point of this
/// screen is that the refusal is ePHPm's own — not whatever the pinned engine
/// happens to do with the wrapped verb today.
///
/// The `PRAGMA` name is read through [`pragma_name_parts`], which sees through a
/// schema qualifier and identifier quoting for the same reason.
fn check_statement(stmt: &str) -> Result<(), String> {
    let trimmed = strip_explain_prefixes(stmt)?;
    if trimmed.is_empty() {
        return Ok(());
    }
    let (keyword, rest) = leading_keyword(trimmed);

    if FORBIDDEN_KEYWORDS.contains(&keyword.as_str()) {
        return Err(keyword);
    }
    if keyword == "PRAGMA" {
        for part in pragma_name_parts(rest) {
            if FORBIDDEN_PRAGMAS.contains(&part.as_str()) {
                return Err(format!("PRAGMA {part}"));
            }
        }
    }
    Ok(())
}

/// The dot-separated identifier components of a `PRAGMA`'s target, lowercased —
/// e.g. `main.data_store_directory` yields `["main", "data_store_directory"]`.
///
/// # Why not just "the first token"
///
/// It used to be exactly that: `take_while(alphanumeric | '_')` over the text
/// after `PRAGMA`. A pentest found that this stops at the `.` in
/// `PRAGMA main.data_store_directory`, so the extracted name was `main` — not on
/// the denylist — and the statement reached the engine (issue #292). SQLite's
/// grammar is `PRAGMA [schema-name '.'] pragma-name`, so the qualified form is
/// the same statement with the same effect; the screen has to see through the
/// qualifier the way #287 taught it to see through an `EXPLAIN` prefix.
///
/// Identifiers may also be quoted (`"main"`, `` `main` ``, `[main]`, and — given
/// SQLite's lenient identifier/string handling — `'main'`), and the quoting
/// applies to the *name* as much as to the qualifier: `PRAGMA "writable_schema"`
/// slipped past the old bare-token extractor for the same reason. Both are
/// handled here.
///
/// **Every** component is returned, and the caller matches all of them against
/// the denylist rather than only the last. A schema legitimately named
/// `writable_schema` does not exist, so this costs nothing in false positives
/// and removes the need to reason about which position the engine treats as the
/// name.
///
/// Whitespace and comments between the tokens are skipped
/// ([`strip_leading_noise`]), so `PRAGMA main /* c */ . writable_schema` is seen.
/// An unterminated quoted identifier stops the scan: the statement is not valid
/// SQL, and the outer [`screen_sql`] scanner has already rejected the quote
/// styles it tracks.
fn pragma_name_parts(rest: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut s = strip_leading_noise(rest);
    loop {
        let Some((ident, after)) = take_identifier(s) else { return parts };
        parts.push(ident.to_ascii_lowercase());
        let after = strip_leading_noise(after);
        let Some(next) = after.strip_prefix('.') else { return parts };
        s = strip_leading_noise(next);
    }
}

/// Split one leading SQL identifier off `s`, returning its unquoted text and the
/// remainder. Handles the bare form (`[A-Za-z0-9_$]+`) and the quoted forms
/// `"x"`, `` `x` ``, `[x]` and `'x'`, including the doubled-delimiter escape.
/// Returns `None` when `s` does not begin with an identifier, or when a quoted
/// one is unterminated.
fn take_identifier(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    let close = match bytes.first()? {
        b'"' => b'"',
        b'`' => b'`',
        b'\'' => b'\'',
        b'[' => b']',
        _ => {
            let len = bytes
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$'))
                .count();
            if len == 0 {
                return None;
            }
            return Some((s[..len].to_string(), &s[len..]));
        }
    };

    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == close {
            // A doubled delimiter escapes itself in the quote styles that have
            // one (`[x]]` is not an escape — brackets do not nest).
            if close != b']' && bytes.get(i + 1) == Some(&close) {
                i += 2;
                continue;
            }
            // `i` and `1` are ASCII delimiter positions, so both are char
            // boundaries even if the identifier holds multi-byte text.
            let inner = &s[1..i];
            let unescaped = match close {
                b'"' => inner.replace("\"\"", "\""),
                b'`' => inner.replace("``", "`"),
                b'\'' => inner.replace("''", "'"),
                _ => inner.to_string(),
            };
            return Some((unescaped, &s[i + 1..]));
        }
        i += 1;
    }
    None
}

/// Split off the leading run of ASCII letters (a SQL keyword) from `s`,
/// uppercased, and return it together with the remainder that follows it.
/// The run is ASCII-only, so its byte length equals its character count and
/// the split point is unambiguous. Returns `("", s)` when `s` does not start
/// with a letter (e.g. it is empty, or begins with a digit or punctuation).
fn leading_keyword(s: &str) -> (String, &str) {
    let len = s.bytes().take_while(u8::is_ascii_alphabetic).count();
    (s[..len].to_ascii_uppercase(), &s[len..])
}

/// See through leading `EXPLAIN` / `EXPLAIN QUERY PLAN` prefixes so the
/// statement they wrap is what [`check_statement`]'s forbidden-verb check
/// inspects. Both are diagnostic wrappers: `EXPLAIN ATTACH DATABASE '…'`
/// parses its *first* keyword as `EXPLAIN`, so a screen that only looked at
/// the leading keyword would wave it through and leave the refusal to the
/// engine's default — exactly the property this screen exists to own.
///
/// It reuses [`strip_leading_noise`] between every token, so it is comment-
/// and whitespace-aware (`EXPLAIN /* c */ ATTACH`, a leading-tab/newline
/// `EXPLAIN`), and it is case-insensitive. It peels *repeated* prefixes in a
/// loop, so a nested `EXPLAIN EXPLAIN ATTACH` (whether or not the engine's
/// grammar even accepts it) still exposes the inner `ATTACH` to the check
/// rather than hiding it behind the outer wrapper. `EXPLAIN QUERY PLAN` is
/// consumed only as the exact three-token unit; a malformed `EXPLAIN QUERY
/// <not PLAN>` is rejected conservatively rather than passed on.
///
/// Legitimate `EXPLAIN SELECT …` / `EXPLAIN QUERY PLAN SELECT …` are
/// unaffected: once the wrapper is stripped the inner `SELECT` is a permitted
/// verb and the statement passes.
fn strip_explain_prefixes(stmt: &str) -> Result<&str, String> {
    let mut rest = strip_leading_noise(stmt);
    loop {
        let (keyword, after) = leading_keyword(rest);
        if keyword != "EXPLAIN" {
            return Ok(rest);
        }
        rest = strip_leading_noise(after);

        // Optional `QUERY PLAN` — the only valid two-word form of EXPLAIN.
        let (next, after_next) = leading_keyword(rest);
        if next == "QUERY" {
            let after_query = strip_leading_noise(after_next);
            let (plan, after_plan) = leading_keyword(after_query);
            if plan != "PLAN" {
                return Err("EXPLAIN".to_string());
            }
            rest = strip_leading_noise(after_plan);
        }
        // Loop again: peel any further stacked EXPLAIN prefix.
    }
}

/// Drop leading whitespace and leading SQL comments so a statement's first
/// keyword is what gets checked. Returns `""` when the input is only
/// whitespace and comments (including an unterminated one).
fn strip_leading_noise(stmt: &str) -> &str {
    let mut s = stmt;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            let Some(nl) = rest.find('\n') else { return "" };
            s = &rest[nl + 1..];
        } else if let Some(rest) = s.strip_prefix("/*") {
            let Some(end) = rest.find("*/") else { return "" };
            s = &rest[end + 2..];
        } else {
            return s;
        }
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
    // The full reset, not `finish()`: the last-call record and staged error
    // outlive `finish()` on purpose, but must never outlive the request that
    // produced them — the next request on this thread would otherwise read
    // another request's `ephpm_db_errno()`.
    reset_staged();
    // Safe point for the sessions of threads that have retired since the last
    // request: this is ordinary code on a live thread, so the engine drop glue
    // that would abort inside a TLS destructor runs harmlessly here (issue
    // #300). Unconditional — parked sessions exist whether or not this process
    // has a bridge registered, and it costs one relaxed load when empty.
    drain_parked_sessions();
    let Some(bridge) = DB_BRIDGE.get() else {
        return;
    };
    DB_HELD.with(|cell| {
        let mut slot = cell.0.borrow_mut();
        let Some(held) = slot.as_mut() else {
            return;
        };
        if !held.session.in_transaction {
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
        if let Err(e) = bridge.handle.block_on(held.session.query("ROLLBACK", &[])) {
            tracing::warn!(
                error = %e,
                "rollback of the abandoned transaction failed — dropping the session so \
                 the next request reconnects with clean state"
            );
            *slot = None;
        }
    });
}

/// Number of rows currently staged. Zero once [`finish`] has released them,
/// and zero for a statement that produced no result set.
#[must_use]
pub fn row_count() -> usize {
    DB_ROWS.with(|r| r.borrow().len())
}

/// Whether the last statement on this thread produced a result set.
///
/// The has-rowset signal of issue #263 — read from the executed
/// [`SessionResult`], never inferred from the SQL text. Survives [`finish`];
/// false before this thread's first statement.
#[must_use]
pub fn had_rowset() -> bool {
    DB_LAST.with(|l| l.borrow().had_rowset)
}

/// Number of columns the last statement's result set had (0 when it produced
/// none).
///
/// Survives [`finish`], and is **independent of the row count** — a zero-row
/// `SELECT` still reports its columns (issue #262).
#[must_use]
pub fn col_count() -> usize {
    DB_LAST.with(|l| l.borrow().columns.len())
}

/// Look at a column name of the last statement's result set. `None` when
/// `col` is out of range or the statement produced no result set.
pub fn with_col_name<R>(col: usize, f: impl FnOnce(Option<&str>) -> R) -> R {
    DB_LAST.with(|l| f(l.borrow().columns.get(col).map(|c| c.name.as_str())))
}

/// Look at a column's declared schema type (`INTEGER`, `TEXT`, ...).
///
/// `None` both when `col` is out of range and when the column is an
/// expression with no declared type (`SELECT a + 1`) — SQLite reports no
/// decltype for either, and the distinction is not one the engine can make.
pub fn with_col_decltype<R>(col: usize, f: impl FnOnce(Option<&str>) -> R) -> R {
    DB_LAST.with(|l| f(l.borrow().columns.get(col).and_then(|c| c.decltype.as_deref())))
}

/// Look at a cell of the staged rows. `None` once [`finish`] has released
/// them, or when `row`/`col` is out of range.
pub fn with_cell<R>(row: usize, col: usize, f: impl FnOnce(Option<&Value>) -> R) -> R {
    DB_ROWS.with(|r| f(r.borrow().get(row).and_then(|cells| cells.get(col))))
}

/// `(affected_rows, last_insert_id)` of the last statement.
///
/// A statement that produced a **result set** reports `(0, 0)` —
/// `ephpm_db_execute` on a SELECT is defined to return zeros rather than
/// error. So does a thread that has not run anything yet. Survives [`finish`].
///
/// `last_insert_id` is whatever the backend reported for the statement
/// (issue #260 asked for this to be pinned down): litewire clamps a missing or
/// negative `last_insert_rowid` to 0, but a non-`INSERT` OK — an `UPDATE`, a
/// `COMMIT` — carries the connection's *most recent* insert rowid, exactly as
/// `mysqli_insert_id()` does on the wire. Do not read it as "this statement
/// inserted something"; read `affected_rows`, or capture the id immediately
/// after the `INSERT` that produced it.
#[must_use]
pub fn ok_info() -> (u64, u64) {
    DB_LAST.with(|l| {
        let last = l.borrow();
        (last.affected_rows, last.last_insert_id)
    })
}

/// Whether this thread's session is currently inside an explicit transaction
/// (issue #260).
///
/// `false` when the thread has no session yet, or no bridge is registered —
/// in both cases no transaction can be open, so the answer is not a guess.
/// Reads the session's own flag, the same one the wire frontends report as
/// `SERVER_STATUS_IN_TRANS`.
#[must_use]
pub fn in_transaction() -> bool {
    DB_HELD.with(|cell| cell.0.borrow().as_ref().is_some_and(|held| held.session.in_transaction))
}

/// Whether an `ephpm_db_*` statement issued right now would reach a database
/// (issue #259).
///
/// True when a backend is registered **and** — in per-site mode — this request
/// has a tenant identity. It is deliberately a pure check: it never opens a
/// database, so a `true` here can still be followed by an [`ERR_CONNECT`]
/// failure if the storage underneath is broken. What it rules out is the two
/// conditions an adapter can act on before running anything:
/// [`ERR_UNAVAILABLE`] and [`ERR_NO_SITE_CONTEXT`].
#[must_use]
pub fn is_available() -> bool {
    available_on(DB_BRIDGE.get())
}

/// [`is_available`] against an explicit bridge. Split out for the same reason
/// as [`run_on`]: unit tests drive a locally-constructed [`DbBridge`] rather
/// than the process-wide, set-once [`DB_BRIDGE`].
fn available_on(bridge: Option<&DbBridge>) -> bool {
    let Some(bridge) = bridge else {
        return false;
    };
    match &bridge.source {
        BackendSource::Single(_) => true,
        BackendSource::PerSite(_) => DB_CURRENT_SITE.with(|s| s.borrow().is_some()),
    }
}

/// Look at the staged error, if any.
///
/// Survives [`finish`] (see the module docs) so the error of a statement whose
/// exception has already been thrown and caught is still readable. Cleared by
/// the next [`run_sql_bytes`] on this thread and by [`on_request_end`], so a
/// `None` here means "the last statement on this thread succeeded", not
/// "an error was never staged".
pub fn with_error<R>(f: impl FnOnce(Option<(u16, &[u8; 5], &str)>) -> R) -> R {
    DB_ERROR.with(|e| match &*e.borrow() {
        Some(err) => f(Some((err.code, &err.sqlstate, err.message.as_str()))),
        None => f(None),
    })
}

/// Code of the staged error, or 0 when the last statement on this thread
/// succeeded (or none has run). Backs `ephpm_db_errno()`.
#[must_use]
pub fn error_code() -> u16 {
    DB_ERROR.with(|e| e.borrow().as_ref().map_or(0, |err| err.code))
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
    /// Release the staged rows. The last-call record and staged error
    /// survive — see the module docs, `What survives finish()`.
    pub finish: Option<unsafe extern "C" fn()>,
    /// Whether the last statement produced a result set (1) or an OK (0).
    /// Valid after `run`, and after `finish`.
    pub had_rowset: Option<unsafe extern "C" fn() -> std::os::raw::c_int>,
    /// Column declared type; `*p` is NULL when the column has none.
    /// Same pointer-lifetime rule as `col_name`, but valid after `finish`
    /// too (the metadata outlives the rows).
    pub col_decltype: Option<
        unsafe extern "C" fn(col: usize, p: *mut *const std::os::raw::c_char, len: *mut usize),
    >,
    /// Whether this thread's session is inside an explicit transaction.
    pub in_transaction: Option<unsafe extern "C" fn() -> std::os::raw::c_int>,
    /// Whether a statement issued now would reach a database (1) or fail for
    /// an infrastructure reason (0).
    pub available: Option<unsafe extern "C" fn() -> std::os::raw::c_int>,
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

#[cfg(php_linked)]
unsafe extern "C" fn db_had_rowset() -> std::os::raw::c_int {
    std::os::raw::c_int::from(had_rowset())
}

#[cfg(php_linked)]
unsafe extern "C" fn db_col_decltype(
    col: usize,
    p: *mut *const std::os::raw::c_char,
    len: *mut usize,
) {
    with_col_decltype(col, |decltype| {
        let (ptr, n) = decltype.map_or((std::ptr::null(), 0), |s| (s.as_ptr().cast(), s.len()));
        // SAFETY: `p` and `len` are valid pointers to locals in our C wrapper
        // (`PHP_FUNCTION(ephpm_db_columns)` / `ephpm_db_run`). The returned
        // pointer aims into the thread-local last-call record, which is not
        // dropped or mutated until the next `run` or request end on this same
        // thread — the C side copies the bytes before returning to PHP.
        unsafe {
            *p = ptr;
            *len = n;
        }
    });
}

#[cfg(php_linked)]
unsafe extern "C" fn db_in_transaction() -> std::os::raw::c_int {
    std::os::raw::c_int::from(in_transaction())
}

#[cfg(php_linked)]
unsafe extern "C" fn db_available() -> std::os::raw::c_int {
    std::os::raw::c_int::from(is_available())
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
    had_rowset: Some(db_had_rowset),
    col_decltype: Some(db_col_decltype),
    in_transaction: Some(db_in_transaction),
    available: Some(db_available),
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
        reset_staged();
        assert_eq!(row_count(), 0);
        assert_eq!(col_count(), 0);
        assert_eq!(ok_info(), (0, 0));
        assert!(!had_rowset());
        assert_eq!(error_code(), 0);
        assert!(!in_transaction());
        with_col_name(0, |n| assert!(n.is_none()));
        with_col_decltype(0, |t| assert!(t.is_none()));
        with_cell(0, 0, |c| assert!(c.is_none()));
        with_error(|e| assert!(e.is_none()));
    }

    // ── The last-call record: has-rowset, empty-set metadata, errno ────
    //
    // Issues #263 / #262 / #259. The shared mechanism is that a statement's
    // metadata outlives `finish()` while its rows do not, so each of these
    // asserts the same fact twice: before the rows are released and after.

    /// #263: the has-rowset signal comes from the executed statement, so it
    /// is right for statement shapes that defeat first-keyword sniffing —
    /// a `WITH` CTE reads as a SELECT, and a bare `SET NAMES` never reaches
    /// the backend at all.
    #[test]
    #[serial]
    fn had_rowset_reflects_the_executed_statement_not_the_sql_text() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_hr (id INTEGER PRIMARY KEY, n TEXT)"), RunStatus::Ok);
        assert!(!had_rowset(), "DDL produces no rowset");

        assert_eq!(run("INSERT INTO bridge_hr (n) VALUES ('a')"), RunStatus::Ok);
        assert!(!had_rowset(), "INSERT produces no rowset");

        assert_eq!(run("SELECT id FROM bridge_hr"), RunStatus::Rows);
        assert!(had_rowset());

        // A dialect no-op never reaches the backend, and still classifies.
        assert_eq!(run("SET NAMES utf8mb4"), RunStatus::Ok);
        assert!(!had_rowset());
        reset_staged();
    }

    /// #262: a zero-row result set still knows its columns — the whole point,
    /// since the rows that would otherwise carry the names do not exist.
    #[test]
    #[serial]
    fn zero_row_result_set_keeps_its_column_metadata() {
        init_bridge();
        assert_eq!(
            run("CREATE TABLE bridge_empty (id INTEGER PRIMARY KEY, label TEXT)"),
            RunStatus::Ok
        );
        assert_eq!(run("SELECT id, label FROM bridge_empty WHERE id = 12345"), RunStatus::Rows);

        assert_eq!(row_count(), 0, "the point of the test: no rows");
        assert!(had_rowset(), "but it WAS a result set");
        assert_eq!(col_count(), 2);
        with_col_name(0, |n| assert_eq!(n, Some("id")));
        with_col_name(1, |n| assert_eq!(n, Some("label")));

        // And the metadata is what `ephpm_db_columns()` reads — i.e. it is
        // still there once the C side has released the (empty) rows.
        finish();
        assert_eq!(col_count(), 2);
        with_col_name(1, |n| assert_eq!(n, Some("label")));
        reset_staged();
    }

    /// Declared schema types reach the accessor, and an expression column
    /// correctly reports none.
    #[test]
    #[serial]
    fn column_decltypes_are_exposed() {
        init_bridge();
        assert_eq!(
            run("CREATE TABLE bridge_decl (id INTEGER PRIMARY KEY, label TEXT)"),
            RunStatus::Ok
        );
        assert_eq!(run("SELECT id, label, id + 1 FROM bridge_decl"), RunStatus::Rows);
        assert_eq!(col_count(), 3);
        with_col_decltype(0, |t| {
            assert_eq!(t.map(str::to_ascii_uppercase).as_deref(), Some("INTEGER"));
        });
        with_col_decltype(1, |t| {
            assert_eq!(t.map(str::to_ascii_uppercase).as_deref(), Some("TEXT"));
        });
        with_col_decltype(2, |t| assert!(t.is_none(), "an expression has no declared type"));
        reset_staged();
    }

    /// The rows are the only thing `finish()` releases. Everything an
    /// introspection native reads — has-rowset, columns, OK counters — is
    /// still there afterwards, which is what makes those natives callable
    /// after `ephpm_db_query()` has already returned.
    #[test]
    #[serial]
    fn finish_releases_rows_but_keeps_the_last_call_record() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_fin (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_fin (id) VALUES (1)"), RunStatus::Ok);
        assert_eq!(ok_info(), (1, 1));

        finish();
        assert_eq!(ok_info(), (1, 1), "OK counters survive finish()");
        assert!(!had_rowset());

        assert_eq!(run("SELECT id FROM bridge_fin"), RunStatus::Rows);
        assert_eq!(row_count(), 1);
        assert_eq!(ok_info(), (0, 0), "a result set reports zero OK counters");

        finish();
        assert_eq!(row_count(), 0, "rows ARE released");
        with_cell(0, 0, |c| assert!(c.is_none()));
        assert!(had_rowset(), "but the classification survives");
        assert_eq!(col_count(), 1);
        reset_staged();
    }

    /// #259: the staged error outlives the throw, so `ephpm_db_errno()` can
    /// still describe a failure whose exception was caught — and the next
    /// successful statement clears it, so 0 means "the last one succeeded".
    #[test]
    #[serial]
    fn error_survives_finish_and_is_cleared_by_the_next_statement() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_errno (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(error_code(), 0);

        assert_eq!(run("INSERT INTO bridge_errno (id) VALUES (1)"), RunStatus::Ok);
        assert_eq!(run("INSERT INTO bridge_errno (id) VALUES (1)"), RunStatus::Err);
        assert_eq!(error_code(), litewire::session::error_map::ER_DUP_ENTRY);

        // This is what the C side does immediately after throwing.
        finish();
        assert_eq!(
            error_code(),
            litewire::session::error_map::ER_DUP_ENTRY,
            "errno must still be readable once the exception has been thrown"
        );
        with_error(|e| assert!(e.expect("still staged").2.to_ascii_lowercase().contains("unique")));

        assert_eq!(run("SELECT id FROM bridge_errno"), RunStatus::Rows);
        assert_eq!(error_code(), 0, "a successful statement clears the error");
        with_error(|e| assert!(e.is_none()));
        reset_staged();
    }

    /// A SQL error's code always stays in the MySQL **server** range, which
    /// is what makes the reserved client-range codes a usable discriminator.
    #[test]
    #[serial]
    fn sql_errors_never_use_a_reserved_code() {
        init_bridge();
        for sql in ["NOT VALID SQL !!!", "SELECT * FROM bridge_no_such_table_at_all"] {
            assert_eq!(run(sql), RunStatus::Err, "{sql}");
            let code = error_code();
            assert!(code != 0 && !(2000..3000).contains(&code), "{sql} reported code {code}");
        }
        reset_staged();
    }

    /// #259: no bridge at all is an infrastructure failure, staged with the
    /// reserved code rather than left for the caller to infer from a message.
    #[test]
    #[serial]
    fn unavailable_bridge_stages_the_reserved_code() {
        reset_staged();
        assert_eq!(run_on(None, b"SELECT 1"), RunStatus::Unavailable);
        assert_eq!(error_code(), ERR_UNAVAILABLE);
        with_error(|e| {
            let (code, sqlstate, msg) = e.expect("unavailable must stage an error");
            assert_eq!(code, ERR_UNAVAILABLE);
            assert_eq!(sqlstate, b"HY000");
            assert!(msg.contains("[db.sqlite]"), "got: {msg}");
        });
        assert!(!available_on(None));
        reset_staged();
    }

    /// #260: transaction state is read from the session, not inferred.
    #[test]
    #[serial]
    fn in_transaction_tracks_the_session_state() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_intx (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert!(!in_transaction());

        assert_eq!(run("BEGIN"), RunStatus::Ok);
        assert!(in_transaction(), "BEGIN opens one");
        assert_eq!(run("INSERT INTO bridge_intx (id) VALUES (1)"), RunStatus::Ok);
        assert!(in_transaction(), "a statement inside it does not close it");

        assert_eq!(run("COMMIT"), RunStatus::Ok);
        assert!(!in_transaction(), "COMMIT closes it");

        assert_eq!(run("BEGIN"), RunStatus::Ok);
        assert!(in_transaction());
        assert_eq!(run("ROLLBACK"), RunStatus::Ok);
        assert!(!in_transaction(), "ROLLBACK closes it");
        reset_staged();
    }

    /// The safety net and the introspection agree: after the request-end
    /// rollback there is nothing open, which is exactly what lets a wrapper's
    /// `transaction()` helper stop firing a blind ROLLBACK.
    #[test]
    #[serial]
    fn in_transaction_is_false_after_the_request_end_rollback() {
        init_bridge();
        assert_eq!(run("CREATE TABLE bridge_intx_end (id INTEGER PRIMARY KEY)"), RunStatus::Ok);
        assert_eq!(run("BEGIN"), RunStatus::Ok);
        assert!(in_transaction());

        on_request_end();
        assert!(!in_transaction());
    }

    /// A single-backend bridge is always available; introspection reads never
    /// disturb the record the executing statements staged.
    #[test]
    #[serial]
    fn availability_and_introspection_do_not_disturb_the_record() {
        init_bridge();
        assert!(is_available(), "single-backend mode is always available");

        assert_eq!(run("SELECT 1 AS one"), RunStatus::Rows);
        // Every introspection read, back to back.
        assert!(is_available());
        assert!(!in_transaction());
        assert!(had_rowset());
        assert_eq!(col_count(), 1);
        assert_eq!(error_code(), 0);
        // ...and the rows are still there.
        assert_eq!(row_count(), 1);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(1))));
        reset_staged();
    }

    /// A record must never outlive the request that produced it: the next
    /// request on this worker thread would otherwise read the previous one's
    /// `ephpm_db_errno()`.
    #[test]
    #[serial]
    fn request_end_clears_the_last_call_record() {
        init_bridge();
        assert_eq!(run("SELECT 1 AS one"), RunStatus::Rows);
        assert_eq!(run("NOT VALID SQL !!!"), RunStatus::Err);
        assert_ne!(error_code(), 0);

        on_request_end();

        assert_eq!(error_code(), 0);
        assert!(!had_rowset());
        assert_eq!(col_count(), 0);
        assert_eq!(ok_info(), (0, 0));
        with_error(|e| assert!(e.is_none()));
    }

    // ── Per-request teardown (on_request_end) ──────────────────────────

    /// Whether this thread's session currently believes it is inside an
    /// explicit transaction (None = no session open).
    fn thread_in_transaction() -> Option<bool> {
        DB_HELD.with(|cell| cell.0.borrow().as_ref().map(|held| held.session.in_transaction))
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
    // Its own assertions are thread-local and would not need serializing, but
    // `on_request_end()` also drains the process-global session graveyard —
    // which would steal the sessions `teardown_tests` is asserting on. See the
    // invariant documented above `mod teardown_tests`.
    #[serial]
    fn request_end_without_a_session_is_a_noop() {
        // This test's thread never ran a query, so no session exists.
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
        (
            DbBridge {
                source: BackendSource::Single(backend),
                handle,
                cache: Arc::new(TranslateCache::default()),
            },
            flags,
        )
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

// ── SQL screening (denylist) tests ──────────────────────────────────────
//
// Pure, no bridge/runtime needed: `screen_sql` is a stub-safe string scan.
#[cfg(test)]
mod screen_tests {
    use super::*;

    #[track_caller]
    fn allowed(sql: &str) {
        assert!(screen_sql(sql).is_ok(), "expected `{sql}` to be allowed");
    }

    #[track_caller]
    fn rejected(sql: &str) {
        assert!(screen_sql(sql).is_err(), "expected `{sql}` to be rejected");
    }

    #[test]
    fn ordinary_statements_pass() {
        allowed("SELECT * FROM wp_posts WHERE id = ?");
        allowed("INSERT INTO t (a) VALUES (1)");
        allowed("UPDATE t SET a = 1");
        allowed("BEGIN");
        allowed("COMMIT");
        allowed("SET NAMES utf8mb4");
        allowed("CREATE TABLE t (id INTEGER PRIMARY KEY)");
        // Ordinary tuning pragmas are fine.
        allowed("PRAGMA foreign_keys = ON");
        allowed("PRAGMA journal_mode = WAL");
        // The forbidden keywords appearing only inside a string/identifier or
        // a comment must NOT trip the screen.
        allowed("SELECT 'ATTACH DATABASE' AS note");
        allowed("SELECT * FROM t -- VACUUM everything\n WHERE a = 1");
        allowed("INSERT INTO t (a) VALUES ('detach me')");
    }

    #[test]
    fn cross_database_and_path_primitives_are_rejected() {
        rejected("ATTACH DATABASE '/etc/passwd' AS steal");
        rejected("attach database 'x.db' as y");
        rejected("DETACH DATABASE y");
        rejected("VACUUM");
        rejected("VACUUM INTO '/tmp/copy.db'");
        rejected("PRAGMA writable_schema = ON");
        rejected("PRAGMA data_store_directory = '/tmp'");
        rejected("PRAGMA temp_store_directory = '/tmp'");
    }

    #[test]
    fn forbidden_hidden_in_a_second_statement_is_caught() {
        // A leading benign statement must not smuggle a trailing ATTACH.
        rejected("SELECT 1; ATTACH DATABASE 'x' AS y");
        rejected("SELECT 1 /* ; */ ; VACUUM");
    }

    #[test]
    fn truncated_quote_or_comment_is_rejected_conservatively() {
        rejected("SELECT 'unterminated");
        rejected("SELECT 1 /* unterminated");
    }

    #[test]
    fn explain_wrapped_forbidden_statements_are_rejected() {
        // `EXPLAIN <forbidden>` parses its first keyword as EXPLAIN, so a
        // leading-keyword screen would miss it. The wrapper must be seen
        // through and the inner verb refused with ePHPm's own error — not left
        // to the engine's ATTACH-disabled default (pentest bypass).
        rejected("EXPLAIN ATTACH DATABASE 'x.db' AS y");
        rejected("explain attach database 'x.db' as y");
        rejected("EXPLAIN QUERY PLAN ATTACH DATABASE 'x.db' AS y");
        rejected("explain query plan attach database 'x.db' as y");
        // Comment- and whitespace-aware between EXPLAIN and the inner verb.
        rejected("EXPLAIN /* c */ ATTACH DATABASE 'x' AS y");
        rejected("EXPLAIN QUERY /* c */ PLAN ATTACH DATABASE 'x' AS y");
        rejected("  \t EXPLAIN\nATTACH DATABASE 'x' AS y");
        rejected("EXPLAIN\t\tVACUUM INTO '/tmp/copy.db'");
        rejected("EXPLAIN VACUUM");
        rejected("EXPLAIN DETACH DATABASE y");
        rejected("EXPLAIN PRAGMA data_store_directory = '/tmp'");
        rejected("EXPLAIN QUERY PLAN PRAGMA writable_schema = ON");
        // Stacked / nested EXPLAIN must still expose the inner forbidden verb.
        rejected("EXPLAIN EXPLAIN ATTACH DATABASE 'x' AS y");
        // A malformed `EXPLAIN QUERY <not PLAN>` is rejected conservatively.
        rejected("EXPLAIN QUERY ATTACH DATABASE 'x' AS y");
        // The wrapper hidden after a benign leading statement is still caught.
        rejected("SELECT 1; EXPLAIN ATTACH DATABASE 'x' AS y");
    }

    #[test]
    fn schema_qualified_and_quoted_pragmas_are_rejected() {
        // Issue #292: `PRAGMA <schema>.<name>` used to slip the screen because
        // name extraction stopped at the `.`, yielding `main` — not on the
        // denylist. Every qualified spelling of the three forbidden pragmas
        // must be refused by ePHPm rather than left to the engine.
        rejected("PRAGMA main.data_store_directory = '/tmp'");
        rejected("PRAGMA main.temp_store_directory = '/tmp'");
        rejected("PRAGMA main.writable_schema = ON");
        rejected("pragma MAIN.DATA_STORE_DIRECTORY = '/tmp'");
        rejected("PRAGMA temp.writable_schema = 1");
        // Any schema name, not just `main`/`temp`.
        rejected("PRAGMA otherdb.writable_schema = 1");
        // Quoted / backticked / bracketed qualifiers.
        rejected("PRAGMA \"main\".data_store_directory = '/tmp'");
        rejected("PRAGMA `main`.writable_schema = 1");
        rejected("PRAGMA [main].temp_store_directory = '/tmp'");
        rejected("PRAGMA 'main'.writable_schema = 1");
        // Quoting the *name* is the same evasion in the other position.
        rejected("PRAGMA \"writable_schema\" = 1");
        rejected("PRAGMA `data_store_directory` = '/tmp'");
        rejected("PRAGMA [temp_store_directory] = '/tmp'");
        rejected("PRAGMA main.\"writable_schema\" = 1");
        // Whitespace and comments around the qualifier dot.
        rejected("PRAGMA main . writable_schema = 1");
        rejected("PRAGMA main/**/.writable_schema = 1");
        rejected("PRAGMA main.\n  data_store_directory = '/tmp'");
        // Stacked with the other wrappers this screen already sees through.
        rejected("EXPLAIN PRAGMA main.data_store_directory = '/tmp'");
        rejected("EXPLAIN QUERY PLAN PRAGMA main.writable_schema = ON");
        rejected("SELECT 1; PRAGMA main.data_store_directory = '/tmp'");
        rejected("  /* c */ PRAGMA main.writable_schema = 1");
    }

    #[test]
    fn schema_qualified_ordinary_pragmas_still_pass() {
        // The qualifier must not turn every pragma into a refusal.
        allowed("PRAGMA main.journal_mode = WAL");
        allowed("PRAGMA main.foreign_keys = ON");
        allowed("PRAGMA temp.synchronous = NORMAL");
        allowed("PRAGMA \"main\".journal_mode = WAL");
        allowed("PRAGMA main.table_info(t)");
        allowed("PRAGMA table_info(t)");
        allowed("PRAGMA main.user_version");
        allowed("EXPLAIN PRAGMA main.foreign_keys = ON");
        // Bare / malformed pragmas are not the screen's business.
        allowed("PRAGMA");
        allowed("PRAGMA .");
    }

    #[test]
    fn explain_wrapped_ordinary_statements_pass() {
        // Legitimate EXPLAIN of a permitted verb must not be broken.
        allowed("EXPLAIN SELECT 1");
        allowed("EXPLAIN QUERY PLAN SELECT * FROM t");
        allowed("explain query plan select * from wp_posts where id = ?");
        allowed("EXPLAIN INSERT INTO t (a) VALUES (1)");
        // Ordinary tuning pragmas are fine even when EXPLAINed.
        allowed("EXPLAIN PRAGMA foreign_keys = ON");
        // Bare EXPLAIN (no inner statement) is harmless.
        allowed("EXPLAIN");
        allowed("EXPLAIN QUERY PLAN");
        // A forbidden keyword only inside a literal, behind EXPLAIN, is data.
        allowed("EXPLAIN SELECT 'ATTACH DATABASE' AS note");
    }
}

// ── Per-site backend routing tests (local PerSite bridge) ───────────────
//
// Drive `run_on` with a locally-constructed `DbBridge` whose source is a
// `PerSite` resolver over two real (file-backed) Turso databases, proving the
// cross-tenant isolation the whole feature exists for — a query for site B
// cannot see site A's table — plus the per-thread session swap and the
// fail-closed behaviour when no site context is set. No PHP runtime needed.
#[cfg(test)]
mod per_site_tests {
    use std::collections::HashMap;

    use serial_test::serial;

    use super::tests::TEST_RT;
    use super::*;

    /// Trivial resolver over a fixed `site -> backend` map.
    struct MapResolver {
        map: HashMap<String, SharedBackend>,
    }

    impl SiteBackendResolver for MapResolver {
        fn resolve(&self, site_key: &str) -> Result<SharedBackend, String> {
            self.map
                .get(site_key)
                .cloned()
                .ok_or_else(|| format!("no backend for site `{site_key}`"))
        }
    }

    pub(super) fn open_turso(path: &std::path::Path) -> SharedBackend {
        let rt = TEST_RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("test runtime")
        });
        let backend = rt
            .block_on(litewire::Turso::open(path.to_str().expect("utf-8 path")))
            .expect("open turso db");
        Arc::new(backend)
    }

    pub(super) fn per_site_bridge(map: HashMap<String, SharedBackend>) -> DbBridge {
        let handle = TEST_RT.get().expect("runtime").handle().clone();
        DbBridge {
            source: BackendSource::PerSite(Arc::new(MapResolver { map })),
            handle,
            cache: Arc::new(TranslateCache::default()),
        }
    }

    fn run(bridge: &DbBridge, sql: &str) -> RunStatus {
        run_on(Some(bridge), sql.as_bytes())
    }

    /// The issue-#274 / pentest-C1 exploit, at the bridge level: site A writes
    /// a secret; site B must NOT be able to read it.
    ///
    /// Serialized because it calls `on_request_end()`, which drains the
    /// process-global session graveyard `teardown_tests` asserts on — see the
    /// invariant documented above `mod teardown_tests`.
    #[test]
    #[serial]
    fn cross_tenant_read_is_impossible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = open_turso(&dir.path().join("site-a.db"));
        let b = open_turso(&dir.path().join("site-b.db"));
        let mut map = HashMap::new();
        map.insert("site-a.test".to_string(), a);
        map.insert("site-b.test".to_string(), b);
        let bridge = per_site_bridge(map);

        // Site A creates a table and inserts a secret.
        set_current_site(Some("site-a.test"));
        assert_eq!(run(&bridge, "CREATE TABLE tenant_probe (secret TEXT)"), RunStatus::Ok);
        assert_eq!(
            run(&bridge, "INSERT INTO tenant_probe (secret) VALUES ('secretA')"),
            RunStatus::Ok
        );
        on_request_end();

        // Site B, on the SAME thread, must not see site A's table at all — its
        // database is a different file and starts empty.
        set_current_site(Some("site-b.test"));
        assert_eq!(
            run(&bridge, "SELECT secret FROM tenant_probe"),
            RunStatus::Err,
            "site B must not see site A's table (cross-tenant isolation)"
        );
        with_error(|e| {
            let (_, _, msg) = e.expect("error staged");
            assert!(
                msg.to_ascii_lowercase().contains("tenant_probe")
                    || msg.to_ascii_lowercase().contains("no such table"),
                "expected a missing-table error, got: {msg}"
            );
        });
        on_request_end();

        // Swapping back to site A on the same thread sees A's persisted data.
        set_current_site(Some("site-a.test"));
        assert_eq!(run(&bridge, "SELECT secret FROM tenant_probe"), RunStatus::Rows);
        assert_eq!(row_count(), 1);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Text("secretA".into()))));
        finish();
    }

    /// Per-site mode with no site context set fails closed — it never falls
    /// back to a shared/default database.
    #[test]
    fn missing_site_context_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = open_turso(&dir.path().join("only.db"));
        let mut map = HashMap::new();
        map.insert("site-a.test".to_string(), a);
        let bridge = per_site_bridge(map);

        set_current_site(None);
        // The pre-flight form (#259): an adapter can see this coming without
        // running a probe query and catching an exception.
        assert!(!available_on(Some(&bridge)), "no tenant context — nothing to reach");

        assert_eq!(run(&bridge, "SELECT 1"), RunStatus::Err);
        with_error(|e| {
            let (code, sqlstate, msg) = e.expect("error staged");
            assert!(msg.contains("per-site database context"), "got: {msg}");
            // Reserved code, distinct from any SQL error the tenant could
            // provoke (#259) — this is infrastructure, not bad SQL.
            assert_eq!(code, ERR_NO_SITE_CONTEXT);
            assert_eq!(sqlstate, b"HY000");
        });
        reset_staged();
    }

    /// The boundary of the has-rowset signal (#263), pinned against the
    /// **production** backend rather than the rusqlite test one.
    ///
    /// `had_rowset` is authoritative for every statement litewire *runs*,
    /// because it reads the executed `SessionResult`. It cannot rescue a
    /// statement litewire never runs correctly: `Session::query` picks
    /// `BackendConn::query` vs `BackendConn::execute` from
    /// `litewire_translate::classify()`, which is itself a first-keyword
    /// match (`litewire-translate/src/lib.rs`). `WITH` falls through to
    /// `StatementKind::Other` and `INSERT ... RETURNING` matches `INSERT`, so
    /// both are sent to `execute()` and the engine rejects them.
    ///
    /// They **error** rather than silently returning nothing, which is the
    /// tolerable failure mode — and they fail identically on the wire path,
    /// so this is not a bridge regression. Fixing it means teaching
    /// litewire's `classify()` about CTEs and `RETURNING`; this test will
    /// start failing the moment that lands, which is the intent.
    #[test]
    fn litewire_classification_bounds_the_has_rowset_signal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bridge = single_turso_bridge(&dir.path().join("classify.db"));

        assert_eq!(
            run(&bridge, "CREATE TABLE cls (id INTEGER PRIMARY KEY, n TEXT)"),
            RunStatus::Ok
        );
        assert_eq!(run(&bridge, "INSERT INTO cls (n) VALUES ('a')"), RunStatus::Ok);
        assert!(!had_rowset());

        for sql in [
            "WITH r AS (SELECT id FROM cls) SELECT id FROM r",
            "INSERT INTO cls (n) VALUES ('b') RETURNING id",
        ] {
            assert_eq!(
                run(&bridge, sql),
                RunStatus::Err,
                "{sql} is expected to fail until litewire's classify() handles it"
            );
            // A SQL-range code, not a reserved one: from the adapter's point
            // of view this is "the statement failed", not "the bridge is
            // broken" — which is the correct classification (#259).
            let code = error_code();
            assert!(!(2000..3000).contains(&code), "{sql} reported reserved code {code}");
        }

        // A plain SELECT over the same table still works, so the failure is
        // the classifier, not the data.
        assert_eq!(run(&bridge, "SELECT id FROM cls"), RunStatus::Rows);
        assert!(had_rowset());
        // BOTH rows are present: the `INSERT ... RETURNING` above reported an
        // error to the caller and *still wrote its row*. The engine performs
        // the insert and only then trips over the returned row that
        // `execute()` has nowhere to put. Pinned deliberately — a caller that
        // retries on that error double-inserts, which is the sharpest edge of
        // this limitation and the reason it is documented rather than merely
        // known.
        assert_eq!(row_count(), 2, "INSERT ... RETURNING errors but is NOT rolled back");
        reset_staged();
    }

    /// A single-backend bridge over a real Turso database file.
    fn single_turso_bridge(path: &std::path::Path) -> DbBridge {
        let handle = super::tests::TEST_RT
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("test runtime")
            })
            .handle()
            .clone();
        DbBridge {
            source: BackendSource::Single(open_turso(path)),
            handle,
            cache: Arc::new(TranslateCache::default()),
        }
    }

    /// With a tenant context the same bridge reports available.
    #[test]
    fn availability_follows_the_site_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = open_turso(&dir.path().join("site-a.db"));
        let mut map = HashMap::new();
        map.insert("site-a.test".to_string(), a);
        let bridge = per_site_bridge(map);

        set_current_site(None);
        assert!(!available_on(Some(&bridge)));

        set_current_site(Some("site-a.test"));
        assert!(available_on(Some(&bridge)));

        // Availability is about having a tenant identity, not about that
        // tenant's database being openable — an unknown key still reads as
        // available and fails later with ERR_CONNECT.
        set_current_site(Some("stranger.test"));
        assert!(available_on(Some(&bridge)));

        set_current_site(None);
        reset_staged();
    }

    /// An unknown site key surfaces the resolver's error rather than any
    /// data.
    #[test]
    fn unknown_site_key_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = open_turso(&dir.path().join("only.db"));
        let mut map = HashMap::new();
        map.insert("site-a.test".to_string(), a);
        let bridge = per_site_bridge(map);

        set_current_site(Some("stranger.test"));
        assert_eq!(run(&bridge, "SELECT 1"), RunStatus::Err);
        with_error(|e| {
            let (code, _, msg) = e.expect("error staged");
            assert!(msg.contains("stranger.test"), "got: {msg}");
            // The tenant is known but its storage could not be opened —
            // ERR_CONNECT, not ERR_NO_SITE_CONTEXT (#259).
            assert_eq!(code, ERR_CONNECT);
        });
        reset_staged();
    }
}

// ── Thread-teardown safety (issue #300) ─────────────────────────────────
//
// The abort these guard against is: a retiring PHP-tainted thread runs the
// `DB_HELD` destructor, which drops a `Session`, which drops the storage
// engine's live page buffers, whose own `Drop` calls `LocalKey::with` on a
// thread-local that was registered LATER and is therefore ALREADY destroyed —
// panic in a TLS destructor, `SIGABRT`, whole process gone.
//
// These run against a real Turso backend on purpose: a mock connection has no
// engine buffers to drop and would pass no matter what.
//
// ── Why every drain-touching test here is `#[serial]` ──────────────────
//
// The graveyard ([`PARKED_SESSIONS`]) and its accounting ([`PARKED_TOTAL`],
// [`DRAINED_TOTAL`]) are **process-global by design**: any thread's request-end
// must be able to reclaim any *other* retired thread's session, whatever bridge
// it came from. `cargo test` runs the whole crate's tests as threads inside one
// process, so a sibling test calling `on_request_end()` drains this test's
// parked sessions first; this test's own drain then finds an empty graveyard
// and its `DRAINED_TOTAL` delta stays at zero. That is a shared-state race, not
// timing noise — it reproduced at ~32% of full-suite runs, and it is invisible
// under `cargo nextest` (process per test) and `--test-threads=1`, which is why
// CI mostly escaped it.
//
// Injecting a per-test graveyard was considered and rejected: the park path
// runs *inside a TLS destructor*, where touching any Rust thread-local is
// precisely the abort (#300) this machinery exists to prevent — so the
// graveyard cannot be selected from thread-local state — and parks also
// originate on libtest's own threads, which a test cannot instrument. Making
// the graveyard per-bridge instead would break the global-reclaim property
// that is the whole point of the design.
//
// So the exclusion is explicit instead. **Invariant: every test in this crate
// that calls `on_request_end()` or `drain_parked_sessions()` must be
// `#[serial]`** — a non-serial drainer anywhere in the crate re-opens this race.
// Note that `#[serial]` is only needed against *drains*; a sibling thread
// merely *parking* a session can never make these assertions fail, because
// parks only ever add.
#[cfg(test)]
mod teardown_tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    use serial_test::serial;

    use super::per_site_tests::{open_turso, per_site_bridge};
    use super::*;

    fn parked_total() -> usize {
        PARKED_TOTAL.load(Ordering::Acquire)
    }

    fn drained_total() -> usize {
        DRAINED_TOTAL.load(Ordering::Acquire)
    }

    /// Build a one-site bridge over a fresh on-disk Turso database.
    ///
    /// Shared as an `Arc` so worker threads can be `spawn`ed and `join`ed
    /// rather than scoped. That is deliberate: `std::thread::scope` releases as
    /// soon as each closure *returns*, which is **before** that thread's TLS
    /// destructors run — so a scoped harness races the very teardown these
    /// tests are about (observed: 3 of 4 parks visible at scope exit).
    /// `JoinHandle::join` waits for full thread termination, dtors included.
    fn one_site_bridge(dir: &std::path::Path, site: &str) -> Arc<DbBridge> {
        let mut map = HashMap::new();
        map.insert(site.to_string(), open_turso(&dir.join(format!("{site}.db"))));
        Arc::new(per_site_bridge(map))
    }

    /// The core guarantee: a thread that used the bridge and then exits does
    /// **not** drop its session on the way out — the session is parked, and a
    /// later `on_request_end()` on a live thread drops it.
    ///
    /// If the park path ever regresses, this test does not fail — the whole
    /// test binary aborts with `fatal runtime error: thread local panicked on
    /// drop`, which is exactly the production failure.
    #[test]
    #[serial]
    fn retiring_thread_parks_its_session_instead_of_dropping_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bridge = one_site_bridge(dir.path(), "park.test");

        let parked_before = parked_total();
        let worker = Arc::clone(&bridge);
        std::thread::spawn(move || {
            set_current_site(Some("park.test"));
            assert_eq!(
                run_on(Some(&worker), b"CREATE TABLE park_probe (id INTEGER PRIMARY KEY)"),
                RunStatus::Ok
            );
            assert_eq!(
                run_on(Some(&worker), b"INSERT INTO park_probe (id) VALUES (1)"),
                RunStatus::Ok
            );
            finish();
            // Thread ends here: TLS teardown must park, not drop.
        })
        .join()
        .expect("worker thread must exit cleanly, not abort");
        assert!(
            parked_total() > parked_before,
            "the retiring thread's session must be parked, not dropped at teardown"
        );

        // Draining happens on a live thread, where the engine's drop glue is
        // harmless.
        let drained_before = drained_total();
        drain_parked_sessions();
        assert!(
            drained_total() > drained_before,
            "drain_parked_sessions must drop what thread teardown parked"
        );

        // The parked session's writes are durable and the database is usable
        // afterwards — parking defers a drop, it does not abandon data.
        set_current_site(Some("park.test"));
        assert_eq!(run_on(Some(&bridge), b"SELECT COUNT(*) FROM park_probe"), RunStatus::Rows);
        with_cell(0, 0, |c| assert_eq!(c, Some(&Value::Integer(1))));
        finish();
    }

    /// `on_request_end()` is the wired drain point, so ordinary request
    /// teardown reclaims the sessions of threads that retired since the last
    /// request. Also the churn test: many short-lived PHP-tainted threads in a
    /// row is precisely the tokio blocking-pool reaping pattern that produced
    /// the abort under load.
    #[test]
    #[serial]
    fn thread_churn_is_reclaimed_by_request_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bridge = one_site_bridge(dir.path(), "churn.test");

        for round in 0..8 {
            let parked_before = parked_total();
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    let bridge = Arc::clone(&bridge);
                    std::thread::spawn(move || {
                        set_current_site(Some("churn.test"));
                        assert_eq!(run_on(Some(&bridge), b"SELECT 1"), RunStatus::Rows);
                        finish();
                    })
                })
                .collect();
            for worker in workers {
                worker.join().expect("worker thread must exit cleanly, not abort");
            }
            assert!(
                parked_total() >= parked_before + 4,
                "round {round}: every retiring thread must park its session"
            );

            let drained_before = drained_total();
            // The seam production uses — no special-case teardown call.
            on_request_end();
            assert!(
                drained_total() > drained_before,
                "round {round}: on_request_end must drain the graveyard"
            );
        }
    }

    /// Draining an empty graveyard is a cheap no-op, and repeated drains are
    /// idempotent — it sits on the per-request path, so a back-to-back drain
    /// must neither panic, deadlock, nor invent accounting.
    ///
    /// Asserted as the cumulative invariant (`drained <= parked`) rather than a
    /// per-window delta. `#[serial]` excludes concurrent *drains*, but it
    /// cannot stop an unrelated test's thread from retiring and *parking*
    /// mid-test, and such a session can be counted as parked before this
    /// window while being reclaimed inside it — which is correct behavior that
    /// a delta assertion reads as a violation. (That is not theoretical: the
    /// delta form failed 1 run in 150.) The cumulative form has no such window:
    /// every drained session was parked first, and both totals only grow, so
    /// reading `drained` before `parked` can never observe an inversion.
    #[test]
    #[serial]
    fn draining_nothing_is_a_noop() {
        drain_parked_sessions();
        drain_parked_sessions();
        drain_parked_sessions();
        // Read order matters: `drained` first, so a park+drain racing between
        // the two loads can only widen the gap, never invert it.
        let drained = drained_total();
        let parked = parked_total();
        assert!(
            drained <= parked,
            "a drain accounted a session that was never parked ({drained} drained > {parked} parked)"
        );
    }

    // ── The platform behavior that makes #300 possible ──────────────────

    thread_local! {
        /// Stands in for `DB_HELD`: initialized FIRST, so its destructor runs
        /// LAST.
        static PROBE_EARLY: RefCell<Option<ProbeValue>> = const { RefCell::new(None) };
        /// Stands in for Turso's `TEMP_BUFFER_CACHE`: initialized SECOND, so
        /// its destructor runs FIRST and it is already gone when `ProbeValue`
        /// reaches for it.
        static PROBE_LATE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    static PROBE_DROPPED: AtomicBool = AtomicBool::new(false);
    static PROBE_SAW_DESTROYED_TLS: AtomicBool = AtomicBool::new(false);

    /// Reaches for another thread-local from its `Drop`, exactly as
    /// `turso_core::io::Buffer::drop` does — except with `try_with`, so it
    /// *observes* the failure the engine *panics* on.
    struct ProbeValue;

    impl Drop for ProbeValue {
        fn drop(&mut self) {
            PROBE_DROPPED.store(true, Ordering::Release);
            if PROBE_LATE.try_with(|c| c.borrow_mut().push(1)).is_err() {
                PROBE_SAW_DESTROYED_TLS.store(true, Ordering::Release);
            }
        }
    }

    /// Pins the assumption the whole fix rests on: thread-local destructors run
    /// in **reverse** registration order, so a value dropped at teardown can
    /// find a thread-local it depends on already destroyed. `LocalKey::with`
    /// there panics, and a panic in a TLS destructor aborts the process — which
    /// is #300 in one sentence.
    ///
    /// If this ever fails, the platform stopped inverting and the module docs
    /// need re-reading; the fix (park, never drop) stays correct either way.
    #[test]
    fn tls_destructors_run_in_reverse_registration_order() {
        std::thread::spawn(|| {
            // Registration order: early first...
            PROBE_EARLY.with(|c| *c.borrow_mut() = Some(ProbeValue));
            // ...then late, mimicking the engine allocating a scratch buffer
            // inside the very call that created the session.
            PROBE_LATE.with(|c| c.borrow_mut().push(0));
        })
        .join()
        .expect("probe thread must exit cleanly, not abort");

        assert!(PROBE_DROPPED.load(Ordering::Acquire), "the probe value must be dropped at exit");
        assert!(
            PROBE_SAW_DESTROYED_TLS.load(Ordering::Acquire),
            "the later-registered thread-local must already be destroyed when an \
             earlier-registered one drops — this inversion is what aborts the process when \
             the drop belongs to a database session (issue #300)"
        );
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

    /// The pointer contract that changed: `col_name` / `col_decltype` /
    /// `error_info` stay valid **after** `finish()`, because the C side reads
    /// them there (`ephpm_db_columns()` runs long after the rows are gone).
    /// Only the cell pointers die with the rows.
    #[test]
    #[serial]
    fn ffi_metadata_pointers_survive_finish_but_cells_do_not() {
        init_bridge();
        // SAFETY: valid pointer + length.
        assert_eq!(
            unsafe { run_c("CREATE TABLE bridge_ffi_meta (id INTEGER PRIMARY KEY, tag TEXT)") },
            2
        );
        // Zero rows on purpose — the #262 case.
        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("SELECT id, tag FROM bridge_ffi_meta WHERE id = 999") }, 1);

        // SAFETY: no arguments.
        assert_eq!(unsafe { db_had_rowset() }, 1);
        // SAFETY: no arguments.
        assert_eq!(unsafe { db_row_count() }, 0);
        // SAFETY: no arguments.
        assert_eq!(unsafe { db_col_count() }, 2);

        // SAFETY: no arguments — releases the rows, keeps the metadata.
        unsafe { db_finish() };

        // SAFETY: no arguments.
        assert_eq!(unsafe { db_had_rowset() }, 1, "classification survives finish");
        // SAFETY: no arguments.
        assert_eq!(unsafe { db_col_count() }, 2, "column count survives finish");

        let mut p: *const std::os::raw::c_char = std::ptr::null();
        let mut len: usize = 0;
        // SAFETY: valid out-pointers.
        unsafe { db_col_name(1, &mut p, &mut len) };
        assert!(!p.is_null(), "column name pointer must be live after finish");
        // SAFETY: p/len aim into the last-call record, live until the next run.
        assert_eq!(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) }, b"tag");

        // SAFETY: valid out-pointers.
        unsafe { db_col_decltype(1, &mut p, &mut len) };
        assert!(!p.is_null(), "declared type must be live after finish");
        // SAFETY: p/len aim into the last-call record, live until the next run.
        let decl = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
        assert_eq!(decl.to_ascii_uppercase(), b"TEXT");

        // An out-of-range column yields a NULL pointer, not a dangling one.
        // SAFETY: valid out-pointers.
        unsafe { db_col_name(99, &mut p, &mut len) };
        assert!(p.is_null());
        assert_eq!(len, 0);

        // The cells, by contrast, are gone.
        let mut ty: std::os::raw::c_int = -1;
        let mut ival: std::os::raw::c_longlong = 0;
        let mut fval: f64 = 0.0;
        // SAFETY: valid out-pointers.
        unsafe { db_cell(0, 0, &mut ty, &mut ival, &mut fval, &mut p, &mut len) };
        assert_eq!(ty, 0, "released rows read as NULL, never as a dangling pointer");
        assert!(p.is_null());

        reset_staged();
    }

    /// `error_info` after `finish()` is what `ephpm_db_errno()` reads (#259),
    /// and `in_transaction` / `available` answer without running SQL (#260,
    /// #259).
    #[test]
    #[serial]
    fn ffi_error_survives_finish_and_state_shims_answer() {
        init_bridge();
        // SAFETY: no arguments.
        assert_eq!(unsafe { db_available() }, 1, "single-backend mode is available");
        // SAFETY: no arguments.
        assert_eq!(unsafe { db_in_transaction() }, 0);

        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("TOTALLY ((( not sql") }, -1);
        // SAFETY: no arguments — the C side calls this right after throwing.
        unsafe { db_finish() };

        let mut code: std::os::raw::c_uint = 0;
        let mut state: *const std::os::raw::c_char = std::ptr::null();
        let mut msg: *const std::os::raw::c_char = std::ptr::null();
        let mut msg_len: usize = 0;
        // SAFETY: valid out-pointers.
        unsafe { db_error_info(&mut code, &mut state, &mut msg, &mut msg_len) };
        assert_eq!(code, u32::from(ER_PARSE_ERROR), "errno readable after the throw");
        assert!(!state.is_null());

        // BEGIN / COMMIT move the flag the shim reports.
        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("BEGIN") }, 2);
        // SAFETY: no arguments.
        assert_eq!(unsafe { db_in_transaction() }, 1);
        // SAFETY: valid pointer + length.
        assert_eq!(unsafe { run_c("ROLLBACK") }, 2);
        // SAFETY: no arguments.
        assert_eq!(unsafe { db_in_transaction() }, 0);

        // A successful statement clears the error the C side reads.
        // SAFETY: valid out-pointers.
        unsafe { db_error_info(&mut code, &mut state, &mut msg, &mut msg_len) };
        assert_eq!(code, 0);
        assert!(state.is_null(), "NULL sqlstate is the C side's `no error` signal");

        reset_staged();
    }
}

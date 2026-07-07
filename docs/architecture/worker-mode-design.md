# ePHPm Worker Mode — Phase 1 Engine Design

**Status:** Design (implementation-ready). Target: 3.0 headline feature.
**Scope:** The Rust/C engine beneath the `Ephpm\Octane\take_request()` /
`send_response()` userland contract. Framework adapters (Octane, PSR-15,
Symfony Runtime, WordPress) are out of scope here — they are Composer
packages that consume this engine.

This document cites the code it builds on. Line ranges are as of the reading
that produced this design; treat them as anchors, re-grep before editing.

---

## 0. The one-paragraph summary

Today ePHPm runs a **php-fpm-shaped** model: every HTTP request does a full
`php_request_shutdown()` → `php_request_startup()` cycle inside
`ephpm_execute_request` (`crates/ephpm-php/ephpm_wrapper.c:744-908`), which
destroys the user symbol table, constants, and included-files list between
requests (that hardening is why vanilla WordPress renders correctly — see the
big comment at `:746-760`). Worker mode **inverts control**: instead of the
runtime calling *into* PHP once per request, PHP calls *out* to the runtime in
a loop. The user's framework boots **once** inside a single, never-torn-down
SAPI request; then PHP blocks in `take_request()` (a new blocking SAPI FFI
call), the runtime hands it the next HTTP request over a channel, PHP runs the
framework handler on the already-booted kernel, calls `send_response()`, and
loops. The 5–20× win comes from never re-running the framework bootstrap. The
crux is **reset semantics**: framework state must persist across the loop while
per-request SAPI state (superglobals, output buffer, response headers/status,
`$_SESSION`) is made fresh — *without* the `php_request_shutdown()` that would
tear down the framework.

---

## 1. SAPI / userland surface

### 1.1 The two primitives (Phase 1)

Phase 1 ships exactly two Rust-backed PHP functions plus one envelope object.
`on_tick()` (from the Octane roadmap) is **deferred to Phase 4** — it is not a
worker-mode primitive, it is an Octane feature.

```php
namespace Ephpm\Worker;   // engine namespace; adapters consume it directly

/**
 * Block until the next HTTP request is routed to this worker thread.
 * Returns an Envelope, or null on graceful shutdown (worker exits its loop).
 */
function take_request(): ?\Ephpm\Worker\Envelope;

/**
 * Hand a response back to the HTTP layer. Must be called exactly once
 * per non-null take_request(). $body accepts a string (Phase 1) or a
 * resource/stream (Phase 3).
 */
function send_response(int $status, array $headers, string $body): void;
```

`take_request()` returns a **plain PHP object**, not a Symfony/PSR-7 request —
the adapters build those from the envelope (see the PSR-15 roadmap's
`$creator->fromArrays(...)` call, `psr-15-worker-mode.md:67-77`, and the Octane
roadmap `laravel-octane-driver.md:129`). Keeping the envelope framework-neutral
is what lets one engine serve all four adapters.

### 1.2 The Envelope object shape

The envelope is the marshaled HTTP request. It mirrors exactly the inputs
`nyholm/psr7-server`'s `ServerRequestCreator::fromArrays()` wants, so the
PSR-15 adapter is a straight pass-through:

```php
final class Envelope {
    public function serverVars(): array;   // $_SERVER-shaped (CGI vars + HTTP_*)
    public function headers(): array;       // ['Name' => 'value', ...]
    public function cookies(): array;       // parsed name => value
    public function query(): array;         // parsed $_GET
    public function parsedBody(): ?array;   // as shipped: ALWAYS null (form parsing is an adapter concern)
    public function files(): array;         // as shipped: ALWAYS [] (use populate_superglobals for $_FILES)
    public function bodyStream();           // as shipped (Phase 3): real readable php:// stream resource
    public function rawBody(): string;      // php://input equivalent
}
```

The envelope is constructed **in C** as a `zend` object (or, simpler for Phase
1, a stdClass-like struct built by `object_init` + `add_property_*`) populated
directly from the Rust-provided request. We do **not** populate PHP
superglobals in worker mode by default (see §3.3 and the Octane roadmap's
explicit "Don't reset superglobals when a request enters a worker",
`laravel-octane-driver.md:261-266`). A config knob (`worker.populate_superglobals`,
§4) can turn native superglobal population back on for the WordPress adapter,
which expects `$_GET`/`$_POST`/`$_SERVER` to be real.

### 1.3 FFI mapping — extend the ops-table pattern, do NOT hand-roll

The established pattern in this codebase is the `#[repr(C)]` function-pointer
ops table set through a C setter, exactly as `kv_bridge.rs` does with
`EphpmKvOps` / `ephpm_set_kv_ops` (`crates/ephpm-php/src/kv_bridge.rs:68-140`,
`crates/ephpm-php/ephpm_wrapper.c:947-960`, `:1786-1791`). Worker mode follows
the **same** pattern:

```rust
// crates/ephpm-php/src/worker_bridge.rs   (new)
#[cfg(php_linked)]
#[repr(C)]
pub struct EphpmWorkerOps {
    /// Block until the next request. Fills the out-params with a borrowed
    /// view of the request (pointers valid until send_response). Returns:
    ///   1  = request available (out-params populated)
    ///   0  = graceful shutdown (worker should return from its loop)
    pub take_request: Option<unsafe extern "C" fn(req: *mut EphpmWorkerRequest) -> c_int>,

    /// Hand back the response. `headers` is "Name: Value\n" packed (reuse the
    /// existing captured-headers convention, ephpm_wrapper.c:331-396).
    pub send_response: Option<unsafe extern "C" fn(
        status: c_int,
        headers: *const c_char, headers_len: usize,
        body: *const c_char, body_len: usize,
    )>,
}
```

The C side registers `Ephpm\Worker\take_request` / `send_response` as native
functions via the **same MINIT window** the KV functions use — appended to the
`additional_functions` table restored inside `ephpm_module_startup`
(`ephpm_wrapper.c:1744-1766`). This is non-negotiable under ZTS: the comment at
`:1717-1743` documents *why* post-`php_embed_init` registration is invisible to
tokio worker threads (the main thread's `CG(function_table)` is frozen into
`GLOBAL_FUNCTION_TABLE` at the end of `zend_startup`; late registrations land in
a freed table). The worker functions must join `ephpm_kv_functions` in the
table `ephpm_module_startup` installs, or extend that shim to install a second
table.

**Judgment call — ops table vs. plain extern "C":** we use the ops table (like
KV) rather than plain `extern "C"` functions in the wrapper, because the Rust
side needs to own the channel endpoints (`thread_local!` receivers, §2). The
ops table lets `ephpm-server` inject those endpoints at startup without the C
layer knowing about tokio. `send_response` could arguably be a thin C function
that reuses the existing `output_buf`/`headers_buf` capture path — but routing
it through the ops table keeps the "PHP → parked oneshot sender" flow in one
place (Rust), which is cleaner for the crash-recovery accounting in §5.

### 1.4 Request/response marshaling & zero-copy

- **Request → PHP.** The envelope's fields are built from an
  `EphpmWorkerRequest` `#[repr(C)]` struct whose string fields are borrowed
  pointers into Rust-owned memory that lives for the whole request (owned by
  the channel message, held on the runtime side until `send_response`). The C
  code copies them into `zend_string`s when building the envelope object —
  copy is unavoidable here because PHP owns object property lifetimes. This is
  the same "pointers valid until execute returns" contract that
  `ephpm_request_set_info` already relies on (`ephpm_wrapper.c:653-672`).
- **Response ← PHP.** `send_response($status, $headers, $body)` hands the body
  as a `zend_string`. The C ops-table shim passes `(ptr, len)` for both headers
  and body straight to Rust. Rust copies once into a `Vec<u8>` that becomes the
  hyper `Full<Bytes>` body (same as `build_php_response` consumes today,
  `router.rs:1132-1187`). Phase 1 is a single copy each way; **zero-copy body
  streaming is Phase 3** (§9), where `bodyStream()` becomes a real
  `php://` stream backed by the incremental hyper body reader.
  **Phase 3 (implemented):** the ops table gained `body_read` (request stream)
  and `response_begin`/`response_chunk`/`response_end` (response stream);
  `WorkerBody`/`WorkerResponse` became buffered-or-streaming enums; large
  request bodies stream in via a bounded channel the worker `blocking_recv`s,
  and `send_response_stream()` streams the response out via a bounded channel
  bridged to a hyper `StreamBody`. See the roadmap's "Phase 3 engine status".
- **The output-buffer path still exists.** A framework that `echo`s instead of
  returning a body still works: `ub_write` (`ephpm_wrapper.c:234-248`) captures
  it into the thread-local `output_buf`, and `send_response` with an empty
  `$body` flushes that buffer. Adapters that build a response object pass the
  body explicitly and never touch `ub_write`. Both paths are supported;
  `send_response` concatenates captured `output_buf` (if any) + explicit
  `$body`.

---

## 2. Persistent-context threading model

### 2.1 Dedicated worker threads — NOT `spawn_blocking`

The fpm path uses `tokio::task::spawn_blocking` (`router.rs:741`). Worker mode
**must not**, because:

1. A worker thread blocks for the **entire lifetime of the process** inside its
   `take_request()` loop. `spawn_blocking`'s pool is bounded (default 512) and
   shared with static-file I/O, KV, and DB blocking work. Parking N threads
   forever in that pool starves everything else — the exact starvation the fpm
   `php_semaphore` design was written to avoid (`router.rs:99-103`,
   `config lib.rs:1167-1179`).
2. Worker identity must be stable: a worker "owns" its booted framework kernel
   and its TSRM context for its whole life. `spawn_blocking` gives no thread
   identity or affinity.

**Design:** `ephpm-server` spawns a fixed pool of `std::thread` OS threads at
startup (a new `WorkerPool` type, likely in `crates/ephpm-server/src/worker_pool.rs`).
Each thread runs `worker_main(worker_id, dispatch_rx)`:

```text
worker_main:
  ephpm_thread_init()                 // TSRM register + one long request (see §2.4)
  run bootstrap script (worker.php)   // boots framework ONCE, then calls take_request()
  // control never returns here until the framework's while-loop ends
```

The framework's `worker.php` *is* the loop. From the engine's perspective, the
worker thread calls **one** `php_execute_script(worker.php)` that never returns
until shutdown, because `worker.php` sits in `while (take_request()) { ... }`.
`take_request()` is the blocking point.

### 2.2 How hyper hands a request to a worker (channels)

Three channel decisions, each with the alternative stated:

**(a) Runtime → worker pool dispatch: `async_channel` (MPMC), bounded.**
The hyper handler (async, on a tokio worker thread) needs to hand a request to
*some* free worker. Workers are blocking OS threads pulling work. This is a
classic async-producer / sync-consumer MPMC queue.

- *Chosen:* `async_channel::bounded(depth)` — it has both `send().await`
  (async, for the hyper side) and `recv_blocking()` (sync, for the worker
  side). One queue, all workers `recv_blocking()` on the same receiver; the
  first free worker wins. Backpressure is the bounded depth.
- *Rejected:* `tokio::sync::mpsc` — its `Receiver` is async-only; a blocking
  worker thread would have to `block_on` a per-thread current-thread runtime to
  poll it, which is wasteful and error-prone. `crossbeam-channel` is sync-only
  and gives no `send().await`, forcing the hyper side to block a tokio worker.
  `async-channel` is the one crate that speaks both sides natively. (It is
  already a transitive dep via several async crates; confirm with `cargo tree`
  before adding to `Cargo.toml`.)

**(b) Worker → runtime response return: `tokio::sync::oneshot`.**
Each dispatched request message carries a `oneshot::Sender<WorkerResponse>`.
The worker fills it from `send_response`. The hyper handler `.await`s the
`oneshot::Receiver`. This is exactly the shape the Octane roadmap predicted
(`laravel-octane-driver.md:145-149`: "parks the PHP thread on a
`tokio::sync::oneshot` receiver"). oneshot is correct because it is
single-use, allocation-light, and its `Sender` is `Drop`-safe under abrupt PHP
exit (§5, and the roadmap's Drop-safety note at `:270-274`).

**(c) Message shape:**

```rust
struct WorkerJob {
    request: WorkerRequestOwned,          // owns all the request strings/bytes
    respond_to: oneshot::Sender<WorkerResponse>,
    deadline: tokio::time::Instant,        // for hung-worker detection (§5)
}
```

The dispatch flow:

```text
hyper handler (async)                worker thread (blocking)
─────────────────────                ────────────────────────
build WorkerRequestOwned
(tx, rx) = oneshot::channel()
dispatch_tx.send(job).await  ───────► recv_blocking() returns job
                                       ┌ store job.respond_to in TLS
                                       │ take_request() unparks, returns Envelope
                                       │ framework handles request
                                       │ send_response(...) →
                                       │   job.respond_to.send(WorkerResponse)
                                       └ loops back to take_request()/recv_blocking()
rx.await ◄──────────────────────────── (response delivered)
build hyper Response (build_php_response reuse)
```

`take_request()` (the FFI call) is implemented on the Rust side as:
`recv_blocking()` on the dispatch queue → stash `respond_to` in a
`thread_local!<Option<oneshot::Sender<..>>>` → return the borrowed request view
to C → C builds the Envelope. `send_response()` reads the stashed sender out of
the TLS cell and `.send()`s. This keeps the sender per-worker-thread and never
shared, matching the roadmap's "per-worker thread_local storage" rule
(`laravel-octane-driver.md:267-269`).

### 2.3 Backpressure when all workers busy

The dispatch channel is **bounded** (default depth = `worker.count`, i.e. one
queued job per worker; tunable via `worker.backlog`). When full,
`dispatch_tx.send(job).await` suspends the hyper handler — this naturally
applies HTTP-layer backpressure without a busy loop. The whole handler is
already wrapped in `tokio::time::timeout(request_timeout, ...)`
(`router.rs:377-386`), so a request that can't get a worker within the timeout
returns **504 Gateway Timeout** exactly like a slow fpm request does today. No
new timeout machinery for the queueing case — reuse the existing outer timeout.

This mirrors the fpm `php_semaphore` semantics (queue past the cap, still
bounded by request timeout — `router.rs:728-738`) but the mechanism is the
bounded channel instead of a semaphore.

### 2.4 Coexistence with the tokio runtime

- PHP is still initialized **before** the tokio runtime is created
  (`crates/ephpm/src/main.rs:554-559` and the SIGPROF rationale at `:422-431`).
  Nothing changes here — `php_embed_init` runs single-threaded, SIGPROF is
  no-op'd via `--wrap` (`ephpm_wrapper.c:532-566`), then the runtime starts.
- The worker OS threads are spawned **after** the runtime exists but do their
  TSRM registration + bootstrap lazily on first `worker_main` entry. They are
  plain `std::thread`s, not tokio tasks, so they don't consume runtime worker
  slots.
- The dispatch channel `Sender` lives in the `Router` (replacing/augmenting
  `php_semaphore`); the `Receiver` clones go to the worker threads.

### 2.5 Keeping the TSRM context alive across the whole loop

Under ZTS, `ephpm_thread_init()` (`ephpm_wrapper.c:442-463`) does
`ts_resource(0)` + `php_request_startup()` and is **called once** at
`worker_main` entry. In fpm mode, `ensure_thread_registered` guards this with a
`thread_local!` bool (`ephpm-php/src/lib.rs:136-139`, `:482-502`). In worker
mode the same guard applies, but the request started by `ephpm_thread_init` is
the **one long-lived request** that the whole worker loop runs inside — it is
*never* shut down between HTTP requests (contrast fpm's
`ephpm_execute_request`, which shuts down and restarts every call,
`ephpm_wrapper.c:761-811`). That single request is the framework-boot context.
The TSRM slot lives for the thread's whole life; `ts_free_thread` runs only at
graceful drain (§5.4).

---

## 3. Reset semantics — the crux

### 3.1 The conflict, precisely

The recently-hardened `ephpm_execute_request` does, **every request**:
`php_request_shutdown(NULL)` then `php_request_startup()`
(`ephpm_wrapper.c:761-811`). `php_request_shutdown` runs
`zend_deactivate() → shutdown_executor()`, which "destroys user symbols,
constants, statics, and included_files" (its own comment, `:752-756`). That is
exactly what we **must not do** in worker mode — destroying the symbol table
destroys the booted framework. So worker mode cannot reuse
`ephpm_execute_request` at all. It needs a new per-iteration reset that is
strictly lighter than a request shutdown.

### 3.2 The three options, and the choice

**Option A — one long request, re-populate SAPI state per iteration (CHOSEN).**
Keep the single `php_request_startup()` from `ephpm_thread_init` open for the
worker's whole life. Between iterations, reset *only* the SAPI-scoped state that
would otherwise leak, and leave the executor (user classes/functions/the
framework's object graph) untouched. Concretely, `take_request()` on each
iteration does:

1. Reset the thread-local C capture buffers: `output_len = 0`,
   `headers_buf_len = 0` (same lines as `ephpm_execute_request:769-771`).
2. Reset response status/header flags — the *exact* three-line fix already
   proven necessary on the reuse path (`ephpm_wrapper.c:823-825`):
   `SG(sapi_headers).http_response_code = 200; SG(headers_sent) = 0;
   SG(request_info).no_headers = 0;`. Without this a `http_response_code(201)`
   or a fatal-500 from the previous request leaks into the next (that comment,
   `:817-822`, documents the leak on the fpm reuse path — the worker path has
   the identical hazard because the request object is reused).
3. Clear `SG(sapi_headers).headers` (the `zend_llist` of emitted headers) so
   the previous response's headers don't accumulate. fpm gets this free from
   `php_request_shutdown`; worker mode must do it explicitly via
   `sapi_headers_struct` reset (`sapi_deactivate`-style, but headers-only).
4. Reset `PG(last_error_type) = 0` so fatal detection is per-iteration
   (mirrors `:844`).
5. **Do NOT** rebuild superglobals by default (§3.3).

**Why A:** it is the minimum work that keeps framework state alive. It reuses
the precise reset lines the hardening effort already validated on the fpm reuse
path — we are not inventing new reset logic, we are *subtracting* the
`php_request_shutdown` from it.

**Option B — lightweight `php_request_shutdown`/`startup` with executor
preservation.** Rejected: there is no supported PHP API to shut down a request
while preserving the executor's symbol tables. `shutdown_executor()` is
all-or-nothing. Any attempt to "shut down but keep classes" means reaching into
Zend internals that change between 8.3/8.4/8.5 — a maintenance and correctness
nightmare across the CI matrix.

**Option C — full per-iteration shutdown/startup (i.e. fpm behavior).**
Rejected by definition — it defeats worker mode (re-boots the framework).

### 3.3 What ePHPm resets vs. what the framework owns

The division of labor the roadmaps mandate:

| State | Who resets it | Why |
|---|---|---|
| C capture buffers (`output_buf`, `headers_buf`) | **ePHPm** (per iteration) | thread-local SAPI plumbing |
| `http_response_code`, `headers_sent`, `no_headers` | **ePHPm** (per iteration) | proven leak (`ephpm_wrapper.c:817-825`) |
| `SG(sapi_headers).headers` llist | **ePHPm** (per iteration) | otherwise response headers accumulate |
| `PG(last_error_type)` | **ePHPm** (per iteration) | per-request fatal detection |
| Superglobals `$_GET/$_POST/$_SERVER/$_COOKIE/$_FILES/$_REQUEST` | **Framework** (via envelope) by default; ePHPm only if `worker.populate_superglobals=true` | Octane/PSR-15 build their own `Request` (`laravel-octane-driver.md:261-266`); WordPress wants real superglobals |
| `$_SESSION` / session lifecycle | **Framework** (via `session_*`) | ePHPm's native handler already closes on request shutdown/bailout; in worker mode the framework must call `session_write_close()` per request. See §3.4 |
| Container bindings, auth, cache, DB connections, app state | **Framework** | PSR-15 doc `:126-131`; Octane's flush listeners `laravel-octane-driver.md:110-115` |

The engine deliberately does the **least** it can. Everything above the
double line is framework territory — this is why the PSR-15 adapter can be 60
lines (`psr-15-worker-mode.md:124-141`).

### 3.4 Reconciling with the hardened lifecycle (UAF, sentinel, status)

Three specific hazards from the fpm hardening that the worker path must
account for:

1. **The manual-superglobal-rebuild UAF (`ephpm_wrapper.c:773-789`).** The fpm
   path learned the hard way that hand-rebuilding `PG(http_globals)` after
   `php_request_startup` causes a use-after-free in `php_default_treat_data` on
   tokio threads under load. **Worker mode sidesteps this entirely** by not
   rebuilding superglobals at all (§3.3). If `worker.populate_superglobals` is
   enabled (WordPress), we do NOT hand-rebuild — instead we set
   `SG(request_info)` fields and let the *framework's* `session`/globals
   handling or an explicit `import_request_variables`-style native call
   populate them through the normal treat_data path, invoked once at
   `take_request` time while the request is quiescent. This must be
   fuzz-tested (§9 risks).
2. **The `server_context` sentinel (`ephpm_wrapper.c:162-165`, `:799-806`).**
   `sapi_activate()` only parses POST when `SG(server_context)` is non-NULL.
   Our long-lived request set it once at boot. In worker mode we are **not**
   calling `sapi_activate` per iteration (no request_startup per iteration), so
   POST parsing for the envelope is driven explicitly: the C shim calls the
   post-reader against the per-iteration body buffer, exactly as the pre-8.4
   compat shim does (`ephpm_wrapper.c:60-111`). The sentinel stays set for the
   whole worker life.
3. **Status reset.** Already covered — reuse `:823-825` verbatim per iteration.

### 3.5 The per-iteration reset, as a single C function

```c
/* ephpm_wrapper.c — new. Called by take_request() at the top of each
 * iteration, on the worker's own TSRM context, inside the long-lived request.
 * Deliberately does NOT call php_request_shutdown/startup. */
void ephpm_worker_reset_request(void) {
    output_len = 0;
    headers_buf_len = 0;
    /* Drop headers emitted by the previous response. */
    zend_llist_clean(&SG(sapi_headers).headers);
    if (SG(sapi_headers).mimetype) { efree(SG(sapi_headers).mimetype); SG(sapi_headers).mimetype = NULL; }
    SG(sapi_headers).http_response_code = 200;   /* :823 */
    SG(headers_sent) = 0;                         /* :824 */
    SG(request_info).no_headers = 0;              /* :825 */
    PG(last_error_type) = 0;                      /* :844 */
    /* per-iteration POST cursor */
    req_post_data_offset = 0;
}
```

Note this touches the **same** SAPI globals the hardened path touches, minus
the shutdown/startup. That symmetry is the safety argument: we know these
resets are correct because the fpm path already depends on them.

---

## 4. Coexistence & config

### 4.1 New `[php]` fields

Extend `PhpConfig` (`crates/ephpm-config/src/lib.rs:1137-1180`). Follow the
`add-config-knob` skill checklist — **no silent no-ops**.

```toml
[php]
mode = "fpm"                 # "fpm" (default, unchanged) | "worker"
# worker-mode only:
worker_script = "worker.php" # entrypoint, relative to document_root; required if mode="worker"
worker_count = 0             # 0 => derive from CPU count (see below); else explicit
worker_max_requests = 500    # recycle a worker after N requests (0 = never)
worker_backlog = 0           # dispatch queue depth; 0 => = worker_count
worker_boot_timeout = 30     # seconds; boot must reach first take_request() within this
worker_populate_superglobals = false  # true for the WordPress adapter
```

- `mode` defaults to `"fpm"` — existing deployments are byte-for-byte
  unchanged. This is a **whole-server** switch, not per-path (§4.4).
- `worker_count = 0` derives a default. **Unlike `[php] workers`** (which
  defaults to 0=unlimited *deliberately* because uncapped-but-lazy is safe for
  fpm — see the pointed comment at `config lib.rs:1460-1481`), worker mode
  *must* pick a concrete count because each worker is a permanently-parked OS
  thread holding a full framework in memory. Derive `num_cpus` (clamped
  `[2, 32]`); document that heavy frameworks (WordPress ~40MB/worker) may want
  it lower.
- `worker_max_requests` is the memory-leak guard (§5.2). Default 500 —
  conservative, matches php-fpm `pm.max_requests` conventions.

### 4.2 Relationship to `[php] workers` (the existing semaphore)

`[php] workers` (`config lib.rs:1167-1179`) is the fpm `max_children`
semaphore. In **worker mode it is ignored** — concurrency is bounded by
`worker_count` (the number of parked threads) and `worker_backlog` (queue
depth), not the semaphore. This must be surfaced at startup: if
`mode="worker"` and `workers>0`, log a WARN that `[php] workers` has no effect
in worker mode (the `add-config-knob` "never a silent no-op" rule). Do **not**
silently repurpose `workers` as `worker_count` — they have different semantics
(cap-with-queue vs. thread-pool-size) and conflating them would surprise fpm
users flipping to worker mode.

### 4.3 Validation (fail fast at startup)

- `mode="worker"` with no `worker_script`, or a `worker_script` that doesn't
  resolve to a file under `document_root` → hard error at config load, before
  the runtime starts. (Reuse the existing config-load error path in
  `crates/ephpm/src/main.rs`.)
- `mode="worker"` on a **NTS build (Windows)** → see §6.1. Either degrade to
  `worker_count=1` with a WARN, or hard-error, per the decision there.
- `mode="worker"` with `sites_dir` (multi-tenant vhosting) → **Phase-1
  unsupported**, hard error. Worker mode boots *one* framework per worker;
  per-host frameworks need per-host worker pools (a Phase-N item). Multi-tenant
  stays on fpm.

### 4.4 Why whole-server, not per-path

Per-path fpm/worker mixing would require the worker threads and the
`spawn_blocking` fpm path to coexist against the same `php_embed` instance,
with two different request-lifecycle models racing on the same shared
`sapi_module`. The per-iteration reset (§3) and the fpm shutdown/startup
(`ephpm_execute_request`) make **incompatible** assumptions about whether the
executor persists. Mixing them per-path invites exactly the UAF class the
hardening fixed. Phase 1 is whole-server: `mode="worker"` routes *all* PHP
through the worker pool; static files, ACME, health, metrics are unchanged
(they never entered PHP anyway — `router.rs:439-469`). A future "worker for
`/`, fpm for `/legacy.php`" split is a separate design.

### 4.5 Warmup & graceful drain

- **Warmup:** at startup, after spawning workers, block server readiness
  (`/_ephpm/ready`, `router.rs:452-454`, `readiness_check` `:893-901`) until at
  least one worker has completed boot and reached its first `take_request()`.
  Extend `readiness_check` to also require `WorkerPool::ready_count() > 0` in
  worker mode. This prevents load balancers from routing before a framework is
  booted.
- **Graceful drain:** on shutdown signal (`shutdown_signal`,
  `server/src/lib.rs:705`), stop accepting new dispatch (close the dispatch
  `Sender`), let in-flight worker iterations finish (their `oneshot` completes
  normally), then each worker's `take_request()` returns 0 (null) → framework
  loop exits → `ts_free_thread` (§5.4). Bound the drain by the existing
  graceful-shutdown window.

---

## 5. Lifecycle & fault tolerance

### 5.1 Worker boot

```text
WorkerPool::spawn(worker_id):
  std::thread::spawn:
    ephpm_thread_init()            // TSRM + long request  (ephpm_wrapper.c:442)
    if err -> record boot_failure metric; mark worker dead; return
    ephpm_worker_run(worker_script_cstr):
       // C: php_execute_script(worker.php) under zend_try/zend_catch bailout guard,
       //    same SETJMP structure as ephpm_execute_request:850-873.
       // worker.php calls Ephpm\Worker\take_request() in a loop.
       // This call returns only when the framework loop ends (shutdown or fatal).
```

Boot happens exactly once per worker. If boot itself fatals (bad
`worker.php`, missing autoload), the worker never reaches `take_request` — the
`zend_try/zend_catch` around `php_execute_script` catches the bailout, we log
the fatal, increment `ephpm_worker_boot_failures_total`, and **respawn** (with
a backoff cap — if boot fails K times in a window, mark the pool degraded and
surface it on `/_ephpm/ready`).

### 5.2 Request loop & recycle after N requests

The worker increments a per-thread request counter each time `send_response`
completes. When it hits `worker_max_requests`, `take_request()` returns **0
(null)** on its *next* call — the framework loop exits cleanly, `ephpm_worker_run`
returns to Rust, and `WorkerPool` respawns a fresh worker (fresh boot). This is
the roadmap's "cooperative retire" option 1 (`laravel-octane-driver.md:286-291`),
chosen over hard thread-recycle because tearing down and re-creating the OS
thread interacts badly with the TSRM per-thread-init guard and gains little.
The recycle re-runs the framework bootstrap, which is the point — it reclaims
any slow memory growth in the framework's own state.

Counting happens in Rust (the `send_response` ops-table callback bumps a
`thread_local!` counter), so the framework can't accidentally defeat it.

### 5.3 Crash recovery — a fatal must not wedge the server

This is the highest-stakes requirement. A PHP fatal / `zend_bailout` inside a
worker request `longjmp`s out of the framework handler. Three things must hold:

1. **The in-flight HTTP request gets a 500.** `ephpm_worker_run` wraps the
   whole loop in `SETJMP` (like `ephpm_execute_request:850-873`). But a
   longjmp from deep in the framework unwinds *past* the current iteration's
   `send_response` — so the parked `oneshot::Sender` for that request would
   never be `.send()`. **Solution:** the `oneshot::Sender` is stashed in a
   `thread_local!` cell (§2.2). On the Rust side, when `ephpm_worker_run`
   returns after a bailout, the worker-supervision code checks the TLS cell: if
   a sender is still present (response never sent), it sends a
   `WorkerResponse::InternalError` so the hyper handler returns **500** instead
   of hanging on `rx.await`. The `oneshot::Receiver` also treats a *dropped*
   sender (sender's `Drop` fires if the whole thread unwinds) as
   `Err(RecvError)` → 500. Two independent safety nets.
2. **No Rust destructors are lost across the bailout.** The
   non-negotiable constraint (CLAUDE.md; roadmap `:270-274`): nothing with a
   meaningful `Drop` may be live on the Rust stack across a PHP call that can
   longjmp. The only Rust state live across `take_request()`/the framework call
   is the `oneshot::Sender` in TLS — and `oneshot::Sender`'s Drop is
   longjmp-safe (it just signals the receiver; no files, no locks). We must
   **not** hold a `MutexGuard`, an open `File`, or a DB handle across the
   worker loop. This is an auditable invariant for the `review-ephpm` lens.
3. **The worker is recycled after any bailout.** A fatal may have left the
   framework's executor in a corrupt-ish state (half-torn objects). After a
   caught bailout, we do **not** resume the loop on that same booted kernel —
   `ephpm_worker_run` returns, and `WorkerPool` respawns a fresh worker with a
   clean boot. Cost: one framework re-boot per fatal. Correct: yes.

### 5.4 Detecting dead / hung workers; timeouts

- **Hung request (infinite loop / blocked syscall in PHP).** SIGPROF-based
  `max_execution_time` is no-op'd in this codebase (`ephpm_wrapper.c:532-566`)
  — enforcement is at the HTTP layer. The existing outer
  `tokio::time::timeout(request_timeout, ...)` (`router.rs:377-386`) fires and
  returns 504 to the client. **But the worker thread is still stuck** running
  PHP — tokio's timeout cancels the *await*, not the OS thread. So: when the
  `oneshot` times out, the supervisor marks that worker "suspect", removes it
  from the dispatch pool (stops sending it new jobs), and spawns a replacement.
  The stuck thread is *abandoned* (it will finish or leak — we cannot safely
  kill a PHP thread mid-execution without corrupting the ZMM). A repeated-hang
  metric surfaces the bad script. This matches how every persistent-PHP server
  (FrankenPHP, RoadRunner) handles a genuinely-wedged worker: replace, don't
  kill.
- **Dead worker (thread panicked / boot-failed).** `WorkerPool` holds
  `JoinHandle`s; a supervision task (`tokio::spawn` a monitor, or a
  `on_thread_stop`-style hook) notices a terminated worker and respawns to
  maintain `worker_count`.
- **Boot-storm protection.** Exponential backoff on respawn; if >K respawns in
  a window, `/_ephpm/ready` reports not-ready so orchestrators stop routing.

---

## 6. Platform reality

### 6.1 Windows (NTS — one PHP context)

Windows builds are NTS (`ZTS=0`) — a **single** PHP interpreter context,
serialized (CLAUDE.md "PHP threading"; `ephpm_wrapper.c:496-501` NTS stubs).
Worker mode's whole premise is multiple parked contexts, one per worker thread.
Under NTS there is exactly **one** context, so:

- **Decision: NTS worker mode = single worker (`worker_count` forced to 1),
  with a WARN.** One booted framework, requests serialized through it — this is
  still a real win over NTS fpm (which re-boots the framework every request),
  just without concurrency. This matches the NTS story elsewhere in the
  codebase: NTS "falls back to serialized execution via mutex" (session locking
  note `ephpm_wrapper.c:1319-1321`). A single worker needs no cross-thread
  TSRM; the dispatch channel still works (one consumer).
- *Rejected:* hard-erroring on Windows. Single-worker worker mode is useful
  (dev, low-traffic Windows deployments) and keeps the config surface uniform.
- Concurrency on Windows therefore = 1 for PHP. Static files etc. remain
  concurrent (they never touch PHP). Document this clearly as a known
  limitation, same tier as "sqld not supported on Windows".

### 6.2 OPcache / JIT with long-lived workers

- OPcache compiled bytecode lives in SHM and already survives the fpm
  shutdown/startup cycle (`ephpm_wrapper.c:757-760`). In worker mode it is
  *even more* favorable: the framework is compiled once at boot and every
  subsequent request hits warm opcache with zero recompilation. This is a large
  part of the win.
- **JIT:** the JIT buffer is process-wide (SHM). Long-lived workers are ideal
  for JIT — hot paths get traced/compiled once and stay hot. No special
  handling needed; ensure `opcache.jit_buffer_size` is set via the generated
  ini (the ini-file-via-MINIT path, `main.rs:506-517`) not runtime
  `ini_set`, since runtime ini changes don't propagate to TSRM threads
  (that comment, `:505-511`).
- **Preloading (`opcache.preload`):** runs once at module startup, before any
  worker boots — fully compatible and a natural pairing with worker mode
  (per-vhost preload is a roadmap item, out of Phase 1).

### 6.3 macOS `--wrap` gap relevance

The SIGPROF/`--wrap` overrides (`ephpm_wrapper.c:527-582`) are what make it
safe to *not* re-init signals per request. Worker mode calls
`php_request_startup` **once** (at boot), so it depends on the same
`__wrap_zend_signal_*` / `__wrap_zend_set_timeout` no-ops being in effect. If
the macOS linker doesn't honor `--wrap` for a given symbol (the known gap),
worker mode has the *same* exposure as the current fpm reuse path — no worse,
no better. Since worker mode calls the signal-init path fewer times (once vs.
per-request), it is if anything *less* exposed. No new mitigation required;
just verify the release build's link map includes the wraps on macOS as part of
Phase-1 exit criteria.

---

## 7. Observability

New metrics (register alongside the existing `ephpm_*` families; the codebase
already uses the `metrics` crate + Prometheus exporter — `router.rs:16`,
`:374-403`, `:780-784`):

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `ephpm_worker_pool_size` | gauge | — | configured `worker_count` |
| `ephpm_worker_busy` | gauge | — | dispatched requests awaiting a worker response (includes jobs still queued, so it can exceed `worker_count` when the backlog is deep) |
| `ephpm_worker_idle` | gauge | — | workers parked in `take_request()` |
| `ephpm_worker_requests_total` | counter | `worker_id` | requests handled per worker |
| `ephpm_worker_recycles_total` | counter | `reason` (`max_requests`\|`script_exit`\|`fatal`\|`hung`) | worker respawns |
| `ephpm_worker_boot_failures_total` | counter | — | worker threads that exited before reaching their first `take_request()` (framework failed to boot) |
| `ephpm_worker_boot_timeouts_total` | counter | — | boots still running when `worker_boot_timeout` expired (the thread is not killed; see §5) |
| `ephpm_worker_boot_duration_seconds` | histogram | — | time to first `take_request()` |
| `ephpm_worker_dispatch_queue_depth` | gauge | — | current bounded-channel occupancy (backpressure signal) |
| `ephpm_worker_request_wait_seconds` | histogram | — | time a job waited in the queue before a worker picked it up |

`idle` is maintained inside the dispatch recv (increment when a worker parks,
decrement when it picks up a job or the loop ends); `busy` is maintained by the
HTTP handler around the response await. `queue_depth` and `wait_seconds`
are the two that tell an operator "add more workers". Keep the existing
`ephpm_php_execution_duration_seconds` (`router.rs:779`) meaningful by recording
it around the worker handle time too, so fpm↔worker latency is comparable.

---

## 8. Packaging / repos

The engine (this design) ships **in the ephpm repo**. Everything framework-facing
is a separate Composer package under the `ephpm/` vendor namespace, distributed
via its GitHub repository as a Composer `vcs` repo (not Packagist).

**Stays in `ephpm/ephpm` (this repo):**
- The Rust/C engine: `worker_bridge.rs`, `worker_pool.rs`, the C
  `take_request`/`send_response`/`ephpm_worker_reset_request`/`ephpm_worker_run`
  functions, config, metrics.
- A **reference worker script** `examples/worker/worker.php` — a ~20-line
  hand-written loop proving the primitive (the Phase-1 exit criterion in
  `laravel-octane-driver.md:339-341`). Not a framework adapter — just
  `while ($e = take_request()) { send_response(200, [...], "hello"); }`.

**New org repos (each its own repo under `github.com/ephpm`, installed as a Composer `vcs` repo):**

| Package | Repo | Phase | Purpose |
|---|---|---|---|
| `ephpm/worker` | `ephpm/php-worker` | 2 | Base PHP SDK: the `Envelope` type shim, `Ephpm\Worker\*` function stubs w/ IDE typehints, fail-fast guard if not running under ephpm. All adapters depend on this. |
| `ephpm/psr15-worker` | `ephpm/psr15-worker` | 2 | The 60-line PSR-15 `Worker` (`psr-15-worker-mode.md:44-98`). Depends on `nyholm/psr7`. |
| `ephpm/octane-driver` | `ephpm/octane-driver` | 2 | Laravel Octane `ephpm` driver: `Client`, `ServerProcess`, worker bootstrap (`laravel-octane-driver.md:80-88`, `:343-350`). |
| `ephpm/symfony-runtime` | `ephpm/symfony-runtime` | 3 | Symfony Runtime component adapter. |
| `ephpm/wordpress-worker` | `ephpm/wordpress-worker` | 3 | WordPress worker (needs `worker_populate_superglobals`; the trickiest — WP assumes real superglobals + globals galore). |
| `ephpm/psr16-cache`, `ephpm/psr6-cache` | (in `ephpm/psr15-worker` or standalone) | 4 | KV-backed PSR-16/6 over `ephpm_kv_*` (`psr-15-worker-mode.md:224-232`). |

The `ephpm/worker` base package is the important **new** dependency the
roadmaps under-specify: all four adapters share the `Envelope` shape and the
`Ephpm\Worker\take_request/send_response` signatures, so those belong in one
base package rather than being copy-pasted. Adapters alias
`Ephpm\Worker\take_request` to `Ephpm\Octane\take_request` etc. as their
contracts require.

---

## 9. Phasing & risks

### Phase 1 — Worker-mode primitive (this design) — the ONLY Rust-heavy phase

Deliver: dedicated worker-thread pool, `async_channel` dispatch + `oneshot`
return, the two SAPI functions + Envelope via the ops-table/MINIT pattern, the
per-iteration reset (`ephpm_worker_reset_request`), boot/recycle/crash-recovery
lifecycle, config knobs, metrics, and the reference `worker.php`.

**Exit criteria** (mirrors `laravel-octane-driver.md:339-341`):
- A hand-written `worker.php` in a `while (Ephpm\Worker\take_request())` loop
  serves "hello world" with **zero per-request bootstrap** (prove via a boot
  counter that increments once, not per request).
- Concurrency: N workers serve N concurrent requests in parallel on Linux
  (ZTS), verified under load; NTS/Windows serves with 1 worker.
- Fatal in a request → that request 500s, worker recycles, next request
  succeeds on a fresh boot, server never wedges (the marquee fault-tolerance
  test).
- `worker_max_requests` recycles a worker at the configured count.
- Graceful drain: in-flight requests finish, workers exit cleanly on SIGTERM.
- Stub mode (no `php_linked`) compiles and all non-PHP tests pass.
- Zero clippy pedantic warnings; every `unsafe` block has a `// SAFETY:`.

### Phase 2 — First adapter (Octane or PSR-15) + `ephpm/worker` base package
Ship `ephpm/worker` + `ephpm/psr15-worker` (smallest surface). **Exit:**
`vendor/bin/ephpm-worker` serves a stock Mezzio and Slim skeleton
(`psr-15-worker-mode.md:206-213`). Then/parallel: `ephpm/octane-driver`, exit =
`php artisan octane:start --server=ephpm` (`laravel-octane-driver.md:343-350`).

### Phase 3 — Streaming bodies
`bodyStream()` becomes a real `php://` stream over hyper's incremental body
reader; `send_response` consumes response streams incrementally. **Exit:** 1 GB
multipart upload without worker memory exceeding `upload_max_filesize`
(`psr-15-worker-mode.md:215-222`). This is where the Phase-1 single-copy
marshaling (§1.4) is replaced.

### Phase 4 — Cache bindings + ticks
PSR-16/6 over `ephpm-kv`; `Ephpm\Worker\on_tick()` on a dedicated TSRM tick
thread (not a request worker — `laravel-octane-driver.md:240-243`).

### Phase 5 — More adapters (Symfony Runtime, WordPress), cluster `Octane::table`.

### Top risks & mitigations

| Risk | Why it bites | Mitigation |
|---|---|---|
| **State leakage between requests** | The framework or the SAPI leaks per-request state into the next request on the same booted kernel (stale `$_SERVER`, leftover response headers, an unclosed session). | Minimal-but-complete per-iteration reset (§3.5) built from the *exact* lines the fpm hardening already proved (`:823-825`, `:844`); adapters own app-state reset; an integration test that asserts request N+1 sees none of request N's superglobals/headers/status. |
| **Memory growth** | Long-lived framework accumulates references (event listeners, static caches, DI singletons) → OOM over hours. | `worker_max_requests` recycle (§5.2) + `ephpm_worker_recycles_total` telemetry; document tuning; recommend adapters flush their own caches (Octane does this via listeners). |
| **Fatal kills the kernel / wedges server** | A `zend_bailout` unwinds past the response send; a hung script parks a worker forever. | Two-net response guarantee (TLS sender check + dropped-sender→500, §5.3); mandatory worker recycle after any bailout; hung-worker → replace-not-kill + 504 via existing outer timeout (§5.4). |
| **TSRM correctness** | Worker registers TSRM once and keeps the request open for its life; a mistake here corrupts per-thread globals across workers. | `ephpm_thread_init` is the *only* TSRM entry, guarded by the existing `thread_local!` bool (`lib.rs:482-502`); the long request is never shut down mid-life; `ts_free_thread` only at drain (§5.4); ZTS-only concurrency, NTS forced to 1 worker (§6.1). |
| **`--wrap` / signal reinit** | Worker relies on signal no-ops for its single boot-time `php_request_startup`. | Fewer calls than fpm (once vs per-request) → strictly less exposure; verify link map on macOS as a Phase-1 exit check (§6.3). |
| **Superglobal UAF re-introduction** | If WordPress mode hand-rebuilds superglobals it can re-trigger the `php_default_treat_data` UAF (`:773-789`). | Default off; when on, drive population through the normal treat_data path at a quiescent point, never hand-rebuild `PG(http_globals)`; fuzz the WordPress path before shipping the WP adapter (Phase 5). |
| **Config foot-guns** | Flipping `mode="worker"` silently ignores `[php] workers`, or worker mode silently no-ops on Windows/multi-tenant. | `add-config-knob` discipline: WARN when `workers>0` under worker mode; hard-error on `sites_dir`+worker (§4.3); explicit NTS single-worker WARN (§6.1). |

---

## 10. Concrete build decomposition (for sub-agents)

Ordered, each with a clear boundary. Items marked (C) touch `ephpm_wrapper.c`,
(R) are Rust, (P) are PHP/packaging.

1. **(config)** Add `PhpConfig` fields + validation + WARN-on-ignored-`workers`
   + startup errors for missing `worker_script` / `sites_dir` conflict.
   *Boundary:* `ephpm-config`, no runtime behavior yet. `add-config-knob` skill.
2. **(C)** `ephpm_worker_reset_request()` and `ephpm_worker_run(script)` (boot +
   bailout-guarded `php_execute_script`), plus register `Ephpm\Worker\take_request`
   / `send_response` in the MINIT `additional_functions` table
   (`ephpm_wrapper.c:1744-1766`). Wire an `EphpmWorkerOps` setter mirroring
   `ephpm_set_kv_ops` (`:1786-1791`).
3. **(R)** `worker_bridge.rs`: `EphpmWorkerOps`, the `take_request` (recv_blocking
   + TLS sender stash) / `send_response` (TLS sender send) callbacks, `#[cfg(php_linked)]`
   gated with stub fallbacks. Mirror `kv_bridge.rs` structure exactly.
4. **(R)** `worker_pool.rs`: OS-thread pool, `async_channel` dispatch, `oneshot`
   return type, boot/warmup, recycle counter, crash-recovery supervision,
   hung-worker replacement.
5. **(R)** Router integration: in `handle_php`, branch on `mode` — worker mode
   sends a `WorkerJob` to the pool and awaits `oneshot` instead of
   `spawn_blocking` (`router.rs:680-787`); reuse `build_php_response`
   (`:1132-1200`) unchanged for the response.
6. **(R)** Server wiring: construct `WorkerPool` after PHP init (`main.rs:554-559`),
   before serving; extend `readiness_check` (`router.rs:893-901`) for worker
   readiness; graceful drain on shutdown (`server/src/lib.rs:705`).
7. **(R)** Metrics (§7).
8. **(P)** Reference `examples/worker/worker.php` + an integration test proving
   the exit criteria (boot-once counter, fatal-recycle, drain).
9. **(P, later phases)** `ephpm/worker` base package, then adapters.

Items 2+3 must land together (the C↔Rust ABI). Items 4–6 depend on 2+3.
Item 1 is independent and can land first. Item 8 is the acceptance harness.

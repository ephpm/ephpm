# ePHPm Worker Mode — 3.0 Roadmap & Repo Plan

Companion to [`worker-mode-design.md`](./worker-mode-design.md) (the Phase-1
engine design). This is the phasing, the repo/packaging plan, and the
sub-agent build decomposition.

**3.0 headline:** persistent worker mode — boot the framework once per worker,
loop over requests without re-bootstrapping. 5–20× throughput for heavy
frameworks (Laravel, Symfony, WordPress). ePHPm's real differentiator vs. a
"php-fpm in a box."

---

## Phases & exit criteria

| Phase | What ships | Where | Exit criterion |
|---|---|---|---|
| **1 — Engine** (Rust/C, the hard core) | dedicated worker-thread pool, `async_channel` dispatch + `oneshot` return, `Ephpm\Worker\take_request()`/`send_response()` + `Envelope` via the ops-table/MINIT pattern, per-iteration reset, boot/recycle/crash-recovery, config, metrics, reference `worker.php` | `ephpm/ephpm` repo | Hand-written `worker.php` serves hello-world with **zero per-request bootstrap** (boot counter increments once); N workers serve N concurrent requests on Linux; a fatal 500s that request, recycles the worker, next request succeeds, server never wedges; graceful drain; stub mode compiles |
| **2 — First adapters** | `ephpm/worker` base package + `ephpm/psr15-worker` + `ephpm/octane-driver` | new org repos | `vendor/bin/ephpm-worker` serves a stock Mezzio and Slim skeleton; `php artisan octane:start --server=ephpm` works |
| **3 — Streaming bodies** | `bodyStream()` → real `php://` stream over hyper's incremental reader; streamed responses; `ephpm/symfony-runtime`, `ephpm/wordpress-worker` | engine + new repos | 1 GB multipart upload without worker memory exceeding `upload_max_filesize` |
| **4 — Cache bindings + ticks** | PSR-16/PSR-6 over `ephpm-kv`; `Ephpm\Worker\on_tick()` on a dedicated tick thread | new repos + engine | framework cache served from `ephpm-kv` across worker reuse |
| **5 — Ecosystem** | more adapters, cluster `Octane::table` equivalent over `ephpm-kv`, per-vhost worker pools (multi-tenant worker mode) | engine + repos | — |

**3.0 = Phase 1 + Phase 2** (engine proven end-to-end by at least the PSR-15
adapter against a real framework). Phases 3–5 are 3.x.

---

## Repos & packaging — "do we need repos for PSR-15?"

**Yes.** The engine lives in `ephpm/ephpm` (Rust/C). Every framework-facing
piece is a **separate Composer package on Packagist** under the `ephpm/` vendor
namespace, PHP namespace `Ephpm\<Area>\*`. Today only **`ephpm/cache-wordpress`**
exists as a shipped org PHP package (it's the naming/convention template:
`composer require ephpm/<name>`, installs to `vendor/ephpm/<name>/`). None of the
worker packages exist yet.

New repos to create (each its own `github.com/ephpm/<repo>`):

| Package | New repo | Phase | Purpose |
|---|---|---|---|
| `ephpm/worker` | `ephpm/php-worker` | 2 | **Base SDK** all adapters depend on: `Ephpm\Worker\Envelope` type, `take_request()`/`send_response()` stubs with IDE typehints, fail-fast guard when not run under ePHPm. (The roadmaps under-specify this shared base — it's the first thing to create.) |
| `ephpm/psr15-worker` | `ephpm/psr15-worker` | 2 | ~60-line PSR-15 `Worker`; unlocks Mezzio, Slim, Yiisoft, every PSR-15 framework. Depends on `nyholm/psr7`. |
| `ephpm/octane-driver` | `ephpm/octane-driver` | 2 | Laravel Octane `ephpm` server driver. |
| `ephpm/symfony-runtime` | `ephpm/symfony-runtime` | 3 | Symfony Runtime component adapter. |
| `ephpm/wordpress-worker` | `ephpm/wordpress-worker` | 3 | WordPress worker (needs `worker_populate_superglobals`; trickiest — WP assumes real superglobals). |

The **reference worker script** (`examples/worker/worker.php`, a ~20-line raw
loop) stays in the ephpm repo — it's the Phase-1 acceptance artifact, not an
adapter.

---

## Phase-1 build decomposition (the engine)

Phase 1 is **one tightly-coupled engine** — the C ABI, the Rust bridge, the
worker pool, and the router branch must all agree on the same types. It is built
as **one coherent workstream / one PR**, not fanned out across independent
agents (splitting a shared-ABI engine across blind agents produces integration
hell). Internal ordering (from the design §10):

1. **(config)** `PhpConfig` worker fields + validation + WARN-on-ignored-`workers` + hard errors (missing `worker_script`, `sites_dir` conflict). Independent; lands first.
2. **(C + R, together)** `ephpm_worker_reset_request` / `ephpm_worker_run` in `ephpm_wrapper.c`; register `Ephpm\Worker\take_request`/`send_response` in the MINIT `additional_functions` table; `EphpmWorkerOps` setter mirroring `ephpm_set_kv_ops`; the Rust `worker_bridge.rs` ops table (recv_blocking + TLS `oneshot::Sender` stash / send). **The C↔Rust ABI must land as one unit.**
3. **(R)** `worker_pool.rs`: OS-thread pool, `async_channel` dispatch, boot/warmup, recycle counter, crash-recovery supervision, hung-worker replacement.
4. **(R)** Router branch on `[php] mode`: worker mode dispatches a `WorkerJob` + awaits `oneshot` instead of `spawn_blocking`; reuse `build_php_response`.
5. **(R)** Server wiring: construct `WorkerPool` after PHP init, before serving; worker-aware readiness; graceful drain.
6. **(R)** Metrics.
7. **(P)** Reference `worker.php` + acceptance integration test (boot-once counter, fatal-recycle, drain).

Phase 2+ (the adapters) **is** the parallel-friendly part — each Composer
package is independent and gets its own repo + agent once the engine is proven.

---

## Per-adapter acceptance gates (do NOT ship an adapter without these)

Each adapter has its own correctness bar. Worker mode's danger is **state
leakage between requests on a booted kernel** — so every adapter needs a suite
that proves request N+1 sees *nothing* from request N.

### WordPress worker (`ephpm/wordpress-worker`) — the hardest, gets the most tests

WP is the trickiest adapter and **must not ship without a full e2e suite**. It's
uniquely dangerous because, unlike Octane/PSR-15 (which build their own
`Request` and never touch superglobals), **WordPress assumes real
`$_GET`/`$_POST`/`$_SERVER`/`$_COOKIE`/`$_FILES` and carries enormous global
state** (`$wp_query`, `$wpdb`, `$wp_object_cache`, the `$GLOBALS` soup, hooks
registered at boot). Required before shipping:

- **`worker_populate_superglobals` path must be fuzzed.** Turning superglobals
  back on re-enters the `php_default_treat_data` path that caused the fpm UAF
  (design §3.4 / `ephpm_wrapper.c:773-789`). Never hand-rebuild `PG(http_globals)`;
  drive population through the normal treat_data path at a quiescent point; fuzz
  GPC/multipart inputs before shipping.
- **State-leakage suite:** two back-to-back requests with different query
  strings / cookies / POST bodies each see only their own superglobals; no
  `$_SESSION`, `$wp_query`, or global-scope bleed from the prior request.
- **Real-WordPress golden-path e2e:** boot a stock WP install once per worker,
  serve the homepage, a post, wp-admin login, a REST API call, and a
  cache-backed page — asserting boot-once (framework bootstraps a single time,
  not per request) and correct isolation across a concurrent load.
- **Plugin/global-mutation stress:** a plugin that mutates globals per request
  must not corrupt the next request (or the worker recycles cleanly).
- **Object cache interaction:** the `ephpm/cache-wordpress` drop-in under worker
  mode (KV persists across requests — verify no stale-cache cross-request bugs).
- **Fatal-in-a-hook recycle:** a fatal inside a WP hook 500s that request,
  recycles the worker, and the next request boots a clean WP — server never
  wedges.

### Other adapters
- **PSR-15 / Octane / Symfony:** state-leakage suite (N+1 sees nothing from N),
  boot-once proof, concurrency under load, fatal→500+recycle, graceful drain,
  each against a stock skeleton app (Mezzio + Slim for PSR-15; a Laravel skeleton
  for Octane; a Symfony skeleton for the runtime adapter).

## Top risks (carried from the design)

- **State leakage** between requests → minimal-but-complete per-iteration reset built from the exact fpm-hardening lines; N+1-sees-nothing-from-N integration test.
- **Memory growth** in the long-lived kernel → `worker_max_requests` recycle + telemetry.
- **Fatal wedges the server** → two-net 500 guarantee (TLS sender check + dropped-sender→`RecvError`), mandatory recycle after any bailout, hung-worker → replace-not-kill + existing 504 timeout.
- **TSRM correctness** → single `ephpm_thread_init` per worker; long request never shut down mid-life; ZTS-only concurrency, NTS forced to 1 worker.
- **Superglobal UAF** (WordPress mode) → default off; never hand-rebuild `PG(http_globals)`; fuzz before shipping the WP adapter.

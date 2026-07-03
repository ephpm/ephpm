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

## Top risks (carried from the design)

- **State leakage** between requests → minimal-but-complete per-iteration reset built from the exact fpm-hardening lines; N+1-sees-nothing-from-N integration test.
- **Memory growth** in the long-lived kernel → `worker_max_requests` recycle + telemetry.
- **Fatal wedges the server** → two-net 500 guarantee (TLS sender check + dropped-sender→`RecvError`), mandatory recycle after any bailout, hung-worker → replace-not-kill + existing 504 timeout.
- **TSRM correctness** → single `ephpm_thread_init` per worker; long request never shut down mid-life; ZTS-only concurrency, NTS forced to 1 worker.
- **Superglobal UAF** (WordPress mode) → default off; never hand-rebuild `PG(http_globals)`; fuzz before shipping the WP adapter.

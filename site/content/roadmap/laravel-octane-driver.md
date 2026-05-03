# Laravel Octane: Native ePHPm Driver

A roadmap for a first-class **`ephpm` driver** in Laravel Octane. Octane is the
Laravel-side adapter layer that turns a stateless framework into a persistent
worker model — boot the app once, handle many requests. ePHPm already
implements that model at the SAPI layer; an Octane driver is the missing piece
that lets a stock Laravel app opt into worker mode without leaving ephpm or
adding a second process (FrankenPHP, Swoole, RoadRunner).

This is a Phase-2 item. It depends on the [PHP Worker Mode](/architecture/#php-worker-mode)
work — a generic per-thread "boot once, handle many" loop must exist before
the Octane driver can plug into it.

---

## Why a Native Driver

Octane already supports three backends:

| Driver | Backend | Cost to ePHPm |
|--------|---------|----|
| Swoole | Swoole/OpenSwoole extension + its own server | Run a second PHP server alongside ephpm — no point. |
| RoadRunner | Go process + binary protocol over pipes | Reintroduces a TCP/pipe hop in front of in-process PHP. |
| FrankenPHP | C extension exposing `frankenphp_handle_request()` | Closest contract; we could shim it but inherit FrankenPHP semantics. |

A **native `ephpm` driver** is the only option that keeps PHP fully
in-process, lets ePHPm's KV store back `Octane::table`, and stays free of
upstream contract drift (FrankenPHP changes its worker API on its own
schedule). The fit is unusually clean because every primitive Octane needs is
already in the ePHPm runtime:

| Octane needs | ePHPm provides |
|---|---|
| Long-lived PHP runtime, no per-request startup/shutdown | Already does this — single `php_embed_init`, manual superglobal reset between requests. |
| Per-worker isolation | Per-thread TSRM context (each `spawn_blocking` thread). |
| Shared in-process table (`Octane::table`) | `ephpm-kv` (DashMap, RESP, gossip-replicated). |
| Interval cache | `ephpm-kv` with TTL. |
| Concurrent task dispatch | tokio + `spawn_blocking`. |
| Tick callbacks | tokio `interval` + `spawn_blocking` into the worker. |

Cluster mode is a free upgrade: `Octane::table` automatically replicates across
nodes via the existing two-tier KV gossip layer — something no other Octane
driver can offer.

---

## Architecture

```
   ┌──────────────────────────────────────────────────────────────────────┐
   │                            ephpm process                              │
   │                                                                      │
   │   hyper ──► router ──► spawn_blocking ──► PHP worker thread          │
   │                                              │                       │
   │                                              ▼                       │
   │                              ┌──────────────────────────────┐        │
   │                              │ TSRM context (per thread)    │        │
   │                              │                              │        │
   │                              │  worker.php (booted once)    │        │
   │                              │  ├─ Laravel Application      │        │
   │                              │  ├─ Octane Worker            │        │
   │                              │  └─ ephpm Octane Client      │        │
   │                              │                              │        │
   │                              │   loop {                     │        │
   │                              │     $req = ephpm_take_req()  │        │
   │                              │     $resp = $worker->handle()│        │
   │                              │     ephpm_send_resp($resp)   │        │
   │                              │   }                          │        │
   │                              └──────────────────────────────┘        │
   │                                              ▲                       │
   │                                              │ SAPI bindings         │
   │                              ┌───────────────┴───────────────┐       │
   │                              │ ephpm-kv  (Octane::table)     │       │
   │                              │ ephpm-cluster  (replication)  │       │
   │                              │ ephpm-db  (pooled connections)│       │
   │                              └───────────────────────────────┘       │
   └──────────────────────────────────────────────────────────────────────┘
```

The driver lives in two places:

1. **PHP side** — a Composer package `ephpm/octane-driver` that registers the
   driver with `laravel/octane` and provides the `Client` / `ServerProcess`
   classes Octane requires.
2. **Rust side** — SAPI-registered functions in `ephpm-php` exposed under the
   namespace `Ephpm\Octane\*`, plus a worker-mode dispatcher in
   `ephpm-server` that hands requests to a long-lived PHP worker rather than
   the per-request handler.

---

## The Octane Driver Contract

Octane drivers implement three things:

| Class / interface | Responsibility |
|---|---|
| `Laravel\Octane\Contracts\Client` | Translate the server's request representation to a `Symfony\Component\HttpFoundation\Request`; ship `Response` back. |
| `Laravel\Octane\Contracts\ServerProcessInspector` (optional) | Health/inspection commands for `octane:status`. |
| Worker bootstrap script | Boots the `ApplicationFactory`, runs the request loop, fires Octane's lifecycle events. |

The lifecycle events the worker must emit (Octane subscribes to these to do
its container/state reset work):

- `WorkerStarting` / `WorkerStopping`
- `RequestReceived` / `RequestHandled` / `RequestTerminated`
- `TaskReceived` (if concurrent tasks supported)
- `TickReceived` (if ticks supported)

**State reset is not our problem.** Octane's listeners — `FlushArrayCache`,
`FlushAuthenticationState`, `FlushSessionState`, `FlushTranslatorState`,
`DisconnectFromDatabases`, `EnsureRequestServerPortMatchesScheme`,
`PrepareInertiaForNextOperation` — handle the framework-side cleanup
automatically as long as we fire the events.

---

## SAPI Surface (Rust → PHP)

The driver needs three Rust-backed PHP functions:

```php
namespace Ephpm\Octane;

/**
 * Block until the next HTTP request lands on this worker thread.
 * Returns null only on shutdown (worker should exit its loop).
 */
function take_request(): ?Request;

/**
 * Hand a fully-formed Symfony Response back to the HTTP layer.
 * Must be called exactly once per take_request().
 */
function send_response(Response $response): void;

/**
 * Register a tick callback. Fired from the runtime on a tokio interval,
 * dispatched into this worker via spawn_blocking.
 */
function on_tick(int $intervalMs, callable $cb): void;
```

Implementation notes:

- `take_request()` is a blocking SAPI call. Internally it parks the PHP
  thread on a `tokio::sync::oneshot` receiver fed by the HTTP router. This is
  the same shape as the existing per-request dispatch — just inverted, with
  PHP as the consumer instead of the runtime as the caller.
- `send_response()` writes via the existing SAPI `ub_write` / response header
  paths. No new code — it just routes through the parked oneshot sender.
- `on_tick()` registers the callback in a per-thread `Vec<TickHandle>` (no
  cross-thread sharing — each worker has its own ticks). The runtime side
  schedules `tokio::time::interval` futures that fire `spawn_blocking` into
  the right TSRM thread.

All three must be `#[cfg(php_linked)]`-gated; in stub mode the package fails
fast with a clear error so that `composer require ephpm/octane-driver` on a
non-ephpm host doesn't silently mis-route.

---

## Worker Loop (PHP side)

```php
// vendor/bin/octane-ephpm-worker (entrypoint)

use Ephpm\Octane\Client;
use Laravel\Octane\ApplicationFactory;
use Laravel\Octane\Worker;

$factory = new ApplicationFactory(getcwd());
$client  = new Client();
$worker  = new Worker($factory, $client);

$worker->boot([
    // Bindings that should survive across requests (cache, db, etc.)
]);

while ($request = \Ephpm\Octane\take_request()) {
    [$req, $context] = $worker->handle($request);
    $worker->terminate($req, $context);
    // $worker->handle() emits RequestReceived/RequestHandled,
    // and internally calls $client->respond() which calls send_response().
}

$worker->terminate();
```

This is structurally identical to the FrankenPHP worker bootstrap; only the
two `Ephpm\Octane\*` calls differ. We can lift much of FrankenPHP's
`frankenphp-worker.php` and rename the SAPI functions.

---

## Mapping Octane Primitives to ePHPm

### `Octane::table()` → `ephpm-kv`

Octane's table primitive is a typed shared-memory hash backed by a Swoole
Table or RoadRunner KV. ePHPm binds it to `ephpm-kv`:

```php
Octane::table('users')
    ->withKey('email', 'string')
    ->withColumn('failed_logins', 'int')
    ->withColumn('locked_until', 'string');
```

**Cluster bonus:** Once the table is registered, every node sees the same
data via gossip replication. This is the only Octane backend where
`Octane::table` works across machines without an external store. Document the
consistency model clearly — it is **eventually consistent** with the gossip
window (~10s), so `failed_logins` counters are safe but `locked_until`
should still write through to the database for hard guarantees.

### `Octane::concurrently()` → tokio `spawn_blocking`

```php
[$users, $orders, $invoices] = Octane::concurrently([
    fn () => User::all(),
    fn () => Order::all(),
    fn () => Invoice::all(),
]);
```

Each closure is shipped to a fresh PHP worker via `spawn_blocking` and the
results joined via `tokio::join_all`. The serialization boundary is
`opis/closure` — same as Octane's other drivers.

### Ticks → tokio `interval`

```php
Octane::tick('flush-metrics', fn () => Metrics::flush())
    ->seconds(5)
    ->immediate();
```

`Octane::tick` calls into `Ephpm\Octane\on_tick()` which registers an
interval on the runtime side. Tick callbacks run on a dedicated TSRM thread
(not a worker thread serving requests) so a long-running tick doesn't park
inbound HTTP traffic.

### Interval cache → `ephpm-kv` with TTL

`Cache::interval('feature-flags', fn () => …, seconds: 60)` is a shallow
wrapper over the existing `ephpm-kv` TTL key handler. No new infrastructure.

---

## State Sandbox: What Octane Handles, What ePHPm Must Avoid

**Octane handles (we get for free):**

- Container binding reset between requests
- Auth/session/cache/translator state flush
- Database connection reset (calls `DB::disconnect()` after each request)
- Request-scoped service provider re-registration

**ePHPm must avoid:**

- **Don't reset superglobals when a request enters a worker.** The existing
  `sapi_module.treat_data` superglobal-reset path runs between every HTTP
  request in the non-Octane path. In Octane mode we skip it: Octane builds
  its own `Symfony\Request` from the data we hand it via `take_request()`,
  and resets those globals itself if it wants them populated.
- **Don't share mutable Rust state across Octane workers.** Each
  `spawn_blocking` thread is its own world; per-worker tick handles, request
  channels, and cached PSR-7 builders all live in `thread_local!` storage.
- **Don't hold Rust destructors across `take_request()`.** The function
  unparks via PHP — if PHP `longjmp`s out of the worker loop on a fatal
  error, Rust destructors won't run. Park the response sender in a struct
  that's `Drop`-safe under abrupt exit (no file handles, no DB
  connections — just a `oneshot::Sender`).

---

## Open Issues

### Worker recycling

Octane's `--max-requests` flag tells a worker to retire after N requests.
ePHPm's tokio `spawn_blocking` pool isn't 1:1 with Octane workers — a single
blocking thread is reused across many short-lived `spawn_blocking` calls.
Two options:

1. **Cooperative retire** — when a worker hits `max_requests`, it returns
   from the loop and the runtime spawns a fresh `spawn_blocking` task on the
   same thread. The TSRM context isn't recycled, only the Laravel app.
2. **Hard retire** — recycle the entire blocking thread (requires
   `tokio::runtime::Builder::on_thread_stop` plumbing). Cleaner from a
   memory-leak standpoint but interacts with the existing TSRM
   per-thread-init guard.

Start with option 1; revisit if leak telemetry shows TSRM context bloat.

### Multiple `php_embed` requests

ePHPm's documented PHP request reuse pattern keeps **one long-running SAPI
request** open and resets superglobals between HTTP requests. Octane wants
the inverse: the SAPI request is the **worker boot**, and HTTP requests are
synthesized via `Symfony\Request`. The two models don't conflict — Octane's
worker.php runs *inside* the long-running SAPI request, takes over from the
runtime, and the HTTP-request envelope never enters the SAPI. Document this
boundary explicitly so future work doesn't accidentally re-introduce the
per-HTTP-request superglobal reset under Octane mode.

### `Octane::table` cluster semantics

What does `Octane::table()->where(…)` do when half the cluster has converged
the value and half hasn't? Document the consistency guarantee
(eventually-consistent with ~gossip-window staleness), and offer a
`->strong()` modifier (future) that routes through a Raft-backed key for the
small subset of users who need linearizability.

### Octane upstreaming vs. fork-and-publish

Three publishing options:

1. **Upstream PR to `laravel/octane`** — clean, but requires Taylor's
   blessing and the maintenance burden of staying in sync.
2. **`ephpm/octane`** Composer package that extends Octane via its driver
   registry — Octane already has `Octane::extend()` hooks for third-party
   drivers.
3. **Fork** — last resort. Bad for adoption.

Default plan: option 2. Most of Octane's driver surface is plugin-friendly;
we only need to register `ephpm` as a server name and ship our `Client`.

---

## Phasing

### Phase 1 — Worker mode primitive (prerequisite)

Generic "boot once, handle many" PHP worker loop in ephpm. Not Laravel- or
Octane-specific. Tracked under [PHP Worker Mode](/architecture/#php-worker-mode).

**Exit criteria:** a hand-written `worker.php` can sit in a
`while (ephpm_take_request())` loop and serve a "hello world" response with
zero per-request bootstrap.

### Phase 2 — Minimal Octane driver

`ephpm/octane-driver` Composer package with `Client`, worker bootstrap, and
the SAPI bindings. Supports `RequestReceived` / `RequestHandled` /
`RequestTerminated` events. No table, no concurrency, no ticks.

**Exit criteria:** `php artisan octane:start --server=ephpm` boots a Laravel
app and serves requests with proper container reset between them.

### Phase 3 — `Octane::table` integration

Wire `Octane::table` to `ephpm-kv`. Single-node only initially.

**Exit criteria:** the Octane test suite passes the table tests against the
ephpm driver.

### Phase 4 — Concurrency, ticks, interval cache

`Octane::concurrently()`, `Octane::tick()`, `Cache::interval()`. Each is a
separate PR.

### Phase 5 — Cluster-aware `Octane::table`

Replicate table writes through the gossip layer. Requires existing two-tier
KV plumbing.

**Exit criteria:** a three-node ePHPm cluster running the same Laravel app
sees consistent `Octane::table` reads within the gossip convergence window.

---

## Out of Scope

- **Octane's `--watch` mode.** ePHPm does not run on top of `chokidar` or any
  filesystem watcher. Use `composer install` + a process restart instead;
  `cargo xtask` flows already do this for us.
- **Roadrunner-protocol compatibility.** Explicitly rejected in
  [the overview](#why-a-native-driver) — adds a pipe hop for no gain.
- **Octane's HTTPS termination flags** (`--https`, `--http-redirect`).
  ePHPm's TLS layer already handles this and is configured via TOML, not
  Octane CLI flags.

---

## References

- [Laravel Octane source](https://github.com/laravel/octane) — driver contracts, lifecycle event surface
- [FrankenPHP worker mode](https://frankenphp.dev/docs/worker/) — closest analog to what we're building
- [Architecture: PHP Worker Mode](/architecture/#php-worker-mode) — prerequisite
- [Octane analysis](../../analysis/laravel-octane/) — what Octane does and doesn't do

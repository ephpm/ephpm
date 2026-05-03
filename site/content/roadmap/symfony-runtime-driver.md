# Symfony Runtime: Native ePHPm Adapter

A roadmap for a first-class **`ephpm` runtime adapter** under
`symfony/runtime`. Symfony Runtime is the framework's built-in seam for
swapping the entry point — PHP-FPM, Swoole, RoadRunner, FrankenPHP, ReactPHP,
and AWS Lambda (Bref) all plug in via the same `RunnerInterface`. An ephpm
adapter is what lets a stock Symfony / API Platform / Drupal-on-Symfony app
opt into worker mode without leaving the binary.

This is the Symfony parallel of the [Laravel Octane Driver](../laravel-octane-driver/).
It shares the same Rust-side prerequisite — see [PHP Worker Mode](/architecture/#php-worker-mode) —
and the same [SAPI surface](../laravel-octane-driver/#sapi-surface-rust--php). This
document focuses on what makes the Symfony side different.

---

## Why a Native Adapter

Symfony already has runtime adapters for the obvious backends — they live in
the [`php-runtime/runtime`](https://github.com/php-runtime/runtime) community
org under `runtime/frankenphp-symfony`, `runtime/swoole`, `runtime/reactphp`,
`runtime/bref`, etc. None of them know about ePHPm, so Symfony users currently
get the same suboptimal options Octane users get:

| Adapter | Backend | Cost to ePHPm |
|--------|---------|----|
| `runtime/swoole` | Swoole extension + its own server | Run a second PHP server alongside ephpm — no point. |
| `runtime/roadrunner` | Go process + binary protocol | Reintroduces a TCP/pipe hop in front of in-process PHP. |
| `runtime/frankenphp-symfony` | C extension exposing `frankenphp_handle_request()` | Closest contract; we could shim it but inherit FrankenPHP semantics. |
| `runtime/reactphp` | Userland PHP event loop | No FFI advantage; same cost as Octane Swoole. |

A **native `ephpm` adapter** keeps PHP fully in-process. The fit is even
cleaner than the Octane case because Symfony does most of the
state-management work inside the framework already:

| Symfony already has | ePHPm provides |
|---|---|
| `kernel.reset` tag + `ResetServicesListener` | Just call `Kernel::handle()` in a loop — services reset themselves between requests |
| `RuntimeInterface` / `RunnerInterface` | A blocking `take_request` SAPI call to drive the runner |
| Built-in PSR-7 / Symfony HttpFoundation request and response | The same `Symfony\Request` we pass through |
| Long-lived process patterns (Messenger workers) | Tokio blocking pool — same model |

The result: the entire Symfony adapter is **~100 lines of PHP**, not a
package the size of Octane. Most of it is parameter wiring.

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
   │                              │  public/index.php (entry)    │        │
   │                              │  ├─ APP_RUNTIME=…\Runtime    │        │
   │                              │  ├─ Kernel boot (once)       │        │
   │                              │  └─ ephpm Runner             │        │
   │                              │                              │        │
   │                              │   loop {                     │        │
   │                              │     $req = ephpm_take_req()  │        │
   │                              │     $resp = $kernel->handle()│        │
   │                              │     ephpm_send_resp($resp)   │        │
   │                              │     $kernel->terminate()     │        │
   │                              │     // kernel.reset listeners│        │
   │                              │     //   fire automatically  │        │
   │                              │   }                          │        │
   │                              └──────────────────────────────┘        │
   │                                              ▲                       │
   │                                              │ SAPI bindings         │
   │                              ┌───────────────┴───────────────┐       │
   │                              │ ephpm-kv  (ephpm_kv_* funcs)  │       │
   │                              │ ephpm-cluster                 │       │
   │                              │ ephpm-db  (pooled connections)│       │
   │                              └───────────────────────────────┘       │
   └──────────────────────────────────────────────────────────────────────┘
```

The adapter lives in two places:

1. **PHP side** — a Composer package `ephpm/runtime-symfony` that registers
   itself via Symfony's runtime resolver. Users opt in by setting
   `APP_RUNTIME` in their environment or `composer.json` — no code change to
   `public/index.php`.
2. **Rust side** — the same SAPI bindings ([`Ephpm\Octane\take_request` etc.](../laravel-octane-driver/#sapi-surface-rust--php))
   added for Octane. **Nothing Symfony-specific is needed in Rust.** Both
   adapters call the same primitives.

---

## The Symfony Runtime Contract

Symfony Runtime drivers implement two interfaces — both small:

| Interface | Methods | What it does |
|---|---|---|
| `Symfony\Component\Runtime\RuntimeInterface` | `getRunner($application)`, `getResolver()` | Discovers the user's `Kernel` (or whatever the entry point returned) and wraps it in a runner |
| `Symfony\Component\Runtime\RunnerInterface` | `run(): int` | The actual request loop — runs until shutdown, returns an exit code |

There is no equivalent of Octane's `Client` — Symfony Runtime hands the
runner the resolved application directly, and the runner calls
`Kernel::handle($req)` itself. There is no equivalent of Octane's worker
lifecycle events either; Symfony fires `kernel.request` / `kernel.response` /
`kernel.terminate` through the existing `EventDispatcher` and the
`ResetServicesListener` hooks into `kernel.terminate` to reset state.

Result: most of the work an Octane driver does is delegated to the
HttpKernel itself.

---

## SAPI Surface

**Identical to the Octane driver.** See
[Laravel Octane: SAPI Surface](../laravel-octane-driver/#sapi-surface-rust--php).
The Symfony adapter calls the same three functions:

```php
\Ephpm\Octane\take_request(): ?Request
\Ephpm\Octane\send_response(Response $r): void
\Ephpm\Octane\on_tick(int $intervalMs, callable $cb): void   // optional
```

The shared namespace (`Ephpm\Octane\*`) is a slight misnomer — these are
generic worker-mode primitives, not Octane-specific. We could rename to
`Ephpm\Worker\*` before stabilizing if both adapters land. Tracked under
[Open Issues](#shared-namespace-naming).

---

## Adapter Code (PHP side)

The complete adapter is roughly the following — three small classes:

```php
namespace Ephpm\Symfony\Runtime;

use Symfony\Component\HttpKernel\HttpKernelInterface;
use Symfony\Component\HttpKernel\TerminableInterface;
use Symfony\Component\Runtime\GenericRuntime;
use Symfony\Component\Runtime\RunnerInterface;
use Symfony\Component\Runtime\SymfonyRuntime;

final class Runtime extends SymfonyRuntime
{
    public function getRunner(?object $application): RunnerInterface
    {
        if ($application instanceof HttpKernelInterface) {
            return new Runner($application);
        }
        return parent::getRunner($application);   // fall back for console etc.
    }
}

final class Runner implements RunnerInterface
{
    public function __construct(private HttpKernelInterface $kernel) {}

    public function run(): int
    {
        while ($request = \Ephpm\Octane\take_request()) {
            $response = $this->kernel->handle($request);
            \Ephpm\Octane\send_response($response);

            if ($this->kernel instanceof TerminableInterface) {
                $this->kernel->terminate($request, $response);
                // kernel.terminate fires ResetServicesListener,
                // which calls reset() on every service tagged kernel.reset.
            }
        }
        return 0;
    }
}
```

That's the whole adapter. Compare against the Octane driver's `Client` plus
worker bootstrap plus driver registration — Symfony Runtime is leaner because
the framework was designed for this seam from the start.

User opts in via:

```bash
# .env or environment
APP_RUNTIME=Ephpm\Symfony\Runtime\Runtime
```

`public/index.php` is unchanged — Symfony's `Runtime` autoloader picks the
class up via `composer.json`'s `extra.runtime.class` field too.

---

## What Symfony Already Does (That Octane Has to Do Manually)

| Concern | Octane | Symfony |
|---|---|---|
| Reset container singletons between requests | `FlushArrayCache`, `FlushAuthenticationState`, … listeners | `kernel.reset` tag + `ResetServicesListener` (fires on `kernel.terminate`) |
| Reset DB connections | `DisconnectFromDatabases` listener | Doctrine's `EntityManager` is tagged `kernel.reset` automatically |
| Reset translator | `FlushTranslatorState` listener | `Translator` implements `ResetInterface` |
| Reset session | `FlushSessionState` listener | Session bag handles it |
| Reset request stack | Per-request scope binding | `RequestStack` is `ResetInterface` |
| Reset arbitrary user services | App responsibility (developer adds listeners) | App responsibility (tag with `kernel.reset` or implement `ResetInterface`) |

The last row is the migration story for app developers. Most modern Symfony
apps use third-party bundles that already implement `ResetInterface`. Apps
with custom stateful services need to either implement that interface or tag
the service `kernel.reset`. We document this clearly; we do not try to
auto-detect or fix it.

---

## What ePHPm Provides That Stock Symfony Doesn't

Symfony's stock setup gets you a worker-mode kernel and that's it. ePHPm adds:

- **Built-in HTTP server** with TLS/ACME, HTTP/2, static files — no nginx in
  front.
- **`ephpm-kv` exposed as `ephpm_kv_*` PHP functions** — gossip-replicated
  shared cache, available without leaving the binary. Equivalent to what
  `Octane::table` provides on the Laravel side, just exposed as functions
  rather than a facade.
- **Cluster awareness** — `ephpm_kv_*` reads/writes replicate via gossip.
- **In-process MySQL proxy** — Doctrine connects to `127.0.0.1:3306` and gets
  pooled, multiplexed connections to the real backend.
- **Embedded SQLite via litewire** — single-file deploys with optional
  clustering.

A stock `runtime/frankenphp-symfony` deploy needs an external Caddy, an
external Redis for the cache, and an external nginx-or-equivalent in
production. ePHPm is the whole stack.

---

## Adapter for Symfony Messenger Workers

`bin/console messenger:consume` is **already** a long-lived PHP process —
it's the `messenger:consume` command running in a loop, not request-driven.
Today it runs under `php-cli`. Under ePHPm it can run inside the same binary
as the HTTP workers, sharing:

- The same TSRM thread pool — Messenger workers are just another kind of
  worker thread.
- The same `ephpm-kv` instance — perfect for stamps, dedup keys, rate limits.
- The same `ephpm-db` connection pool — no separate connection budget for
  workers vs. HTTP.
- The same metrics/observability surface — Prometheus picks up worker stats
  alongside request stats.

This is **not** part of the minimum Runtime adapter. It's a Phase-3 add-on
covered under Open Issues. The hook point: `runtime/symfony` already routes
`Symfony\Component\Console\Application` through `getRunner()`, so a
console-aware runner can claim Messenger commands and run them on the
TSRM pool instead of forking a separate `php-cli` process.

---

## Open Issues

### Shared namespace naming

The SAPI primitives are currently scoped under `Ephpm\Octane\*` because
that's where they were defined first. Both adapters use them; the name is
misleading. Before either package leaves alpha, rename to a neutral
`Ephpm\Worker\*` (or `Ephpm\Runtime\*`) and have the Octane and Symfony
adapters both consume the new namespace. Either rename early or live with
the mismatch — don't add a deprecation shim layer for two packages that
ship together.

### `getenv()` pollution between requests

This is a known issue across **all** Symfony Runtime adapters in worker
mode. Symfony Runtime reads `$_SERVER` / `$_ENV` at boot to populate
`$context`; subsequent requests can leak env vars set during request handling
into later requests. The mitigation is the standard one used by FrankenPHP
and Swoole adapters: snapshot the env at boot, reset it from the snapshot at
the top of each request loop iteration. Document this as part of the
adapter's `run()` method.

### Where to publish the package

Three options, ranked:

1. **`ephpm/runtime-symfony`** under our own org — fastest to ship, full
   control, predictable maintenance. Default plan.
2. **Submit `runtime/ephpm` to the `php-runtime` org** — natural home (it's
   where FrankenPHP, Swoole, ReactPHP, Bref live). Requires their review.
   Ideal endgame once stable.
3. **Both** — publish under our org for early adopters, mirror to
   `php-runtime/runtime/ephpm` once it stabilizes. Adds some maintenance
   burden but the adapter is so small it doesn't matter.

Path: ship as `ephpm/runtime-symfony` first; submit upstream after Phase 2
stabilizes.

### Drupal-on-Symfony reuse

Drupal 11+ uses Symfony's HttpKernel under the hood. In principle the same
adapter works for Drupal — `\Drupal\Core\DrupalKernel` is a Symfony Kernel.
In practice Drupal has its own state-management story (caches, plugin
discovery, render cache) that may or may not survive worker mode cleanly.
Out of scope for the initial adapter; revisit once stable.

### `EventLoop` integration

Symfony 7.1+ ships
[`symfony/scheduler`](https://symfony.com/doc/current/scheduler.html) and
[`symfony/clock`](https://symfony.com/doc/current/components/clock.html)
which integrate with [Revolt](https://revolt.run/). ePHPm's tokio runtime
could in principle expose itself as a Revolt driver, allowing Symfony's
scheduler to drive callbacks via tokio rather than via a userland event
loop. Out of scope for the initial adapter; tracked separately under
[Architecture](/architecture/).

---

## Phasing

### Phase 1 — Worker mode primitive (prerequisite)

Same as Octane. See [PHP Worker Mode](/architecture/#php-worker-mode).
Shared with the Octane track — implementing it once unblocks both adapters.

**Exit criteria:** generic `while (ephpm_take_request())` loop in PHP serves
HTTP responses with zero per-request bootstrap.

### Phase 2 — Minimal Symfony Runtime adapter

`ephpm/runtime-symfony` Composer package. The three classes shown above plus
`composer.json`, `extra.runtime.class` registration, and an end-to-end test
against a stock `symfony/skeleton` app.

**Exit criteria:** `APP_RUNTIME=Ephpm\Symfony\Runtime\Runtime` in `.env`
makes a stock Symfony app serve requests through the ephpm worker loop, with
correct `kernel.reset` behavior.

### Phase 3 — Messenger worker integration

Detect `Symfony\Component\Console\Application` in `getRunner()`; for
`messenger:consume` commands, run the consumer loop on the ephpm TSRM pool
rather than as a separate CLI process.

**Exit criteria:** `messenger:consume async --time-limit=…` runs inside the
ephpm process and shares the in-process KV / DB pool.

### Phase 4 — Upstream to `php-runtime` org

Submit the package to the [`php-runtime/runtime`](https://github.com/php-runtime/runtime)
monorepo as `runtime/ephpm`. Coordinate with maintainers.

### Phase 5 — Optional: Revolt event-loop driver

Expose ephpm's tokio runtime as a Revolt driver so Symfony Scheduler / Clock
work without a userland event loop. Separate effort, separate PR.

---

## Out of Scope

- **Drupal-specific shims.** Drupal uses Symfony's kernel but has its own
  state quirks. Phase 5+ at the earliest.
- **Twig template compilation cache invalidation.** Symfony already handles
  this via `kernel.reset` on the Twig environment in dev mode. Production
  ships compiled templates; no special handling needed.
- **`bin/console` for arbitrary commands.** Worker mode for HTTP only.
  One-shot console commands run through the existing PHP CLI path.
- **`runtime/symfony-runtime`'s `--no-debug` / debug toggles.** Symfony
  handles these itself via `APP_DEBUG`; we don't intercept.

---

## References

- [Symfony Runtime Component](https://symfony.com/doc/current/components/runtime.html) — official docs
- [`php-runtime/runtime`](https://github.com/php-runtime/runtime) — community org with FrankenPHP, Swoole, RoadRunner, ReactPHP, Bref adapters
- [Symfony `kernel.reset` tag](https://symfony.com/doc/current/reference/dic_tags.html#kernel-reset) — service reset semantics
- [ePHPm Laravel Octane Driver](../laravel-octane-driver/) — sister roadmap; shares the SAPI surface
- [ePHPm Architecture: PHP Worker Mode](/architecture/#php-worker-mode) — prerequisite

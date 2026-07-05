+++
title = "Laravel Octane (Worker Mode)"
weight = 8
aliases = ["/roadmap/laravel-octane-driver/"]
+++

ePHPm 3.0 ships **persistent worker mode** (`[php] mode = "worker"`) and a native
**Laravel Octane driver** — boot the Laravel application once per worker thread,
then handle requests in a loop with zero per-request bootstrap. Octane's own
listeners (`FlushArrayCache`, `FlushAuthenticationState`, `DisconnectFromDatabases`,
…) reset framework state between requests; ePHPm supervises the workers.

The driver ships as the Composer package **`ephpm/octane-driver`**
([github.com/ephpm/octane-driver](https://github.com/ephpm/octane-driver)),
built on the shared base package **`ephpm/worker`**
([github.com/ephpm/php-worker](https://github.com/ephpm/php-worker)) which
provides the `Ephpm\Worker\Envelope` type and IDE stubs for the engine
primitives.

> **Packagist status:** not yet published. Until then, install from the VCS
> repository (below).

## 1. Install the driver

In your Laravel project:

```json
// composer.json
"repositories": [
    { "type": "vcs", "url": "https://github.com/ephpm/octane-driver" },
    { "type": "vcs", "url": "https://github.com/ephpm/php-worker" }
]
```

```bash
composer require laravel/octane ephpm/octane-driver
```

This installs the worker entrypoint at `vendor/bin/ephpm-octane-worker`.
(Worker scripts starting with a `#!/usr/bin/env php` shebang are handled — the
engine skips the shebang line, so Composer bin proxies work as `worker_script`.)

## 2. Configure ePHPm

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/myapp"        # the PROJECT ROOT, not public/

[php]
mode = "worker"
worker_script = "vendor/bin/ephpm-octane-worker"
```

`worker_script` must resolve to a file under `document_root` (config load
hard-errors otherwise) — that is why `document_root` points at the project
root: `vendor/bin/…` lives there.

Tell the worker where the Laravel application lives via the `EPHPM_APP_BASE`
environment variable:

```bash
export EPHPM_APP_BASE=/var/www/myapp
```

## 3. Start ePHPm — not `octane:start`

```bash
ephpm
```

`php artisan octane:start --server=ephpm` is **not supported**. With Swoole or
RoadRunner, Octane's CLI supervises the server processes; with ePHPm the roles
are inverted — ePHPm *is* the server and supervises the worker threads itself
(spawn, boot watchdog, recycling, crash recovery, graceful drain). You start
`ephpm`; it boots the workers.

## Worker lifecycle & tuning

All knobs live under `[php]` — see the [config reference](/reference/config/)
for the full table:

| Key | Default | What it does |
|---|---|---|
| `worker_count` | `0` (CPU-derived, clamped 2–32) | Persistent worker threads, each holding a booted Laravel app. |
| `worker_max_requests` | `500` | Recycle a worker after N requests (php-fpm `pm.max_requests` semantics). `0` = never. |
| `worker_backlog` | `0` (= worker count) | Dispatch-queue depth; a full queue applies backpressure. |
| `worker_boot_timeout` | `30` | Seconds to reach the first `take_request()`; expiry logs an error and increments `ephpm_worker_boot_timeouts_total` (the thread is not killed — it still becomes ready if the boot completes). |
| `worker_stream_threshold` | `1048576` | Bodies at/above this (or chunked) stream into the worker instead of buffering. |

Notes:

- `[php] workers` (the fpm-mode concurrency semaphore) is **ignored** in worker
  mode — startup logs a WARN if it is set.
- `worker_populate_superglobals` stays `false` for Octane: the driver builds
  requests from the engine's `Envelope`, never from `$_GET`/`$_POST`.
- A fatal error or an `exit()`/`die()` mid-request never wedges the server: the
  request gets a response (synthesized from SAPI headers + captured output for
  `exit()`; a 500 for a fatal) and the worker is recycled with a fresh boot.
- Worker mode is a whole-server switch and is **not supported together with
  `[server] sites_dir`** (multi-tenant vhosting) — config load hard-errors.

## Observability

Worker metrics (`ephpm_worker_pool_size`, `ephpm_worker_busy`,
`ephpm_worker_recycles_total`, boot duration/failures/timeouts, dispatch queue
depth) are documented in the [metrics reference](/reference/metrics/).

## Not yet implemented

The following Octane features are **planned — not yet implemented** in the
ePHPm driver:

- `Octane::table()` backed by `ephpm-kv` (use the `ephpm_kv_*` functions or the
  [Redis-compatible listener](/guides/kv-from-php/) directly in the meantime)
- `Octane::tick()` / interval callbacks (no `on_tick` engine primitive exists)
- `Octane::concurrently()`
- Octane's `--watch` mode

## See also

- [Laravel guide](/guides/laravel/) — classic (fpm-mode) Laravel deployment
- [Config reference — `[php]`](/reference/config/) — authoritative worker knobs
- [PSR-15 worker mode](/roadmap/psr-15-worker-mode/) — planned generic adapter
- [Symfony Runtime adapter](/roadmap/symfony-runtime-driver/) — planned

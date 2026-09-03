+++
title = "Laravel Octane (Worker Mode)"
weight = 8
aliases = ["/roadmap/laravel-octane-driver/"]
+++

ePHPm ships **persistent worker mode** (`[php] mode = "worker"`) and a native
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

ePHPm's PHP packages are distributed via their GitHub repositories (not
Packagist). Install them by adding each repo in the dependency tree as a
Composer `vcs` repository.

## 1. Install the driver

In your Laravel project, add every ePHPm repo in the tree to `composer.json`.
The driver depends on `ephpm/worker`, so **both** repos are listed — Composer
does **not** resolve a VCS dependency's own VCS repositories transitively, so
each ePHPm package needs its own `repositories` entry:

```json
// composer.json
{
  "repositories": [
    { "type": "vcs", "url": "https://github.com/ephpm/octane-driver" },
    { "type": "vcs", "url": "https://github.com/ephpm/php-worker" }
  ],
  "require": {
    "ephpm/octane-driver": "^0.1"
  }
}
```

Both `ephpm/octane-driver` and its `ephpm/worker` dependency are tagged
`v0.1.0`, so `^0.1` resolves for each; each still needs its own `repositories`
entry because Composer does not resolve VCS repos transitively. Then:

```bash
composer require laravel/octane
composer update
```

This installs the worker entrypoint at `vendor/bin/ephpm-octane-worker`.
(Worker scripts starting with a `#!/usr/bin/env php` shebang are handled — the
engine skips the shebang line, so Composer bin proxies work as the worker
`script`.)

## 2. Configure ePHPm

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/myapp"        # the PROJECT ROOT, not public/

[php]
mode = "worker"

[php.worker]
script = "vendor/bin/ephpm-octane-worker"
```

`[php.worker] script` must resolve to a file under `document_root` (config load
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

Sizing knobs live under `[php]`; worker-lifecycle knobs live under
`[php.worker]` — see the [config reference](/reference/config/) for the full
table:

| Key | Default | What it does |
|---|---|---|
| `[php] concurrency` | `0` (cgroup-quota- or CPU-derived, clamped 2–32) | Persistent worker threads, each holding a booted Laravel app. Derives from the cgroup CPU quota when running under one (Linux), otherwise host parallelism — clamped 2–32 on both paths. |
| `[php.worker] max_requests` | `10000` | Recycle a worker after N requests — pure leak guard for the framework kernel. `0` = never. |
| `[php] queue_depth` | `0` (= worker count) | Dispatch-queue depth; a full queue applies backpressure. |
| `[php.worker] boot_timeout` | `30` | Seconds to reach the first `take_request()`; expiry logs an error and increments `ephpm_worker_boot_timeouts_total` (the thread is not killed — it still becomes ready if the boot completes). |
| `[php.worker] stream_threshold` | `1048576` | Bodies at/above this (or chunked) stream into the worker instead of buffering. |

Notes:

- `[php.worker] populate_superglobals` stays `false` for Octane: the driver builds
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

- [Laravel guide](/guides/laravel/) — classic (per-request mode) Laravel deployment
- [Config reference — `[php]`](/reference/config/) — authoritative worker knobs
- [PSR-15 worker adapter](/guides/psr15-worker/) — shipped generic adapter (Slim, Mezzio, …)
- [Symfony Runtime adapter](/roadmap/symfony-runtime-driver/) — planned

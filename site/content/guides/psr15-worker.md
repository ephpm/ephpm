+++
title = "PSR-15 Apps (Worker Mode)"
weight = 9
aliases = ["/roadmap/psr-15-worker-mode/"]
+++

ePHPm ships a **generic PSR-15 worker adapter** — run any application that
exposes a `Psr\Http\Server\RequestHandlerInterface` (Slim 4, Mezzio, Yiisoft,
or a hand-rolled middleware pipeline) in persistent worker mode: boot the
application once per worker thread, then handle requests in a loop.

The adapter ships as the Composer package **`ephpm/psr15-worker`**
([github.com/ephpm/psr15-worker](https://github.com/ephpm/psr15-worker)),
built on the shared base package **`ephpm/worker`**
([github.com/ephpm/php-worker](https://github.com/ephpm/php-worker)). PSR-7
objects come from `nyholm/psr7` + `nyholm/psr7-server`.

> **Packagist status:** not yet published. Until then, install from the VCS
> repositories (below).

## 1. Install

```json
// composer.json
"repositories": [
    { "type": "vcs", "url": "https://github.com/ephpm/psr15-worker" },
    { "type": "vcs", "url": "https://github.com/ephpm/php-worker" }
]
```

```bash
composer require ephpm/psr15-worker
```

This installs the worker entrypoint at `vendor/bin/ephpm-worker` (shebang'd
Composer bin proxies work as `worker_script` — the engine skips the shebang
line).

## 2. Write a bootstrap

The bootstrap is a plain PHP file that **returns your app as a
`RequestHandlerInterface`**. Slim 4:

```php
<?php
// bootstrap.php
use Slim\Factory\AppFactory;

$app = AppFactory::create();
$app->addRoutingMiddleware();
$app->addErrorMiddleware(false, true, true);

// ... routes ...

return $app;   // Slim\App implements RequestHandlerInterface
```

Mezzio: `return $container->get(\Mezzio\Application::class);` — any framework
that hands you a PSR-15 handler works the same way.

## 3. Configure ePHPm

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/myapp"        # project root (contains vendor/)

[php]
mode = "worker"
worker_script = "vendor/bin/ephpm-worker"
```

Point the entrypoint at your bootstrap with the `EPHPM_WORKER_BOOTSTRAP`
environment variable (the engine passes no CLI arguments):

```bash
EPHPM_WORKER_BOOTSTRAP=/var/www/myapp/bootstrap.php ephpm serve
```

Each worker then loops: engine `Envelope` → PSR-7 `ServerRequest` →
`$handler->handle($request)` (your whole middleware pipeline) → PSR-7
`Response` → engine.

## What the adapter handles for you

The engine deliberately hands adapters raw material; this one does the same
marshalling work as the Octane driver:

- **Bodies** — `application/x-www-form-urlencoded` and `multipart/form-data`
  are parsed by the adapter (the engine's `parsedBody()` is always null).
  Uploads are spooled to temp files, exposed as PSR-7 `UploadedFile`
  instances, and unlinked after each request.
- **Query & cookies** — re-parsed and url-decoded (`a[]=` bracket syntax
  included); the engine's arrays arrive raw.
- **Multiple `Set-Cookie` headers** — sent via the engine's list-array header
  contract, one wire header per element (never comma-joined).
- **Streaming responses** — response bodies larger than 1 MiB, or whose size
  is unknown, are detached to their underlying resource and streamed to the
  client in chunks with backpressure (flat worker memory); small bodies stay
  on the cheaper buffered path.
- **Exactly-once protocol** — a throwable escaping the handler produces a
  single fallback 500; the worker survives and keeps serving.

## Worker-mode hygiene

PSR-15 frameworks have no unified between-request reset story — each manages
its own container. The usual Octane rules apply: don't stash per-request
state in singletons, reset static caches you introduce (a cleanup middleware
at the bottom of the pipeline is the idiomatic place).

## Planned — not yet implemented

- **PSR-16 / PSR-6 cache bindings** backed by `ephpm-kv`, so framework caches
  ride the embedded KV store without ePHPm-specific code.
- **Per-framework recipe pages** (Mezzio, Yiisoft) beyond the Slim example.

## References

- [PSR-15: HTTP Server Request Handlers](https://www.php-fig.org/psr/psr-15/)
- [PSR-7: HTTP Message Interface](https://www.php-fig.org/psr/psr-7/)
- [Laravel Octane driver](/guides/laravel-octane/) — sibling adapter
- [WordPress worker](/guides/wordpress-worker/) — sibling adapter
- [Symfony Runtime adapter](/roadmap/symfony-runtime-driver/) — still roadmap

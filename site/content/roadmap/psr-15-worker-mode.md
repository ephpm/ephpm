# Generic PSR-15 Worker Mode

A roadmap for a PSR-15 worker-mode adapter in ePHPm. PSR-15 is the PHP-FIG
middleware standard: a `Psr\Http\Server\RequestHandlerInterface` consumes a
`Psr\Http\Message\ServerRequestInterface` and returns a
`Psr\Http\Message\ResponseInterface`. Mezzio, Slim, and a long tail of
microframeworks all expose their applications as a top-level
`RequestHandlerInterface`. One generic adapter unlocks all of them.

**Status: the adapter package is planned — not yet implemented.** Its
prerequisites all shipped in 3.0: the [worker-mode engine](/architecture/#php-worker-mode)
(`[php] mode = "worker"`, the `\Ephpm\Worker\take_request()` /
`send_response()` / `send_response_stream()` primitives and the `Envelope`
object) is live, and the [Laravel Octane driver](/guides/laravel-octane/) and
[WordPress adapter](/guides/wordpress-worker/) are built on it. This page
describes the PSR-15 adapter that has not been built yet.

---

## Why a Generic Adapter

The Octane and Symfony adapters target framework-specific contracts. PSR-15
is the **lingua franca** of modern microframeworks: build the adapter once,
get every PSR-15 framework for free.

| Framework | Speaks PSR-15? | Today's persistent-mode story |
|---|---|---|
| Mezzio (formerly Zend Expressive) | Native | Runs on RoadRunner via `spiral/roadrunner-http` PSR worker. |
| Slim 4 | Native | Same. |
| Phlow, Equip, Yiisoft v3 | Native | Various community shims. |
| Laravel | Wraps PSR-15 inside its own kernel | Use the shipped [Octane driver](/guides/laravel-octane/). |
| Symfony | Wraps PSR-15 via `symfony/psr-http-message-bridge` | Use the [Symfony Runtime adapter](../symfony-runtime-driver/). |

A native ePHPm PSR-15 adapter:

- Replaces RoadRunner for Mezzio/Slim deployments — same model, in-process.
- Surfaces ePHPm's KV/DB/cluster features through standard PSR interfaces
  (`Psr\SimpleCache\CacheInterface` for KV, PDO for DB) so apps don't need
  ePHPm-specific code.

---

## Adapter Code (PHP side)

The complete adapter is short — about 60 lines:

```php
namespace Ephpm\Psr15;

use Nyholm\Psr7\Factory\Psr17Factory;
use Nyholm\Psr7Server\ServerRequestCreator;
use Psr\Http\Server\RequestHandlerInterface;

final class Worker
{
    public function __construct(
        private RequestHandlerInterface $handler,
        private Psr17Factory $psr17 = new Psr17Factory(),
    ) {}

    public function run(): int
    {
        $creator = new ServerRequestCreator(
            $this->psr17,  // ServerRequestFactory
            $this->psr17,  // UriFactory
            $this->psr17,  // UploadedFileFactory
            $this->psr17,  // StreamFactory
        );

        while ($envelope = \Ephpm\Worker\take_request()) {
            $request = $creator->fromArrays(
                $envelope->serverVars(),
                $envelope->headers(),
                $envelope->cookies(),
                $envelope->query(),
                $envelope->parsedBody(),
                $envelope->files(),
                $envelope->bodyStream(),
            );

            $response = $this->handler->handle($request);

            \Ephpm\Worker\send_response(
                $response->getStatusCode(),
                $response->getHeaders(),
                (string) $response->getBody(),
            );
        }

        return 0;
    }
}
```

Notes on the shipped engine contract the adapter must account for:

- The primitives live under `\Ephpm\Worker\*` (not `\Ephpm\Octane\*` as older
  drafts assumed), and `send_response(int $status, array $headers, string $body)`
  takes unpacked values, not a response object. Header values may be arrays
  (one wire header per element — e.g. multiple `Set-Cookie`).
- `Envelope::parsedBody()` always returns `null` and `files()` always returns
  `[]` today — form/multipart parsing is the adapter's job (or enable
  `worker_populate_superglobals` and read `$_POST`/`$_FILES`).
- `cookies()` and `query()` are parsed but **not url-decoded**; the adapter
  must decode.
- `bodyStream()` returns a real readable `php://` stream resource — large
  bodies stream in without ePHPm buffering them (see `worker_stream_threshold`
  in the [config reference](/reference/config/)).

User wires this in at the worker entrypoint:

```php
// bin/ephpm-worker.php
require __DIR__ . '/../vendor/autoload.php';

$container = require __DIR__ . '/../config/container.php';
$app       = $container->get(\Mezzio\Application::class);  // or Slim\App, etc.

(new \Ephpm\Psr15\Worker($app))->run();
```

That's the whole package. PSR-15's strength is that the framework's
container and middleware pipeline already know how to be re-entrant — they
were designed for it.

---

## ePHPm Integrations Exposed via PSR Interfaces

The adapter ships PSR-friendly bindings for ePHPm-specific features so apps
don't need ePHPm-aware code:

| ePHPm feature | PSR interface | Implementation |
|---|---|---|
| `ephpm-kv` | `Psr\SimpleCache\CacheInterface` (PSR-16) | Thin wrapper over `ephpm_kv_*` functions; cluster replication transparent. |
| `ephpm-kv` | `Psr\Cache\CacheItemPoolInterface` (PSR-6) | Same backend, PSR-6 façade for frameworks that prefer it. |
| `ephpm-db` proxy | PDO (`pdo_mysql`) | No code — pointed at `127.0.0.1:3306` via standard config. |
| Logging via `tracing` | `Psr\Log\LoggerInterface` | Forwarded to ePHPm's tracing subscriber; appears in `ephpm` access logs alongside HTTP traffic. |

A Mezzio or Slim app written against these PSR interfaces today runs
unchanged on ephpm — the bindings register themselves via the worker
bootstrap.

---

## What's NOT in This Adapter

- **State reset between requests.** PSR-15 frameworks don't have a unified
  reset story. Each framework manages its own container — Mezzio uses
  Laminas ServiceManager, Slim uses Pimple/PHP-DI/whatever. Apps that need
  per-request reset implement it themselves via PSR-15 middleware (a
  cleanup middleware sitting at the bottom of the pipeline). The adapter
  doesn't try to be smart.
- **Lifecycle events.** No `RequestReceived` / `RequestHandled` analog.
  PSR-15 middleware is the lifecycle.
- **Tick / interval / concurrency primitives.** PSR-15 is HTTP-only.
  Frameworks that want background work implement it themselves
  (typically with `react/event-loop` or `amphp/parallel`).
- **WebSocket / SSE.** Out of scope; covered in the HTTP/2 + push roadmap
  separately.

This is by design: the adapter is small precisely because it doesn't
re-implement what frameworks already provide.

---

## PSR-7 Implementation Choice

PSR-15 itself doesn't ship request/response objects — it just defines the
interfaces. The adapter needs a concrete PSR-7 implementation. Three
candidates:

| Implementation | Pros | Cons |
|---|---|---|
| `nyholm/psr7` + `nyholm/psr7-server` | Fastest in benchmarks; minimal deps; widely used | None significant |
| `laminas/laminas-diactoros` | Officially blessed by PSR-7 working group | ~2× slower than nyholm in benchmarks |
| `slim/psr7` | Slim's bundled implementation | Slimmer feature set; tied to Slim |

Default to **`nyholm/psr7`** — it's the fastest, has zero hard dependencies,
and is what RoadRunner's PSR worker uses. Document how to swap in another
implementation for users with strong preferences.

---

## Open Issues

### Streaming request bodies

The engine half of this shipped in 3.0: `Envelope::bodyStream()` is a real
readable `php://` stream over hyper's incremental body reader (bodies at/above
`worker_stream_threshold`, or chunked bodies, are never fully buffered by
ePHPm). The remaining adapter work is wrapping that resource in a
`Psr\Http\Message\StreamInterface` rather than draining it to a string.

### `ServerRequestCreator::fromArrays` performance

`nyholm/psr7-server`'s `ServerRequestCreator` parses headers and constructs
multiple intermediate objects per request. On the hot path this is ~50 µs
overhead. We can ship a custom `Ephpm\Psr15\FastRequestCreator` that builds
the `ServerRequestInterface` directly from the SAPI envelope without
allocating intermediate arrays. Optional perf optimization; revisit after
benchmarking against RoadRunner.

### Streaming responses

The engine primitive exists:
`\Ephpm\Worker\send_response_stream(int $status, array $headers, $bodyResource)`
pumps a stream/resource to the client in chunks. The adapter should detach the
PSR-7 body stream and hand it to `send_response_stream` for large responses
instead of stringifying it.

### Framework-specific bootstrap recipes

Every PSR-15 framework has a slightly different "build the application
graph" idiom (Mezzio's `config/container.php`, Slim's `AppFactory::create()`,
…). The adapter package itself stays framework-agnostic; we ship recipe docs
for the top three frameworks (Mezzio, Slim, Yiisoft) showing the
`bin/ephpm-worker.php` entrypoint per framework. No code change to the
adapter.

---

## Phasing

### Phase 1 — Worker mode primitive (prerequisite) — SHIPPED

Shipped in ePHPm 3.0: see [PHP Worker Mode](/architecture/#php-worker-mode).

### Phase 2 — Minimal PSR-15 adapter

`ephpm/psr15-worker` Composer package. The `Worker` class above plus
`composer.json` and an end-to-end test against a stock Mezzio skeleton and
a stock Slim 4 skeleton.

**Exit criteria:** `vendor/bin/ephpm-worker mezzio` and `vendor/bin/ephpm-worker slim`
both serve their respective skeleton apps from a long-lived worker.

### Phase 3 — Streaming bodies

Replace the in-memory body with a `StreamInterface` backed by hyper's
incremental body reader on the request side, and consume PSR-7 response
streams incrementally on the response side.

**Exit criteria:** uploading a 1 GB file via multipart works without the
PHP worker's memory growing past `upload_max_filesize`.

### Phase 4 — PSR-6/16 cache bindings

Ship `Ephpm\Psr16\Cache implements Psr\SimpleCache\CacheInterface` and
`Ephpm\Psr6\CachePool implements Psr\Cache\CacheItemPoolInterface` —
both backed by `ephpm-kv`. Auto-register via container factories in the
recipe docs.

**Exit criteria:** `Mezzio\Cache` configured with our cache pool serves
cached responses across worker reuse.

### Phase 5 — Framework recipe docs

Recipe pages for Mezzio, Slim, Yiisoft, and one or two niche frameworks
(Phlow, Equip). Each shows how to wire the worker entrypoint and how to
swap the framework's default cache for `ephpm-kv`.

---

## Out of Scope

- **Routing.** PSR-15 is just middleware; routing libraries
  (FastRoute, Symfony Router, Aura.Router) layer on top. The adapter
  doesn't care which one the app uses.
- **Authentication, sessions, CSRF.** Same — framework concern.
- **Frameworks that don't speak PSR-15.** Laravel and Symfony each get
  their own adapter; CodeIgniter / Yii2 don't speak PSR-15 natively and
  would need framework-specific work that doesn't belong in a generic
  package.

---

## References

- [PSR-15: HTTP Server Request Handlers](https://www.php-fig.org/psr/psr-15/) — the standard
- [PSR-7: HTTP Message Interface](https://www.php-fig.org/psr/psr-7/) — request/response interfaces
- [`nyholm/psr7`](https://github.com/Nyholm/psr7) — default PSR-7 implementation
- [Mezzio docs](https://docs.mezzio.dev/) — reference framework
- [Slim 4 docs](https://www.slimframework.com/docs/v4/) — reference framework
- [ePHPm Laravel Octane driver](/guides/laravel-octane/) — shipped adapter on the same engine
- [ePHPm WordPress Worker Mode](/guides/wordpress-worker/) — shipped adapter on the same engine
- [ePHPm Symfony Runtime Adapter](../symfony-runtime-driver/) — sister roadmap

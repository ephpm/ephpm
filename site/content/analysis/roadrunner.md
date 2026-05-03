# RoadRunner

- **Language:** Go
- **Architecture:** Go process manages a pool of long-lived PHP worker processes. Communication via Goridge binary protocol (pipes/TCP/Unix sockets).
- **Creator:** Spiral Scout / Temporal Technologies
- **Maturity:** Very high. In production since 2018. Most battle-tested Go-based PHP server. Latest: v2025.1.8 (Feb 2026).
- **License:** MIT

---

## How It Works

- Maintains a pool of persistent PHP workers. Go handles HTTP, protocol parsing, load balancing, and plugin services. Workers communicate via Goridge (~300k calls/sec over pipes).
- Plugin architecture makes it a platform, not just a server.

---

## Features

- HTTP/1, HTTP/2, HTTP/3, gRPC, FastCGI via plugins
- ACME/Let's Encrypt support (since v2.5), auto-renewal
- Prometheus metrics plugin, OpenTelemetry support (gRPC, HTTP, Jaeger)
- KV plugin with drivers: in-memory, BoltDB, Redis, Memcached (single-node only)
- Job queues: RabbitMQ, Kafka, SQS, Beanstalk, NATS, in-memory
- gRPC server built-in
- Temporal workflow engine integration
- Distributed lock plugin
- Auto worker scaling (up to 100 extra workers, added v2024.3)

---

## Does NOT Have

- Database connection pooling (requested since 2021, never built)
- Multi-node clustering for KV store
- Debug/profiling UI
- Query analysis tools

---

## PHP-Side Requirements

- Requires Composer packages: `spiral/roadrunner-http`, `spiral/roadrunner-worker`, `spiral/goridge` (transitive), and a PSR-17 implementation (e.g., `nyholm/psr7`)
- Entry point must be rewritten as a `while(true)` loop with PSR-7 request/response
- **Superglobals do NOT work** (`$_GET`, `$_POST`, `$_SERVER` not populated)
- `session_start()` does not work
- App must be 100% PSR-7 driven

**Worker example:**
```php
$worker  = Worker::create();
$factory = new Psr17Factory();
$psr7    = new PSR7Worker($worker, $factory, $factory, $factory);

while ($request = $psr7->waitRequest()) {
    $response = $app->handle($request); // must return PSR-7 ResponseInterface
    $psr7->respond($response);
}
```

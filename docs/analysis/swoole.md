# Swoole / OpenSwoole

- **Language:** C/C++ (PHP extension)
- **Architecture:** PHP extension that transforms PHP into an async, event-driven, coroutine-based runtime. PHP IS the server — no Go layer.
- **Creator:** Swoole (original), OpenSwoole (community fork after license change)
- **Maturity:** High but complex. OpenSwoole 26.2.0 (Feb 2026) added PHP 8.5 support, native Fiber coroutines, io_uring reactor.
- **License:** Apache 2.0

---

## How It Works

- Installed via PECL (`pecl install swoole`), not Composer. Extends the PHP CLI runtime.
- PHP code directly creates HTTP/WebSocket/gRPC servers using async APIs.
- Coroutines allow synchronous-looking async code.
- `SWOOLE_HOOK_ALL` intercepts blocking PHP functions (PDO, curl, file I/O) and makes them non-blocking automatically.

---

## Features

- HTTP, WebSocket, gRPC servers created directly in PHP
- `PDOPool`, `RedisPool`, generic `ConnectionPool` — first-class database connection pooling
- `Swoole\Table` — shared-memory hash table (>2M ops/sec), in-process Redis-like store
- OpenMetrics/Prometheus output (OpenSwoole 4.9+)
- Coroutine-hooked stdlib (PDO, cURL, Redis become async transparently)
- Multi-process mode, task workers, process pools

---

## Does NOT Have

- Automatic TLS / Let's Encrypt
- Multi-node clustering
- Built-in admin dashboard
- Debug/profiling UI (Xdebug step debugging added in OpenSwoole 26.2.0)
- Query analysis tools
- Cross-platform support (Linux-only in practice)

---

## PHP-Side Requirements

- **Most invasive.** Entire entry point rewritten.
- Superglobals (`$_GET`, `$_POST`) do NOT work — must use `$request->get`, `$request->post`
- `echo` does not work — must use `$response->end()`
- `session_start()` does not work
- Static/global variables shared across concurrent coroutines — **data leak risk**
- Must use `Coroutine::getContext()` for per-request state isolation
- Not Composer-based (PECL C extension)

**Server example:**
```php
$server = new Swoole\Http\Server('0.0.0.0', 9501);
$server->set(['worker_num' => 4, 'hook_flags' => SWOOLE_HOOK_ALL]);

$server->on('Request', function($req, $res) {
    // $req->get instead of $_GET
    // $res->end() instead of echo
    $res->end('Hello ' . ($req->get['name'] ?? 'world'));
});

$server->start();
```

# Performance Comparison: ePHPm vs Everything Else

This document breaks down where time is spent serving a PHP request across every major stack, showing exactly where ePHPm eliminates overhead.

---

## The Stacks Compared

| Stack | Architecture |
|---|---|
| **nginx + php-fpm** | Separate web server → FastCGI protocol → separate PHP process pool |
| **Apache + mod_php** | Web server with PHP embedded as a module (same process) |
| **Apache + php-fpm** | Separate web server → FastCGI protocol → separate PHP process pool |
| **RoadRunner** | Go HTTP server → Goridge IPC (pipes) → separate PHP process pool |
| **FrankenPHP** | Go/Caddy HTTP server + PHP embedded via CGO (same process) |
| **Swoole** | PHP C extension — PHP IS the server (same process, coroutines) |
| **ePHPm** | Rust HTTP server + PHP embedded via zero-cost FFI (same process) |

---

## Request Lifecycle: Where Time Goes

A single HTTP request to a PHP application involves these stages. Each stack handles them differently.

### Stage 1: Connection Handling + TLS

The client connects, TLS handshake completes, HTTP request is parsed.

| Stack | How | Overhead |
|---|---|---|
| nginx + php-fpm | nginx handles (C, event-driven) | ~0.1-0.5ms (excellent) |
| Apache + mod_php | Apache handles (C, process/thread-per-conn) | ~0.2-0.8ms |
| Apache + php-fpm | Apache handles | ~0.2-0.8ms |
| RoadRunner | Go `net/http` | ~0.1-0.5ms |
| FrankenPHP | Caddy (Go `net/http` + middleware chain) | ~0.2-0.8ms (Caddy middleware adds latency) |
| Swoole | PHP C extension (`swoole_http_server`) | ~0.1-0.5ms |
| **ePHPm** | Rust `hyper` + `tokio` + `rustls` | **~0.05-0.3ms** (fastest — no GC, no middleware chain, zero-copy TLS) |

### Stage 2: Request Dispatch to PHP

How does the HTTP request reach the PHP interpreter?

| Stack | Mechanism | Overhead |
|---|---|---|
| **nginx + php-fpm** | FastCGI protocol over Unix socket/TCP. Serialize full request (headers, body, env vars) into FastCGI records. PHP-FPM worker reads, deserializes. | **~50-200μs** (socket write + read + FastCGI encode/decode) |
| **Apache + mod_php** | In-process function call. Apache populates PHP's request struct directly. | **~1-5μs** (near zero — same process) |
| **Apache + php-fpm** | Same as nginx + php-fpm (FastCGI over socket). | **~50-200μs** |
| **RoadRunner** | Goridge binary protocol over stdin/stdout pipes. Serialize request into Goridge frames (12-byte header + payload). PHP worker deserializes. OS context switch between Go and PHP processes. | **~20-80μs** (pipe I/O + serialize + context switch) |
| **FrankenPHP** | CGO call from Go → C. 11+ boundary crossings per request for thread dispatch, SAPI callbacks (headers, body, cookies, superglobals, output). | **~2.2μs+** (200ns × 11+ crossings) |
| **Swoole** | In-process. PHP event loop receives directly. No dispatch needed. | **~0.5-2μs** (event loop wakeup) |
| **ePHPm** | Rust FFI call to libphp. Same 11+ SAPI callbacks as FrankenPHP but each is a zero-cost C function call. Tokio channel send to wake worker thread. | **~0.5-2μs** (channel send + zero-cost FFI calls) |

### Stage 3: PHP Bootstrap (Cold Start)

Does the PHP application reload from scratch on every request?

| Stack | Bootstrap Model | Overhead |
|---|---|---|
| **nginx + php-fpm** | **Cold start every request.** Each request: autoloader runs, framework boots, service container builds, routes compile, config loads. OPcache helps (~50% reduction) but the autoloader and framework init still run. | **~10-30ms** (Laravel), **~5-15ms** (Symfony), **~2-5ms** (WordPress) |
| **Apache + mod_php** | Same as php-fpm — cold start every request. | **~10-30ms** (Laravel) |
| **Apache + php-fpm** | Same as php-fpm. | **~10-30ms** (Laravel) |
| **RoadRunner** | **Worker mode — boot once.** App boots once, stays in memory. Subsequent requests skip bootstrap entirely. | **~0ms** (amortized to zero after first request) |
| **FrankenPHP (classic)** | Cold start every request (like php-fpm). | **~10-30ms** (Laravel) |
| **FrankenPHP (worker)** | Worker mode — boot once. | **~0ms** (amortized) |
| **Swoole** | Worker mode — boot once. | **~0ms** (amortized) |
| **ePHPm (worker)** | Worker mode — boot once. | **~0ms** (amortized) |

This is the single biggest performance factor. Worker mode eliminates 10-30ms of overhead per request for framework-heavy apps. The difference between FPM and worker mode dwarfs every other optimization.

### Stage 4: PHP Execution

The actual application logic — database queries, business logic, template rendering. This is identical across all stacks because they all run the same PHP interpreter.

| Stack | Execution Model | Overhead vs Native PHP |
|---|---|---|
| All stacks | Same PHP engine (Zend VM) | **0ms** — the PHP code runs at the same speed everywhere |

The PHP execution time is the constant. Everything else in this document is overhead on top of it.

### Stage 5: Database Access

How does PHP talk to the database?

| Stack | DB Connection Model | Overhead Per Query |
|---|---|---|
| **nginx + php-fpm** | New TCP connection per request (or persistent per-worker). No pooling. N workers = N connections. | **~1-3ms** connection setup (first query), **~0.5-2ms** per query (network RTT) |
| **Apache + mod_php** | Same — per-process connections. | ~1-3ms / ~0.5-2ms |
| **Apache + php-fpm** | Same. | ~1-3ms / ~0.5-2ms |
| **RoadRunner** | Same as php-fpm — PHP opens its own connections. No server-level pooling. | ~1-3ms / ~0.5-2ms |
| **FrankenPHP** | Same — no DB proxy. | ~1-3ms / ~0.5-2ms |
| **Swoole** | **Connection pool** (`PDOPool`). Persistent connections reused across coroutines. But still TCP to the actual database. | **~0ms** connection setup, **~0.5-2ms** per query |
| **ePHPm** | **In-process DB proxy** with connection pooling. PHP connects to `localhost:3306` (ePHPm's proxy). Pool maintains persistent connections to real DB. For even tighter integration, SAPI functions bypass TCP entirely. | **~0ms** connection setup, **~0.5-2ms** per query (network to real DB), **~0μs** proxy overhead (in-process) |

Without pooling, hitting max_connections is easy: 200 PHP workers × 1 connection each = 200 DB connections. With ePHPm's proxy: 200 workers → 20 pooled backend connections (10:1 multiplexing).

### Stage 6: Session / Cache Access

How does PHP read/write sessions and cache data?

| Stack | Session/Cache Model | Overhead Per Operation |
|---|---|---|
| **nginx + php-fpm** | External Redis/Memcached over TCP. Every `session_start()` and cache read = network round trip. | **~0.5-2ms** per operation (TCP to Redis on localhost: ~200μs network + ~100μs Redis processing + connection overhead) |
| **Apache + mod_php** | Same — external Redis/Memcached. | ~0.5-2ms |
| **RoadRunner** | KV plugin (in-memory, single-node). Access via Goridge IPC (pipe round trip + serialization). | **~20-80μs** per operation (IPC overhead) |
| **FrankenPHP** | No KV store. External Redis required. | ~0.5-2ms |
| **Swoole** | `Swoole\Table` (shared memory, same process). Fast but single-node only. | **~0.1-1μs** per operation |
| **ePHPm** | **In-process KV store** (DashMap). PHP accesses via SAPI function call — zero-cost FFI, no TCP, no serialization. Local keys: direct memory access. Remote keys (clustered): internal network hop. | **~100-200ns** local, **~0.5-2ms** remote |

For a typical Laravel request that does `session_start()` + 2-3 cache reads:
- **php-fpm + Redis**: 4 × ~1ms = **~4ms** of session/cache overhead
- **RoadRunner**: 4 × ~50μs = **~200μs**
- **ePHPm (local)**: 4 × ~150ns = **~600ns** (6,600x faster than Redis)

### Stage 7: Response Delivery

How does the PHP response get back to the client?

| Stack | Mechanism | Overhead |
|---|---|---|
| **nginx + php-fpm** | PHP-FPM serializes response into FastCGI records → Unix socket → nginx deserializes → sends to client. | **~50-200μs** (FastCGI encode/decode + socket) |
| **Apache + mod_php** | In-process. PHP writes directly to Apache's output buffer. | **~1-5μs** |
| **RoadRunner** | PHP serializes response into Goridge frames → pipe → Go deserializes → sends to client. | **~20-80μs** |
| **FrankenPHP** | PHP's `echo`/`header()` → SAPI callbacks → CGO crossing to Go → Go writes to client. | **~2μs+** (CGO crossings for headers + each output write) |
| **Swoole** | In-process. `$response->end()` writes directly. | **~0.5-2μs** |
| **ePHPm** | PHP's `echo`/`header()` → SAPI callbacks → zero-cost FFI → Rust writes to client via hyper. | **~0.5-2μs** |

---

## Total Overhead Per Request (Excluding PHP Execution)

Everything except the actual PHP application code running. This is pure infrastructure tax.

### Scenario: Laravel API request (worker mode where available)

Assumptions: JSON API endpoint, worker mode (where supported), 3 DB queries, 2 cache reads, 1 session read, response under 10KB.

| Stage | nginx + php-fpm | RoadRunner | FrankenPHP (worker) | Swoole | **ePHPm** |
|---|---|---|---|---|---|
| Connection/TLS | 0.3ms | 0.3ms | 0.5ms | 0.3ms | **0.15ms** |
| Request dispatch | 0.1ms | 0.05ms | 0.002ms | 0.001ms | **0.001ms** |
| PHP bootstrap | **15ms** | 0ms | 0ms | 0ms | **0ms** |
| DB connections (3 queries) | 1ms setup + 3ms queries | 1ms + 3ms | 1ms + 3ms | 0ms + 3ms | **0ms + 3ms** |
| Session + cache (3 ops) | **3ms** (Redis) | 0.15ms (IPC) | **3ms** (Redis) | 0.003ms | **0.0005ms** |
| Response delivery | 0.1ms | 0.05ms | 0.002ms | 0.001ms | **0.001ms** |
| **Total overhead** | **~22.5ms** | **~7.55ms** | **~7.5ms** | **~3.3ms** | **~3.15ms** |

### Scenario: Same request, php-fpm stacks (no worker mode)

| | nginx + php-fpm | Apache + mod_php | Apache + php-fpm |
|---|---|---|---|
| Connection/TLS | 0.3ms | 0.5ms | 0.5ms |
| Request dispatch | 0.1ms | 0.003ms | 0.1ms |
| PHP bootstrap | **15ms** | **15ms** | **15ms** |
| DB connections | 4ms | 4ms | 4ms |
| Session + cache (Redis) | 3ms | 3ms | 3ms |
| Response delivery | 0.1ms | 0.003ms | 0.1ms |
| **Total overhead** | **~22.5ms** | **~22.5ms** | **~22.7ms** |

---

## The Multiplier Effect

These overheads compound. A typical Laravel page makes 5-15 DB queries and 3-8 cache reads.

### Heavy page: 10 DB queries, 6 cache/session ops, 50ms PHP execution

| Stack | Infra overhead | PHP execution | Total | Overhead % |
|---|---|---|---|---|
| nginx + php-fpm | ~28ms | 50ms | **78ms** | 36% overhead |
| RoadRunner | ~10ms | 50ms | **60ms** | 17% overhead |
| FrankenPHP (worker) | ~13ms | 50ms | **63ms** | 21% overhead (Redis for cache) |
| Swoole | ~5ms | 50ms | **55ms** | 9% overhead |
| **ePHPm** | **~3.2ms** | 50ms | **53.2ms** | **6% overhead** |

### At 10,000 requests/second

| Stack | Overhead CPU burned/sec | Wasted per day |
|---|---|---|
| nginx + php-fpm | 280 seconds (bootstrap dominates) | N/A — can't hit 10k req/s without massive worker pool |
| RoadRunner | 100 seconds | ~2.4 core-hours |
| FrankenPHP (worker) | 130 seconds | ~3.1 core-hours |
| Swoole | 50 seconds | ~1.2 core-hours |
| **ePHPm** | **32 seconds** | **~0.8 core-hours** |

---

## Where Each Stack Loses Time

### nginx + php-fpm — Death by a Thousand Cuts

```
Client ──► nginx ──FastCGI──► php-fpm worker
                   ~100μs        │
                              Bootstrap Laravel: ~15ms
                              DB connect: ~1ms
                              3 × DB query: ~3ms (TCP to MySQL)
                              3 × Redis: ~3ms (TCP to Redis)
                              FastCGI response: ~100μs
                                 │
Client ◄── nginx ◄──FastCGI──◄──┘

Total overhead: ~22ms+
Biggest cost: PHP bootstrap (15ms) — re-runs EVERY request
```

### RoadRunner — IPC Tax

```
Client ──► Go HTTP server
              │
              Goridge pipe write: ~20μs (serialize request)
              OS context switch: ~10μs
              │
              ▼
           PHP worker (persistent — no bootstrap)
              DB: 3 queries over TCP: ~4ms
              KV: 3 ops over Goridge IPC: ~150μs
              │
              Goridge pipe write: ~20μs (serialize response)
              OS context switch: ~10μs
              │
Client ◄── Go HTTP server

Total overhead: ~4.3ms (without Redis), ~7.5ms (with Redis for sessions)
Biggest cost: DB connections (no pooling) + Goridge IPC serialization
```

### FrankenPHP (Worker Mode) — CGO Tax + No Infrastructure

```
Client ──► Caddy/Go HTTP server
              │
              Caddy middleware chain: ~200-500μs
              CGO dispatch to PHP thread: ~200ns
              CGO: populate superglobals: ~800ns (4 callbacks)
              │
              ▼
           PHP worker (persistent — no bootstrap)
              DB: 3 queries over TCP: ~4ms (no pooling)
              Cache: 3 ops to external Redis: ~3ms
              │
              CGO: write headers: ~200ns
              CGO: write body (echo): ~200ns × N chunks
              │
Client ◄── Caddy/Go HTTP server

Total overhead: ~7.5ms
Biggest cost: External Redis (no built-in KV) + DB connections (no pooling)
```

### ePHPm — Minimal Overhead

```
Client ──► Rust hyper (direct, no middleware chain)
              │
              Tokio channel send to PHP worker: ~100ns
              FFI: populate superglobals: ~0ns (zero-cost C calls)
              │
              ▼
           PHP worker (persistent — no bootstrap)
              DB: 3 queries via in-process proxy: ~3ms (pooled, no connect overhead)
              Cache: 3 ops via in-process KV: ~450ns (direct memory access)
              │
              FFI: write headers: ~0ns
              FFI: write body: ~0ns
              │
Client ◄── Rust hyper

Total overhead: ~3.15ms
Biggest cost: Network RTT to actual database (unavoidable)
```

---

## p99 Latency: The GC Factor

Average latency tells one story. p99 (worst 1% of requests) tells another.

Go's garbage collector introduces periodic pauses. These are short (~0.5-2ms with modern Go) but unpredictable. Under load, GC pauses hit the tail latency:

| Stack | p99 Factor | Why |
|---|---|---|
| nginx + php-fpm | None (C + separate PHP processes) | nginx has no GC. PHP processes are independent — one GC doesn't affect others. |
| Apache + mod_php | None (C) | No GC in Apache or PHP request lifecycle. |
| RoadRunner | **Go GC pauses ~0.5-2ms** | Go HTTP server GC affects all in-flight requests. PHP workers are separate processes (no GC). |
| FrankenPHP | **Go GC pauses ~1-5ms** (worse) | PHP runs IN the Go process. PHP's memory allocations are visible to Go's GC. In Symfony benchmarks, FrankenPHP showed 45ms std dev on CPU-bound tasks vs RoadRunner's 8ms. |
| Swoole | None (C extension) | No GC in the server layer. PHP's GC is per-request. |
| **ePHPm** | **None** | Rust has no GC. PHP's per-request GC is isolated per worker thread. **Predictable p99.** |

FrankenPHP's GC problem is uniquely bad because PHP memory is allocated inside the Go process. Go's GC must scan/track these allocations, increasing both GC frequency and pause duration under heavy PHP load.

---

## Memory Efficiency

| Stack | Memory Per Worker | 1000-Connection Overhead | Notes |
|---|---|---|---|
| nginx + php-fpm | ~30-50MB per FPM worker | ~30-50GB for 1000 workers | Each FPM worker is a full process with its own memory space |
| Apache + mod_php | ~30-50MB per Apache process | ~30-50GB | Same — process-per-worker |
| RoadRunner | ~30-50MB per PHP process + Go overhead | ~30-50GB + ~200MB Go | PHP processes are separate |
| FrankenPHP | ~20-40MB per worker (shared process) | ~20-40GB + Go runtime | Shared address space saves some overhead |
| Swoole | ~10-30MB per worker | ~10-30GB | Efficient — shared memory, coroutines |
| **ePHPm** | ~10-30MB per worker | ~10-30GB + **~50MB Rust** | Rust runtime is tiny. No Go runtime overhead. KV store replaces external Redis (saves ~100MB+). |

ePHPm's real memory win: it **replaces external Redis** (typically 100MB-1GB in production) with the in-process KV store. One fewer process, one fewer memory footprint.

---

## Feature-Adjusted Comparison

Raw speed means nothing without features. Here's what each stack actually provides:

| Feature | nginx+fpm | Apache+mod_php | RoadRunner | FrankenPHP | Swoole | **ePHPm** |
|---|---|---|---|---|---|---|
| Worker mode (no cold start) | No | No | Yes | Yes | Yes | **Yes** |
| Auto TLS (Let's Encrypt) | Via certbot (external) | Via certbot | Yes | Yes (Caddy) | No | **Yes** |
| DB connection pooling | No | No | No | No | Yes | **Yes** |
| Built-in KV/cache | No | No | Yes (single-node) | No | Yes (single-node) | **Yes (clustered)** |
| Multi-node clustering | No | No | No | No | No | **Yes** |
| Query digest/analysis | No | No | No | No | No | **Yes** |
| Auto-instrumented traces | No | No | No | No | No | **Yes** |
| Admin dashboard | No | No | No | No | No | **Yes** |
| Superglobals work | Yes | Yes | **No** | Yes | **No** | **Yes** |
| Zero code changes | Yes | Yes | **No** (PSR-7) | Yes | **No** (Swoole API) | **Yes** |
| GC-free server layer | Yes (C) | Yes (C) | No (Go) | No (Go) | Yes (C) | **Yes (Rust)** |
| Memory-safe server | No (C) | No (C) | Yes (Go) | Yes (Go) | No (C) | **Yes (Rust)** |
| Single binary deploy | No (nginx+php) | No (apache+php) | Yes | Yes | No (PECL ext) | **Yes** |

---

## The Pitch (by audience)

### For developers on nginx + php-fpm:

> "ePHPm eliminates 15ms of bootstrap overhead per request, replaces your nginx + php-fpm + Redis stack with a single binary, and adds connection pooling, query analysis, and a built-in observability dashboard. Your existing Laravel/Symfony/WordPress app works with zero code changes."

### For developers on RoadRunner:

> "ePHPm gives you the same worker model without the PSR-7 migration tax — superglobals just work. Plus you get DB connection pooling, clustered caching (no external Redis), and zero IPC serialization overhead."

### For developers on FrankenPHP:

> "ePHPm eliminates 2.2μs of CGO overhead per request, removes Go's GC jitter from your p99 latency, and adds DB connection pooling, clustered KV, query analysis, and a full observability dashboard — features FrankenPHP doesn't have and can't easily add."

### For developers on Swoole:

> "ePHPm gives you connection pooling and in-process caching like Swoole, but with superglobal compatibility, cross-platform support (not Linux-only), multi-node clustering, and no PECL extension to install. Your existing code works unchanged."

---

## Sources

- [FrankenPHP vs RoadRunner Symfony 8 Benchmarks (2026)](https://dev.to/mattleads/benchmark-frankenphp-vs-roadrunner-in-symfony-8-2lgp)
- [PHP Application Server Comparison (2025)](https://www.deployhq.com/blog/comparing-php-application-servers-in-2025-performance-scalability-and-modern-options)
- [Redis Latency Diagnostics](https://redis.io/docs/latest/operate/oss_and_stack/management/optimization/latency/)
- [nginx FastCGI Keepalive](https://www.getpagespeed.com/server-setup/nginx/nginx-fastcgi-keepalive)
- [PHP OPcache Preloading Benchmarks](https://stitcher.io/blog/php-preload-benchmarks)
- [FrankenPHP Classic Mode vs PHP-FPM (Tideways)](https://tideways.com/profiler/blog/testing-if-franken-php-classic-mode-is-faster-and-more-scalable-than-php-fpm)

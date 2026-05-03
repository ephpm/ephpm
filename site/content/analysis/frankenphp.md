# FrankenPHP

- **Language:** Go + C (cgo bindings to `libphp`)
- **Architecture:** Caddy module that embeds the PHP interpreter directly into the Go/Caddy binary
- **Creator:** Kevin Dunglas (API Platform). Moved under the official PHP GitHub org (May 2025), now supported by the PHP Foundation.
- **Maturity:** High. 10,000+ GitHub stars, 100+ contributors. Used by Laravel Cloud, Upsun, Clever Cloud.
- **License:** MIT

---

## How It Works

### Classic Mode

Drop-in FPM replacement. Runs PHP scripts per-request, superglobals work, zero code changes. Essentially php-fpm but inside the same binary.

### Worker Mode

App boots once, stays in memory. Workers are persistent HTTP request handlers in a loop — not background task processors or message queue consumers. The lifecycle is:

1. PHP app boots once (framework, config, service container, routes — the expensive work)
2. Worker blocks on `frankenphp_handle_request()`, waiting for Go to hand it an HTTP request
3. When a request arrives, Go populates `$_GET`, `$_POST`, `$_SERVER` etc. via the SAPI, then invokes the callback
4. Callback returns, Go flushes the response to the client
5. Loop back to step 2

The mental model is php-fpm but the process never restarts: `boot → handle → handle → handle → ...` instead of `boot → handle → die → boot → handle → die`. The win is amortizing boot cost (10-30ms for a Laravel app) to zero.

Each worker is a goroutine locked to an OS thread (`runtime.LockOSThread()`). Execution within a worker is synchronous — one request at a time per worker. Concurrency comes from having N workers in the pool. Go's HTTP layer dispatches incoming requests to idle workers.

Workers cannot be spawned mid-request for background tasks. There is no async task or job queue capability — background work still requires external tools (Redis + Horizon, RabbitMQ, etc.).

`frankenphp_handle_request()` is a C-level function injected by the PHP extension — no Composer package needed. Superglobals are repopulated per request internally. No PSR-7 requirement.

**Worker mode example:**
```php
$app = require __DIR__ . '/bootstrap.php';

while (frankenphp_handle_request(function() use ($app) {
    // $_GET, $_POST, $_SERVER all work here
    $app->handle();
})) {
    gc_collect_cycles();
}
```

---

## How FrankenPHP Embeds Into Caddy

### Caddy's Module System

Caddy uses Go's `init()` mechanism for plugins. Any Go package that calls `caddy.RegisterModule()` in its `init()` function becomes a Caddy module — simply by being imported:

```go
// frankenphp's caddy/caddy.go
func init() {
    caddy.RegisterModule(FrankenPHPApp{})
    caddy.RegisterModule(FrankenPHPModule{})
}
```

Go runs `init()` as a side effect of importing a package. To include a module, you just need a blank import in `main.go`:

```go
package main

import (
    caddycmd "github.com/caddyserver/caddy/v2/cmd"
    _ "github.com/dunglas/frankenphp/caddy"  // triggers init() → registers modules
)

func main() { caddycmd.Main() }
```

This is literally FrankenPHP's `main.go`. It calls Caddy's main function. The FrankenPHP binary IS Caddy.

### Module Registration Pattern

Modules register with a hierarchical dot-separated namespace that determines the Go interface they must implement:

```go
// Top-level "app" module — implements caddy.App (Start/Stop lifecycle)
type FrankenPHPApp struct { /* config fields */ }

func (FrankenPHPApp) CaddyModule() caddy.ModuleInfo {
    return caddy.ModuleInfo{
        ID:  "frankenphp",
        New: func() caddy.Module { return new(FrankenPHPApp) },
    }
}

func (f *FrankenPHPApp) Start() error { return frankenphp.Init(/* options */) }
func (f *FrankenPHPApp) Stop() error  { frankenphp.Shutdown(); return nil }

// HTTP handler module — implements caddyhttp.MiddlewareHandler
type FrankenPHPModule struct {
    Root      string   `json:"root,omitempty"`
    SplitPath []string `json:"split_path,omitempty"`
}

func (FrankenPHPModule) CaddyModule() caddy.ModuleInfo {
    return caddy.ModuleInfo{
        ID:  "http.handlers.php",
        New: func() caddy.Module { return new(FrankenPHPModule) },
    }
}

func (f *FrankenPHPModule) ServeHTTP(w http.ResponseWriter, r *http.Request, next caddyhttp.Handler) error {
    // call frankenphp.ServeHTTP(w, req)
}
```

The `caddy.App` lifecycle is:
1. `Provision(ctx caddy.Context)` — setup, called before start
2. `Validate()` — sanity checks
3. `Start()` — begin running
4. `Stop()` — graceful shutdown

### xcaddy Build Tool

`xcaddy` (`github.com/caddyserver/xcaddy`) automates building custom Caddy binaries:

```bash
xcaddy build --with github.com/dunglas/frankenphp/caddy
```

What it does:
1. Creates a temporary directory
2. Generates a `main.go` that blank-imports Caddy + every `--with` module
3. Generates a `go.mod` for the temporary module
4. Runs `go get` to resolve versions
5. Runs `go build` (with CGO flags if needed)
6. Outputs the binary and cleans up

Multiple modules can be combined:
```bash
xcaddy build \
    --with github.com/dunglas/frankenphp/caddy \
    --with github.com/dunglas/mercure/caddy \
    --with github.com/darkweak/souin/plugins/caddy
```

FrankenPHP's pre-built binary includes Mercure, Vulcain, and cbrotli by default.

---

## CGO / libphp Embedding

### PHP as an Embeddable Library

PHP must be compiled with two critical flags:
- `--enable-zts` — Zend Thread Safety (required for multi-threaded operation in Go)
- `--enable-embed` — produces `libphp.a` (static) or `libphp.so` (dynamic)

### CGO Directives

In FrankenPHP's Go source, the CGO configuration links against PHP's C libraries:

```go
// #cgo CFLAGS: ... (from php-config --includes)
// #cgo LDFLAGS: ... (from php-config --ldflags --libs)
// #include <stdlib.h>
// #include <stdint.h>
// #include <php_variables.h>
// #include <zend_llist.h>
// #include <SAPI.h>
// #include "frankenphp.h"
import "C"
```

Linked C libraries include: `libphp` (ZTS, embed SAPI), `libssl`, `libcrypto`, `libxml2`, `libz`, `libpcre2`, `libsqlite3`, `libcurl`, and others depending on compiled PHP extensions.

For static builds (musl + static-php-cli), all become `.a` files linked into a single binary with zero runtime dependencies.

### Custom PHP SAPI

FrankenPHP implements a custom **SAPI (Server API)** — the same kind of interface that `php-fpm` and `apache2handler` implement. The SAPI is defined in C (`frankenphp.c`) and registers function pointers that PHP calls for I/O:

| PHP Operation | SAPI Function | What It Does |
|---|---|---|
| `echo "hello"` | `frankenphp_ub_write` | Writes to Go's `http.ResponseWriter` |
| `header("Content-Type: ...")` | `frankenphp_send_headers` | Sets HTTP response headers in Go |
| Reading POST body | `frankenphp_read_post` | Reads from Go's `http.Request.Body` |
| Reading cookies | `frankenphp_read_cookies` | Reads cookie header from Go request |
| Populating `$_SERVER` | `frankenphp_register_variables` | Fills superglobals from Go request data |

These C functions call **back into Go** via CGO `//export` annotations:

```go
//export go_ub_write
func go_ub_write(threadIndex C.uintptr_t, str *C.char, length C.size_t) C.size_t {
    // write to the http.ResponseWriter
}
```

This bidirectional C↔Go bridge is the core of FrankenPHP's PHP embedding and is why superglobals work in worker mode.

### Thread Safety

PHP requires `php_module_startup` to run on the main OS thread. Go's goroutine scheduler moves goroutines between threads freely. FrankenPHP works around this with `runtime.LockOSThread()` — pinning the main PHP thread to the process's initial OS thread. Each PHP worker thread is a separate locked goroutine-thread pair.

---

## Worker Suspend/Resume Mechanism

The core question of worker mode is: how does PHP block waiting for a request while keeping the entire app in memory? The answer is **Go channels as the only synchronization primitive** — no OS-level mutexes, condition variables, or semaphores.

### Step 1: PHP calls into C, C calls into Go

When the PHP worker script calls `frankenphp_handle_request(callback)`, the C implementation (`frankenphp.c`) disables the execution timeout and makes a CGo call into Go:

```c
// frankenphp.c — PHP_FUNCTION(frankenphp_handle_request)
zend_unset_timeout();

// THIS IS THE BLOCKING CALL — PHP suspends here
struct go_frankenphp_worker_handle_request_start_return result =
    go_frankenphp_worker_handle_request_start(thread_index);

// ...execution resumes only when a request arrives...
frankenphp_worker_request_startup();   // re-arm superglobals
zend_call_function(&fci, &fcc);        // execute PHP callback
frankenphp_worker_request_shutdown();   // cleanup
go_frankenphp_finish_worker_request(thread_index);  // signal Go
```

The C thread enters Go via CGo and **blocks on a channel read**. The PHP stack, all variables, the loaded application — everything stays in memory on that OS thread.

### Step 2: Go blocks on a channel select

The CGo call lands in `waitForWorkerRequest()` (`threadworker.go`), which contains the actual suspension point:

```go
func (handler *workerThread) waitForWorkerRequest() (bool, any) {
    handler.state.MarkAsWaiting(true)

    // THIS SELECT IS THE SUSPENSION POINT
    var requestCH contextHolder
    select {
    case <-handler.thread.drainChan:
        return false, nil  // shutdown signal

    case requestCH = <-handler.thread.requestChan:
        // direct dispatch: request sent to this specific thread

    case requestCH = <-handler.worker.requestChan:
        // queue dispatch: picked up from the shared worker queue
    }

    handler.workerFrankenPHPContext = requestCH.frankenPHPContext
    handler.state.MarkAsWaiting(false)
    return true, handler.workerFrankenPHPContext.handlerParameters
}
```

The Go scheduler **parks the goroutine** (and its locked OS thread, which is also the C/PHP thread). Zero CPU consumed while waiting.

### Step 3: HTTP request dispatch pairs a request with an idle worker

When an HTTP request arrives, `handleRequest()` (`worker.go`) uses a two-stage dispatch:

```go
func (worker *worker) handleRequest(ch contextHolder) error {
    // STAGE 1: Try non-blocking send to each thread's private channel
    if worker.queuedRequests.Load() == 0 {
        for _, thread := range worker.threads {
            select {
            case thread.requestChan <- ch:
                // Sent! Unblocks that thread's waitForWorkerRequest()
                <-ch.frankenPHPContext.done  // Block until PHP finishes
                return nil
            default:
                // Thread busy, try next
            }
        }
    }

    // STAGE 2: No idle thread — queue on the shared channel
    worker.queuedRequests.Add(1)
    select {
    case worker.requestChan <- ch:
        <-ch.frankenPHPContext.done
        return nil
    case <-timeoutChan(maxWaitTime):
        return ErrMaxWaitTimeExceeded
    }
}
```

Stage 1 uses non-blocking sends (`select` with `default`) to try each thread's private `requestChan`. If a thread is parked in `waitForWorkerRequest()`, the send succeeds and that thread wakes up. Stage 2 falls back to a shared channel. After dispatch, the HTTP goroutine blocks on `<-ch.frankenPHPContext.done`, waiting for PHP to finish.

### Step 4: Superglobal repopulation

After the CGo call returns but before the PHP callback runs, C re-arms PHP's auto-globals via `frankenphp_reset_super_globals()`:

```c
static void frankenphp_reset_super_globals() {
    // Destroy $_FILES, remove $_SESSION
    zval *files = &PG(http_globals)[TRACK_VARS_FILES];
    zval_ptr_dtor_nogc(files);

    // Re-arm all auto globals except $_ENV
    ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), auto_global) {
        if (auto_global->name != _env) {
            auto_global->armed = auto_global->auto_global_callback(auto_global->name);
        }
    }
    ZEND_HASH_FOREACH_END();
}
```

This causes `$_GET`, `$_POST`, `$_SERVER` etc. to lazily re-populate through the SAPI callbacks, which now read from the new `frankenPHPContext` that was stored during dispatch.

### Step 5: Request completion unblocks the HTTP goroutine

After the PHP callback returns, C calls `go_frankenphp_finish_worker_request()`:

```go
//export go_frankenphp_finish_worker_request
func go_frankenphp_finish_worker_request(threadIndex C.uintptr_t) {
    fc := phpThreads[threadIndex].frankenPHPContext()
    close(fc.done)  // Unblocks the HTTP handler goroutine
}
```

The `close(fc.done)` unblocks the HTTP goroutine that was waiting on `<-ch.frankenPHPContext.done`, which then sends the response to the client. The PHP worker loops back to `frankenphp_handle_request()` and blocks again.

### Full suspend/resume flow

```
HTTP Request Arrives
       │
       ▼
 worker.handleRequest()
       │
       ├─ Stage 1: non-blocking send to thread.requestChan ──┐
       │                                                      │
       ├─ Stage 2: blocking send to worker.requestChan ───────┤
       │                                                      │
       ▼                                                      ▼
 Caller blocks on:                              In the PHP/C thread:
 <-fc.done                                      waitForWorkerRequest() was
       .                                        blocked on select{} reading
       .                                        from requestChan
       .                                              │
       .                                              │ (channel unblocks)
       .                                              ▼
       .                                        Returns to C via CGo
       .                                              │
       .                                              ▼
       .                                        frankenphp_reset_super_globals()
       .                                        re-arms $_GET, $_POST, $_SERVER
       .                                              │
       .                                              ▼
       .                                        zend_call_function() — runs
       .                                        your PHP callback
       .                                              │
       .                                              ▼
       .                                        go_frankenphp_finish_worker_request()
       .                                              │
       .                                              ▼
       ◄──────────────────────────────────────  close(fc.done)
       │
       ▼
 Response sent to client
```

### Implications for ePHPm

This mechanism is elegant and ePHPm can reuse the same pattern: Go channels as the dispatch/synchronization layer, CGo calls as the blocking boundary, and SAPI callbacks for superglobal repopulation. The key data structures to replicate are:

- **Per-thread `requestChan`** (unbuffered) for direct dispatch to idle workers
- **Shared `requestChan`** for queued overflow
- **`done` channel** (per-request) for completion signaling back to the HTTP goroutine
- **`drainChan`** for graceful shutdown

The approach is entirely portable — it depends only on Go's channel semantics and standard CGo interop, with no OS-specific synchronization primitives.

---

## Features

- HTTP/1.1, HTTP/2, HTTP/3 (QUIC) via Caddy
- Automatic TLS via Let's Encrypt (inherited from Caddy, zero-config ACME)
- Prometheus/OpenMetrics endpoint; FrankenPHP-specific worker metrics (v1.3: busy workers, crashes, queue depth)
- Mercure protocol support (SSE/push) built-in
- Early Hints (103), Zstandard compression
- Single static binary deployment

---

## Does NOT Have

- Database connection pooling
- In-memory KV store / clustered cache
- Native multi-node clustering
- Integrated debug/profiling UI
- Query analysis / slow query tools

---

## PHP-Side Requirements

- Classic mode: None. Drop-in.
- Worker mode: Thin bootstrap loop calling `frankenphp_handle_request()`. No Composer packages. Superglobals still work inside the handler callback.

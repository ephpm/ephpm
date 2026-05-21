# Native Middleware

ePHPm runs PHP in-process via the embed SAPI. The HTTP server, the
router, the KV store, and the gossip cluster all live in the same
binary alongside the Zend Engine. That layout makes it possible to do
something every other PHP application server pushes out to a separate
reverse proxy: run **middleware in front of PHP** as compiled native
code, in the same address space, with a documented C ABI.

This page describes the design for a generic middleware loader that
runs `.so` / `.dylib` / `.dll` files between hyper's connection
handler and the PHP SAPI dispatch — the same pattern used by Caddy
modules, Envoy HTTP filters, nginx modules, and Traefik plugins —
adapted to a PHP-first runtime.

---

## The problem in PHP land

In every other PHP stack, "middleware" lives in PHP itself: a Symfony
HttpKernel layer, a Laravel middleware class, a PSR-15 pipeline. That
works for application-level concerns, but it has a fundamental cost
problem: **every request, including the ones you'll reject, has to
boot PHP first**.

A typical "is this JWT valid?" check costs:

| Layer | Cost |
|---|---|
| TCP accept + TLS handshake | ~µs |
| HTTP parse | ~µs |
| Spawn PHP-FPM worker / boot framework | **single-digit ms** |
| Run framework middleware to validate token | **fractions of ms** |
| Return 401 if invalid | µs |

For an API that's 70 % bots, expired tokens, and rate-limited
clients, you pay the framework-boot tax on every rejection. The
typical workaround is to push auth into a reverse proxy (nginx with
auth_request, Envoy with ext_authz, a sidecar JWT validator), which
introduces an IPC hop, a separate deployment, and a different
language to maintain.

ePHPm starts from a different place: PHP is already in-process. The
HTTP server is already in-process. There's no proxy in front. If we
want to reject a request before PHP runs, we can — we just need a
mechanism that lets users plug native code into the request pipeline.

---

## What ePHPm has that PHP-FPM doesn't

Three primitives line up to make this clean:

1. **Single-process HTTP pipeline.** `ephpm-server` already owns the
   request from accept to response. Inserting a middleware chain
   between routing and SAPI dispatch is a local change, not a
   cross-process protocol.
2. **Embedded KV store with gossip replication.** Middleware that
   needs shared state (rate limit buckets, cached auth decisions,
   feature flags) can read and write the in-process KV store — and
   that state is automatically replicated across the cluster via
   chitchat. No Redis, no separate state service.
3. **Tokio executor.** Middleware calls run on the existing tokio
   thread pool. CPU-bound checks (JWT verify, regex, CIDR match) run
   inline; calls that block on I/O can opt into `spawn_blocking`.

Combined: a `.so` loaded once at startup, called per request from the
hyper handler, optionally invoking host callbacks into the KV store
to coordinate cluster-wide state — all inside one process, with one
documented ABI.

---

## Design

### The C ABI

A middleware module is any shared library that exports four C
functions:

```c
/* Called once per process after dlopen. Returns 0 on success. */
int32_t ephpm_middleware_init(uint32_t abi_version, const char* config_json);

/* Called once per request before PHP dispatch. */
int32_t ephpm_middleware_invoke(
    const ephpm_request_t*  request,
    ephpm_response_t*       response_out
);

/* Called once per process before dlclose. */
void ephpm_middleware_shutdown(void);

/* Optional: name + version metadata for logs / introspection. */
const char* ephpm_middleware_describe(void);
```

`abi_version` is a `u32` whose major byte controls compatibility —
v1 is `0x01_00_00_00`. Modules check it and refuse to initialise
if the host's ABI is newer than they were built against.

`config_json` is the per-mount middleware configuration block from
`site.toml`, serialised as JSON. Letting the middleware parse its own
config (rather than us defining a schema per type) keeps the ABI flat.

### Request / response shapes

`ephpm_request_t` is an opaque pointer plus accessor functions, not a
flat struct. This keeps the ABI stable as the request model evolves:

```c
const char* ephpm_request_method(const ephpm_request_t*);
const char* ephpm_request_path(const ephpm_request_t*);
const char* ephpm_request_query(const ephpm_request_t*);
const char* ephpm_request_remote_ip(const ephpm_request_t*);

/* Header access: returns NULL if absent. Multi-value returns first. */
const char* ephpm_request_header(const ephpm_request_t*, const char* name);

/* Body: zero-copy view. Lifetime = duration of invoke() call only. */
size_t      ephpm_request_body(const ephpm_request_t*, const uint8_t** out_ptr);
```

`ephpm_response_t` is filled by the middleware to express its
decision:

```c
typedef enum {
    EPHPM_MW_CONTINUE = 0,  /* proceed to PHP dispatch */
    EPHPM_MW_RESPOND  = 1,  /* short-circuit; return status/body to client */
    EPHPM_MW_REWRITE  = 2,  /* mutate request, then continue */
} ephpm_mw_action;

typedef struct ephpm_response {
    ephpm_mw_action  action;
    uint16_t         status;            /* for RESPOND */
    const char*      body;              /* nullable for RESPOND */
    size_t           body_len;
    /* For REWRITE: new path / header overrides, both nullable */
    const char*      rewrite_path;
    const ephpm_header_kv* header_overrides;
    size_t           header_overrides_len;
} ephpm_response_t;
```

All pointers handed to the host via `response_out` must remain valid
until `ephpm_middleware_invoke` returns. The host copies before
unwinding back to the request loop.

### Host callbacks

Middleware that wants to interact with ePHPm's KV store, log via
`tracing`, or get per-tenant metadata calls back into the host. We
expose a small, versioned `ephpm_host_v1` symbol table that the
middleware can dlsym at init time:

```c
typedef struct ephpm_host_v1 {
    /* KV store — same operations as the PHP-side ephpm_kv_* helpers */
    int32_t (*kv_get)(const char* k, size_t k_len,
                      uint8_t** out, size_t* out_len);
    int32_t (*kv_set)(const char* k, size_t k_len,
                      const uint8_t* v, size_t v_len, int32_t ttl_secs);
    int32_t (*kv_setnx)(const char* k, size_t k_len,
                        const uint8_t* v, size_t v_len, int32_t ttl_secs);
    int64_t (*kv_incr)(const char* k, size_t k_len, int64_t by);
    void    (*kv_free)(uint8_t* ptr);

    /* Logging */
    void    (*log)(int32_t level, const char* msg, size_t msg_len);

    /* Vhost identity */
    const char* (*vhost_id)(const ephpm_request_t*);
} ephpm_host_v1;
```

The host symbol table is what makes ePHPm middleware genuinely
different from a generic plugin system: a distributed rate limiter is
~30 lines of Rust because the cluster-replicated counter is one
`kv_incr` call away.

### Configuration

Middleware mounts live in `site.toml`, declared per vhost, ordered
explicitly:

```toml
[[middleware]]
library = "middleware/auth-jwt"   # resolved to auth-jwt.<os>-<arch>.so
match   = "/api/*"
order   = 10
config  = { issuer = "https://auth.example.com", audience = "api" }

[[middleware]]
library = "middleware/rate-limit"
match   = "/api/*"
order   = 20
config  = { per_ip_rps = 50, burst = 100 }

[[middleware]]
library = "middleware/cors"
order   = 30
config  = { allow_origins = ["https://app.example.com"] }
```

`library` resolves through a search path (next to `site.toml`,
then `$EPHPM_MIDDLEWARE_DIR`, then `/usr/local/lib/ephpm/middleware`)
plus a per-platform suffix. The same source build produces:

```
auth-jwt.linux-x86_64.so
auth-jwt.linux-aarch64.so
auth-jwt.darwin-aarch64.dylib
auth-jwt.windows-x86_64.dll
```

`match` is the same glob the rest of the router uses; omitting it
applies the middleware to every request for that vhost. `order` is
mandatory and breaks ties between mounts that overlap.

### Dispatch path

The middleware chain is evaluated in `ephpm-server/src/router.rs`
between path resolution and SAPI dispatch:

1. Resolve the request to a vhost (existing code).
2. Walk the vhost's middleware list in `order`, filter by `match`.
3. For each matching middleware, call `ephpm_middleware_invoke`.
   - `CONTINUE` → keep walking.
   - `RESPOND` → build a `Response<Body>` from `(status, body)` and
     return immediately; PHP is never dispatched.
   - `REWRITE` → apply path / header overrides to the request, keep
     walking.
4. If the chain completes with `CONTINUE`, dispatch to the PHP SAPI
   as today.

Middleware runs on the tokio executor, not the blocking pool.
Modules that block on I/O are expected to use the host's
asynchronous callbacks (KV access is non-blocking; logging is
non-blocking). A future ABI version may add an `invoke_async`
variant that returns a future-like object, but v1 is sync.

### Crash isolation

A buggy `.so` segfaults the entire ePHPm process. v1 documents this
clearly: middleware runs in-process with the same trust level as
ePHPm itself. We don't sandbox.

The pragmatic mitigation is the reference Rust crate (see below):
most users will write middleware in Rust against a safe trait, and
the FFI boundary is generated, so application code rarely touches
raw pointers.

If sandboxed middleware becomes a requirement we add a second
loader (`ephpm-middleware-wasm`) that runs WASM filters via
`wasmtime`. That's a v2 doc.

---

## Reference Rust crate

Most middleware authors will use `ephpm-middleware`, a small crate
that wraps the C ABI in a safe Rust trait:

```rust
use ephpm_middleware::{Middleware, Request, Response, MwAction, host};

pub struct AuthJwt {
    issuer: String,
    audience: String,
}

impl Middleware for AuthJwt {
    fn init(config: &serde_json::Value) -> Self {
        Self {
            issuer:   config["issuer"].as_str().unwrap().to_string(),
            audience: config["audience"].as_str().unwrap().to_string(),
        }
    }

    fn invoke(&self, req: &Request) -> Response {
        let Some(hdr) = req.header("Authorization") else {
            return Response::respond(401, "missing Authorization");
        };
        match validate_jwt(hdr, &self.issuer, &self.audience) {
            Ok(_)  => Response::cont(),
            Err(_) => Response::respond(401, "invalid token"),
        }
    }
}

ephpm_middleware::declare!(AuthJwt);
```

The `declare!` macro generates the four `extern "C"` functions
(`ephpm_middleware_init`, `_invoke`, `_shutdown`, `_describe`) and
handles all marshaling. Users build with
`cargo build --release --crate-type cdylib`; the result is a
drop-in `.so` for any ePHPm install built against the same ABI
version.

---

## Authoring middleware in PHP

The pitch most PHP developers care about: **write middleware in PHP,
using the framework patterns you already know, and compile it to a
native `.so` for the request pipeline.** This is the path that opens
once elephc ships `--emit cdylib` (see below). The middleware ABI
was designed scalar-first specifically so that an AOT-compiled
subset of PHP could realistically target it.

The authoring experience looks like a normal PHP class implementing
a small interface — no FFI, no `extern "C"`, no Rust:

```php
<?php
// auth.php — your business logic, written as ordinary PHP

use Ephpm\Middleware\{Middleware, Request, Response, Host};

final class JwtAuth implements Middleware
{
    private string $issuer;
    private string $audience;
    private string $publicKey;

    public function __construct(array $config)
    {
        $this->issuer    = $config['issuer'];
        $this->audience  = $config['audience'];
        $this->publicKey = file_get_contents($config['public_key_path']);
    }

    public function invoke(Request $req): Response
    {
        $auth = $req->header('Authorization');
        if ($auth === null || !str_starts_with($auth, 'Bearer ')) {
            return Response::respond(401, 'missing bearer token');
        }

        $token = substr($auth, 7);
        $claims = $this->verifyJwt($token);   // your existing logic
        if ($claims === null) {
            return Response::respond(401, 'invalid token');
        }
        if ($claims['exp'] < time()) {
            return Response::respond(401, 'expired token');
        }

        // Cluster-replicated revocation check via ephpm's KV store
        if (Host::kv_get("jwt:revoked:{$claims['jti']}") !== null) {
            return Response::respond(401, 'revoked');
        }

        // Pass user id downstream as a header so PHP code can read it
        return Response::rewrite()->setHeader('X-User-Id', $claims['sub']);
    }

    public function shutdown(): void {}

    private function verifyJwt(string $token): ?array
    {
        // Pure PHP JWT verify — same code you'd write for any framework.
        // No I/O, no extensions, fits the elephc-compilable subset.
    }
}
```

You compile it once:

```bash
elephc compile --emit cdylib --emit header auth.php
# → auth.linux-x86_64.so, auth.h
```

And mount it the same way as any other middleware:

```toml
[[middleware]]
library = "auth"
match   = "/api/*"
order   = 10
config  = {
    issuer = "https://auth.example.com",
    audience = "api",
    public_key_path = "/etc/ephpm/jwt-public.pem"
}
```

The `Ephpm\Middleware\*` interfaces ship as a PHP stub package
(`ephpm/middleware`) that's also valid elephc — it gives users
IDE autocomplete and type checking against the same shape the
compiled `.so` exposes. The compiled `.so` and the host's
expectations stay in lockstep because the C ABI is the single
source of truth.

**Reuse story for existing apps.** Most of the per-request logic
in a typical PHP app already lives in pure PHP classes with no
external dependencies — JWT validation, signature checks,
feature-flag evaluation, request classification, custom rate
limiting. Those classes lift cleanly into middleware: rename the
class, implement the `Middleware` interface, recompile. The hot
path now runs as native code, before PHP boots, at µs latency.

**Mixed-language pipelines work fine.** A vhost can mount Rust,
C, and elephc-PHP middlewares in the same chain — they all speak
the same ABI. Use Rust for the perf-critical bits (HMAC verify,
CIDR matching, regex), use elephc-PHP for the business logic
that's easier to express in PHP (per-tenant feature flag rules,
domain-specific validations, A/B routing).

---

## Off-the-shelf middleware

We ship a curated catalog in-tree under `crates/middleware/*`. The
goal is "batteries included" — most users find what they need
without writing any Rust:

| Crate | Purpose |
|---|---|
| `ephpm-middleware-jwt` | JWT validation (sig, exp, issuer, audience, custom claims) |
| `ephpm-middleware-ratelimit` | Token-bucket rate limiting, KV-backed, **cluster-replicated via gossip** |
| `ephpm-middleware-basicauth` | HTTP Basic |
| `ephpm-middleware-cors` | CORS preflight + headers |
| `ephpm-middleware-security-headers` | HSTS, CSP, X-Frame-Options, Referrer-Policy, X-Content-Type-Options |
| `ephpm-middleware-iplist` | Allow / deny lists with CIDR matching |
| `ephpm-middleware-sizelimit` | Reject requests over a max body size before they reach PHP |
| `ephpm-middleware-webhook-sig` | HMAC signature verification (GitHub, Stripe, generic) |
| `ephpm-middleware-geoip` | MaxMind DB routing / blocking |
| `ephpm-middleware-cache` | Response cache backed by KV (cluster-replicated TTL'd cache) |
| `ephpm-middleware-otel` | OpenTelemetry trace context propagation |
| `ephpm-middleware-requestid` | Inject `X-Request-Id` (UUIDv4) for downstream correlation |

The cluster-replicated rate limiter and response cache are the
distinguishing pieces — they're cheap to build because the KV store
is already there, and no other PHP server bundles them.

---

## Phases

### Phase 1 — Loader + reference crate + 4 core middlewares

- `ephpm-middleware` crate: trait, derive macro, FFI bindings,
  request/response marshaling.
- Loader in `ephpm-server`: `libloading`-based dlopen, symbol
  lookup, lifecycle calls, ABI-version check.
- `[[middleware]]` block in `SiteConfig`.
- Dispatch hook in `router.rs`: chain walk, action handling,
  request mutation for `REWRITE`.
- Host callback table v1 (KV + logging + vhost id) — exposed via
  a single symbol the middleware dlsyms at init.
- Ship `jwt`, `ratelimit`, `cors`, `security-headers` in-tree as
  the v1 "batteries included" set.
- Docs: ABI reference, Rust trait quickstart, example walkthrough.

Roughly one week of focused work. The ABI is the load-bearing
artifact; once published, breaking it is painful — so the design
review is the slow part, not the code.

### Phase 2 — Expand the catalog

- `iplist`, `sizelimit`, `webhook-sig`, `cache`, `otel`, `geoip`,
  `basicauth`, `requestid`.
- Each is a separate crate of ~100–500 lines of Rust. Most use
  existing well-known crates (`jsonwebtoken`, `cidr`, `maxminddb`,
  `opentelemetry`, etc.).

### Phase 3 — Path mutation, async invoke, WASM lane

- `REWRITE` action: full implementation of header / path mutation
  propagating to the downstream PHP request.
- `invoke_async` ABI: a future-like return type for middleware that
  needs to await on something genuinely async (database lookup
  against an external service, not the in-process KV).
- Optional WASM loader (`ephpm-middleware-wasm`) for sandboxed
  middleware where crash isolation matters more than raw perf.

### Phase 4 — Hot reload, observability

- File-watcher on middleware `.so` files: on rebuild, drain in-flight
  calls, dlclose, dlopen the new version, swap the symbol table.
- Per-middleware metrics: invocation count, latency histogram, action
  distribution (CONTINUE / RESPOND / REWRITE), errors.
- Tracing spans for each middleware call, integrated with the
  existing `tracing` infrastructure.

---

## Relationship to the elephc cdylib ask

[elephc](https://elephc.dev) is a Rust-based AOT compiler that
translates a static subset of PHP into native machine code. Today
it only emits executables, but we've filed an upstream request for
a `--emit cdylib` mode with a documented C ABI for exported
functions.

If that ships, the middleware system gets a second supported
language for free: PHP developers can write middleware in PHP and
compile it with elephc, producing the same `.so` shape the loader
already accepts. No changes on our side — the middleware ABI was
designed scalar-first specifically to match what an AOT-compiled
subset of PHP can realistically produce.

The middleware system is **not blocked on elephc**. v1 ships with
the Rust authoring story. elephc-authored middleware is a bonus
language path that lights up when their cdylib lands.

---

## Why this matters

ePHPm currently competes with PHP-FPM + nginx on the
"deploy a PHP app" axis. Native middleware adds a second axis:
**ePHPm replaces the reverse proxy too**. Auth, rate limiting,
CORS, caching, security headers, observability — all in one
binary, all configured in `site.toml`, all written in safe Rust
(or eventually PHP).

That's a Caddy-class architectural story for PHP. Combined with
WordPress on embedded SQLite and OPcache JIT, the demo geometry
becomes "one binary, no external proxy, no external database,
native-speed middleware in front of a JIT-compiled PHP app" — a
configuration that's structurally impossible to replicate with
PHP-FPM.

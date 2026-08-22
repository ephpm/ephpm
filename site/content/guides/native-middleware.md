+++
title = "Native Middleware"
weight = 10
aliases = ["/roadmap/native-middleware/"]
+++

ePHPm can run **compiled middleware in front of PHP**, called per request
*before* PHP dispatch — and before any request-body bytes are read. A
rejected request (bad JWT, rate-limited client, CORS preflight) never boots
PHP and never pays for the body transfer.

Middleware can call back into the host: the embedded (cluster-replicated)
KV store and the `tracing` logger are one function call away. That's what
makes a cluster-wide rate limiter a ~100-line module — the replicated
counter is a single `kv_incr`.

There are **two ways a module runs**:

- **Built-in (static registry).** Ten official modules — `jwt`, `cors`,
  `ratelimit`, `security-headers`, `api-key`, `ip-allowlist`,
  `maintenance-mode`, `redirect`, `request-id`, and `header-transform` — are
  compiled into **every** ePHPm binary. `library = "jwt"` just works: no
  shared library on disk, no `dlopen`, no special build. Two of them
  (`request-id`, `header-transform`) also run in the **response phase**.
- **Dynamic (shared library).** Custom out-of-tree modules are `.so` /
  `.dylib` / `.dll` files speaking a small, versioned C ABI, loaded once at
  startup via `dlopen` (`LoadLibrary` on Windows). This works out of the
  box with the stock release binaries on every platform — see
  [the dynamic lane](#the-dynamic-lane).

## Quick start

Built-ins need nothing but configuration — this works with the stock
release binary on every platform:

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"

[[middleware]]
library = "security-headers"
order   = 10
config  = { csp = "default-src 'self'" }

[[middleware]]
library = "cors"
order   = 20
config  = { allow_origins = ["https://app.example"] }

[[middleware]]
library = "jwt"
match   = "/api/*"
order   = 30
config  = { secret = "change-me", claims_header = "X-Jwt-Claims" }

[[middleware]]
library = "ratelimit"
match   = "/api/*"
order   = 40
config  = { per_ip_rps = 1, burst = 2 }
```

Startup logs each module as it initialises, then the whole chain:

```
INFO ephpm_server::middleware: middleware initialised (builtin) module=security-headers describe=...
...
INFO ephpm_server: middleware chain loaded count=4 modules=[...]
```

And the behavior, as observed with `curl`:

- `GET /index.php` → `200` with the PHP body **plus**
  `Strict-Transport-Security`, `Content-Security-Policy`,
  `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy` appended.
- `OPTIONS /api/x.php` with `Origin` + `Access-Control-Request-Method`
  from an allowed origin → `204` with the `Access-Control-*` headers.
  PHP never runs, and neither do later mounts (the JWT 401 does not fire).
- `GET /api/x.php` without a token → `401 missing bearer token`, PHP
  never runs. With a valid HS256 token → PHP runs and reads the verified
  claims from `$_SERVER['HTTP_X_JWT_CLAIMS']`.
- Hammering `/api/x.php` → exactly `per_ip_rps × 10 + burst` requests
  succeed per 10-second window, then `429` with a `Retry-After`.

## Configuration

Mounts are `[[middleware]]` blocks in `ephpm.toml`, ordered explicitly:

```toml
[[middleware]]
library = "security-headers"                    # built-in (compiled in)
order   = 10
config  = { csp = "default-src 'self'" }

[[middleware]]
library = "/etc/ephpm/middleware/libmy_auth.so" # custom module, explicit path
match   = "/api/*"
order   = 30
config  = { api_key = "..." }
```

| Key | Required | Meaning |
|-----|----------|---------|
| `library` | yes | Built-in name or shared library to load — bare name or explicit path (see below). Must not be empty. |
| `match` | no | Path glob; the mount only runs when the request path matches. `*` matches any character sequence, **including `/`**. Unset = every PHP-bound request. |
| `order` | yes | Chain position. Lower runs first; equal orders keep declaration order. |
| `config` | no | Arbitrary table, serialised to JSON and handed to the module's `init`. |

Mounts are **global** — they apply to every vhost. A module that needs
per-tenant behavior reads the request's vhost id (the server name) and
decides itself.

Loading is **fail-fast**: a builtin whose `init` rejects its config, a
library that can't be found, a missing ABI symbol, or a dynamic module
whose `init` returns an error aborts server startup with a message naming
the mount.

### Library resolution

The `library` value is checked against the **builtin registry first**.
Each built-in answers to its short name and its crate name, with `-` and
`_` interchangeable: `jwt`, `cors`, `ratelimit` (also `rate-limit`),
`security-headers`, `api-key`, `ip-allowlist`, `maintenance-mode`,
`redirect`, `request-id`, `header-transform`, and the
`ephpm-middleware-*` / `ephpm_middleware_*` long forms. Builtin mounts
never touch the filesystem.

Anything else is resolved as a shared library. A value containing a path
separator or a file extension is used as-is. A bare name tries, in each
search directory:

1. `<name>.<os>-<arch>.<ext>` — e.g. `my-auth.linux-x86_64.so`
2. `lib<name>.<ext>` — cargo's own artifact naming
3. `<name>.<ext>`

Search directories, in order:

1. the server's working directory
2. `$EPHPM_MIDDLEWARE_DIR` (when set)
3. `/usr/local/lib/ephpm/middleware`

The startup error lists every candidate path tried, so a typo'd mount is
easy to diagnose.

## Chain semantics (v1)

Per request, the chain walks mounts in ascending `order`, skipping mounts
whose `match` doesn't match the request path. Each module returns one of
three verdicts:

- **CONTINUE** — keep walking; optionally append headers to the eventual
  client response (CORS headers, security headers).
- **RESPOND** — short-circuit *immediately*: the module's status/body/headers
  go back to the client and **PHP never runs**. Later mounts don't run
  either.
- **REWRITE** — accumulate a request-path override (last writer wins) and/or
  request-header overrides (chain order), then keep walking. May also append
  response headers, like CONTINUE.

v1 rules worth knowing:

- **Every module sees the original request.** Rewrites are applied *after*
  the whole chain ran — a later module does not observe an earlier module's
  path/header overrides.
- **Header overrides reach PHP** as normal request headers (`HTTP_*` in
  `$_SERVER`), replacing any client-sent header of the same name — that's
  how `jwt`'s `claims_header` hands verified claims to PHP.
- **A path rewrite affects `REQUEST_URI`** (and `PATH`). In fpm mode the
  script was already resolved before the chain ran, so the originally
  resolved script still executes; in worker mode the framework routes on the
  rewritten `REQUEST_URI`, so rewrites fully re-route.
- **Failures are fail-closed.** A dynamic module whose `invoke` returns
  non-zero, a Rust panic caught by the authoring kit, or a panicking
  built-in (caught by the host) all produce a plain 500, never a silent
  pass-through.
- **Request bodies are hidden by default.** The chain runs before the body is
  read (rejecting before the transfer is the point), so `req.body()` is empty
  unless the operator opts in with `[server.request] middleware_body_limit`
  (bytes) `> 0`. When set, the body is buffered up front and `req.body()`
  returns up to that many bytes — for webhook/HMAC signature checks,
  CSRF-with-body, and payload validation — while the full body still reaches
  PHP intact. See the [config reference](../../reference/config/).

**Coverage.** The request phase runs on **both** the PHP path and the
**static-file** path, in every mode — so a `RESPOND` verdict gates a static
asset just as it gates a PHP request, and it does so *before the file is read
from disk*. Only the router's own pre-routing replies (internal
`/_ephpm/*` and `/metrics` endpoints, ACME challenges, and the
trusted-host / hidden-file / blocked-path 4xx gates) answer ahead of the chain
and are never seen by it.

> **Static assets are gated too.** A gating mount (`basic-auth`,
> `session-cookie`, `github-auth`, `jwt`, …) that matches a static asset's path
> now denies it before the bytes leave disk — scope the mount's `match` glob to
> cover the asset paths you mean to protect (e.g. `match = "/wp-content/uploads/*"`).
> Defense in depth still applies: keep truly confidential files out of a
> web-served document root where practical.

See **[Response phase](#response-phase-transforming-the-response)** below for
the *second* phase, which runs after the response is generated.

## Response phase (transforming the response)

The verdicts above are the **request phase** — they decide what to do *before*
a request is served. A module may also implement an optional **response
phase** that runs *after* the response is generated (PHP output, a static
file, an error page, or a request-phase `RESPOND`), to **transform** it:
compression, ETag / conditional-GET, response-header injection based on the
body. This is the mechanism that lets those features be middleware at all, and
it is what makes them apply to **static** files, not just PHP.

Key properties:

- **Runs in reverse chain order** (onion model): the last request-phase module
  unwinds first.
- **Buffered responses only.** A streamed response (worker-mode
  `send_response_stream`, large files streamed from disk) bypasses the response
  phase untouched — a stream is never buffered or corrupted to transform it.
- **Fails safe.** Unlike the request phase (which fails *closed*), a response
  handler that errors or panics leaves the response **unchanged**. A response
  module is a transform, **not a security gate** — do your gating in the
  request phase.
- **May run without a request phase.** On the static path no request phase ran
  for this module, and an upstream `RESPOND` can short-circuit before it — so a
  response handler must not assume state its own request phase would have set.
- **Opt-in per module.** Only modules that export the optional
  `ephpm_middleware_invoke_response` symbol participate; every existing v1
  module is unaffected (see [The C ABI](#the-c-abi-for-non-rust-modules)).

In Rust, implement the `ResponseMiddleware` trait *in addition* to
`Middleware`, and opt in with `declare!(Type, response)`:

```rust
use ephpm_middleware::{Middleware, Request, Response, ResponseMiddleware, ResponseView};

struct Stamp;

impl Middleware for Stamp {
    fn init(_c: &serde_json::Value) -> Result<Self, String> { Ok(Self) }
    fn invoke(&self, _req: &Request<'_>) -> Response { Response::cont() }
}

impl ResponseMiddleware for Stamp {
    fn invoke_response(&self, req: &Request<'_>, resp: &mut ResponseView<'_>) {
        // Read the generated response…
        let _status = resp.status();
        let _ctype = resp.header("Content-Type");
        let _body: &[u8] = resp.body();
        // …and stage edits (applied by the host: removes, then sets, then
        // status, then body; Content-Length is recomputed if you replace the
        // body).
        resp.set_header("X-Served-By", "ephpm");
        resp.remove_header("X-Powered-By");
        // resp.set_status(203);
        // resp.set_body(transformed);
    }
}

declare!(Stamp, response);   // note the extra `, response`
```

A complete, real example — a gzip response compressor that runs on static
files too — ships as
[`crates/ephpm-server/examples/mw_response_gzip.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-server/examples/mw_response_gzip.rs).

## The built-in modules

All ten are compiled into every ePHPm binary and run in-process — mount
them by short name (`library = "jwt"`) with no shared library on disk.
`request-id` and `header-transform` additionally run in the response phase.
Loadable cdylib builds of the same implementations (for the dlopen lane, or
as authoring templates) live in the
[`ephpm/middleware-examples`](https://github.com/ephpm/middleware) repo.

### `security-headers`

Always CONTINUEs; the configured headers ride along on whatever response
PHP produces for every matching request. All config keys optional:

| key | default | header |
|-----|---------|--------|
| `hsts` (bool) | `true` | `Strict-Transport-Security: max-age=63072000; includeSubDomains` |
| `csp` (string) | unset | `Content-Security-Policy` |
| `frame_options` (string) | `"DENY"` | `X-Frame-Options` (empty string disables) |
| `content_type_options` (bool) | `true` | `X-Content-Type-Options: nosniff` |
| `referrer_policy` (string) | `"strict-origin-when-cross-origin"` | `Referrer-Policy` (empty string disables) |

### `cors`

Answers CORS preflights directly (`204`, PHP never runs) and appends
`Access-Control-Allow-Origin` / `Vary: Origin` to actual cross-origin
responses. Requests without an `Origin` header, or from a disallowed origin,
pass through untouched (per spec, the browser enforces the failure).

| key | default | meaning |
|-----|---------|---------|
| `allow_origins` (array) | **required** | allowed origins; `"*"` allows all |
| `allow_methods` (string) | `"GET, POST, PUT, PATCH, DELETE, OPTIONS"` | preflight `Access-Control-Allow-Methods` |
| `allow_headers` (string) | `"Content-Type, Authorization"` | preflight `Access-Control-Allow-Headers` |
| `allow_credentials` (bool) | `false` | emit `Access-Control-Allow-Credentials: true` and echo the origin instead of `*` |
| `max_age` (integer) | `86400` | preflight `Access-Control-Max-Age` seconds |

### `jwt`

Validates **HS256** bearer tokens before PHP runs. Missing/invalid tokens
short-circuit with `401`. The signature is verified first (constant-time
HMAC), `alg` is pinned to HS256 (`alg: none` is rejected), `exp` is
**required** and must be in the future, `nbf` is honoured, and `iss`/`aud`
are enforced when configured.

| key | default | meaning |
|-----|---------|---------|
| `secret` (string) | **required** | HS256 shared secret |
| `issuer` (string) | unset | required `iss` claim value |
| `audience` (string) | unset | required `aud` value (string or array member) |
| `header` (string) | `"Authorization"` | request header carrying the token; `Bearer ` prefix stripped |
| `claims_header` (string) | unset | forward the verified claims JSON to PHP in this request header |

With `claims_header = "X-Jwt-Claims"`, PHP reads the verified claims from
`$_SERVER['HTTP_X_JWT_CLAIMS']` without re-verifying the token. Any
client-sent header of that name is **stripped at ingest** — before the
middleware chain runs and before any header crosses to PHP — so a request
that never matches this module's `match` glob (or bypasses it entirely) can
never smuggle a forged claims value through. When a valid token is present
the `jwt` module then sets the header to the verified claims JSON. PHP can
therefore trust `HTTP_X_JWT_CLAIMS` regardless of request path.

> ePHPm also always strips the `Proxy` request header at ingest (httpoxy
> defense), so it never surfaces as `$_SERVER['HTTP_PROXY']`.

v1 is HS256 only — RS256/JWKS is not implemented.

### `ratelimit`

Fixed-window per-client rate limiting backed by the embedded KV store.
Requests are counted in 10-second windows; each window allows
`per_ip_rps × 10 + burst` requests per client. Over the limit: `429` with
`Retry-After` for the seconds left in the window.

**Cluster scope: per-node only, not cluster-wide (v1).** The counter is
maintained with KV `INCR`, which is not yet gossip-replicated across
nodes — only `SET`/`DEL` writes propagate. That means each node enforces
its own window independently: a client hitting N nodes gets up to N ×
the configured allowance. A cluster-wide window is planned (issue #150),
tracked with replicated `INCR`. Startup logs a `warn!` when `ratelimit`
is mounted with `[cluster].enabled = true` so operators see the gap.

| key | default | meaning |
|-----|---------|---------|
| `per_ip_rps` (integer) | **required**, > 0 | sustained requests/second per client |
| `burst` (integer) | `per_ip_rps` | extra headroom per window |
| `key_headers` (array) | unset | identify clients by the first present header (e.g. `X-Api-Key`) instead of client IP |

**Fail-open by design:** if the KV store is unavailable, requests are
allowed through with a warning log — a rate limiter that hard-fails would
turn a soft protection into an outage. Don't use it as an auth gate.

Note this is a *fixed-window* limiter (a full window's allowance can be
consumed instantly at a window boundary), and it is distinct from the
built-in connection-level limiter in `[server.limits]` — the two are
independent.

### `api-key`

Validates an API key on the request before PHP runs, then forwards the
resolved **consumer identity** to PHP. A recognised key `REWRITE`s the
request, injecting the consumer id in a header PHP reads; a missing or
unrecognised key short-circuits with `401` (PHP never runs). Static keys are
compared **constant-time** (`subtle`), and the presented key is never logged.
At least one of `keys` / `kv_key_template` must be configured.

| key | default | meaning |
|-----|---------|---------|
| `header` (string) | `"X-Api-Key"` | request header carrying the key |
| `query_param` (string) | unset (disabled) | also accept the key from this query parameter — off by default, since URLs leak into logs |
| `keys` (object) | unset | static `key → consumer-id` map |
| `kv_key_template` (string) | unset | KV lookup key with a `<key>` placeholder (e.g. `apikey:<key>`); the stored value is the consumer id |
| `consumer_header` (string) | `"X-Consumer-Id"` | header injected for PHP with the resolved consumer id |

### `ip-allowlist`

Allows or denies requests by client IP against CIDR lists (IPv4 + IPv6).
The client IP is taken from the host's trusted-proxy-resolved value — this
module never parses `X-Forwarded-For` itself. **Fail-closed:** a malformed
CIDR fails startup, and an unparseable client IP is denied unless
`default = "allow"`. `deny` always wins over `allow`.

| key | default | meaning |
|-----|---------|---------|
| `allow` (array of CIDR strings) | `[]` | client IPs allowed through; a bare address is `/32` (v4) or `/128` (v6) |
| `deny` (array of CIDR strings) | `[]` | client IPs rejected with `403`; takes precedence over `allow` |
| `default` (string) | `"deny"` | verdict when no rule matches: `"allow"` or `"deny"` |

### `maintenance-mode`

Flips a tenant into a `503` holding page the instant a per-site flag appears
in the embedded (cluster-replicated) KV store — no redeploy. Per request it
builds a per-site key from `key_template` (default `mw:maintenance:<vhost>`)
and reads it; a truthy value serves the holding page. **Fail-open by
design:** a KV blip means CONTINUE, so a transient hiccup can't black-hole
every tenant. Never use it as an access-control gate.

| key | default | meaning |
|-----|---------|---------|
| `key_template` (string) | `"mw:maintenance:<vhost>"` | KV key checked per request; `<vhost>` is substituted |
| `retry_after` (integer seconds) | `300` | `Retry-After` header on the 503 |
| `body` (string) | built-in minimal HTML | holding-page body |
| `content_type` (string) | `"text/html; charset=utf-8"` | holding-page `Content-Type` |
| `bypass_ips` (array of strings) | unset | exact IPs or CIDR ranges whose requests continue during maintenance |
| `bypass_paths` (array of strings) | unset | path prefixes kept live (e.g. `/healthz`) |

### `redirect`

Enforces canonical URLs with a single `301`/`308` **before** PHP runs.
Composes scheme, host, and trailing-slash rules, computes the canonical URL
once, and redirects only when the request is not already canonical (so it
can never loop). Since the v1 ABI exposes no request scheme, the current
scheme is read from `forwarded_proto_header` (default `X-Forwarded-Proto`).

| key | default | meaning |
|-----|---------|---------|
| `force_https` (bool) | `false` | redirect `http` → `https` |
| `canonical_host` (string) | unset | `"www"` forces apex → `www.`; `"apex"` (alias `"non-www"`) strips a leading `www.` |
| `host_map` (object) | unset | explicit `source-host` → `canonical-host` map; wins over `canonical_host` |
| `trailing_slash` (string) | unset | `"add"` or `"strip"` (root and file-like paths are left alone) |
| `status` (integer) | `308` | redirect status — `301` or `308` |
| `forwarded_proto_header` (string) | `"X-Forwarded-Proto"` | header the current scheme is derived from |

### `request-id`

**Request + response phase.** Gives every request a stable correlation id,
injects it for PHP (`$_SERVER['HTTP_X_REQUEST_ID']`), and echoes it on the
response — so the access log, the application log, and the client all share
one id. The response phase guarantees the header even on responses no request
phase touched (static files), and is idempotent on the PHP path. An inbound
id is trusted only when `trust_inbound` is on **and** it is a short, printable
ASCII token (no CR/LF smuggling).

| key | default | meaning |
|-----|---------|---------|
| `header` (string) | `"X-Request-Id"` | request/response header carrying the id |
| `trust_inbound` (bool) | `false` | reuse a well-formed inbound value instead of generating |

### `header-transform`

**Request + response phase.** Sets request headers PHP sees (before PHP runs)
and sets/removes response headers on the way out — on **every** response (PHP,
static file, error page). Both request and response `set` are replace-or-add;
header **removal is response-side only** (the v1 ABI request phase can only
override a request header, not delete one — a request-side `remove` is
rejected at `init` rather than silently ignored).

```toml
[middleware.config.request]
set = { "X-Env" = "prod" }

[middleware.config.response]
set    = { "X-Served-By" = "ephpm" }
remove = ["Server", "X-Powered-By"]
```

| section | key | effect |
|---------|-----|--------|
| `request` | `set` (object) | replace-or-add each request header PHP sees |
| `response` | `set` (object) | replace-or-add each response header |
| `response` | `remove` (array) | delete each response header (case-insensitive) |

## The dynamic lane

Custom out-of-tree modules load through `dlopen` (`LoadLibrary` on
Windows). This works with the stock release binaries on every platform:

- **Linux** release binaries (`cargo xtask release`, the
  `docker/Dockerfile` image, and the published release artifacts) are
  **glibc-dynamic** — a single file that targets
  `<arch>-unknown-linux-gnu` and can `dlopen()` shared middleware (and
  shared PHP extensions — see [PHP Extensions](/guides/php-extensions/))
  out of the box.
- **macOS** release binaries are dynamically linked against the system
  runtime (`dlopen` is always available there).
- **Windows** builds use `LoadLibrary` — no special build needed.

The module's libc must match the host binary's: on Linux, build modules
for the gnu target (the default on every mainstream distro toolchain) —
see [Building modules on Linux](#building-modules-on-linux).

**Building a fully static binary yourself:** you can still produce a
fully static musl ePHPm (`x86_64-unknown-linux-musl` with `crt-static`)
if your deployment demands it, but be aware that a fully static binary
**cannot `dlopen()` anything** — every `[[middleware]]` mount that
resolves to a shared library (and every `[php] extensions` entry) fails
startup with `Dynamic loading not supported`. Built-ins keep working;
custom static composition tooling for that scenario is future work
(`docs/architecture/build-compose-design.md`). Planned — not yet
implemented.

## Writing your own module in Rust

A complete worked example — all three verdicts, the KV host callbacks, a demo
site and a build-and-verify walkthrough — lives in the repository at
[`examples/rust-middleware/`](https://github.com/ephpm/ephpm/tree/main/examples/rust-middleware).
The sketch below is the minimum.

Add the authoring crate and implement one trait:

```toml
[package]
name = "my-auth"

[lib]
crate-type = ["cdylib"]

[dependencies]
ephpm-middleware = { git = "https://github.com/ephpm/ephpm" }
serde_json = "1"
```

```rust
use ephpm_middleware::{declare, Middleware, Request, Response};

struct MyAuth { api_key: String }

impl Middleware for MyAuth {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let api_key = config.get("api_key")
            .and_then(|v| v.as_str())
            .ok_or("`api_key` is required")?;
        Ok(Self { api_key: api_key.to_owned() })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        match req.header("X-Api-Key") {
            Some(k) if k == self.api_key => Response::cont(),
            _ => Response::respond(401, "nope"),
        }
    }
}

declare!(MyAuth);
```

`declare!(MyAuth)` generates the four C ABI exports, the ABI major-version
check, config JSON parsing, response marshaling, and panic containment (a
panicking `invoke` becomes a fail-closed 500). To add the optional response
phase, implement `ResponseMiddleware` and write `declare!(MyAuth, response)` —
see [Response phase](#response-phase-transforming-the-response).

Inside `invoke`, `req.host()` exposes host services:

```rust
let host = req.host();
host.kv_set("k", b"v", 60);          // TTL in seconds; 0 = no expiry
let v = host.kv_get("k");            // Option<Vec<u8>>
let created = host.kv_set_nx("k", b"0", 30);
let n = host.kv_incr("counter", 1);  // Option<i64>, atomic
host.log(ephpm_middleware::abi::LOG_INFO, "hello from middleware");
```

The KV operations hit the same embedded store PHP sees through
`ephpm_kv_*` — replicated across the cluster when clustering is enabled.

The `Request` also exposes the connection and (optional) body:

```rust
fn invoke(&self, req: &Request<'_>) -> Response {
    // Scheme is authoritative from the connection (ePHPm terminates TLS) —
    // no X-Forwarded-Proto sniffing. This is the correct force-https basis.
    if !req.is_secure() {
        let target = format!("https://{}{}", req.http_host(), req.path());
        return Response::respond(301, "").header("Location", target);
    }
    // req.body() is empty unless `[server.request] middleware_body_limit > 0`.
    // Verify an HMAC signature over the buffered body:
    let sig = req.header("X-Signature").unwrap_or("");
    if !verify_hmac(req.body(), sig) {
        return Response::respond(401, "bad signature");
    }
    Response::cont()
}
```

`req.scheme()` returns `"http"`/`"https"`, `req.http_host()` the normalized
`Host` (port/trailing-dot stripped, lowercased — distinct from
`req.vhost_id()`, the raw server name), and `req.body()` the bounded buffered
body. Against an older host that predates these (ABI minor < 2) they fall back
to `"http"` / `false` / `""` rather than reading past a shorter table.

### Building modules on Linux

The module must match the host binary's libc. The release binary is
glibc-dynamic (gnu target), so a plain release build on any mainstream
distro produces a compatible `.so`:

```bash
cargo build --release -p my-auth
```

The artifact lands at `target/release/lib<crate_name>.so`; a bare
`library = "<crate_name>"` mount finds the `lib<name>.so` form through
the search path. The ten official modules are already compiled into every
ePHPm binary (mount them by short name); loadable cdylib builds of the same
implementations, useful as authoring templates, live in the
[`ephpm/middleware-examples`](https://github.com/ephpm/middleware) repo and
build exactly the same way.

Build on a distro whose glibc is not newer than the deployment target's
(the usual glibc forward-compatibility rule — a module built on Debian 12
runs on anything with glibc >= Debian 12's).

## The C ABI (for non-Rust modules)

A module is any shared library exporting:

```c
int32_t ephpm_middleware_init(uint32_t abi_version,
                              const char* config_json,
                              const ephpm_host_v1* host);
int32_t ephpm_middleware_invoke(const ephpm_request_t* request,
                                ephpm_response_t* response_out);
void    ephpm_middleware_shutdown(void);
const char* ephpm_middleware_describe(void);   /* optional, nullable */

/* Optional response phase (ABI minor 1). A module that does not export it is
   unaffected; the host skips it after generating the response. Return 0 to
   apply the edit, non-zero to leave the response unchanged (fail-safe). */
int32_t ephpm_middleware_invoke_response(const ephpm_request_t* request,
                                         const ephpm_response_ctx_t* response,
                                         ephpm_response_edit_t* edit_out);
```

- `abi_version` is `0x01_00_00_02` — major **1**, minor **2**. The **major
  byte** gates compatibility; modules must refuse to init (return non-zero)
  when the host's major is newer than they were built for. The lower three
  bytes are an additive **minor** level: growth (new host-table fields, the
  optional response symbol) bumps the minor and keeps the major, so existing
  major-1 modules keep loading. A module that uses a newer field must check
  the host's advertised minor (`host->abi_version & 0x00FFFFFF`) first —
  minor **1** added the response phase, minor **2** the appended request
  accessors (`request_scheme`/`request_is_secure`/`request_host`).
- `config_json` is the mount's `config` table serialised to JSON (NULL when
  the mount has no config).
- The host callback table is passed **by pointer at `init`** and is valid
  for the process lifetime — modules do not `dlsym` host symbols (that
  would need `-rdynamic` on Linux and has no clean Windows analogue). It
  contains request accessors (method, path, query, remote IP, header
  lookup, vhost id, and — minor 2 — scheme/`is_secure`, normalized host, and
  the bounded buffered body), the KV operations (`kv_get`/`kv_set`/
  `kv_set_nx`/`kv_incr`/`kv_incr_ttl`/`kv_free`), `log`, and — for the
  response phase (minor 1) — the response accessors (`response_status`/
  `response_headers`/`response_body`).
- The request pointer is only valid during `invoke`; never store it.
  Everything a module writes into `response_out` must stay valid until its
  `invoke` returns — the host copies before unwinding. The same rule applies
  to the response phase: the `ephpm_response_ctx_t*` and everything a module
  writes into `ephpm_response_edit_t` are valid only until
  `ephpm_middleware_invoke_response` returns.
- New host capabilities append to the end of the table under the same major
  version (bumping the minor).

The authoritative definition is
[`crates/ephpm-middleware/src/abi.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-middleware/src/abi.rs).

## Observability

Each request-phase invocation increments
`ephpm_middleware_invocations_total{module, action}` where `action` is the
verdict (`continue` / `respond` / `rewrite`; module errors count as
`respond` since they fail closed as 500s). Each response-phase invocation
increments `ephpm_middleware_response_invocations_total{module}`. Module `log`
calls surface through the host's `tracing` subscriber under the
`ephpm_middleware` target.

## Trust model

Middleware runs **in-process with the same privileges as ePHPm itself**.
There is no sandbox: a buggy module can crash the server; a malicious one
owns it. Only load modules you built or trust — treat a `.so` mount like a
binary you're executing, because it is. (Rust-authored modules get panic
containment from `declare!`, but that is not a security boundary — a
*memory* fault is not a panic and is not contained.)

When a module does fault, ePHPm writes a fatal-signal report to stderr
naming the faulting `.so` and function before it dies — see
[Diagnosing Crashes](/guides/diagnosing-crashes/).

## Not implemented (yet)

Planned — not yet implemented: request-body access from middleware, an
async `invoke` variant, hot reload of modules, per-vhost mounts, a WASM
loader for sandboxed modules, and the wider module catalog (basic-auth,
webhook signatures, GeoIP, response cache, OpenTelemetry). The design notes
live in the git history of the roadmap page this guide replaced.

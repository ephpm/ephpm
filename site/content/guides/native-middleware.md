+++
title = "Native Middleware"
weight = 10
aliases = ["/roadmap/native-middleware/"]
+++

ePHPm can run **compiled middleware in front of PHP**: shared libraries
(`.so` / `.dylib` / `.dll`) loaded once at startup and called per request
*before* PHP dispatch — and before any request-body bytes are read. A
rejected request (bad JWT, rate-limited client, CORS preflight) never boots
PHP and never pays for the body transfer.

Modules speak a small, versioned C ABI and can call back into the host: the
embedded (cluster-replicated) KV store and the `tracing` logger are one
function call away. That's what makes a cluster-wide rate limiter a ~100-line
module — the replicated counter is a single `kv_incr`.

Four modules ship in-tree: `jwt`, `cors`, `ratelimit`, and
`security-headers`.

## Linux release binaries: read this first

The stock Linux release binary — `cargo xtask release`, the
`docker/Dockerfile` image, and the published release artifacts — targets
`x86_64-unknown-linux-musl` with the C runtime **statically linked**. A
fully static musl binary cannot `dlopen()` anything, so configuring any
`[[middleware]]` mount makes startup fail fast with:

```
error: failed to load native middleware chain: failed to load middleware
"/mw/libephpm_middleware_security_headers.so" from ...:
dlopen failed for ...: Dynamic loading not supported
```

To run native middleware on Linux you need an ePHPm binary with the C
runtime **dynamically linked**. Build one by disabling `crt-static` (note
the exact spelling — `-crt-static`; the underscore form is silently
ignored by current rustc):

```bash
RUSTFLAGS="-C target-feature=-crt-static" cargo xtask release
```

This produces a dynamically-linked musl binary that loads middleware and
serves PHP normally (verified with all four in-tree modules on Alpine).
The trade-offs:

- It needs a musl dynamic loader (`/lib/ld-musl-x86_64.so.1`) and
  `libgcc_s.so.1` at runtime. On Alpine: `apk add libgcc` (the loader is
  already there). It is no longer the run-anywhere static binary — don't
  expect it to start on glibc-only distros.
- Development builds (`cargo build` on a glibc host) are dynamically
  linked already and load middleware without any of this.

macOS release binaries are dynamically linked against the system runtime
(`dlopen` is always available there), and Windows builds use
`LoadLibrary` — neither needs a special build. The static-musl limitation
is Linux-release-specific.

## Quick start (Linux, containerized)

With a `crt-static`-disabled binary in an Alpine image and the four
in-tree modules copied to `/usr/local/lib/ephpm/middleware/`:

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"

[[middleware]]
library = "ephpm_middleware_security_headers"
order   = 10
config  = { csp = "default-src 'self'" }

[[middleware]]
library = "ephpm_middleware_cors"
order   = 20
config  = { allow_origins = ["https://app.example"] }

[[middleware]]
library = "ephpm_middleware_jwt"
match   = "/api/*"
order   = 30
config  = { secret = "change-me", claims_header = "X-Jwt-Claims" }

[[middleware]]
library = "ephpm_middleware_ratelimit"
match   = "/api/*"
order   = 40
config  = { per_ip_rps = 1, burst = 2 }
```

Startup logs each module as it initialises, then the whole chain:

```
INFO ephpm_server::middleware: middleware initialised module=ephpm_middleware_security_headers path=/usr/local/lib/ephpm/middleware/libephpm_middleware_security_headers.so
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
library = "ephpm_middleware_security_headers"   # bare name → search path
order   = 10
config  = { csp = "default-src 'self'" }

[[middleware]]
library = "/etc/ephpm/middleware/libephpm_middleware_jwt.so"  # explicit path
match   = "/api/*"
order   = 30
config  = { secret = "...", claims_header = "X-Jwt-Claims" }
```

| Key | Required | Meaning |
|-----|----------|---------|
| `library` | yes | Module to load — bare name or explicit path (see below). Must not be empty. |
| `match` | no | Path glob; the mount only runs when the request path matches. `*` matches any character sequence, **including `/`**. Unset = every PHP-bound request. |
| `order` | yes | Chain position. Lower runs first; equal orders keep declaration order. |
| `config` | no | Arbitrary table, serialised to JSON and handed to the module's `init`. |

Mounts are **global** — they apply to every vhost. A module that needs
per-tenant behavior reads the request's vhost id (the server name) and
decides itself.

Loading is **fail-fast**: a library that can't be found, a missing ABI
symbol, or a module whose `init` returns an error aborts server startup with
a message naming the mount.

### Library resolution

A `library` value containing a path separator or a file extension is used
as-is. A bare name tries, in each search directory:

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
- **Failures are fail-closed.** A module whose `invoke` returns non-zero —
  including a Rust panic caught by the authoring kit — produces a plain 500,
  never a silent pass-through.
- **Request bodies are not visible** to middleware. The chain runs before
  the body is read (rejecting before the transfer is the point);
  the ABI's body accessor currently always returns length 0.

The chain runs on **PHP-bound requests only** — static file responses are
not affected.

## The shipped modules

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
client-sent header of that name is replaced, so PHP can trust it.

v1 is HS256 only — RS256/JWKS is not implemented.

### `ratelimit`

Fixed-window per-client rate limiting backed by the embedded KV store —
when KV replication is on, the limit is **cluster-wide**. Requests are
counted in 10-second windows; each window allows
`per_ip_rps × 10 + burst` requests per client. Over the limit: `429` with
`Retry-After` for the seconds left in the window.

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

## Writing your own module in Rust

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

`declare!` generates the four C ABI exports, the ABI major-version check,
config JSON parsing, response marshaling, and panic containment (a panicking
`invoke` becomes a fail-closed 500).

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

### Building modules for the Linux musl target

The module must match the host binary's libc. For the release (musl)
binary, build the module for `x86_64-unknown-linux-musl` **with
`crt-static` disabled** — this is required, not optional:

```bash
RUSTFLAGS="-C target-feature=-crt-static" \
cargo build --release --target x86_64-unknown-linux-musl -p my-auth
```

With the default (static) C runtime, rustc does not error — it prints
`warning: dropping unsupported crate type 'cdylib' for target
'x86_64-unknown-linux-musl'` and **produces no `.so` at all**.

Two more Linux notes, both observed on Ubuntu's `musl-tools`:

- The `musl-gcc` wrapper ships no dynamic `libgcc_s`, so linking fails
  with `cannot find libgcc_s.so.1`. Workaround:
  `ln -s /usr/lib/x86_64-linux-gnu/libgcc_s.so.1 /usr/lib/x86_64-linux-musl/`.
  (Building inside Alpine with its native toolchain avoids this.)
- The produced `.so` depends on `libgcc_s.so.1` and musl `libc.so` at
  runtime — on Alpine, `apk add libgcc`.

The artifact lands at
`target/x86_64-unknown-linux-musl/release/lib<crate_name>.so`; a bare
`library = "<crate_name>"` mount finds the `lib<name>.so` form through
the search path. The four in-tree modules build exactly the same way
(`-p ephpm-middleware-jwt -p ephpm-middleware-cors
-p ephpm-middleware-ratelimit -p ephpm-middleware-security-headers`).

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
```

- `abi_version` is `0x01_00_00_00` for v1; the **major byte** gates
  compatibility. Modules must refuse to init (return non-zero) when the
  host's major is newer than they were built for.
- `config_json` is the mount's `config` table serialised to JSON (NULL when
  the mount has no config).
- The host callback table is passed **by pointer at `init`** and is valid
  for the process lifetime — modules do not `dlsym` host symbols (that
  would need `-rdynamic` on Linux and has no clean Windows analogue). It
  contains request accessors (method, path, query, remote IP, header
  lookup, vhost id), the KV operations (`kv_get`/`kv_set`/`kv_set_nx`/
  `kv_incr`/`kv_free`) and `log`.
- The request pointer is only valid during `invoke`; never store it.
  Everything a module writes into `response_out` must stay valid until its
  `invoke` returns — the host copies before unwinding.
- New host capabilities append to the end of the table under the same major
  version.

The authoritative definition is
[`crates/ephpm-middleware/src/abi.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-middleware/src/abi.rs).

## Observability

Each module invocation increments
`ephpm_middleware_invocations_total{module, action}` where `action` is the
verdict (`continue` / `respond` / `rewrite`; module errors count as
`respond` since they fail closed as 500s). Module `log` calls surface
through the host's `tracing` subscriber under the `ephpm_middleware`
target.

## Trust model

Middleware runs **in-process with the same privileges as ePHPm itself**.
There is no sandbox: a buggy module can crash the server; a malicious one
owns it. Only load modules you built or trust — treat a `.so` mount like a
binary you're executing, because it is. (Rust-authored modules get panic
containment from `declare!`, but that is not a security boundary.)

## Not implemented (yet)

Planned — not yet implemented: request-body access from middleware, an
async `invoke` variant, hot reload of modules, per-vhost mounts, a WASM
loader for sandboxed modules, and the wider module catalog (basic-auth,
IP lists, webhook signatures, GeoIP, response cache, OpenTelemetry,
request-id). The design notes live in the git history of the roadmap page
this guide replaced.

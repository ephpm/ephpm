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

- **Built-in (static registry).** Five modules — `jwt`, `cors`,
  `ratelimit`, `security-headers`, `session-cookie` — are compiled into
  **every** ePHPm binary. `library = "jwt"` just works: no shared library
  on disk, no `dlopen`, no special build.
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
`security-headers`, `session-cookie`, and the `ephpm-middleware-*` /
`ephpm_middleware_*` long forms. Builtin mounts never touch the
filesystem.

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
- **Request bodies are not visible** to middleware. The chain runs before
  the body is read (rejecting before the transfer is the point);
  the ABI's body accessor currently always returns length 0.

**Coverage.** In **fpm mode** the chain runs on **PHP-dispatched requests
only**: static-file responses and router error responses (403/404) do **not**
pass through middleware. If you need a rule (rate limit, auth, security
headers) to cover static assets or error pages under fpm mode, enforce it in
front of ePHPm. In **worker mode** every request is routed through PHP, so the
chain sees everything — static, dynamic, and error paths alike.

## The built-in modules

All five are compiled into every ePHPm binary and run in-process — the
sections below apply identically whether you mount them by short name
(built-in) or dlopen their cdylib builds on a dynamic binary.

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

### `session-cookie`

Gates a whole site in a **browser** on a signed session cookie minted by an
external identity service, and redirects unauthenticated visitors to that
service. Same crypto as `jwt` (they share one verification core), different
threat model: a token in a cookie instead of a header, and a redirect
instead of a `401`, because a browser can do nothing useful with a `401`.

They are separate modules on purpose. Mounting `jwt` in front of an API and
`session-cookie` in front of a UI keeps each one's failure behaviour where
it belongs — no key in the `jwt` config can turn its `401`s into redirects,
and no key here can silently stop redirecting.

**ePHPm never calls the identity provider.** Verification is one HMAC over
bytes already in the request: no network call on the request path, no
provider rate limits, and no OAuth client secret on the serving nodes. The
nodes hold one shared HMAC secret; compromising a node lets an attacker mint
sessions for that deployment, and gets them nothing at the provider.

| key | default | meaning |
|-----|---------|---------|
| `secret` (string) | **required** | HS256 shared secret — the same key the login service signs with |
| `login_url` (string) | **required** | where to send unauthenticated browsers. Must be an `https://`/`http://` URL or a same-origin absolute path; anything else (`javascript:`, `//host`) fails startup |
| `cookie` (string) | `"ephpm_session"` | name of the cookie carrying the token |
| `return_to_param` (string) | unset — **no return-to is sent** | query parameter on `login_url` carrying the validated return path |
| `site_param` (string) | unset | query parameter carrying this request's vhost id, so one login service can serve many sites |
| `issuer` (string) | unset | required `iss` claim value |
| `audience` (string) | unset | required `aud` claim value (string or array member) |
| `claims_header` (string) | unset | forward the verified claims JSON to PHP in this request header |
| `require_https` (bool) | `true` | refuse to accept a session cookie on a cleartext request (loopback exempt) |

```toml
[[middleware]]
library = "session-cookie"
order   = 10
config  = {
  secret          = "shared-with-the-login-service",
  login_url       = "https://previews.example/auth/start",
  cookie          = "preview_session",
  return_to_param = "next",
  site_param      = "site",
  claims_header   = "X-Session-Claims",
}
```

What the module does per request:

- **No cookie, wrong cookie name, expired, tampered, wrong signing key, or
  `alg: none`** → a redirect to `login_url`, and **PHP never runs**. Every
  rejection produces the same response, so a client cannot learn *why* it
  was refused.
- **Valid cookie** → the request continues to PHP. With `claims_header` set,
  the module REWRITEs that request header to the verified claims JSON and
  PHP reads it from `$_SERVER['HTTP_X_SESSION_CLAIMS']`.

The redirect is `302` for `GET`/`HEAD` and `303` for every other method —
after an unauthenticated `POST`, `303` is what tells the browser to *GET*
the login page rather than re-submit the form body to the identity service.
It carries `Cache-Control: no-store` and `Vary: Cookie` so no shared cache
serves one visitor's redirect to another.

Verification is exactly `jwt`'s: signature checked **first** (constant-time
HMAC, so no unauthenticated JSON is ever parsed), `alg` pinned to HS256,
`exp` **required** and in the future, `nbf` honoured, `iss`/`aud` enforced
when configured. **A session token with no `exp` is rejected** — a session
that cannot expire is a configuration bug, not a session.

#### Return-to, and why it is not `?next=`

With `return_to_param`, the redirect tells the login service where to send
the visitor afterwards. That value is taken from **the path the browser
actually requested**, never from a client-supplied parameter — an
unvalidated `?next=` is an open redirect and a ready-made phishing
primitive, so this module does not read one.

The request line is still client-controlled, so the target is validated
anyway before it is emitted. It must be an absolute path and nothing else:

- starts with a single `/` — `//evil.example` and `///evil.example` are
  protocol-relative URLs, not paths, and are rejected;
- no `\` anywhere — browsers normalise `\` to `/`, so `/\evil.example`
  behaves exactly like `//evil.example`;
- no ASCII control character, space, `DEL`, `#`, or non-ASCII byte
  (anything that could split a header or truncate a URL);
- at most 2048 bytes.

Anything that fails is **dropped**, not sanitised: the visitor still reaches
the login page, just without a return-to. The value that does survive is
percent-encoded aggressively (everything outside `A-Za-z0-9-._~`), so it
cannot introduce a parameter, a fragment, or a second URL into the login
URL's query.

This is one half of the defence. **The login service must validate the
return-to it receives as well** — it is the component that actually issues
the final `Location`, and it must not follow one off its own origin.

#### HTTPS

A session cookie is a bearer credential: anyone who reads it is the user. On
a cleartext connection it is readable in transit, so with the default
`require_https = true` the module refuses such a request outright with a
`403` rather than accepting the cookie.

- The verdict is **the host's**, not a header the module read. ePHPm uses
  the accepted connection's TLS state, overridden by `X-Forwarded-Proto`
  only for peers matching `[server.proxy] trusted_proxies` — the same value
  PHP sees as `$_SERVER['HTTPS']`. A client-set `X-Forwarded-Proto: https`
  from an untrusted peer does **not** satisfy this. If ePHPm sits behind a
  TLS-terminating proxy, that proxy's address must be in `trusted_proxies`
  or every request will look like cleartext.
- **Loopback clients are exempt**, so `http://127.0.0.1:8080` works for
  local development. This mirrors the web platform's own treatment of
  `localhost` as a secure context, and it means you never have to ship a
  `require_https = false` you meant only for your laptop.
- It is a hard `403`, not a redirect: bouncing to an HTTPS login service
  that sets a cookie the browser then returns over HTTP would loop forever,
  and a loop is a worse diagnostic than a message.
- What the module **cannot** do is make the cookie itself secure. Only the
  issuer can, with the cookie's own attributes — see below.

#### The other half: what the login service must do

ePHPm validates; something else must issue. A login service ("switchboard"
in the preview-hosting case) has to:

1. **Authenticate the visitor** however it likes — OAuth, SSO, a magic link
   — and decide whether they may see this site. It knows which site to
   decide about from the `site_param` value on the redirect. This is where
   all provider-specific work lives, and it happens **once per session**,
   not once per request.
2. **Mint an HS256 JWT** signed with the same `secret`, with an `exp` in the
   future (required) and whatever `sub`/`iss`/`aud`/custom claims the app
   needs. Keep it short-lived: this module has no revocation list, so
   `exp` is the only thing that ends a session early.
3. **Set it as a cookie** on the site's domain, under the name in `cookie`,
   with `Secure` (so the browser never sends it over cleartext),
   `HttpOnly` (so page JavaScript cannot read it), a `SameSite` value the
   flow tolerates, and a `Path`/`Domain` no broader than necessary. These
   attributes are the issuer's job — ePHPm never sets the cookie and cannot
   add them after the fact.
4. **Validate the return-to** it was handed and redirect the visitor back
   to it, refusing anything that is not a path on the site it is returning
   the user to.

Both halves must be configured with the same secret and the same cookie
name, and the login service must be able to set cookies on the site's
domain (typically a parent domain of the preview hostnames).

#### Claims forwarding

With `claims_header = "X-Session-Claims"`, PHP reads the verified claims
from `$_SERVER['HTTP_X_SESSION_CLAIMS']` without re-verifying. As with
`jwt`'s `claims_header`, any client-sent header of that name is **stripped
at ingest** — before the chain runs and before any header crosses to PHP —
so a request on a path this mount's `match` glob skips can never smuggle a
forged claims value through.

#### Limits worth knowing

- **HS256 only**, like `jwt` — no RS256/JWKS, so the signing key is a shared
  secret and every serving node can mint tokens as well as verify them.
- **No revocation.** A token is valid until its `exp`. Rotating `secret`
  invalidates every outstanding session at once, which is the only
  bulk revocation available.
- **Coverage follows the chain's** (see below): in fpm mode the gate runs on
  PHP-dispatched requests only, so static assets under the document root are
  **not** gated. Gate a site whose static files are also confidential with
  worker mode, or in front of ePHPm.

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

Inside `invoke`, `req` exposes the request (`method()`, `path()`,
`query()`, `remote_ip()`, `vhost_id()`, `header()`, and `is_https()` — the
host's trusted-proxy-aware secure-transport verdict), and `req.host()`
exposes host services:

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

### Building modules on Linux

The module must match the host binary's libc. The release binary is
glibc-dynamic (gnu target), so a plain release build on any mainstream
distro produces a compatible `.so`:

```bash
cargo build --release -p my-auth
```

The artifact lands at `target/release/lib<crate_name>.so`; a bare
`library = "<crate_name>"` mount finds the `lib<name>.so` form through
the search path. The four in-tree modules build exactly the same way
(`-p ephpm-middleware-jwt -p ephpm-middleware-cors
-p ephpm-middleware-ratelimit -p ephpm-middleware-security-headers
-p ephpm-middleware-session-cookie`).

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
  lookup, vhost id, secure-transport flag), the KV operations
  (`kv_get`/`kv_set`/`kv_set_nx`/`kv_incr`/`kv_incr_ttl`/`kv_free`) and
  `log`.
- Repeated request-header lines are pre-joined before a module sees them:
  `", "` for ordinary headers, `"; "` for `Cookie` (HTTP/2 is allowed to
  split it across field lines).
- `request_is_https` is the **host's** trusted-proxy-aware verdict, the
  same value PHP sees as `$_SERVER['HTTPS']`. Use it instead of reading
  `X-Forwarded-Proto`, which reaches modules exactly as the client sent it.
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
containment from `declare!`, but that is not a security boundary — a
*memory* fault is not a panic and is not contained.)

When a module does fault, ePHPm writes a fatal-signal report to stderr
naming the faulting `.so` and function before it dies — see
[Diagnosing Crashes](/guides/diagnosing-crashes/).

## Not implemented (yet)

Planned — not yet implemented: request-body access from middleware, an
async `invoke` variant, hot reload of modules, per-vhost mounts, a WASM
loader for sandboxed modules, and the wider module catalog (basic-auth,
IP lists, webhook signatures, GeoIP, response cache, OpenTelemetry,
request-id). The design notes live in the git history of the roadmap page
this guide replaced.

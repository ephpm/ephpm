+++
title = "Built-in Middleware"
type = "docs"
weight = 6
+++

The twelve middleware modules compiled into every ePHPm binary, on one page.
Mount any of them with `[[middleware]] library = "<name>"` — no shared library
on disk, no `dlopen`, works in a fully static build. How mounts, ordering,
verdicts and the response phase work is in the
[Native Middleware guide](/guides/native-middleware/); each module's full
behaviour is in its section there.

The source of truth is the static registry in
[`crates/ephpm-server/src/middleware.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-server/src/middleware.rs)
(`fn builtin`) and each module's `CONFIG_KEYS` in
[`crates/ephpm-middleware-builtins/src/`](https://github.com/ephpm/ephpm/tree/main/crates/ephpm-middleware-builtins/src).

## Catalog

| Module | Phase | On internal failure | `config` keys (anything else fails startup) |
|---|---|---|---|
| [`security-headers`](/guides/native-middleware/#security-headers) | request | — | `hsts`, `csp`, `frame_options`, `content_type_options`, `referrer_policy` |
| [`cors`](/guides/native-middleware/#cors) | request | — | `allow_origins`, `allow_methods`, `allow_headers`, `allow_credentials`, `max_age` |
| [`jwt`](/guides/native-middleware/#jwt) | request | closed (`401`) | `secret`, `issuer`, `audience`, `header`, `claims_header` |
| [`session-cookie`](/guides/native-middleware/#session-cookie) | request | closed (redirect / `403`) | `secret`, `login_url`, `cookie`, `return_to_param`, `site_param`, `issuer`, `audience`, `claims_header`, `require_https`, `require_site` |
| [`preview-gate`](/guides/native-middleware/#preview-gate) | request | closed (redirect) | `secret`, `login_url`, `cookie`, `exempt_paths`, `share_param`, `share_epoch`, `share_revocation`, `return_to_param`, `site_param`, `issuer`, `audience`, `require_https`, `require_site` |
| [`ratelimit`](ratelimit/) | request | **open** (KV unavailable → allowed, `WARN` logged) | `per_ip_rps`, `burst`, `key_headers` |
| [`api-key`](/guides/native-middleware/#api-key) | request | closed (`401`) | `header`, `query_param`, `keys`, `kv_key_template`, `consumer_header` |
| [`ip-allowlist`](/guides/native-middleware/#ip-allowlist) | request | closed (`403`; unparseable IP denied unless `default = "allow"`) | `allow`, `deny`, `default` |
| [`maintenance-mode`](/guides/native-middleware/#maintenance-mode) | request | **open** (KV error → serve normally) | `key_template`, `retry_after`, `body`, `content_type`, `bypass_ips`, `bypass_paths` |
| [`redirect`](/guides/native-middleware/#redirect) | request | — | `force_https`, `canonical_host`, `host_map`, `trailing_slash`, `status`, `forwarded_proto_header` |
| [`request-id`](/guides/native-middleware/#request-id) | request **+ response** | — | `header`, `trust_inbound` |
| [`header-transform`](/guides/native-middleware/#header-transform) | request **+ response** | — | `request`, `response` (each a table: `request.set`; `response.set`, `response.remove`) |

A panic inside any module's request phase is caught by the host and answered
with a plain `500` — never a pass-through. A response-phase failure leaves the
response unchanged.

Free-form maps whose keys are your data are never key-checked: `api-key`'s
`keys`, `redirect`'s `host_map`, and `header-transform`'s `set` tables.

## Accepted `library` spellings

Each module answers to its short name and to its crate name, with `-` and `_`
interchangeable: `jwt`, `ephpm-middleware-jwt` and `ephpm_middleware_jwt` all
select the same builtin. `ratelimit` additionally answers to `rate-limit`
(`rate_limit`). The registry is consulted **before** the shared-library search
path, so no file on disk can shadow a builtin name.

## Secrets are literal

`jwt`, `session-cookie` and `preview-gate` take `secret` as the HMAC key byte
for byte. **`env:NAME` and `file:/path` are not expanded** — `secret =
"env:X"` makes the key the string `env:X` — and there is no minimum length.
The per-site `[preview_auth] session_secret` override is the one ePHPm-side
place that resolves `env:`/`file:` and enforces ≥ 32 bytes. Details:
[Secrets in `config` are literal](/guides/native-middleware/#secrets-in-config-are-literal).

## Client IP

Modules that act on the client address — `ratelimit` (when no `key_headers`
header is present), `ip-allowlist`, `maintenance-mode`'s `bypass_ips` — all
see the same value: the TCP peer address, unless that peer is listed in
`[server.security] trusted_proxies`, in which case it is the rightmost
`X-Forwarded-For` entry that is not itself a trusted proxy. No builtin parses
forwarding headers itself, and `Forwarded` / `X-Real-IP` are not consulted.
Behind a load balancer, set `trusted_proxies` to the balancer's addresses or
every client appears as the balancer — see
[ratelimit](ratelimit/#behind-a-load-balancer-or-reverse-proxy).

## Tenancy

Every module that touches KV (`ratelimit`, `api-key`'s `kv_key_template`,
`maintenance-mode`, `preview-gate`'s share revocation) reads and writes the
**request's own tenant keyspace** on a multi-tenant node, and the process-global
store on a single-site node or for a request whose `Host` matched no vhost. See
[KV scope](/guides/native-middleware/#kv-scope-per-tenant-by-default).

## Startup log lines

| Level | When | Message (abridged) |
|---|---|---|
| `INFO` | each builtin mount initialised | `middleware initialised (builtin) module=<library> …` |
| `INFO` | each dynamic (`dlopen`ed) mount initialised | `middleware initialised module=<library> … config_keys=declared` — or `config_keys=undeclared` when the module exports no key list, so ePHPm could not check its `config` |
| `INFO` | chain built | `middleware chain loaded count=… modules=[…]` |
| `INFO` | any `php:` mount configured | `PHP middleware mounted (EXPERIMENTAL) …` |
| `WARN` | the ratelimit module (any spelling of its `library`), with `[cluster].enabled = true` | `[middleware] ratelimit mounted with [cluster].enabled = true — …` — see [ratelimit](ratelimit/#the-cluster-startup-warning) |
| error (startup aborts) | unknown `config` key or bad value in a builtin mount | `… builtin middleware "<library>" init failed: …` |
| error (startup aborts) | a non-builtin `library` that cannot be found | `failed to load native middleware chain: middleware library "<name>" not found; tried: …` |
| error (startup aborts) | unknown `config` key in a dynamic mount whose module declares its keys | `… middleware "<library>": unknown config key … (did you mean …?)` |
| error (startup aborts) | a dynamic module whose `ephpm_middleware_config_keys` answer is not a JSON array of strings | `… middleware "<library>" exports ephpm_middleware_config_keys but its answer is unusable: …` |

## ABI version

Dynamic (`dlopen`ed) modules speak C ABI major **1**, minor **5**
(`0x01_00_00_05`). Minor 5 added `node_is_multi_tenant` to the host table, so a
module can tell an untenanted node from an unmatched host. The optional
module-exported `ephpm_middleware_config_keys` symbol shipped in the same
release; it lets the host check a module's `config` keys. Modules built against
any earlier minor keep loading. See
[The C ABI](/guides/native-middleware/#the-c-abi-for-non-rust-modules).

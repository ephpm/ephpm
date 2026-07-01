# HTTP Server Architecture

This document covers the HTTP server design, PHP execution model, request lifecycle, and configuration system.

---

## Request Lifecycle

```
   Client request
        │
        ▼
   TCP accept (tokio async)
        │
        ▼
   HTTP parse (hyper — HTTP/1.1; HTTP/2 over TLS)
        │
        ▼
   Router::handle (wrapped by request timeout)
        │
        ▼
   Security checks
     · hidden files · blocked paths · trusted proxy
        │
        ▼
   Fallback resolution
     $uri → $uri/ → rewrite or status
        │
        ▼
   Resolves to:
        │
        ├──── PHP file ─────► Allowlist check ──┐
        │                     Body size check    │
        │                     PHP execute        │
        │                     Gzip               │
        │                                        │
        ├──── Static file ──► Path traversal ───┤
        │                     MIME detect       │
        │                     Cache-Control     │
        │                     Gzip              │
        │                                       │
        └──── Status code ──► 404 / 403 / etc.──┤
                                                ▼
                                            Response
```

## PHP Execution Model

### Per-Request Lifecycle

Each HTTP request runs a full `php_request_shutdown()` / `php_request_startup()` cycle (`ephpm_execute_request()` in `ephpm_wrapper.c`) — the classic php-fpm isolation model. Request shutdown destroys user functions, classes, constants, statics, and the global symbol table, so nothing leaks between requests. OPcache's compiled bytecode lives in shared memory and survives the cycle, so the opcode cache (and JIT buffer) are preserved.

Per-request sequence:
1. Tear down the previous request (`php_request_shutdown()`); reset the thread-local output/header buffers and the POST read cursor
2. Populate `SG(request_info)` (method, URI, query string, content type, cookies, content length) **before** startup — the SAPI callbacks read these fields during startup
3. Set a non-NULL `SG(server_context)` sentinel — `sapi_activate()` only parses the POST body into `$_POST` when the server context is non-NULL (the same gate cli-server/cgi use)
4. `php_request_startup()` builds all superglobals natively through the SAPI callbacks
5. Reset response state (`http_response_code = 200`, `headers_sent`, `no_headers`) so an explicit status from a prior request on the same worker thread can't leak
6. Replay buffered per-request INI overrides (e.g. per-vhost `open_basedir`) at `PHP_INI_STAGE_ACTIVATE` — they're buffered because applying them before startup would be undone by the shutdown/startup cycle
7. Execute the script under bailout protection

An earlier design reused one long-lived embed request and manually rebuilt the superglobals (destroying `PG(http_globals)` and re-running `sapi_module.treat_data`). That manual rebuild was removed: once the per-request lifecycle called `php_request_startup()` every request, it destroyed arrays startup had just created and caused a use-after-free SIGSEGV under load on tokio `spawn_blocking` threads. Superglobal construction is now owned entirely by `php_request_startup()`.

### Superglobal Population

All superglobals are built natively by PHP request startup, driven by the installed SAPI callbacks:

| Variable | Source |
|----------|--------|
| `$_SERVER` | `register_server_variables` callback — values provided by Rust (`PhpRequest::server_variables()`) |
| `$_GET` | Parsed by PHP from `SG(request_info).query_string` during request startup |
| `$_POST` / `$_FILES` | Parsed natively by `sapi_activate()` (including multipart/rfc1867), fed by the `read_post` callback |
| `$_COOKIE` | `read_cookies` callback returns the raw `Cookie` header; PHP parses it |
| `$_REQUEST` | Built by PHP from `$_GET` + `$_POST` + `$_COOKIE` per `request_order` |

### `$_SERVER` Variables

Key distinction after fallback rewrites (e.g. `/blog/hello` -> `/index.php`):

| Variable | Value | Description |
|----------|-------|-------------|
| `REQUEST_URI` | `/blog/hello` | Original URI from client |
| `SCRIPT_NAME` | `/index.php` | Resolved script (relative to docroot) |
| `SCRIPT_FILENAME` | `/var/www/html/index.php` | Absolute path to script |
| `PHP_SELF` | `/index.php` | Same as `SCRIPT_NAME` |
| `DOCUMENT_ROOT` | `/var/www/html` | Document root |
| `QUERY_STRING` | `preview=true` | Without leading `?` |
| `GATEWAY_INTERFACE` | `CGI/1.1` | Required by many PHP apps |
| `REDIRECT_STATUS` | `200` | Required by some PHP apps |

HTTP headers are mapped to `HTTP_*` variables, except `Content-Type` -> `CONTENT_TYPE` and `Content-Length` -> `CONTENT_LENGTH` (no `HTTP_` prefix per CGI spec).

### Thread Safety

PHP is compiled with ZTS (Zend Thread Safety). Each `spawn_blocking` thread auto-registers with TSRM on first use, getting its own isolated PHP context. Multiple PHP requests execute concurrently. The `Mutex<Option<PhpRuntime>>` only protects one-time init/shutdown, not request execution. Windows builds use NTS with serialized execution via mutex.

### Signal Handling

PHP installs a `SIGPROF` handler for `max_execution_time`. This signal is process-wide and would crash tokio worker threads (NULL dereference in PHP's handler on non-PHP threads). On Linux we override PHP's signal functions with no-ops via GNU ld's `--wrap` linker flags. macOS's ld64 and MSVC's link.exe don't support `--wrap` (see `crates/ephpm/build.rs`), so the wrapping is not applied there. On all platforms, request timeout enforcement is done at the HTTP layer, not by PHP's signal-based timer.

### Bailout Protection

PHP uses `setjmp`/`longjmp` for error handling. All PHP calls go through `ephpm_wrapper.c` which wraps execution in `zend_try`/`zend_catch`. PHP 8.x `exit()`/`die()` throws an unwind exit exception, which we detect and treat as a normal response (with captured output).

### PHP Fatal → HTTP 500

A PHP fatal error must surface as an HTTP 500 even though the embed SAPI gives us several different ways for one to happen. `ephpm_execute_request()` in `ephpm_wrapper.c` covers two distinct detection paths:

1. **`zend_bailout()` longjmp.** Out-of-memory, max-execution-time, and the older fatal-class errors call `zend_bailout()`, which longjmps out of `php_execute_script`. The `SETJMP(__bailout) == 0` guard in the wrapper catches that case and sets `fatal_bailout = 1`.

2. **PHP 8.x uncaught `Throwable`.** When a script throws and nothing catches it, `zend_exception_error()` formats the message via `zend_error_va(... | E_DONT_BAIL ...)` and lets `php_execute_script` return normally. `SETJMP` sees nothing — no longjmp ever happens. To catch this path the wrapper also checks `PG(last_error_type)` against a fatal-class mask (`E_ERROR | E_CORE_ERROR | E_COMPILE_ERROR | E_USER_ERROR | E_RECOVERABLE_ERROR | E_PARSE`). Without that second check, "Fatal error: Uncaught Error: Call to undefined function …" comes back as `200 OK`.

`PG(last_error_type)` is reset to `0` before each request so a fatal from a previous reuse of the embed request can't leak into the next one.

Once a fatal has been detected by either path, the wrapper only overrides the status when it is still the default `200`. Anything the script set explicitly via `http_response_code()` or `exit($status)` is preserved — the contract is "200 → 500 on fatal", not "always 500 on fatal".

## SAPI Callbacks

| Callback | Purpose |
|----------|---------|
| `ub_write` | Captures PHP output into a growable buffer |
| `read_post` | Feeds POST body from Rust to PHP |
| `read_cookies` | Returns raw Cookie header string |
| `register_server_variables` | Populates `$_SERVER` from Rust-provided key/value pairs |
| `send_headers` | No-op (headers captured separately after execution) |
| `log_message` | Routes PHP errors to stderr |

### Response Header Capture

After script execution, headers are read from `SG(sapi_headers).headers`. If no explicit `Content-Type` was set by PHP (e.g. `phpinfo()` relies on the default), we synthesize one from `SG(sapi_headers).mimetype` or PHP's `default_mimetype`/`default_charset` settings.

## Server Setup

### Initialization Sequence

PHP must be initialized **before** the tokio runtime to avoid signal conflicts:

```
1. Parse CLI args + load config        (single-threaded)
2. Init tracing with configured level  (single-threaded)
3. Init PHP runtime                    (single-threaded)
   - php_embed_init()
   - ephpm_install_sapi()
   - ephpm_apply_ini_settings()
   - ephpm_finalize_for_http()
4. Create tokio runtime                (spawns worker threads)
5. Run HTTP server
6. Shutdown PHP runtime
```

### Hyper Connection Settings

| Setting | Source | Purpose |
|---------|--------|---------|
| `keep_alive(true)` | hardcoded | HTTP/1.1 persistent connections |
| `header_read_timeout` | `server.timeouts.header_read` | Slow client header protection |
| `max_buf_size` | `server.request.max_header_size` | Header size limit |
| Timer | `TokioTimer` | Required for timeout functionality |

## Static File Serving

- MIME type detection via `mime_guess` (file extension based)
- Path traversal protection via `canonicalize()` + prefix check
- Gzip compression for compressible content types above minimum size
- `Cache-Control` header when configured
- `ETag` generation (weak, hash-based) + `If-None-Match` → 304 Not Modified

### Percent-Decoding of URI Paths

hyper hands the router the raw URI path, so `/test%2Ehtml` would otherwise be looked up as the literal name `test%2Ehtml`. Before any routing or filesystem lookup happens, the request path is run through `percent_decode_path()` (in `crates/ephpm-server/src/router.rs`) so `%XX` escapes resolve to their bytes — `/test%2Ehtml` becomes `/test.html` and matches the file on disk.

The decoder is deliberately strict:

| Input | Result |
|-------|--------|
| `%XX` with valid hex | decoded to the byte |
| Truncated `%`, `%X` (one digit) | 400 Bad Request |
| Non-hex digits (`%ZZ`, `%G1`) | 400 Bad Request |
| Encoded slash (`%2F`) or backslash (`%5C`) | 400 Bad Request |
| Decoded byte stream not valid UTF-8 | 400 Bad Request |

Rejecting `%2F` / `%5C` is what keeps percent encoding from being used to sneak past path-traversal protection or prefix-based blocks like `/vendor/*` — a request such as `/vendor%2Fconfig.php` cannot decode into a `/`-containing path that bypasses the glob check. UTF-8 validation on the decoded bytes lets non-ASCII paths work normally while still rejecting malformed escape sequences.

## PHP Response Cache

The static file `ETag` support only covers non-PHP assets. PHP frameworks (WordPress, Laravel) generate their own `ETag` headers for dynamic content, but without help every request still hits PHP to compute whether the content changed.

**Implemented:** the ETag-based 304 short-circuit. Configured via `[server.php_etag_cache]` (enabled by default; see `Router::handle` in `crates/ephpm-server/src/router.rs`). When PHP sets an `ETag` on a GET/HEAD response, the server stores it in the KV store keyed by method + path + query string. A repeat request with a matching `If-None-Match` returns `304 Not Modified` without executing PHP at all. In clustered mode the KV entries replicate via gossip, so the short-circuit works across nodes.

**Future work:** full-response caching (serving the cached body on requests without `If-None-Match`), which would turn ePHPm into an edge cache — see the design decisions below.

### Flow (as implemented)

```
1. First request: /blog/hello
   → PHP executes, returns response with ETag: "abc123"
   → Server stores in KV: <key_prefix><method>:<path>?<query> → "abc123" (with TTL)
   → Response sent to client

2. Repeat request: /blog/hello + If-None-Match: "abc123"
   → Server checks KV for the stored ETag
   → ETag matches → return 304 Not Modified immediately
   → No PHP execution

3. Works across all nodes via gossip replication
```

### Design Decisions

| Decision | Options | Notes |
|----------|---------|-------|
| **Cache key** | URL alone vs URL + vary headers (cookies, auth) | Must not serve cached authenticated pages to anonymous users. WordPress sets different cookies for logged-in users — key should include a cookie-based cache group or skip caching entirely when auth cookies are present. |
| **Invalidation** | TTL, purge header, PHP hook | TTL is simplest. A `X-Ephpm-Cache-Purge` response header from PHP could signal immediate invalidation. For WordPress, a must-use plugin could call a purge endpoint on content updates. |
| **Storage scope** | ETag-only (304s) vs full response (edge cache) | ETag-only saves KV space but still requires PHP on cache miss. Full response storage turns ephpm into an edge cache — much bigger win but needs memory/eviction policy. Start with full response. |
| **Cache bypass** | `Cache-Control: no-cache`, `no-store`, `private` | Respect standard HTTP cache directives from PHP. Never cache responses with `Set-Cookie` or `private`. |

### Impact

This is a significant performance multiplier for PHP applications. Most WordPress page views are anonymous and return identical content. Skipping PHP entirely for repeat visitors frees the `spawn_blocking` worker threads and lets the async HTTP server handle cached responses at full throughput across all nodes.

## TLS

Manual TLS via `rustls` (pure Rust, no OpenSSL dependency). Certificate and key loaded from PEM files at startup.

### Modes

| Config | Behavior |
|--------|----------|
| No `[server.tls]` | Plain HTTP on `server.listen` (default) |
| `tls.cert` + `tls.key` only | HTTPS on `server.listen`, no HTTP listener |
| `tls.cert` + `tls.key` + `tls.listen` | HTTPS on `tls.listen`, HTTP on `server.listen` |
| + `tls.redirect_http = true` | HTTP listener sends 301 redirects to HTTPS |

### Connection Flow (TLS)

```
TCP Accept → TLS Handshake (tokio-rustls) → HTTP/1.1 or HTTP/2 → Router
                  ↓ timeout
         header_read_timeout
```

- TLS handshake timeout reuses `server.timeouts.header_read` (default 30s)
- ALPN advertises `h2` and `http/1.1` (h2 preferred — see `crates/ephpm-server/src/tls.rs`); hyper-util's `auto::Builder` serves whichever protocol was negotiated. Plain-TCP listeners are HTTP/1.1 only.
- `is_tls` flag propagated to router so `$_SERVER['HTTPS']` is set correctly
- When behind a trusted proxy, `X-Forwarded-Proto` takes precedence over native TLS status

### Automatic TLS (ACME)

Zero-config HTTPS via Let's Encrypt, like Caddy. Uses `rustls-acme` crate with TLS-ALPN-01 challenge (works on port 443 alone, no port 80 needed).

**Single-node** (implemented): `DirCache` stores certs on the filesystem. On startup, requests a cert from Let's Encrypt (~5-30s), then hot-swaps on renewal. No restarts needed. Uses `LazyConfigAcceptor` to inspect each TLS `ClientHello` — ACME challenges are handled inline, normal connections pass through to hyper.

**Renewal timing**: `rustls-acme` renews at 2/3 of remaining certificate validity (~30 days before expiry for standard 90-day Let's Encrypt certs). This is hardcoded in the library — there is no API to configure it. If we need customizable renewal timing in the future (e.g., for shorter-lived certs or different CAs), options are: contribute the feature upstream to `rustls-acme`, or switch to `instant-acme` which gives full control over the ACME flow at the cost of managing renewal scheduling ourselves.

```toml
[server.tls]
domains = ["example.com", "www.example.com"]
email = "admin@example.com"
cache_dir = "/var/lib/ephpm/certs"
# staging = true  # use for testing to avoid rate limits
```

**Clustered** (Phase 2 — requires KV store and gossip): In a multi-node deployment, naive ACME creates several problems that the clustered KV store solves:

| Problem | What happens | Solution |
|---------|-------------|----------|
| **Renewal stampede** | N nodes all try to renew simultaneously, hitting Let's Encrypt rate limits (50 certs/domain/week) | Distributed lock via KV (`acme:lock:<domain>` key with TTL). One node wins, renews, others wait. |
| **Challenge routing** | Let's Encrypt connects to the domain, DNS round-robins to any node, but only the initiating node has the challenge token | Share challenge tokens via KV (`acme:challenge:<token>` keys). Any node can respond. |
| **Cert distribution** | After one node obtains the cert, all nodes need it immediately | Store cert in KV (`certs:<domain>` key), replicate via gossip. All nodes pick it up. |
| **Leader election** | Only one node should drive renewals to avoid redundant work | KV-based leader (`acme:leader` key with TTL heartbeat). Leader renews, followers watch. |

The `rustls-acme` crate has a pluggable `Cache` trait — swap `DirCache` for a `KvCache` implementation when clustering is built. Zero changes to the ACME logic itself.

```
Phase 1 (single-node):  AcmeConfig → DirCache (filesystem)
Phase 2 (clustered):    AcmeConfig → KvCache (gossip-replicated KV store)
```

## Compression

Applied to both PHP and static responses based on the client's `Accept-Encoding` header. Brotli (`br`) is implemented and preferred when the client accepts it (better ratio); gzip is the fallback for clients that only accept `gzip` (see `build_php_response` in `crates/ephpm-server/src/router.rs`).

| Check | Condition |
|-------|-----------|
| Enabled | `server.response.compression = true` |
| Algorithm | Brotli if the client accepts `br`, else gzip if it accepts `gzip`, else identity |
| Min size | Response body >= `server.response.compression_min_size` |
| Content type | `text/*`, `*javascript`, `*json`, `*xml`, `*svg` |
| Smaller | Compressed size < original size |

Level controlled by `server.response.compression_level` (1=fast, 9=best).

## Security Layers

Evaluated in order for every request:

1. **Hidden files** — Paths with dot-prefixed segments (`.env`, `.git`, `.htaccess`) are blocked based on `server.static.hidden_files` (`deny`=403, `ignore`=404, `allow`=pass).

2. **Blocked paths** — URI matched against `server.security.blocked_paths` glob patterns. Any match returns 403. Supports `*` wildcards (`/vendor/*`, `/wp-config.php`).

3. **PHP allowlist** — When `server.security.allowed_php_paths` is non-empty, only matching PHP files execute. Others get 403. Prevents arbitrary PHP execution in upload directories.

4. **Body size limit** — `Content-Length` checked against `server.request.max_body_size` before reading the body. Returns 413.

5. **Path traversal** — Static file paths canonicalized and verified within document root.

### Trusted Proxy Resolution

When `server.security.trusted_proxies` contains CIDR ranges and the connecting IP matches:
- `X-Forwarded-For` is parsed right-to-left, returning the first untrusted IP as `REMOTE_ADDR`
- `X-Forwarded-Proto: https` sets `$_SERVER['HTTPS'] = 'on'`

## Fallback Resolution

Nginx-style `try_files` implemented as a configurable `fallback` chain:

```toml
fallback = ["$uri", "$uri/", "/index.php?$query_string"]
```

- Variables: `$uri` (request path), `$query_string` (raw query string)
- Entries ending with `/` check for directory + index files
- Last entry is the fallback: either a rewrite target or `=NNN` status code
- For static-only sites: `["$uri", "$uri/", "=404"]`

---

## Configuration Reference

### Implemented

| Config | Type | Default | Description |
|--------|------|---------|-------------|
| `server.listen` | string | `"0.0.0.0:8080"` | Bind address |
| `server.document_root` | path | `"."` | Document root directory |
| `server.index_files` | string[] | `["index.php", "index.html"]` | Index file names |
| `server.fallback` | string[] | `["$uri", "$uri/", "/index.php?$query_string"]` | URL resolution chain |
| `server.request.max_body_size` | int | `10485760` (10 MiB) | Max request body (0=unlimited) |
| `server.request.max_header_size` | int | `8192` (8 KiB) | Max header buffer size |
| `server.timeouts.header_read` | int | `30` | Seconds to receive headers |
| `server.timeouts.idle` | int | `60` | Idle connection timeout (seconds) |
| `server.timeouts.request` | int | `300` | Total request timeout (seconds) |
| `server.timeouts.shutdown` | int | `30` | Graceful shutdown drain: grace period for in-flight connections on SIGTERM |
| `server.response.compression` | bool | `true` | Enable response compression (Brotli preferred, gzip fallback) |
| `server.response.compression_level` | int | `1` | Compression level (1-9) |
| `server.response.compression_min_size` | int | `1024` | Min bytes to compress |
| `server.response.headers` | [string, string][] | `[]` | Custom response headers (CORS, CSP, HSTS) |
| `server.static.cache_control` | string | `""` | Cache-Control header for static files |
| `server.static.hidden_files` | string | `"deny"` | Dotfile handling: deny, ignore, allow |
| `server.static.etag` | bool | `true` | `ETag` headers + 304 Not Modified support |
| `server.request.trusted_hosts` | string[] | `[]` | Host header validation (421 if no match) |
| `server.security.trusted_proxies` | string[] | `[]` | CIDR ranges for proxy trust |
| `server.security.blocked_paths` | string[] | `[]` | Glob patterns to block (403) |
| `server.security.allowed_php_paths` | string[] | `[]` | PHP execution allowlist |
| `server.logging.level` | string | `"info"` | Log level (trace/debug/info/warn/error) |
| `server.logging.access` | string | `""` | Access log file path |
| `server.tls.cert` | path | — | PEM certificate chain file (enables HTTPS) |
| `server.tls.key` | path | — | PEM private key file |
| `server.tls.listen` | string | — | Separate HTTPS listen address |
| `server.tls.redirect_http` | bool | `false` | 301 redirect HTTP to HTTPS |
| `server.tls.domains` | string[] | `[]` | Domain names for ACME auto-TLS |
| `server.tls.email` | string | — | Contact email for ACME registration |
| `server.tls.cache_dir` | path | `"certs"` | ACME certificate cache directory |
| `server.tls.staging` | bool | `false` | Use Let's Encrypt staging environment |
| `php.max_execution_time` | int | `30` | PHP per-request timeout (seconds) |
| `php.memory_limit` | string | `"128M"` | PHP memory limit |
| `php.ini_overrides` | [string, string][] | `[]` | INI directive overrides |
| `server.metrics.enabled` | bool | `false` | Prometheus metrics endpoint |
| `server.metrics.path` | string | `"/metrics"` | Metrics endpoint path |
| `server.limits.max_connections` | int | `0` | Max total concurrent connections (0 = unlimited) |
| `server.limits.per_ip_max_connections` | int | `0` | Max concurrent connections per client IP |
| `server.limits.per_ip_rate` | float | `0.0` | Max requests/second per client IP (token bucket) |
| `server.limits.per_ip_burst` | int | `50` | Burst size for per-IP rate limiting |

### CLI Flags

| Flag | Scope | Description |
|------|-------|-------------|
| `-c, --config` | serve | Config file path (default: `ephpm.toml`) |
| `-l, --listen` | serve | Listen address (overrides config) |
| `-d, --document-root` | serve | Document root (overrides config) |
| `-v` | serve | Debug logging (`-vv` for trace) |

The full CLI has grown beyond `serve`: there are `dev`, `php`, and `kv` subcommands, plus service-lifecycle commands (`install`, `uninstall`, `start`, `stop`, `restart`, `status`). See the [CLI reference](/reference/cli/) for the complete list.

Precedence: `RUST_LOG` env var > `-v` flag > `server.logging.level` > `"info"`

### Environment Variables

All config values can be overridden with `EPHPM_`-prefixed environment variables using `__` as the nesting separator:

```bash
EPHPM_SERVER__LISTEN=0.0.0.0:9090
EPHPM_SERVER__TIMEOUTS__IDLE=120
EPHPM_PHP__MEMORY_LIMIT=256M
```

### Roadmap

Rate limiting (`[server.limits]`), Prometheus metrics (`[server.metrics]`), and graceful shutdown drain (`server.timeouts.shutdown`) have shipped and moved to the Implemented table above.

| Config | Description | Priority |
|--------|-------------|----------|
| `server.static.expires` | Per-extension cache lifetimes (e.g., images 1yr, CSS 1wk) | Medium |
| `server.static.index_fallback` | Serve `index.html` for SPA routes (distinct from PHP fallback) | Medium |
| `server.response.server_header` | Custom or disabled `Server:` header (fingerprinting prevention) | Low |
| `server.request.max_uri_length` | Reject abnormally long URIs (defense in depth) | Low |
| `server.worker_threads` | Tokio worker thread count (auto-detect by default) | Low |
| `server.rewrites` | Regex-based URL rewriting | Low |
| `server.logging.format` | Text vs JSON structured logging | Low |
| `server.logging.access_format` | Common/combined access log format | Low |
| `php.env` | Environment variables passed to PHP (12-factor app support) | Medium |
| `php.disable_functions` | Shortcut for INI directive | Low |
| `php.error_log` | Separate PHP error log path | Low |

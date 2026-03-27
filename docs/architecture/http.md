# HTTP Server Architecture

This document covers the HTTP server design, PHP execution model, request lifecycle, and configuration system.

---

## Request Lifecycle

```
Client Request
    |
    v
TCP Accept (tokio async)
    |
    v
HTTP/1.1 Parse (hyper)
    |
    v
Router::handle()
    |
    +-- Request timeout wrapper (server.timeouts.request)
    |
    v
Security Checks (in order)
    |-- Hidden file check (.env, .git, .htaccess)
    |-- Blocked path check (server.security.blocked_paths)
    |-- Trusted proxy resolution (X-Forwarded-For, X-Forwarded-Proto)
    |
    v
Fallback Resolution
    |-- Try $uri as literal file
    |-- Try $uri/ as directory (check index_files)
    |-- Apply fallback rewrite or status code
    |
    v
+------------------+--------------------+
|                  |                    |
v                  v                    v
PHP file       Static file          Status code
|              |                    |
|-- Allowlist  |-- Path traversal   +-- 404/403/etc
|   check      |   check
|-- Body size  |-- MIME detection
|   check      |-- Cache-Control
|-- PHP exec   |-- Gzip compress
|-- Gzip       |
|   compress   v
|              Response
v
Response
```

## PHP Execution Model

### Request Reuse

The embed SAPI starts a single PHP request during `php_embed_init()`. Instead of calling `php_request_shutdown()` / `php_request_startup()` per HTTP request (which crashes in the embed SAPI), we reuse that initial request and manually reset state between requests.

Per-request reset:
1. Clear output buffer and response headers
2. Reset `SG(read_post_bytes) = -1` so PHP re-reads POST data
3. Close and recreate `request_body` stream for fresh `php://input`
4. Destroy all `PG(http_globals)` arrays (stale superglobals)
5. Rebuild `$_SERVER`, `$_GET`, `$_POST`, `$_FILES`, `$_COOKIE`, `$_REQUEST`

### Superglobal Population

| Variable | Source |
|----------|--------|
| `$_SERVER` | Built by Rust (`PhpRequest::server_variables()`), registered via C callback |
| `$_GET` | Parsed from query string via `sapi_module.treat_data(PARSE_STRING, ...)` |
| `$_POST` | `sapi_handle_post()` — dispatches to content-type-specific parser |
| `$_FILES` | Populated by `sapi_handle_post()` for multipart/form-data (rfc1867) |
| `$_COOKIE` | Manual parsing of `Cookie` header (`key=value; key=value`) |
| `$_REQUEST` | Merge of `$_GET` + `$_POST` + `$_COOKIE` |

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

PHP is NTS (Non-Thread-Safe). A global `Mutex<Option<PhpRuntime>>` serializes all PHP execution. The async HTTP server runs on tokio, but PHP calls use `spawn_blocking` to avoid blocking the event loop. One PHP request executes at a time.

### Signal Handling

PHP installs a `SIGPROF` handler for `max_execution_time`. This signal is process-wide and would crash tokio worker threads (NULL dereference in PHP's handler on non-PHP threads). We override PHP's signal functions with no-ops via `--wrap` linker flags and manage timeouts at the HTTP layer instead.

### Bailout Protection

PHP uses `setjmp`/`longjmp` for error handling. All PHP calls go through `ephpm_wrapper.c` which wraps execution in `zend_try`/`zend_catch`. PHP 8.x `exit()`/`die()` throws an unwind exit exception, which we detect and treat as a normal response (with captured output).

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
TCP Accept → TLS Handshake (tokio-rustls) → HTTP/1.1 Parse → Router
                  ↓ timeout
         header_read_timeout
```

- TLS handshake timeout reuses `server.timeouts.header_read` (default 30s)
- ALPN negotiated to `http/1.1` only
- `is_tls` flag propagated to router so `$_SERVER['HTTPS']` is set correctly
- When behind a trusted proxy, `X-Forwarded-Proto` takes precedence over native TLS status

## Compression

Applied to both PHP and static responses when the client sends `Accept-Encoding: gzip`.

| Check | Condition |
|-------|-----------|
| Enabled | `server.response.compression = true` |
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
| `server.response.compression` | bool | `true` | Enable gzip compression |
| `server.response.compression_level` | int | `1` | Gzip level (1-9) |
| `server.response.compression_min_size` | int | `1024` | Min bytes to compress |
| `server.static.cache_control` | string | `""` | Cache-Control header for static files |
| `server.static.hidden_files` | string | `"deny"` | Dotfile handling: deny, ignore, allow |
| `server.security.trusted_proxies` | string[] | `[]` | CIDR ranges for proxy trust |
| `server.security.blocked_paths` | string[] | `[]` | Glob patterns to block (403) |
| `server.security.allowed_php_paths` | string[] | `[]` | PHP execution allowlist |
| `server.logging.level` | string | `"info"` | Log level (trace/debug/info/warn/error) |
| `server.logging.access` | string | `""` | Access log file path |
| `server.tls.cert` | path | — | PEM certificate chain file (enables HTTPS) |
| `server.tls.key` | path | — | PEM private key file |
| `server.tls.listen` | string | — | Separate HTTPS listen address |
| `server.tls.redirect_http` | bool | `false` | 301 redirect HTTP to HTTPS |
| `php.max_execution_time` | int | `30` | PHP per-request timeout (seconds) |
| `php.memory_limit` | string | `"128M"` | PHP memory limit |
| `php.ini_overrides` | [string, string][] | `[]` | INI directive overrides |

### CLI Flags

| Flag | Scope | Description |
|------|-------|-------------|
| `-c, --config` | serve | Config file path (default: `ephpm.toml`) |
| `-l, --listen` | serve | Listen address (overrides config) |
| `-d, --document-root` | serve | Document root (overrides config) |
| `-v` | serve | Debug logging (`-vv` for trace) |

Precedence: `RUST_LOG` env var > `-v` flag > `server.logging.level` > `"info"`

### Environment Variables

All config values can be overridden with `EPHPM_`-prefixed environment variables using `__` as the nesting separator:

```bash
EPHPM_SERVER__LISTEN=0.0.0.0:9090
EPHPM_SERVER__TIMEOUTS__IDLE=120
EPHPM_PHP__MEMORY_LIMIT=256M
```

### Roadmap

| Config | Description | Priority |
|--------|-------------|----------|
| `server.tls.auto` | ACME / Let's Encrypt auto-provisioning | High |
| `server.static.etag` | ETag headers + 304 Not Modified support | Medium |
| `server.static.expires` | Per-extension cache lifetimes | Medium |
| `server.response.headers` | Custom response headers (CORS, CSP, HSTS) | Medium |
| `server.request.trusted_hosts` | Host header validation | Medium |
| `php.env` | Environment variables passed to PHP | Medium |
| `php.disable_functions` | Shortcut for INI directive | Low |
| `php.error_log` | Separate PHP error log path | Low |
| `server.logging.format` | Text vs JSON structured logging | Low |
| `server.logging.access_format` | Common/combined access log format | Low |
| `server.metrics.enabled` | Prometheus metrics endpoint | Low |
| `server.rewrites` | Regex-based URL rewriting | Low |

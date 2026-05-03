# Web Server Feature Parity: Apache / Nginx / Caddy vs ephpm

Gap analysis and design notes for bringing ephpm's built-in HTTP server up to
feature parity with the capabilities PHP developers expect from traditional
web servers.

---

## Current ephpm Capabilities

| Feature | Status |
|---------|--------|
| Listen on address:port | Done (`server.listen`) |
| Document root | Done (`server.document_root`) |
| Index file fallback | Done (`server.index_files`) |
| Static file serving w/ MIME detection | Done (`mime_guess` crate) |
| Path traversal protection | Done (canonicalize + boundary check) |
| PHP routing (`.php` + clean URLs) | Done (extensionless → `index.php`) |
| PHP ini overrides | Done (`php.ini_overrides`) |
| Graceful shutdown (Ctrl+C) | Done |
| URL rewriting / `try_files` fallback | Done (`server.fallback`) |
| TLS / HTTPS (manual + ACME) | Done (`[server.tls]`) |
| Custom error pages | Done (via `=404` fallback status codes) |
| Response headers | Done (`server.response.headers`) |
| Gzip compression | Done (`server.response.compression`) |
| Brotli compression | Done (HTTP responses; preferred over gzip when client supports `br`) |
| Timeouts (request, idle, header read) | Done (`server.timeouts`) |
| Virtual hosts | Done (`server.sites_dir`) |
| Per-site config overrides | Done (drop a `site.toml` in each vhost directory) |
| Request size limits | Done (`server.request.max_body_size`) |
| Keep-alive | Done (HTTP/1.1 keep-alive with idle timeout) |
| Rate limiting | Done (`server.limits.per_ip_rate`) |
| IP connection limiting | Done (`server.limits.per_ip_max_connections`) |
| ETag / 304 (static + PHP) | Done (`server.static.etag`, `server.php_etag_cache`) |
| File cache (metadata + content) | Done (`server.file_cache`) |
| Blocked paths / security | Done (`server.security.blocked_paths`, `hidden_files`) |
| PHP execution allowlist | Done (`server.security.allowed_php_paths`) |
| Trusted proxy / X-Forwarded-For | Done (`server.security.trusted_proxies`) |
| Host header validation | Done (`server.request.trusted_hosts`) |
| Prometheus metrics | Done (`server.metrics`) |
| HTTP/2 | Done (via ALPN negotiation on TLS connections) |
| HTTP → HTTPS redirect | Done (`server.tls.redirect_http` — 301 from a separate HTTP listener) |
| Open file pre-compression (gzip + brotli) | Done (`server.file_cache.precompress`) |
| open_basedir per vhost | Done (`server.security.open_basedir`) |
| Disable dangerous PHP funcs per vhost | Done (`server.security.disable_shell_exec` — turns off `exec`, `shell_exec`, `system`, `passthru`, `proc_open`, `popen`, `pcntl_exec`) |

---

## Feature Gap Matrix

Features ranked by priority for PHP application hosting (WordPress, Laravel,
Drupal, etc.).

### P0 — Must Have for Production PHP Hosting

| Feature | Apache | Nginx | Caddy | ephpm | Notes |
|---------|--------|-------|-------|-------|-------|
| **URL rewriting / redirects** | mod_rewrite | rewrite + try_files | rewrite + try_files | **Done** | Configurable `fallback` chain (equivalent to `try_files`). |
| **TLS / HTTPS** | mod_ssl (manual) | ssl directives (manual) | Automatic ACME | **Done** | Manual cert/key + automatic ACME via Let's Encrypt. Caddy-style auto-HTTPS. |
| **Custom error pages** | ErrorDocument | error_page | handle_errors | **Done** | Custom error page served via the `fallback` chain — e.g. `fallback = ["{path}", "/errors/404.html", "=404"]` returns the custom HTML and a 404 status when nothing else matches. |
| **Response headers** | mod_headers | add_header | header directive | **Done** | `server.response.headers` adds custom headers to every response. |
| **Compression (gzip)** | mod_deflate | gzip (built-in) | encode (built-in) | **Done** | Gzip compression with configurable level and minimum size. |
| **Access logging** | CustomLog | access_log | log (structured JSON) | **Done** | `server.logging.access` writes to a file via tracing appender. |
| **Timeouts** | Timeout, KeepAlive | client_body_timeout, etc. | read_body, read_header, etc. | **Done** | `header_read`, `idle`, `request`, and `shutdown` timeouts. |

### P1 — Important for Real Deployments

| Feature | Apache | Nginx | Caddy | ephpm | Notes |
|---------|--------|-------|-------|-------|-------|
| **Virtual hosts** | `<VirtualHost>` | `server` blocks | Site blocks | **Done** | `server.sites_dir` — directory-based vhosts with lazy discovery. |
| **Reverse proxy** | mod_proxy | proxy_pass + upstream | reverse_proxy | Missing | API backends, microservices. |
| **Request size limits** | LimitRequestBody | client_max_body_size | request_body max_size | **Done** | `server.request.max_body_size` — returns 413 on oversized bodies. |
| **Keep-alive tuning** | KeepAliveTimeout, MaxKeepAliveRequests | keepalive_timeout, keepalive_requests | idle timeout | **Done** | `server.timeouts.idle` controls keepalive timeout. |
| **MIME type overrides** | AddType | types block | header directive | Missing | We use `mime_guess` but no user overrides. |

### P2 — Nice to Have

| Feature | Apache | Nginx | Caddy | ephpm | Notes |
|---------|--------|-------|-------|-------|-------|
| Rate limiting | mod_evasive (3rd party) | limit_req (built-in) | Plugin | **Done** | Per-IP rate limiting + connection limits via `server.limits`. |
| IP allow/deny | Require ip | allow/deny | remote_ip matcher | **Partial** | Blocked paths and trusted proxies. No IP allowlist/denylist yet. |
| Basic auth | mod_auth_basic | auth_basic | basic_auth | Missing | |
| Directory listing | Options +Indexes | autoindex | file_server browse | Missing | |
| HTTP/2 | mod_http2 | listen ... http2 | Automatic | **Done** | ALPN negotiation on TLS connections. |
| HTTP/3 (QUIC) | Not supported | Experimental | Built-in | Missing | |
| Brotli compression (HTTP) | mod_brotli | Module | Plugin/built-in | **Done** | HTTP responses prefer Brotli over gzip when the client supports `br`. KV store also supports gzip/brotli/zstd for stored values. |
| Zstd compression (HTTP) | N/A | N/A | Plugin | Missing | KV store supports zstd; HTTP response negotiation does not. |
| Pre-compressed file serving | N/A | gzip_static | precompressed | **Done** | `server.file_cache.precompress` pre-computes gzip + brotli variants for cached files. |

---

## Deep Dive: URL Rewriting & Redirects

This is the single most important missing feature. Every PHP framework and CMS
requires URL rewriting to function. Without it, ephpm can only serve direct
`.php` file requests.

### What PHP Apps Need

**WordPress** (`.htaccess`):
```apache
RewriteEngine On
RewriteBase /
RewriteRule ^index\.php$ - [L]
RewriteCond %{REQUEST_FILENAME} !-f
RewriteCond %{REQUEST_FILENAME} !-d
RewriteRule . /index.php [L]
```

**Laravel** (`.htaccess` in `public/`):
```apache
RewriteEngine On
RewriteCond %{REQUEST_FILENAME} !-d
RewriteCond %{REQUEST_FILENAME} !-f
RewriteRule ^ index.php [L]
```

**Drupal**:
```apache
RewriteCond %{REQUEST_FILENAME} !-f
RewriteCond %{REQUEST_FILENAME} !-d
RewriteCond %{REQUEST_URI} !=/favicon.ico
RewriteRule ^ index.php [L,QSA]
```

The pattern is nearly universal: **if the file doesn't exist on disk, route to
the front controller (`index.php`)**.

### Nginx Equivalent

All three frameworks use the same Nginx config:
```nginx
location / {
    try_files $uri $uri/ /index.php?$query_string;
}
```

### What ephpm Already Does

The router in `ephpm-server/src/router.rs` already has WordPress-style permalink
detection: extensionless paths that don't match a file get routed to PHP. But
this is hardcoded behavior, not configurable. Users can't:

- Define custom rewrite rules
- Set up redirects (301/302) for old URLs
- Rewrite based on conditions (host, query string, headers)
- Choose a different front controller (not all apps use `index.php`)

### Proposed Config: `[[server.rewrites]]`

Design philosophy: **don't replicate mod_rewrite**. Apache's rewrite engine is
notoriously complex. Instead, take the Caddy/Nginx approach — cover 95% of use
cases with simple, composable directives.

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
index_files = ["index.php", "index.html"]

# Front-controller mode (covers WordPress, Laravel, Drupal, Symfony, etc.)
# Equivalent to: try_files $uri $uri/ /index.php?$query_string
try_files = ["{path}", "{path}/", "/index.php?{query}"]

# Explicit redirect rules — evaluated in order, first match wins
[[server.redirects]]
# Old blog URL structure → new structure
from = "^/blog/(\\d{4})/(\\d{2})/(.+)$"
to = "/articles/$3"
status = 301

[[server.redirects]]
# Single page redirect
from = "^/old-page\\.html$"
to = "/new-page/"
status = 301

[[server.redirects]]
# Non-www to www (when virtual hosts are added)
host = "^example\\.com$"
to = "https://www.example.com{uri}"
status = 301

# Internal rewrite rules — evaluated in order, first match wins
# These change the internal path without sending a redirect to the client
[[server.rewrites]]
from = "^/api/v1/(.+)$"
to = "/api.php?route=$1"

[[server.rewrites]]
# Maintenance mode
from = ".*"
to = "/maintenance.html"
condition = { file_exists = "/var/www/html/.maintenance" }
```

### Config Semantics

#### `try_files`

```toml
try_files = ["{path}", "{path}/", "/index.php?{query}"]
```

- Evaluated for every request that doesn't match a static file or direct `.php`
- Each entry is checked in order against the filesystem
- `{path}` = the request URI path
- `{query}` = the original query string
- Last entry is the fallback (used as internal rewrite target, not a filesystem check)
- This single directive covers every major PHP framework's needs

This replaces the current hardcoded permalink logic in the router.

#### `[[server.redirects]]`

External redirects — the client receives an HTTP 301/302/307/308 and follows it.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `from` | regex string | Yes | Pattern matched against the request path |
| `to` | string | Yes | Replacement (supports `$1`, `$2` captures and `{uri}`, `{query}`, `{host}` placeholders) |
| `status` | integer | No | HTTP status code (default: 302) |
| `host` | regex string | No | Only match requests with this Host header |
| `methods` | string array | No | Only match these HTTP methods (default: all) |

#### `[[server.rewrites]]`

Internal rewrites — the URL is changed server-side, transparent to the client.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `from` | regex string | Yes | Pattern matched against the request path |
| `to` | string | Yes | Replacement target |
| `condition` | table | No | Additional conditions (see below) |

#### Conditions (for rewrites)

```toml
# File existence checks (like RewriteCond !-f / !-d)
condition = { file_exists = "/path/to/flag" }
condition = { not_file = true }    # request path doesn't resolve to a file
condition = { not_dir = true }     # request path doesn't resolve to a directory
condition = { header = { name = "X-Forwarded-Proto", pattern = "^http$" } }
condition = { query = "^debug=" }
```

### Processing Order

1. **Redirects** — checked first, in order. First match sends redirect response.
2. **Static file check** — if the path resolves to a file in document_root, serve it.
3. **try_files** — if configured, check each entry against the filesystem.
4. **Rewrites** — checked in order. First match rewrites the internal path.
5. **PHP routing** — the (possibly rewritten) path is routed to PHP if applicable.
6. **404** — nothing matched.

This order ensures redirects always win (you can redirect away from existing
files), static files are served efficiently, and the front-controller pattern
works as expected.

### Comparison with Apache mod_rewrite

| mod_rewrite Feature | ephpm Equivalent | Covered? |
|---------------------|------------------|----------|
| `RewriteRule` with regex | `[[server.rewrites]]` from/to | Yes |
| `RewriteRule` with `[R=301]` flag | `[[server.redirects]]` | Yes |
| `RewriteCond %{REQUEST_FILENAME} !-f` | `try_files` or `condition.not_file` | Yes |
| `RewriteCond %{REQUEST_FILENAME} !-d` | `try_files` or `condition.not_dir` | Yes |
| `RewriteCond %{HTTP_HOST}` | `redirects[].host` | Yes |
| `RewriteCond %{HTTPS}` | `redirects[].condition.header` | Partial |
| `RewriteCond %{REQUEST_METHOD}` | `redirects[].methods` | Yes |
| `RewriteCond %{QUERY_STRING}` | `condition.query` | Yes |
| `RewriteRule` `[QSA]` flag | `{query}` placeholder in `to` | Yes |
| `RewriteRule` `[L]` flag | First-match-wins (implicit) | Yes |
| `RewriteRule` `[NC]` flag | `(?i)` in regex pattern | Yes |
| `RewriteRule` `[C]` chain flag | Not supported | No |
| `RewriteRule` `[N]` next/loop flag | Not supported (by design) | No |
| `RewriteRule` `[E]` env variable | Not supported | No |
| `RewriteRule` `[P]` proxy flag | Not supported (until reverse proxy) | No |
| `.htaccess` per-directory overrides | Not supported (by design) | No |
| `RewriteMap` external programs | Not supported | No |

The unsupported features are deliberately excluded — they're the source of
mod_rewrite's complexity and security pitfalls. Chain rules, loops, and
per-directory `.htaccess` files are anti-patterns that Nginx and Caddy also
chose not to replicate.

---

## Proposed Config: Other P0 Features

### Custom Error Pages

```toml
[server.error_pages]
404 = "/errors/404.html"     # serve this file for 404s
500 = "/errors/500.html"
503 = "/errors/maintenance.html"
```

### Response Headers

```toml
[[server.headers]]
name = "X-Content-Type-Options"
value = "nosniff"

[[server.headers]]
name = "X-Frame-Options"
value = "DENY"

[[server.headers]]
name = "Strict-Transport-Security"
value = "max-age=63072000; includeSubDomains"

[[server.headers]]
name = "X-Powered-By"
action = "remove"

# Path-scoped headers
[[server.headers]]
path = "/api/*"
name = "Cache-Control"
value = "no-store"

[[server.headers]]
path = "*.css,*.js,*.png,*.jpg,*.gif,*.svg,*.woff2"
name = "Cache-Control"
value = "public, max-age=31536000, immutable"
```

### Compression

```toml
[server.compression]
enabled = true
algorithms = ["gzip", "br"]       # brotli if available
min_length = 256                   # bytes, don't compress tiny responses
types = [                          # MIME types to compress
    "text/html",
    "text/css",
    "text/plain",
    "application/javascript",
    "application/json",
    "application/xml",
    "image/svg+xml",
]
```

### Timeouts

```toml
[server.timeouts]
read_header = "30s"
read_body = "60s"
write = "120s"
idle = "5m"            # keepalive idle timeout
```

### Access Logging

```toml
[server.logging]
access_log = "/var/log/ephpm/access.log"   # or "stdout"
format = "combined"                         # "combined", "common", "json"
```

---

## Implementation Status

### Phase 1 — URL Rewriting :white_check_mark: Done

The pieces every PHP app actually needs are working:

1. ~~Add `try_files` to `ServerConfig`~~ → Implemented as `server.fallback`
2. ~~Refactor router to use the fallback chain instead of hardcoded permalink logic~~ → Done
3. ~~Internal rewrite targets in the fallback chain~~ → Done (the last entry is an internal rewrite)
4. ~~Status-code fallbacks (`=404`, `=403`, etc.)~~ → Done

The front-controller pattern (`fallback = ["{path}", "{path}/", "/index.php?{query}"]`) covers WordPress, Laravel, Drupal, Symfony, and effectively any framework that routes through `index.php`.

**Optional extensions still outstanding** — neither blocks PHP application hosting; both are separate features that some users will want for SEO migrations or non-standard URL schemes:

- `[[server.redirects]]` with regex matching → still TBD
- `[[server.rewrites]]` with arbitrary conditions → still TBD

### Phase 2 — Production Hardening :white_check_mark: Complete
5. ~~Custom error pages~~ → Partial (`=404` fallback status codes)
6. ~~Response header middleware~~ → Done (`server.response.headers`)
7. ~~Gzip compression~~ → Done (`server.response.compression`)
8. ~~Timeout configuration~~ → Done (`server.timeouts`)
9. ~~Access logging~~ → Done (`server.logging.access`)

### Phase 3 — TLS & Multi-site :white_check_mark: Complete
10. ~~TLS with manual cert paths~~ → Done (`server.tls.cert` / `server.tls.key`)
11. ~~Automatic ACME (Let's Encrypt)~~ → Done (`server.tls.domains`)
12. ~~HTTP → HTTPS redirect~~ → Done (`server.tls.redirect_http`, 301 from separate HTTP listener)
13. ~~Virtual hosts / multi-site routing~~ → Done (`server.sites_dir`)
14. ~~Per-site config overrides~~ → Done (drop `site.toml` into each vhost dir)

### Phase 4 — Advanced (Partially Complete)
15. Reverse proxy — Not yet
16. ~~Rate limiting~~ → Done (`server.limits`)
17. IP allow/deny — Not yet (blocked_paths covers path-based blocking)
18. ~~HTTP/2~~ → Done (ALPN negotiation on TLS)
19. ~~Brotli HTTP responses~~ → Done (preferred over gzip when client supports `br`)
20. ~~Pre-compressed static file serving~~ → Done (`server.file_cache.precompress` for gzip + brotli)
21. ~~`disable_shell_exec` for multi-tenant safety~~ → Done

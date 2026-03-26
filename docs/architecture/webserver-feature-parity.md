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

Everything else is missing.

---

## Feature Gap Matrix

Features ranked by priority for PHP application hosting (WordPress, Laravel,
Drupal, etc.).

### P0 — Must Have for Production PHP Hosting

| Feature | Apache | Nginx | Caddy | ephpm | Notes |
|---------|--------|-------|-------|-------|-------|
| **URL rewriting / redirects** | mod_rewrite | rewrite + try_files | rewrite + try_files | Missing | Critical for every PHP framework. See deep-dive below. |
| **TLS / HTTPS** | mod_ssl (manual) | ssl directives (manual) | Automatic ACME | Missing | Planned `[tls]` section. Caddy-style auto-HTTPS is the gold standard. |
| **Custom error pages** | ErrorDocument | error_page | handle_errors | Missing | 404/500 pages are table stakes. |
| **Response headers** | mod_headers | add_header | header directive | Missing | Security headers (HSTS, X-Frame-Options, CSP) are expected. |
| **Compression (gzip)** | mod_deflate | gzip (built-in) | encode (built-in) | Missing | Massive performance win for PHP HTML/JSON output. |
| **Access logging** | CustomLog | access_log | log (structured JSON) | Missing | Only tracing exists today. Need request-level access logs. |
| **Timeouts** | Timeout, KeepAlive | client_body_timeout, etc. | read_body, read_header, etc. | Missing | Slowloris protection, PHP script timeout enforcement. |

### P1 — Important for Real Deployments

| Feature | Apache | Nginx | Caddy | ephpm | Notes |
|---------|--------|-------|-------|-------|-------|
| **Virtual hosts** | `<VirtualHost>` | `server` blocks | Site blocks | Missing | Multiple domains on one instance. |
| **Reverse proxy** | mod_proxy | proxy_pass + upstream | reverse_proxy | Missing | API backends, microservices. |
| **Request size limits** | LimitRequestBody | client_max_body_size | request_body max_size | Missing | Prevent upload abuse. |
| **Keep-alive tuning** | KeepAliveTimeout, MaxKeepAliveRequests | keepalive_timeout, keepalive_requests | idle timeout | Missing | Connection reuse. |
| **MIME type overrides** | AddType | types block | header directive | Missing | We use `mime_guess` but no user overrides. |

### P2 — Nice to Have

| Feature | Apache | Nginx | Caddy | ephpm | Notes |
|---------|--------|-------|-------|-------|-------|
| Rate limiting | mod_evasive (3rd party) | limit_req (built-in) | Plugin | Missing | |
| IP allow/deny | Require ip | allow/deny | remote_ip matcher | Missing | |
| Basic auth | mod_auth_basic | auth_basic | basic_auth | Missing | |
| Directory listing | Options +Indexes | autoindex | file_server browse | Missing | |
| HTTP/2 | mod_http2 | listen ... http2 | Automatic | Missing | |
| HTTP/3 (QUIC) | Not supported | Experimental | Built-in | Missing | |
| Brotli/Zstd compression | mod_brotli | Module | Plugin/built-in | Missing | |
| Pre-compressed file serving | N/A | gzip_static | precompressed | Missing | |

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

## Implementation Priority

### Phase 1 — URL Rewriting (unblocks all PHP frameworks)
1. Add `try_files` to `ServerConfig`
2. Refactor router to use `try_files` instead of hardcoded permalink logic
3. Add `[[server.redirects]]` with regex matching
4. Add `[[server.rewrites]]` with basic conditions

### Phase 2 — Production Hardening
5. Custom error pages
6. Response header middleware
7. Gzip compression (tower middleware or custom)
8. Timeout configuration
9. Access logging

### Phase 3 — TLS & Multi-site
10. TLS with manual cert paths
11. Automatic ACME (Let's Encrypt)
12. Virtual hosts / multi-site routing

### Phase 4 — Advanced
13. Reverse proxy
14. Rate limiting
15. IP allow/deny
16. HTTP/2

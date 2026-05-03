# Web Server Feature Parity: Apache / Nginx / Caddy vs ephpm

Status of ephpm's built-in HTTP server against the capabilities PHP developers
expect from traditional web servers. This page started life as a gap analysis;
most of the gap is now closed. What's below is a snapshot of where we are,
followed by the small remaining backlog.

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

## Comparison vs Apache / Nginx / Caddy

How ephpm stacks up against the traditional servers PHP developers reach for. Everything in the "must-have for production PHP hosting" tier is shipping; the remaining gaps are in the optional / nice-to-have tier.

### Production must-haves — all shipping in ephpm

| Feature | Apache | Nginx | Caddy | ephpm |
|---------|--------|-------|-------|-------|
| URL rewriting / front-controller routing | mod_rewrite | rewrite + try_files | rewrite + try_files | **Yes** (`server.fallback`) |
| TLS / HTTPS — manual + automatic | mod_ssl (manual) | ssl directives (manual) | Automatic ACME | **Yes** (manual + ACME, Caddy-style auto-HTTPS) |
| HTTP → HTTPS redirect | RewriteRule | return 301 | redir | **Yes** (`server.tls.redirect_http`) |
| Custom error pages | ErrorDocument | error_page | handle_errors | **Yes** (via `fallback` chain — e.g. `["{path}", "/errors/404.html", "=404"]`) |
| Response headers | mod_headers | add_header | header directive | **Yes** (`server.response.headers`) |
| Gzip compression | mod_deflate | gzip (built-in) | encode (built-in) | **Yes** (`server.response.compression`) |
| Brotli compression (HTTP) | mod_brotli | Module | Plugin/built-in | **Yes** (preferred over gzip when client supports `br`) |
| Pre-compressed static file serving | N/A | gzip_static | precompressed | **Yes** (`server.file_cache.precompress` for gzip + brotli) |
| Access logging | CustomLog | access_log | log (structured JSON) | **Yes** (`server.logging.access`) |
| Timeouts (read header / read body / write / idle) | Timeout, KeepAlive | client_body_timeout, etc. | read_body, read_header, etc. | **Yes** (`server.timeouts`) |
| Virtual hosts | `<VirtualHost>` | `server` blocks | Site blocks | **Yes** (`server.sites_dir`, directory-based + lazy discovery) |
| Per-vhost config overrides | per-vhost block | per-server block | per-site block | **Yes** (`site.toml` in each vhost dir) |
| Request size limits | LimitRequestBody | client_max_body_size | request_body max_size | **Yes** (`server.request.max_body_size`) |
| Keep-alive tuning | KeepAliveTimeout | keepalive_timeout | idle timeout | **Yes** (`server.timeouts.idle`) |
| HTTP/2 | mod_http2 | listen ... http2 | Automatic | **Yes** (ALPN negotiation on TLS) |
| Rate limiting | mod_evasive (3rd party) | limit_req | Plugin | **Yes** (`server.limits`) |
| Trusted proxy / X-Forwarded-For handling | mod_remoteip | real_ip module | trusted_proxies | **Yes** (`server.security.trusted_proxies`) |
| Path-based access controls | Require, Deny | allow/deny | path matcher | **Yes** (`server.security.blocked_paths`, `hidden_files`) |
| Multi-tenant PHP isolation (`open_basedir`, disable shell funcs) | per-vhost `php_admin_value` | per-server fastcgi_param | per-site env | **Yes** (`server.security.open_basedir` + `disable_shell_exec`) |

### Optional gaps

| Feature | Apache | Nginx | Caddy | ephpm | Notes |
|---------|--------|-------|-------|-------|-------|
| Reverse proxy | mod_proxy | proxy_pass + upstream | reverse_proxy | Not yet | API backends, microservices, sidecars |
| MIME type overrides | AddType | types block | header directive | Not yet | `mime_guess` is used; no user overrides yet |
| IP allow/deny lists | Require ip | allow/deny | remote_ip matcher | Not yet | path-based blocking is covered |
| Regex `[[server.redirects]]` (301/302 from regex) | mod_rewrite `[R]` | rewrite ... permanent | redir | Not yet | SEO migrations / vanity URLs |
| Regex `[[server.rewrites]]` with conditions | mod_rewrite `RewriteCond` | rewrite if blocks | rewrite matcher | Not yet | Beyond `server.fallback` |
| HTTP/3 (QUIC) | Not supported | Experimental | Built-in | Not yet | Would need `quinn` integration |
| Basic auth | mod_auth_basic | auth_basic | basic_auth | Not yet | Useful for staging gating |
| Directory listing / autoindex | Options +Indexes | autoindex | file_server browse | Not yet | Largely replaced by S3-style buckets these days |
| Zstd compression (HTTP responses) | N/A | N/A | Plugin | Not yet | KV store supports zstd; HTTP response negotiation does not |

---

## URL Rewriting in ephpm

The piece every PHP framework actually needs — "if the file doesn't exist on disk, route to the front controller" — is implemented as `server.fallback`. It's the direct equivalent of nginx's `try_files $uri $uri/ /index.php?$query_string;`.

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
index_files = ["index.php", "index.html"]

# Try the literal path, then with a trailing slash, then fall back to
# the front controller. Covers WordPress, Laravel, Drupal, Symfony, and
# effectively any framework that routes through index.php.
fallback = ["{path}", "{path}/", "/index.php?{query}"]
```

### Semantics

- Each entry is checked in order against the filesystem (relative to the document root).
- `{path}` = the request URI path; `{query}` = the original query string.
- The last entry is treated as an internal rewrite target (not checked against the filesystem) — so a request to `/about` that doesn't resolve to a file gets rewritten to `/index.php?…` internally.
- Status-code fallbacks are written `=404`, `=403`, etc. — handy for "if nothing else matches, return a status."

### Custom error page

Use a fallback entry that points at a static HTML file before the status-code entry:

```toml
fallback = ["{path}", "/errors/404.html", "=404"]
```

When `/foo` doesn't resolve and `/errors/404.html` exists, the HTML is served with a 404 status. If the HTML is missing too, the bare `=404` is returned.

### Comparison with Apache mod_rewrite

| mod_rewrite feature | ephpm equivalent | Covered? |
|---------------------|------------------|----------|
| `RewriteCond %{REQUEST_FILENAME} !-f` | implicit in `fallback` (filesystem check on each entry) | Yes |
| `RewriteCond %{REQUEST_FILENAME} !-d` | implicit in `fallback` | Yes |
| `RewriteRule` front-controller pattern (`. /index.php`) | last entry of `fallback` | Yes |
| `RewriteRule` `[QSA]` flag | `{query}` placeholder in the fallback target | Yes |
| `RewriteRule` `[L]` flag | first-match-wins is implicit | Yes |
| `RewriteRule ^foo$ /bar [R=301]` (regex 301/302 redirects) | regex `[[server.redirects]]` — **not yet** | Optional |
| `RewriteRule` with arbitrary `RewriteCond` (host, header, query) | regex `[[server.rewrites]]` with conditions — **not yet** | Optional |
| `RewriteRule` `[P]` proxy flag | requires reverse proxy — **not yet** | Optional |
| `.htaccess` per-directory overrides | not supported, by design | No |
| `RewriteMap` external programs | not supported | No |

The "not supported" items are deliberately excluded — chain rules, loops, and per-directory `.htaccess` files are the source of mod_rewrite's complexity and security pitfalls; nginx and Caddy don't replicate them either.

### Future: regex-based `[[server.redirects]]` and `[[server.rewrites]]`

Some users will want explicit regex redirects (e.g. `^/blog/(\d{4})/(\d{2})/(.+)$ → /articles/$3` with a 301) for SEO migrations or non-standard URL schemes. That's a separate feature on top of `fallback` — `fallback` covers framework routing, `[[server.redirects]]` would cover URL-rewriting-in-the-Apache-sense. Sketch of the eventual config:

```toml
[[server.redirects]]
from = "^/blog/(\\d{4})/(\\d{2})/(.+)$"
to = "/articles/$3"
status = 301

[[server.rewrites]]
from = "^/api/v1/(.+)$"
to = "/api.php?route=$1"
condition = { not_file = true }
```

Neither blocks production PHP hosting today.


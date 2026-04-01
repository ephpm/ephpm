# Migrating from Nginx + PHP-FPM

You're running the modern PHP stack — Nginx as a reverse proxy, PHP-FPM handling PHP execution via FastCGI. It's fast and battle-tested, but it's two services to configure, monitor, and scale independently.

ePHPm replaces both Nginx and PHP-FPM with a single binary. No FastCGI socket. No upstream configuration. No separate process manager.

## What You're Replacing

| Component | Nginx + PHP-FPM | ePHPm |
|-----------|----------------|-------|
| HTTP server | Nginx | Built-in (hyper) |
| PHP runtime | PHP-FPM (separate process) | Embedded via FFI (same process) |
| PHP ↔ HTTP communication | FastCGI over Unix socket | In-process function call |
| Process management | `pm.dynamic` / `pm.static` | Worker thread pool (`php.workers`) |
| Static files | Nginx serves directly | Built-in with compression |
| TLS termination | Nginx + certbot | Built-in ACME |
| Services to manage | 2 (nginx + php-fpm) | 1 |

## Step-by-Step Migration

### 1. Translate Nginx Config

Typical Nginx config for a PHP site:

```nginx
server {
    listen 80;
    server_name example.com;
    root /var/www/html;
    index index.php index.html;

    location / {
        try_files $uri $uri/ /index.php?$query_string;
    }

    location ~ \.php$ {
        fastcgi_pass unix:/run/php/php8.2-fpm.sock;
        fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
        include fastcgi_params;
    }

    location ~ /\.ht {
        deny all;
    }

    location ~* \.(css|js|gif|ico|jpeg|jpg|png|svg|woff2?)$ {
        expires 30d;
        add_header Cache-Control "public, immutable";
    }
}
```

ePHPm equivalent:

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
index_files = ["index.php", "index.html"]
fallback = ["$uri", "$uri/", "/index.php?$query_string"]

[server.static]
cache_control = "public, max-age=2592000"
```

That's the entire Nginx server block in 6 lines. No `fastcgi_pass`, no `location` blocks, no `fastcgi_params`.

### 2. Translate PHP-FPM Config

Typical `www.conf` pool config:

```ini
[www]
user = www-data
group = www-data
listen = /run/php/php8.2-fpm.sock
pm = dynamic
pm.max_children = 50
pm.start_servers = 5
pm.min_spare_servers = 5
pm.max_spare_servers = 35
pm.max_requests = 500

php_admin_value[memory_limit] = 256M
php_admin_value[max_execution_time] = 30
php_admin_value[upload_max_filesize] = 64M
```

ePHPm equivalent:

```toml
[php]
workers = 8            # replaces pm.max_children (auto-detected from CPU count)
memory_limit = "256M"
max_execution_time = 30

[server.request]
max_body_size = 67108864   # 64 MB
```

PHP-FPM's process model (`pm.dynamic`, `pm.start_servers`, spare servers) doesn't apply — ePHPm uses a fixed-size thread pool that's always warm. No cold starts, no process spawning overhead.

### 3. Common Nginx Location Blocks

**Block dotfiles:**

```nginx
location ~ /\. { deny all; }
```

ePHPm: default behavior. Dotfiles are blocked automatically.

**Block vendor directory:**

```nginx
location ^~ /vendor/ { deny all; }
```

```toml
[server.security]
blocked_paths = ["vendor/*"]
```

**PHP execution restriction:**

```nginx
location ~* /uploads/.*\.php$ { deny all; }
```

```toml
[server.security]
allowed_php_paths = ["/index.php", "/wp-admin/*", "/wp-login.php"]
```

**Gzip compression:**

```nginx
gzip on;
gzip_types text/css application/javascript text/plain;
gzip_min_length 1024;
```

```toml
[server.response]
compression = true
compression_min_size = 1024
```

**Custom headers:**

```nginx
add_header X-Frame-Options "SAMEORIGIN";
add_header X-Content-Type-Options "nosniff";
```

```toml
[server.response]
headers = [
    ["X-Frame-Options", "SAMEORIGIN"],
    ["X-Content-Type-Options", "nosniff"],
]
```

**Client body size:**

```nginx
client_max_body_size 64m;
```

```toml
[server.request]
max_body_size = 67108864
```

### 4. TLS / HTTPS

**Nginx + certbot:**

```nginx
server {
    listen 443 ssl http2;
    ssl_certificate /etc/letsencrypt/live/example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/example.com/privkey.pem;
}
```

**ePHPm with automatic ACME:**

```toml
[server.tls]
acme_domains = ["example.com"]
acme_email = "you@example.com"
```

Or with existing certificates:

```toml
[server.tls]
cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
key = "/etc/letsencrypt/live/example.com/privkey.pem"
```

### 5. Reverse Proxy / Upstream

If Nginx is proxying to multiple PHP-FPM pools or other backends, ePHPm doesn't replace that. For pure PHP serving (the most common case), ePHPm replaces both Nginx and PHP-FPM. If you need Nginx as a reverse proxy to non-PHP services, keep Nginx for those and point it at ePHPm for PHP.

### 6. Switch Over

```bash
# Stop both services
sudo systemctl stop nginx php8.2-fpm

# Start ePHPm
./ephpm --config ephpm.toml

# Verify
curl http://localhost:8080

# When satisfied
sudo systemctl disable nginx php8.2-fpm
sudo systemctl enable ephpm
```

## What You Gain

| | Nginx + PHP-FPM | ePHPm |
|---|---|---|
| Services | 2 | 1 |
| Config files | `nginx.conf` + site config + `php-fpm.conf` + pool config | One `ephpm.toml` |
| PHP ↔ HTTP overhead | FastCGI serialization over Unix socket | Zero (in-process) |
| Cold start | FPM spawns new workers on demand | Workers always warm |
| Memory (idle) | ~100 MB (Nginx) + ~150 MB (FPM pool) | ~30 MB |
| TLS | Nginx + certbot + cron | Built-in, automatic |
| PHP version upgrade | `apt install`, restart FPM | Download new binary |
| Log files | Nginx access/error + FPM error + PHP error | One log stream |

## What You Lose

- **Nginx as a reverse proxy** — if you're proxying to non-PHP backends (Node, Python, etc.), you still need a reverse proxy for those.
- **Multiple FPM pools** — ePHPm has one worker pool. If you run separate pools for different sites with different users, use ePHPm's virtual hosts instead (same isolation, simpler config).
- **Nginx modules** — `ngx_pagespeed`, `ngx_brotli`, etc. Most functionality is built into ePHPm or handled at the application level.
- **HTTP/3 (QUIC)** — not yet implemented in ePHPm. Nginx supports it via `nginx-quic`.

## Laravel-Specific Notes

Laravel's Nginx config:

```nginx
location / {
    try_files $uri $uri/ /index.php?$query_string;
}
```

ePHPm:

```toml
[server]
document_root = "/var/www/laravel/public"
fallback = ["$uri", "$uri/", "/index.php?$query_string"]
```

The `document_root` points to `public/` — same as your Nginx `root` directive. Everything else works automatically.

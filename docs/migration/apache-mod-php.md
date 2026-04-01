# Migrating from Apache + mod_php

You're running Apache with mod_php — the classic PHP stack. `.htaccess` files, `AllowOverride All`, maybe some `mod_rewrite` rules. It works, but it's slow, uses a lot of memory, and every config change means restarting Apache.

ePHPm replaces Apache and mod_php with a single binary. Your PHP files don't change. Your `.htaccess` rewrite rules translate to a few lines of TOML config.

## What You're Replacing

| Component | Apache + mod_php | ePHPm |
|-----------|-----------------|-------|
| HTTP server | Apache (`httpd`) | Built-in (hyper) |
| PHP runtime | mod_php (loaded into Apache) | Embedded via FFI |
| URL rewriting | `.htaccess` + `mod_rewrite` | `fallback` config |
| SSL/TLS | mod_ssl + certbot | Built-in ACME |
| Static files | Apache serves directly | Built-in with compression |
| Process model | prefork (one process per request) | Async I/O + PHP worker pool |
| Memory per connection | ~30-50 MB (full Apache + PHP process) | ~50 MB shared across all |

## Step-by-Step Migration

### 1. Install ePHPm

```bash
# Download the latest release binary
curl -fSL https://github.com/ephpm/ephpm/releases/latest/download/ephpm-linux-x86_64 -o ephpm
chmod +x ephpm
```

Or build from source:

```bash
cargo xtask release
```

### 2. Create the Config File

Your Apache config probably looks something like this:

```apache
<VirtualHost *:80>
    ServerName example.com
    DocumentRoot /var/www/html

    <Directory /var/www/html>
        AllowOverride All
        Require all granted
    </Directory>

    # SSL via certbot
    SSLEngine on
    SSLCertificateFile /etc/letsencrypt/live/example.com/fullchain.pem
    SSLCertificateKeyFile /etc/letsencrypt/live/example.com/privkey.pem
</VirtualHost>
```

The ePHPm equivalent:

```toml
# ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
```

That's it. ePHPm handles static files, PHP execution, and compression by default. No `AllowOverride`, no `Directory` blocks, no `Require` directives.

### 3. Translate .htaccess Rules

The most common `.htaccess` pattern — WordPress/Laravel pretty permalinks:

```apache
# .htaccess
RewriteEngine On
RewriteCond %{REQUEST_FILENAME} !-f
RewriteCond %{REQUEST_FILENAME} !-d
RewriteRule ^(.*)$ index.php [QSA,L]
```

ePHPm equivalent (this is the default):

```toml
[server]
fallback = ["$uri", "$uri/", "/index.php?$query_string"]
```

This means: try the exact file, try as a directory (with index files), fall back to `index.php`. Same behavior as the `.htaccess` rewrite.

**Common .htaccess patterns and their ePHPm equivalents:**

| .htaccess | ephpm.toml |
|-----------|-----------|
| `RewriteRule ^(.*)$ index.php` | `fallback = ["$uri", "/index.php?$query_string"]` |
| `DirectoryIndex index.php index.html` | `index_files = ["index.php", "index.html"]` |
| `Options -Indexes` | Default (directory listing is never enabled) |
| `deny from all` on `.env` | Default (dotfiles blocked automatically) |
| `Header set X-Frame-Options DENY` | `[server.response] headers = [["X-Frame-Options", "DENY"]]` |
| `php_value upload_max_filesize 10M` | `[server.request] max_body_size = 10485760` |
| `php_value max_execution_time 60` | `[php] max_execution_time = 60` |

### 4. Translate PHP Settings

Apache lets you set PHP directives in `.htaccess` or `httpd.conf`:

```apache
php_value memory_limit 256M
php_value max_execution_time 60
php_value upload_max_filesize 20M
php_value display_errors Off
```

ePHPm:

```toml
[php]
memory_limit = "256M"
max_execution_time = 60
ini_overrides = [
    ["display_errors", "Off"],
]

[server.request]
max_body_size = 20971520   # 20 MB in bytes
```

### 5. TLS / HTTPS

**If you're using certbot:**

Remove certbot entirely. ePHPm handles Let's Encrypt automatically:

```toml
[server.tls]
acme_domains = ["example.com", "www.example.com"]
acme_email = "you@example.com"
```

**If you have existing cert files:**

```toml
[server.tls]
cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
key = "/etc/letsencrypt/live/example.com/privkey.pem"
```

### 6. Security Settings

Apache's common security patterns:

```apache
# Block access to sensitive files
<FilesMatch "^\.">
    Require all denied
</FilesMatch>

# Block vendor directory
<Directory /var/www/html/vendor>
    Require all denied
</Directory>
```

ePHPm (most of this is default):

```toml
[server.security]
# Dotfiles are blocked by default (hidden_files = "deny")
blocked_paths = ["vendor/*", "wp-config.php"]
```

### 7. Switch Over

```bash
# Stop Apache
sudo systemctl stop apache2

# Start ePHPm
./ephpm --config ephpm.toml

# Verify it works
curl http://localhost:8080

# When satisfied, disable Apache and enable ePHPm
sudo systemctl disable apache2
```

Create a systemd service for ePHPm:

```ini
# /etc/systemd/system/ephpm.service
[Unit]
Description=ePHPm PHP Application Server
After=network.target

[Service]
ExecStart=/usr/local/bin/ephpm --config /etc/ephpm/ephpm.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable ephpm
sudo systemctl start ephpm
```

## What You Gain

| | Apache + mod_php | ePHPm |
|---|---|---|
| Memory (idle) | ~150-300 MB | ~30 MB |
| Memory (under load) | ~30-50 MB per connection | ~50 MB shared |
| Config files | `httpd.conf` + `.htaccess` per directory | One `ephpm.toml` |
| TLS setup | Install certbot, configure cron, restart Apache | Two lines of config |
| Deployment | Install Apache + mod_php + restart | Copy one binary |
| Static file performance | Moderate (prefork overhead) | Fast (async I/O) |
| PHP version upgrade | `apt install`, restart Apache | Download new binary |

## What You Lose

- **`.htaccess` per-directory overrides** — ePHPm uses one config file. If you rely on `.htaccess` in subdirectories for different rules, consolidate them into `ephpm.toml`.
- **Apache modules** — `mod_security`, `mod_pagespeed`, etc. are not available. Most can be replaced by application-level solutions or ePHPm features.
- **CGI scripts** — ePHPm only runs PHP. If you have Perl/Python CGI scripts, they need a separate solution.

## WordPress-Specific Notes

WordPress works out of the box. The default `fallback` config handles pretty permalinks. Key settings:

```toml
[server]
document_root = "/var/www/wordpress"
fallback = ["$uri", "$uri/", "/index.php?$query_string"]

[server.request]
max_body_size = 67108864   # 64 MB for media uploads

[php]
max_execution_time = 120   # longer for admin operations
memory_limit = "256M"
```

Remove the `.htaccess` file from your WordPress root — it's not needed.

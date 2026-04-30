+++
title = "WordPress"
weight = 1
+++

ePHPm runs WordPress as a drop-in. No PHP-FPM, no MySQL server, no reverse proxy. One binary, one config, one directory of WordPress files.

## 1. Get WordPress

```bash
sudo mkdir -p /var/www/wordpress
cd /var/www/wordpress
sudo curl -fsSL https://wordpress.org/latest.tar.gz | sudo tar xz --strip-components=1
sudo chown -R ephpm:ephpm /var/www/wordpress   # match your service user
```

## 2. Configure ePHPm

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/wordpress"
index_files = ["index.php"]

# Pretty permalinks: try the URI as a file → as a dir → fall back to index.php
fallback = ["$uri", "$uri/", "/index.php?$query_string"]

[server.security]
# Block direct PHP execution outside of the WordPress entry points.
allowed_php_paths = [
  "/index.php",
  "/wp-login.php",
  "/wp-admin/*.php",
  "/wp-cron.php",
  "/wp-comments-post.php",
  "/xmlrpc.php",
  "/wp-trackback.php",
]
blocked_paths = ["/wp-config.php", "/.htaccess", "/.env"]

[php]
memory_limit = "256M"
max_execution_time = 60

# Embedded SQLite — WordPress thinks it's MySQL, it's actually SQLite.
[db.sqlite]
path = "/var/lib/ephpm/wordpress.db"
```

Restart the service:

```bash
sudo systemctl restart ephpm
```

## 3. Tell WordPress about the database

`wp-config.php`:

```php
define('DB_HOST',     '127.0.0.1');
define('DB_USER',     'ephpm');
define('DB_PASSWORD', '');
define('DB_NAME',     'wordpress');
```

PHP connects to `127.0.0.1:3306` via `pdo_mysql`. ePHPm answers with [litewire](https://github.com/ephpm/litewire), which translates MySQL wire to SQLite. WordPress doesn't know.

If you'd rather use a real MySQL server, swap `[db.sqlite]` for `[db.mysql]`:

```toml
[db.mysql]
url = "mysql://wpuser:secret@db.internal:3306/wordpress"
```

ePHPm becomes a connection-pooling MySQL proxy. PHP still connects to `127.0.0.1:3306`; nothing in `wp-config.php` changes.

## 4. Visit it

Open `http://your-server:8080/`. WordPress' setup wizard runs against the embedded database.

## WP-CLI

Use the embedded interpreter so the PHP version always matches:

```bash
ephpm php wp-cli.phar plugin list --path=/var/www/wordpress
ephpm php wp-cli.phar user create alice alice@example.com --role=administrator --path=/var/www/wordpress
```

## Multisite

Multi-tenant WordPress (one ePHPm process, many independent installs):

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/marketing"   # fallback for unmatched hosts
sites_dir = "/var/www/sites"
```

```
/var/www/sites/
├── alice-blog.com/
│   ├── index.php
│   └── wp-config.php       # uses ephpm.db in this directory
└── bobs-recipes.com/
    ├── index.php
    └── wp-config.php
```

The `Host:` header chooses the directory. With `[server.security] open_basedir = true` (default in vhost mode), each site's PHP is filesystem-sandboxed to its own directory. See [Architecture → Architecture](/docs/architecture/) for the vhost design.

## Add HTTPS

```toml
[server.tls]
domains = ["yourdomain.com", "www.yourdomain.com"]
email   = "admin@yourdomain.com"
cache_dir = "/var/lib/ephpm/certs"

# Optionally also keep an HTTP listener that 301-redirects:
listen = "0.0.0.0:443"
redirect_http = true
```

Set `[server] listen = "0.0.0.0:80"` for the HTTP redirect side.

## Performance

WordPress benefits from two things ePHPm does well:

- **Object cache** via the KV store. Drop in any WP plugin that supports Predis (`[kv.redis_compat] enabled = true`) or the `ephpm_kv_*` SAPI functions. See [KV from PHP](kv-from-php/).
- **Static asset serving** is handled directly in Rust with sendfile and ETag — PHP isn't involved for `.jpg`/`.css`/`.js`.

## See also

- [Apache mod_php → ePHPm migration](/docs/migration/apache-mod-php/)
- [Nginx + php-fpm → ePHPm migration](/docs/migration/nginx-php-fpm/)

+++
title = "Laravel"
weight = 2
+++

Laravel runs unmodified on ePHPm. Same `php artisan` commands, same `.env`, same routes — just no separate PHP-FPM and no database server to manage.

## 1. Configure ePHPm

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/myapp/public"
index_files = ["index.php"]

# Laravel routes through public/index.php for any URL it doesn't have a static asset for
fallback = ["$uri", "$uri/", "/index.php?$query_string"]

[php]
memory_limit = "256M"
max_execution_time = 60
ini_overrides = [
    ["display_errors", "Off"],
    ["error_reporting", "E_ALL"],
]

# Embedded SQLite — fine for dev and small/medium apps
[db.sqlite]
path = "/var/lib/ephpm/myapp.db"
```

`fallback` is the equivalent of `try_files $uri $uri/ /index.php?$query_string;` from Nginx — Laravel needs it for routing.

## 2. Configure Laravel

`.env`:

```dotenv
APP_URL=http://your-server:8080

DB_CONNECTION=mysql
DB_HOST=127.0.0.1
DB_PORT=3306
DB_DATABASE=laravel
DB_USERNAME=ephpm
DB_PASSWORD=
```

Laravel uses `pdo_mysql` to connect. ePHPm's [litewire](https://github.com/ephpm/litewire) layer accepts the connection on `127.0.0.1:3306` and routes it to the embedded SQLite — Laravel doesn't know the difference.

For a real MySQL backend, swap `[db.sqlite]` for `[db.mysql]`:

```toml
[db.mysql]
url = "mysql://laravel:secret@db.internal:3306/laravel"
```

ePHPm becomes a connection-pooling proxy and (with `[db.read_write_split] enabled = true`) handles read/write splitting if you have replicas.

## 3. Run migrations

```bash
ephpm php artisan migrate
```

`ephpm php` runs the embedded interpreter. Same PHP version, same extensions, same `php.ini` overrides as the live server — no version drift between CLI and web.

## 4. Start serving

```bash
sudo ephpm restart
```

## Queues and the scheduler

Run them as separate systemd services pointing at `ephpm php`:

```ini
# /etc/systemd/system/myapp-queue.service
[Service]
ExecStart=/usr/local/bin/ephpm php /var/www/myapp/artisan queue:work --sleep=3 --tries=3
Restart=always
```

```ini
# /etc/systemd/system/myapp-scheduler.timer
[Timer]
OnCalendar=*:*:00      # once a minute
Unit=myapp-scheduler.service

# /etc/systemd/system/myapp-scheduler.service
[Service]
ExecStart=/usr/local/bin/ephpm php /var/www/myapp/artisan schedule:run
```

## Sessions in the KV store

`config/session.php` driver `redis`, plus enable the RESP listener:

```toml
[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"
```

Laravel speaks Redis to ePHPm's embedded KV store. No external Redis. See [KV from PHP](kv-from-php/) for the full picture.

## Octane?

You don't need it. ePHPm already keeps the PHP runtime resident across requests inside a single Rust process — that's the whole architecture. Octane (Swoole/RoadRunner) and ePHPm solve the same "skip the PHP boot per request" problem; pick one.

## See also

- [`ephpm php` reference](/reference/cli/php/)
- [Database guide](/architecture/database/)
- [Query stats with Prometheus](query-stats-prometheus/) — observability for Laravel's queries

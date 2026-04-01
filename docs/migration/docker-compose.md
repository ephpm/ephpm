# Migrating from Docker Compose

You're running the standard Docker PHP stack — `nginx`, `php-fpm`, `mysql`, and probably `redis` — all wired together with a `docker-compose.yml`. It works, but it's 4 containers, 4 config files, networking between them, volume management, and a compose file that grows more complex every quarter.

ePHPm replaces all four containers with one binary.

## What You're Replacing

```yaml
# What you have now (docker-compose.yml)
services:
  nginx:
    image: nginx:alpine
    volumes:
      - ./nginx.conf:/etc/nginx/conf.d/default.conf
      - ./src:/var/www/html
    ports:
      - "80:80"
      - "443:443"

  php:
    image: php:8.2-fpm
    volumes:
      - ./src:/var/www/html
      - ./php.ini:/usr/local/etc/php/php.ini

  mysql:
    image: mysql:8.0
    environment:
      MYSQL_ROOT_PASSWORD: secret
      MYSQL_DATABASE: app
    volumes:
      - mysql_data:/var/lib/mysql

  redis:
    image: redis:7-alpine
    volumes:
      - redis_data:/data

volumes:
  mysql_data:
  redis_data:
```

**That's 4 images to pull, 4 containers to run, 4 services to monitor, and config files for each.**

```toml
# What you replace it with (ephpm.toml)
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"

[db.sqlite]
path = "/var/www/data/app.db"

[kv]
memory_limit = "128MB"
```

**One binary. One config file. One process.**

## The Mapping

| Docker Container | What It Does | ePHPm Equivalent |
|-----------------|-------------|-----------------|
| `nginx` | HTTP server, static files, TLS | Built-in HTTP server with compression + ACME |
| `php-fpm` | PHP execution | Built-in PHP runtime (embedded via FFI) |
| `mysql` | Database | Built-in SQLite (via litewire) or DB proxy to external MySQL |
| `redis` | Cache, sessions | Built-in KV store (RESP compatible) |
| `docker-compose.yml` | Orchestration | Not needed — it's one process |
| Docker volumes | Data persistence | Regular files on disk |
| Docker networking | Service-to-service comms | Not needed — everything is in-process |

## Step-by-Step Migration

### 1. Export Your MySQL Data

```bash
# From your running docker-compose stack
docker compose exec mysql mysqldump -u root -psecret app > backup.sql
```

### 2. Copy Your Source Code

Your source is probably mounted from `./src`. Just keep it where it is — ePHPm serves it directly from disk.

### 3. Create ephpm.toml

Map your compose config to TOML:

**docker-compose.yml ➜ ephpm.toml:**

```yaml
# Docker: nginx port mapping
ports:
  - "80:80"
  - "443:443"
```
```toml
# ePHPm
[server]
listen = "0.0.0.0:80"

[server.tls]
acme_domains = ["example.com"]
```

```yaml
# Docker: PHP config
volumes:
  - ./php.ini:/usr/local/etc/php/php.ini
```
```toml
# ePHPm
[php]
memory_limit = "256M"
max_execution_time = 30
ini_overrides = [
    ["display_errors", "Off"],
]
```

```yaml
# Docker: MySQL
environment:
  MYSQL_DATABASE: app
```
```toml
# ePHPm: embedded SQLite (replaces MySQL container entirely)
[db.sqlite]
path = "/var/www/data/app.db"

# OR connect to external MySQL (replaces container with managed DB)
# [db.mysql]
# url = "mysql://root:secret@db-host:3306/app"
```

```yaml
# Docker: Redis
image: redis:7-alpine
```
```toml
# ePHPm: built-in KV store (replaces Redis container)
[kv]
memory_limit = "128MB"

# Optional: enable RESP protocol for redis-cli compatibility
# [kv.redis_compat]
# enabled = true
# listen = "127.0.0.1:6379"
```

### 4. Update Your Application's Environment

Your `.env` file probably has:

```env
DB_CONNECTION=mysql
DB_HOST=mysql
DB_PORT=3306
DB_DATABASE=app
DB_USERNAME=root
DB_PASSWORD=secret

REDIS_HOST=redis
REDIS_PORT=6379

CACHE_DRIVER=redis
SESSION_DRIVER=redis
```

**With ePHPm + SQLite:**

```env
DB_CONNECTION=mysql
DB_HOST=127.0.0.1
DB_PORT=3306
DB_DATABASE=app
DB_USERNAME=root
DB_PASSWORD=

CACHE_DRIVER=file
SESSION_DRIVER=file
```

ePHPm's litewire translates MySQL connections to SQLite transparently. The `DB_CONNECTION=mysql` setting stays — PHP thinks it's talking to MySQL.

For the KV store, use the `ephpm_kv_*` SAPI functions instead of predis/phpredis. Or enable RESP compatibility and point at `127.0.0.1:6379`.

### 5. Stop Docker, Start ePHPm

```bash
# Stop the Docker stack
docker compose down

# Start ePHPm
./ephpm --config ephpm.toml
```

### 6. Keep Docker for Development (Optional)

You can use Docker for development and ePHPm for production. They serve the same files. Or drop Docker entirely — ePHPm runs the same binary on your laptop as in production.

```bash
# Development: just run ePHPm locally
./ephpm --config ephpm.toml
# Visit http://localhost:8080
```

No `docker compose up`. No waiting for containers to start. No "which container has the error log?"

## Resource Comparison

On a 2 GB RAM VPS:

| | Docker Compose (4 containers) | ePHPm (1 binary) |
|---|---|---|
| Idle memory | ~500-800 MB | ~130 MB |
| Startup time | 10-30 seconds (pull + start 4 containers) | < 1 second |
| Disk usage | ~500 MB (4 images) | ~50 MB (1 binary) |
| Processes | 10+ (nginx workers + fpm workers + mysqld + redis-server) | 1 (+ PHP worker threads) |
| Log streams | 4 (one per container) | 1 |
| Config files | 4+ (nginx.conf, php.ini, my.cnf, docker-compose.yml) | 1 (ephpm.toml) |

On a 2 GB VPS, Docker Compose with 4 containers barely fits. ePHPm leaves over 1.5 GB free.

## What You Gain

- **Simplicity** — one process, one config, one log stream
- **Speed** — no container startup, no network hops between services, no FastCGI overhead
- **Resources** — 4-6x less memory, 10x less disk
- **No Docker dependency** — no Docker daemon, no image registry, no compose files
- **Same binary everywhere** — dev, CI, staging, production. No "works in my container" problems
- **Built-in database** — no MySQL container to manage, backup, and secure

## What You Lose

- **Container isolation** — services run in one process, not separate containers. For multi-tenant setups, ePHPm uses KV namespacing and filesystem isolation instead.
- **Docker ecosystem** — no Docker Hub, no Dockerfile, no multi-stage builds. Deployment is `scp` + `systemctl restart` (or git-based with switchboard).
- **MySQL features** — if you need stored procedures, triggers, or complex queries that don't translate to SQLite, use ePHPm's DB proxy to an external MySQL instead of the embedded SQLite.
- **Redis features** — ePHPm's KV store covers strings, TTL, and counters. If you need Redis streams, pub/sub, sorted sets, or Lua scripting, keep a Redis container alongside ePHPm.

## Keeping Docker for Other Services

ePHPm replaces the PHP stack, not everything. If your compose file includes other services (Elasticsearch, Mailhog, MinIO), keep those as containers and just remove the nginx/php/mysql/redis services:

```yaml
# Slimmed-down docker-compose.yml (alongside ePHPm)
services:
  elasticsearch:
    image: elasticsearch:8
    ports:
      - "9200:9200"

  mailhog:
    image: mailhog/mailhog
    ports:
      - "8025:8025"
```

ePHPm handles the PHP stack. Docker handles the rest.

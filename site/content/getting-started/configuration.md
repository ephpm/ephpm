+++
title = "Configuration"
weight = 3
+++

ePHPm reads a single TOML file (`ephpm.toml` by default) plus environment variables. Defaults are sane — most installations need very little config.

## Top-level structure

```toml
[server]    # HTTP server: listen address, document root, timeouts, TLS
[php]       # PHP runtime: workers, memory limit, ini overrides
[db]        # database backends: mysql/postgres proxy + embedded sqlite
[kv]        # built-in KV store
[cluster]   # gossip clustering
```

Every section is optional. Omit a section to use defaults; omit a key inside a section to use that key's default.

## A minimal config

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
```

Everything else (PHP, KV, etc.) initializes with defaults. The KV store on `127.0.0.1:6379` is only enabled when you explicitly turn on `[kv.redis_compat]`.

## A more typical config

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
index_files = ["index.php", "index.html"]

# Pretty permalinks: try the URI as a file, then as a dir, then route to index.php
fallback = ["$uri", "$uri/", "/index.php?$query_string"]

[server.metrics]
enabled = true                 # exposes /metrics for Prometheus
# path = "/metrics"            # default

[php]
memory_limit = "256M"
max_execution_time = 30
ini_overrides = [
    ["display_errors", "Off"],
    ["error_reporting", "E_ALL"],
]

# Embed SQLite (PHP connects via pdo_mysql to 127.0.0.1:3306)
[db.sqlite]
path = "/var/lib/ephpm/app.db"
```

## TLS

Manual cert + key:

```toml
[server.tls]
cert = "/etc/ssl/ephpm/fullchain.pem"
key  = "/etc/ssl/ephpm/privkey.pem"
```

Automatic Let's Encrypt:

```toml
[server.tls]
domains = ["example.com", "www.example.com"]
email   = "admin@example.com"
cache_dir = "/var/lib/ephpm/certs"
# staging = true               # use during testing to avoid rate limits
```

## Environment variable overrides

Every TOML key can be overridden with an `EPHPM_` env var. Section nesting uses double underscore:

```bash
EPHPM_SERVER__LISTEN=0.0.0.0:9090
EPHPM_PHP__MEMORY_LIMIT=512M
EPHPM_DB__SQLITE__REPLICATION__ROLE=primary
```

Precedence: env vars > TOML file > defaults. Useful in containers — bake a default `ephpm.toml` into the image, override per-environment via env vars.

## Where to go next

- [Reference → Configuration](/reference/config/) — exhaustive list of every key, type, and default.
- [Reference → Environment Variables](/reference/environment-variables/) — the `EPHPM_` mapping rules.
- [Guides](/guides/) — task-oriented configs (WordPress, Laravel, vhosts, TLS, clustering).

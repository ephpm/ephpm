+++
title = "Configuration"
weight = 2
+++

Every key in `ephpm.toml`, with type, default, and a short description. The source of truth is [`crates/ephpm-config/src/lib.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — if a field has been added there but not here, that's a doc bug.

All sections and keys are optional. Missing sections use defaults; `Config::default_config()` produces a fully working configuration.

## `[server]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `listen` | string | `"0.0.0.0:8080"` | Address to listen on. |
| `document_root` | path | `"."` | Document root for static files and PHP scripts. |
| `sites_dir` | path | (none) | Virtual host directory. Each subdirectory is named after a domain. Omit for single-site mode. |
| `index_files` | array of strings | `["index.php", "index.html"]` | Index file names to try when a directory is requested. |
| `fallback` | array of strings | `["$uri", "$uri/", "/index.php?$query_string"]` | URL fallback chain. Variables: `$uri`, `$query_string`. Last entry is the fallback (prefix `=` for status code, e.g. `=404`). |

### `[server.request]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_body_size` | u64 (bytes) | `10_485_760` (10 MiB) | Max request body. `0` = unlimited. Exceeding sends 413. |
| `max_header_size` | usize (bytes) | `8192` | Max total request header size. |
| `trusted_hosts` | array of strings | `[]` | Allowed `Host` header values. Empty = allow all. Mismatched hosts get 421. |

### `[server.timeouts]` (all in seconds)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `header_read` | u64 | `30` | Time to receive complete request headers after connect. |
| `idle` | u64 | `60` | Idle connection timeout. |
| `request` | u64 | `300` | Total request timeout including PHP execution. |
| `shutdown` | u64 | `30` | Grace period for in-flight connections during shutdown. |

### `[server.response]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `compression` | bool | `true` | Enable gzip compression for text responses. |
| `compression_level` | u32 | `1` | gzip level (1=fastest, 9=best). |
| `compression_min_size` | usize (bytes) | `1024` | Minimum response size before compression applies. |
| `headers` | array of `[string, string]` | `[]` | Custom headers added to every response. |

### `[server.static]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cache_control` | string | `""` | Cache-Control header value for static files. Empty = no header. |
| `hidden_files` | string | `"deny"` | How to handle dot-files: `"deny"` (403), `"ignore"` (404), `"allow"`. |
| `etag` | bool | `true` | Emit `ETag` headers and serve `304 Not Modified` on conditional requests. |

### `[server.php_etag_cache]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Cache PHP-emitted ETags in the KV store; serve 304s without re-running PHP. |
| `ttl_secs` | i64 | `300` | TTL for cached entries. `<=0` means cache indefinitely. |
| `key_prefix` | string | `"etag:"` | KV key prefix for cached entries. |

### `[server.security]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `trusted_proxies` | array of strings | `[]` | CIDR ranges trusted for `X-Forwarded-For`/`X-Forwarded-Proto`. |
| `blocked_paths` | array of strings | `[]` | Glob patterns blocked with 403. |
| `allowed_php_paths` | array of strings | `[]` | When non-empty, only matching PHP paths execute. Others get 403. |
| `open_basedir` | bool | `true` if `sites_dir` set, else `false` | Restrict PHP filesystem access to the site's document root. |
| `disable_shell_exec` | bool | `true` if `sites_dir` set, else `false` | Disable `exec`, `shell_exec`, `system`, `passthru`, `proc_open`, `popen`, `pcntl_exec`. |

### `[server.logging]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `access` | string | `""` | Path to access log file. Empty = disabled. |
| `level` | string | `"info"` | Log level: `trace`, `debug`, `info`, `warn`, `error`. Overridden by `RUST_LOG`. |

### `[server.metrics]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable the Prometheus `/metrics` endpoint. |
| `path` | string | `"/metrics"` | URL path for the metrics endpoint. |

### `[server.limits]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_connections` | usize | `0` | Total concurrent connections. `0` = unlimited. New connections beyond limit get 503. |
| `per_ip_max_connections` | usize | `0` | Per-IP concurrent connections. `0` = unlimited. |
| `per_ip_rate` | f64 | `0.0` | Per-IP requests/second (token bucket). `0` = unlimited. |
| `per_ip_burst` | u32 | `50` | Burst allowance for per-IP rate limiting. |

### `[server.file_cache]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | In-memory cache for static file metadata + small-file content. |
| `max_entries` | usize | `10_000` | Max cached entries. Oldest evicted on overflow. |
| `valid_secs` | u64 | `30` | Re-stat interval. |
| `inactive_secs` | u64 | `60` | Evict entries not accessed within this many seconds. |
| `inline_threshold` | usize (bytes) | `1_048_576` (1 MiB) | Cache file content below this size; metadata-only above. |
| `precompress` | bool | `true` | Pre-compute gzip-compressed variants for small compressible files. |

### `[server.tls]`

Two mutually exclusive modes — manual (`cert`+`key`) or ACME (`domains`). If both are set, manual wins.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cert` | path | (none) | PEM-encoded certificate chain (manual mode). |
| `key` | path | (none) | PEM-encoded private key (manual mode). |
| `domains` | array of strings | `[]` | Domains for ACME / Let's Encrypt (auto mode). |
| `email` | string | (none) | Contact email for ACME registration. |
| `cache_dir` | path | `"certs"` | Directory for ACME cert + account key cache. **Set this in production.** |
| `staging` | bool | `false` | Use Let's Encrypt staging (untrusted certs, generous rate limits). |
| `listen` | string | (none) | Separate HTTPS listener. When set, `[server] listen` serves HTTP and this serves HTTPS. |
| `redirect_http` | bool | `false` | When `listen` is set, the HTTP listener redirects everything to HTTPS (301). |

## `[php]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_execution_time` | u32 (sec) | `30` | PHP `max_execution_time` per request. |
| `memory_limit` | string | `"128M"` | PHP `memory_limit`. |
| `ini_file` | path | (none) | Custom `php.ini` loaded before `ini_overrides`. |
| `ini_overrides` | array of `[string, string]` | `[]` | INI directives applied after `ini_file`. |
| `workers` | usize | `min(logical_cpus, 16)` | Dedicated PHP worker threads. |

## `[db]`

### `[db.mysql]` / `[db.postgres]` / `[db.tds]`

All three share the same backend config schema. Adding a section enables the proxy.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | (required) | Connection URL: `mysql://user:pass@host:port/db`, `postgres://...`. |
| `listen` | string | `"127.0.0.1:3306"` (mysql), `"127.0.0.1:5432"` (postgres) | TCP address PHP connects to. |
| `socket` | path | (none) | Unix socket path (faster than TCP for local PHP). |
| `min_connections` | u32 | `2` | Warm pool size (idle connections kept open). |
| `max_connections` | u32 | `20` | Max total backend connections. |
| `idle_timeout` | duration string | `"300s"` | Close idle backend connections after this. |
| `max_lifetime` | duration string | `"1800s"` | Recycle connections older than this. |
| `pool_timeout` | duration string | `"5s"` | Time to wait for a connection before failing. |
| `health_check_interval` | duration string | `"30s"` | Frequency of backend health checks. |
| `inject_env` | bool | `true` | Inject `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`, `DB_PASSWORD`, `DATABASE_URL` into PHP. |
| `reset_strategy` | string | `"smart"` | `"smart"` (reset after non-SELECT), `"always"`, `"never"`. |
| `replicas.urls` | array of strings | `[]` | Read replica URLs. Reads distributed across; writes go to primary. |

### `[db.sqlite]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `path` | string | `"ephpm.db"` | SQLite database file path. |

#### `[db.sqlite.proxy]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mysql_listen` | string | `"127.0.0.1:3306"` | MySQL wire protocol address (PHP connects here with `pdo_mysql`). |
| `hrana_listen` | string | (none) | Hrana HTTP API listener. |
| `postgres_listen` | string | (none) | PostgreSQL wire protocol listener. |
| `tds_listen` | string | (none) | TDS (SQL Server) wire protocol listener. |

#### `[db.sqlite.sqld]` (clustered mode only)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `http_listen` | string | `"127.0.0.1:8081"` | sqld HTTP listener (litewire → sqld). |
| `grpc_listen` | string | `"0.0.0.0:5001"` | sqld gRPC listener (inter-node replication). |

#### `[db.sqlite.replication]` (clustered mode only)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `role` | string | `"auto"` | `"auto"` (gossip-elected), `"primary"`, `"replica"`. |
| `primary_grpc_url` | string | `""` | Primary gRPC URL (set automatically in `auto` mode; required for `replica`). |

### `[db.read_write_split]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable R/W splitting. Requires backend with `replicas`. |
| `strategy` | string | `"sticky-after-write"` | After a write, reads stick to primary for `sticky_duration`. `"lag-aware"` is parsed but **not yet implemented**. |
| `sticky_duration` | duration string | `"2s"` | How long reads stay on primary after a write. |
| `max_replica_lag` | duration string | `"500ms"` | **Not yet implemented** — parsed but unused. |

### `[db.analysis]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `query_stats` | bool | `true` | Track per-digest timing/throughput metrics. |
| `slow_query_threshold` | duration string | `"1s"` | Queries exceeding this are logged at WARN. |
| `auto_explain` | bool | `false` | **Not yet implemented** — parsed but unused. |
| `auto_explain_target` | string | `"stderr"` | **Not yet implemented**. |
| `digest_store_max_entries` | usize | `100_000` | Max in-memory query digests; oldest evicted on overflow. |

## `[kv]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `memory_limit` | string | `"256MB"` | Max memory for the KV store. |
| `eviction_policy` | string | `"allkeys-lru"` | `noeviction`, `allkeys-lru`, `volatile-lru`, `allkeys-random`. |
| `compression` | string | `"none"` | `none`, `gzip`, `brotli`, `zstd`. |
| `compression_level` | u32 | `6` | 1=fastest, 9=best. |
| `compression_min_size` | usize (bytes) | `1024` | Values below this are stored uncompressed. |
| `secret` | string | (none) | Master secret for per-site RESP AUTH. Auto-generated if absent. |

### `[kv.redis_compat]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable the RESP listener. Off by default; in multi-tenant mode keep it off. |
| `listen` | string | `"127.0.0.1:6379"` | RESP listener address. |
| `socket` | string | (none) | **Not yet implemented** — parsed but unused. |
| `password` | string | (none) | RESP `AUTH` password. |

## `[cluster]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable gossip clustering. |
| `bind` | string | `"0.0.0.0:7946"` | Gossip UDP listener. |
| `join` | array of strings | `[]` | Seed addresses for initial cluster join. |
| `secret` | base64 string | `""` | 32-byte symmetric key for gossip encryption. |
| `node_id` | string | (auto) | Unique node identifier. Auto-generated if empty. |
| `cluster_id` | string | `"ephpm"` | Nodes with different `cluster_id`s ignore each other. |

### `[cluster.kv]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `small_key_threshold` | usize (bytes) | `512` | Boundary between gossip tier and TCP data plane. |
| `replication_factor` | usize | `2` | Replicas for large-tier values. |
| `replication_mode` | string | `"async"` | `"async"` or `"sync"`. |
| `hot_key_cache` | bool | `true` | Promote frequently-fetched remote values to a local cache. |
| `hot_key_threshold` | u32 | `5` | Remote fetches in `hot_key_window_secs` before promotion. |
| `hot_key_window_secs` | u64 | `10` | Window for counting fetches. |
| `hot_key_local_ttl_secs` | u64 | `30` | Max age of cached hot-key values. |
| `hot_key_max_memory` | string | `"64MB"` | Memory budget for hot-key cache. |
| `data_port` | u16 | `7947` | TCP listener for the KV data plane. |

## See also

- [Environment variables](environment-variables/) — how to override any of these via `EPHPM_*`
- [`crates/ephpm-config/src/lib.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — definitive source

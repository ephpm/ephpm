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
| `sites_domain_suffix` | string | (none) | Suffix stripped from the `Host` header before resolving vhosts against `sites_dir` (e.g. `".localhost"` maps `blog.localhost` → `<sites_dir>/blog`). Used by `ephpm dev --sites`. |
| `index_files` | array of strings | `["index.php", "index.html"]` | Index file names to try when a directory is requested. |
| `fallback` | array of strings | `["$uri", "$uri/", "/index.php?$query_string"]` | URL fallback chain. Variables: `$uri`, `$query_string`. Last entry is the fallback (prefix `=` for status code, e.g. `=404`). |

### `[server.request]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_body_size` | u64 (bytes) | `10_485_760` (10 MiB) | Max request body. `0` = unlimited. Exceeding sends 413. |
| `max_header_size` | usize (bytes) | `8192` | Max total request header size. |
| `trusted_hosts` | array of strings | `[]` | Allowed `Host` header values. Empty = allow all. Mismatched hosts get 421. `/_ephpm/health`, `/_ephpm/ready`, and the metrics path are exempt (probes/scrapes address pods by IP). |

### `[server.timeouts]` (all in seconds)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `header_read` | u64 | `30` | Time to receive complete request headers after connect. |
| `idle` | u64 | `60` | Idle connection timeout. |
| `request` | u64 | `300` | Total request timeout including PHP execution. `0` disables the per-request deadline (the router skips arming a tokio timer per request); a stuck request then relies on the idle/header-read timeouts instead of a hard cutoff. |
| `shutdown` | u64 | `30` | Grace period for in-flight connections during shutdown. |

### `[server.response]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `compression` | bool | `true` | Enable compression for text responses — brotli when the client accepts it, gzip fallback. |
| `compression_level` | u32 | `1` | Compression level (1=fastest, 9=best). |
| `compression_min_size` | usize (bytes) | `1024` | Minimum response size before compression applies. |
| `compression_streaming` | string | `"off"` | Streamed worker-response (`send_response_stream`) compression: `"off"` (identity, byte-for-byte the previous behavior), `"sse"` (brotli with a per-event flush and a stream-lifetime window for `text/event-stream` responses), `"all"` (every streamed response). Needs `compression = true` and a client `Accept-Encoding: br`; unknown values warn at startup and act as `"off"`. Buffered responses are unaffected. |
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
| `open_basedir` | bool | `true` if a `[server.security]` section is present **or** `server.sites_dir` is set, else `false` | Restrict PHP filesystem access to the site's document root. |
| `disable_shell_exec` | bool | `true` if a `[server.security]` section is present **or** `server.sites_dir` is set, else `false` | Disable `exec`, `shell_exec`, `system`, `passthru`, `proc_open`, `popen`, `pcntl_exec`. |

**Note:** an explicitly set value always wins. When unset, these two resolve to `true` if either the `[server.security]` section is present (matching earlier releases) or `server.sites_dir` is set — so multi-tenant deployments get filesystem isolation and shell-exec hardening by default, even without a `[server.security]` section. To opt out in multi-tenant mode you must set them to `false` explicitly (ephpm logs a warning at startup when you do).

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
| `memory_limit` | string | `"128M"` | PHP `memory_limit`. Serves as the dev-mode value and the ultimate fallback; in serve mode it is superseded by the auto-derived per-request limit (see `php_memory_limit` and [Resource-aware autotuning](#resource-aware-autotuning)). |
| `opcache_validate_timestamps` | bool | (mode default) | Override `opcache.validate_timestamps`. Unset resolves per mode: **off** (`0`) under `ephpm serve` (trust the cache — refresh code with `ephpm deploy` / `ephpm cache reset`), **on** (`1`) under `ephpm dev` (instant edit-refresh). Set `true`/`false` to force a value in either mode. See the [deploy guide](/guides/opcache-cluster-invalidation/) for the deploys-are-events contract. |
| `opcache_revalidate_freq` | u32 (sec) | (none → PHP default `2`) | Override `opcache.revalidate_freq`. Only meaningful when timestamp validation is on: how often (at most) the engine re-`stat()`s a cached script. Raising it (e.g. `60`) cuts `stat()` traffic on overlay/network filesystems at the cost of slower edit pickup. Ignored when validation is off. |
| `opcache_memory_consumption` | u32 (MB) | (auto-derived) | Override `opcache.memory_consumption`. Unset → auto-derived in serve mode (~18% of the detected memory budget, clamped `[64, 512]` MB); dev keeps PHP's 128 MB. See [Resource-aware autotuning](#resource-aware-autotuning). |
| `opcache_interned_strings_buffer` | u32 (MB) | (auto-derived) | Override `opcache.interned_strings_buffer`. Unset → auto-derived (~1 MB per 16 MB of opcache SHM, clamped `[8, 64]` MB) in serve mode; PHP default in dev. |
| `opcache_jit_buffer_size` | u32 (MB) | (auto-derived) | Override `opcache.jit_buffer_size`. Unset → auto-sized (~1/64 of memory, clamped `[32, 64]` MB) in serve mode. **Sizes the buffer only — JIT is NOT auto-enabled** (`opcache.jit` stays at PHP's default; opt in via `ini_overrides`). JIT helps CPU-bound work but can regress I/O-bound web apps, so auto-enable is a separate benched decision. |
| `opcache_max_accelerated_files` | u32 | `20000` (serve) | Override `opcache.max_accelerated_files`. A generous **fixed** default in serve mode (PHP default in dev). Deliberately *not* derived from memory — the right value is shaped by how many `.php` files the app has, not the machine size. |
| `php_memory_limit` | string | (auto-derived) | Override the per-request `memory_limit`, taking precedence over `memory_limit` **and** the derivation. Unset → serve mode derives `(memory_budget − opcache_shm − ~64 MB overhead) / worker_count`, floored at `128 MB`; with no detectable memory budget it keeps PHP's `128M`. Dev keeps `memory_limit`. |
| `realpath_cache_size` | string | `16M` (serve) | Override `realpath_cache_size`. Serve uses `16M` (vs PHP's `256K`) to cut `realpath()`/`stat()` traffic on deep autoload trees; dev keeps the PHP default so new files resolve instantly. |
| `realpath_cache_ttl` | u32 (sec) | `600` (serve) | Override `realpath_cache_ttl`. Serve uses `600` (vs PHP's `120`); dev keeps the PHP default. |
| `zend_assertions` | i8 | `-1` (serve) / `1` (dev) | Override `zend.assertions`. Serve uses `-1` (assertions compiled out — zero runtime cost, production-recommended); dev uses `1` (assertions active). Set `-1`/`0`/`1` to pin. |
| `ini_file` | path | (none) | Custom `php.ini` loaded before `ini_overrides`. |
| `ini_overrides` | array of `[string, string]` | `[]` | INI directives applied after `ini_file`. In worker mode, `log_errors=On` is seeded as a default before `ini_file`/`ini_overrides` (either can override it) so worker-script fatals reach the engine log — `display_errors` output is captured into a buffer that is discarded when no request is in flight. |
| `extensions` | array of string | `[]` | Shared PHP extensions loaded at startup as `extension=` lines in the generated php.ini, emitted **before** `ini_file`/`ini_overrides`. Bare names (`"redis"`) use PHP's `extension_dir` search; paths load verbatim. Must match the embedded PHP's ABI: same PHP minor, ZTS (Linux/macOS) / NTS (Windows), glibc on Linux — PHP reports a mismatch at startup. Note distro/[Sury](https://deb.sury.org/) extension packages are NTS-only (no ZTS variants as of July 2026) — on Linux, compile the extension for ZTS (phpize/gcc against matching ZTS headers). Empty entries fail validation. See the [PHP Extensions guide](/guides/php-extensions/). |
| `workers` | usize | `0` (unlimited) | Max concurrent PHP executions (php-fpm `pm.max_children` semantics); excess requests queue. `0` = unlimited. **Ignored in worker mode** (startup logs a WARN if set). |
| `mode` | string | `"fpm"` | Request-execution model. `"fpm"` = per-request startup/shutdown (default, unchanged). `"worker"` = persistent worker mode: boot the framework once per worker, loop over requests (Octane/RoadRunner model). |
| `worker_script` | path | (none) | Worker-mode entrypoint, relative to `document_root`. **Required** when `mode = "worker"`; config load hard-errors if absent or not a file under `document_root`. |
| `worker_count` | usize | `0` (derive) | Number of persistent worker threads. `0` derives from the cgroup CPU quota when running under one (Linux), otherwise from host parallelism clamped `[2, 32]`. Forced to `1` on Windows (NTS, single PHP context). Startup logs the derivation source. Worker mode only. |
| `worker_max_requests` | u64 | `10000` | Recycle a worker after N requests — pure leak guard for the framework kernel. For a leak-free loop, recycling is pure overhead (framework re-boot cost); prefer `0` when you trust your kernel. Each recycle is logged at debug (worker id, requests served, uptime). Worker mode only. |
| `worker_backlog` | usize | `0` (= `worker_count`) | Dispatch-queue depth. A full queue applies backpressure; a starved queue becomes a 504 via the request timeout. Worker mode only. |
| `worker_boot_timeout` | u64 (sec) | `30` | Seconds a worker gets to boot and reach its first `take_request()`. A boot still running when this expires is logged as an error and counted in `ephpm_worker_boot_timeouts_total`; the thread is not killed and still becomes ready if the boot completes. (A boot that *fails* — the script exits before its first `take_request()` — is counted as a boot failure and respawned with backoff, independent of this timeout.) Worker mode only. |
| `worker_populate_superglobals` | bool | `false` | Populate native `$_GET`/`$_POST`/`$_SERVER`/... per request. Off for Octane/PSR-15 (they build their own request); on for the WordPress adapter. Worker mode only. |
| `worker_stream_threshold` | u64 (bytes) | `1048576` (1 MiB) | Request-body size at/above which the body **streams** into the worker in fixed-size chunks instead of buffering whole (Phase 3). Requests with a `Content-Length` at/above this — or with no `Content-Length` (chunked) — flow through `Envelope::bodyStream()` / PHP's POST reader with flat worker memory (a multi-GB upload never materializes in RAM). Smaller bodies stay buffered. Worker mode only. |

> **Worker mode is not supported with `[server] sites_dir`** (multi-tenant vhosting) in Phase 1 — config load hard-errors. Worker mode boots one framework per worker; per-host worker pools are a later phase.

### Resource-aware autotuning

On boot, `ephpm serve` detects the container's CPU and memory limits (cgroup-aware) and derives a tuned set of PHP/OPcache ini defaults sized to the box it is actually running on. This is the *deploys-are-events, right-size-the-runtime* story: you ship the same image to a 320 MiB / 0.25-CPU pod and a 4 GiB / 4-CPU node, and each one sizes OPcache, the per-request memory limit, interned-string and JIT buffers, and the realpath cache to fit — without a per-environment config file.

**Detection (Linux):**

1. **CPU quota** — cgroup v2 `cpu.max`, else v1 `cpu.cfs_quota_us`/`cpu.cfs_period_us`. `None` when unlimited. (Already drives `worker_count`.)
2. **Memory budget** — cgroup v2 `/sys/fs/cgroup/memory.max`, else v1 `memory.limit_in_bytes`; `"max"`/the unlimited sentinel means no limit, in which case ePHPm falls back to total system memory (`/proc/meminfo` `MemTotal`). No new crate — it reads the same cgroupfs/`/proc` files as the CPU path.

Non-Linux platforms have no cgroup limit and keep PHP defaults for memory-shaped knobs.

**Derivation (serve mode):**

| Directive | Formula | Clamp |
|-----------|---------|-------|
| `opcache.memory_consumption` | ~18% of memory budget | `[64, 512]` MB |
| `opcache.interned_strings_buffer` | ~1 MB per 16 MB of opcache SHM | `[8, 64]` MB |
| `opcache.jit_buffer_size` | ~1/64 of memory budget (**JIT stays off**) | `[32, 64]` MB |
| `opcache.max_accelerated_files` | fixed `20000` (app-shaped, not memory-shaped) | — |
| `memory_limit` (per request) | `(budget − opcache_shm − ~64 MB overhead) / worker_count` | floor `128` MB |
| `realpath_cache_size` | `16M` | — |
| `realpath_cache_ttl` | `600` | — |
| `zend.assertions` | `-1` (compiled out) | — |

Dev mode (`ephpm dev` / bare `ephpm`) derives none of these: it keeps PHP-friendly defaults (timestamp validation on, assertions on, loose realpath) so the edit-refresh loop stays tight.

**Resolution precedence (per directive):** explicit `[php]` value → auto-derived → PHP stock default. Pin any single knob (e.g. `opcache_memory_consumption = 256`) and the rest keep auto-tuning. `ini_overrides` still layers last as the ultimate escape hatch.

**Transparency:** serve startup logs one INFO line summarizing what was detected and derived, marking any explicitly-pinned value with a `*`. Example for a 320 MiB / 0.25-CPU pod:

```
autotune (serve): cpu_quota=0.25 mem=320MiB (cgroup v2) -> workers=1[cgroup_quota] opcache.memory_consumption=64MB memory_limit=192M interned=8MB jit_buffer=32MB (buffer-only, jit off) max_files=20000 realpath=16M/ttl=600 validate_timestamps=0 assertions=-1
```

and for a 4 GiB / 4-CPU node:

```
autotune (serve): cpu_quota=4.00 mem=4096MiB (cgroup v2) -> workers=4[cgroup_quota] opcache.memory_consumption=512MB memory_limit=880M interned=32MB jit_buffer=64MB (buffer-only, jit off) max_files=20000 realpath=16M/ttl=600 validate_timestamps=0 assertions=-1
```

## `[db]`

### `[db.mysql]` / `[db.postgres]` / `[db.tds]`

All three share the same backend config schema. Adding a `[db.mysql]` or `[db.postgres]` section enables that proxy. The TDS proxy is **not yet implemented** — a `[db.tds]` section is accepted, but startup logs a warning and skips it.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | (required) | Connection URL: `mysql://user:pass@host:port/db`, `postgres://...`. |
| `listen` | string | `"127.0.0.1:3306"` (mysql), `"127.0.0.1:5432"` (postgres) | TCP address PHP connects to. |
| `socket` | path | (none) | **Planned** — Unix socket path. Currently only accepted for the MySQL proxy and not yet wired there; PostgreSQL ignores it. |
| `min_connections` | u32 | `2` | Warm pool size (idle connections kept open). |
| `max_connections` | u32 | `20` | Max total backend connections. |
| `idle_timeout` | duration string | `"300s"` | Close idle backend connections after this. |
| `max_lifetime` | duration string | `"1800s"` | Recycle connections older than this. |
| `pool_timeout` | duration string | `"5s"` | Time to wait for a connection before failing. |
| `health_check_interval` | duration string | `"30s"` | Frequency of backend health checks. |
| `inject_env` | bool | `true` | Inject `DB_CONNECTION`, `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`, `DB_PASSWORD`, `DATABASE_URL` into PHP. |
| `reset_strategy` | string | `"smart"` | `"smart"` (reset after non-SELECT), `"always"`, `"never"`. On PostgreSQL it also decides whether [query stats](#dbanalysis) see proxied statements — `"always"`/`"never"` without replicas take an opaque splice and record nothing. The MySQL proxy frames every session regardless, so its stats coverage does not depend on this knob. |
| `replicas.urls` | array of strings | `[]` | Read replica URLs. Reads distributed across; writes go to primary. |

> **The proxy listener is unauthenticated — keep it on loopback.**
> The proxy does not validate client credentials. The MySQL proxy reads the client handshake response and discards it; the PostgreSQL proxy answers any startup message with `AuthenticationOk`. The real credentials in `url` are used only for the proxy's own pooled connections to the backend, and are never required of the client.
>
> Binding `listen` to a non-loopback address therefore gives full read/write access to your database to any host that can reach the port. ePHPm logs a startup warning when `listen` is a non-loopback IP literal (`0.0.0.0:3306`, `10.0.0.5:3306`, …), but does **not** refuse to start — binding `0.0.0.0` inside a container that is firewalled by a network policy is a legitimate deployment. Addresses given as hostnames (`localhost:3306`, `db.internal:3306`) are not classified, because that would require a DNS lookup at startup; no warning is logged for them either way.

#### Startup: the upstream does not have to be reachable

The proxy binds `listen` at startup and reaches `url` from a background task,
retrying forever with exponential backoff (250 ms doubling to a 30 s ceiling).
A database that is slower to start than ePHPm — or that restarts later — is
picked up when it appears; no ePHPm restart is needed. Clients that connect
during the window queue in the kernel accept backlog rather than getting
`ECONNREFUSED`, so PHP's first request waits instead of erroring.

Two things *are* fatal at startup, because both are configuration errors: a
`url` that cannot be parsed, and a `listen` address that cannot be bound.

#### Readiness and the database proxy

`/_ephpm/ready` reports **503 until every configured proxy has reached its
upstream once**. A process whose proxy has never connected cannot serve a
single query, so it must stay out of load-balancer rotation, and a rollout
containing such a pod should stall rather than replace healthy pods.

After that first success, readiness never flaps on upstream state again. This
is deliberate: gating readiness on live database reachability would fail every
replica's probe at the same instant during a shared-database outage, empty the
Service, and turn a degraded database into a total outage — including for the
static assets and non-DB routes those pods could still serve. Liveness
(`/_ephpm/health`) stays green throughout; restarting the process does not
bring a remote database back.

A post-startup outage is reported instead of routed around:
`ephpm_db_proxy_upstream_up` drops to `0`,
`ephpm_db_proxy_connect_failures_total` climbs, and the proxy logs at ERROR
(throttled to one line per minute). See
[Metrics → Database (proxy upstream health)](/reference/metrics/#database-proxy-upstream-health).

### `[db.sqlite]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `path` | string | `"ephpm.db"` | SQLite database file path. |
| `engine` | string | `"sqlite"` | **Experimental knob.** `"sqlite"` = the genuine SQLite C engine (default, production-supported). `"turso"` = the [Turso Database](https://github.com/tursodatabase/turso) engine (Rust rewrite of SQLite, **Beta upstream — experimental, not for production data**; single-node only, rejected at startup in clustered mode; `VACUUM` and multi-process access unsupported). See the [Turso engine roadmap](/roadmap/turso-engine/). |

#### `[db.sqlite.proxy]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mysql_listen` | string | `"127.0.0.1:3306"` | MySQL wire protocol address (PHP connects here with `pdo_mysql`). |
| `hrana_listen` | string | (none) | Hrana HTTP API listener. |
| `postgres_listen` | string | (none) | PostgreSQL wire protocol listener. |
| `tds_listen` | string | (none) | TDS (SQL Server) wire protocol listener. |
| `max_connections` | integer | `0` (unlimited) | Cap on concurrent wire connections across the MySQL/PostgreSQL/TDS frontends combined. Beyond the cap, connections are refused at accept time (MySQL clients get error 1040 "Too many connections"), never queued. Each wire session holds one OS thread, so this also bounds those threads. Hrana (stateless HTTP) is not counted. Same semantics as `[db.mysql] max_connections`. |

> **These listeners are unauthenticated too.** litewire's MySQL, Hrana, PostgreSQL, and TDS frontends accept any client — the PostgreSQL frontend is explicitly wired to a no-op startup handler, and the others never ask for credentials. The design assumes only PHP inside this process reaches them. As with `[db.mysql]`, each of these four keys is checked at startup and a non-loopback IP literal logs a warning naming the risk; startup is not blocked. Bind loopback unless the port is firewalled from untrusted networks.

#### `[db.sqlite.sqld]` (clustered mode only)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `http_listen` | string | `"127.0.0.1:8081"` | sqld HTTP listener (litewire → sqld). |
| `grpc_listen` | string | `"0.0.0.0:5001"` | sqld gRPC listener (inter-node replication). |
| `write_permits` | integer | `0` (unlimited) | Maximum writes in flight against sqld at once. **Default 0 (off) in v0.6.x; planned default 1 in v0.7.0 — a single permit already saturates sqld's single writer and prevents the c>=8 write collapse ([issue #217](https://github.com/ephpm/ephpm/issues/217)).** SQLite has one writer; past ~4 concurrent writers sqld queues them so badly that requests stop completing altogether. Setting this admits `n` writes and queues the rest FIFO — never refused, only delayed (a queued write fails only after litewire's 30s acquire timeout, as a retriable `SQLITE_BUSY`). Reads are never capped. Clustered SQLite only: single-node SQLite and the MySQL proxy ignore it. Values above `1` do not raise throughput (SQLite serializes writes regardless) and `8` reproduces the collapse; `1` is also required for workloads dominated by multi-statement explicit transactions, since a transaction holds its permit until `COMMIT`. Full measurements and reasoning: [From zero to a plateau](/benchmarking/findings/#from-zero-to-a-plateau-write-admission-for-sqld) and the [v0.6.1 results](/benchmarking/results/#v061--the-clustered-sqld-write-collapse-fixed). |

#### `[db.sqlite.replication]` (clustered mode only)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `role` | string | `"auto"` | `"auto"` (gossip-elected), `"primary"`, `"replica"`. |
| `primary_grpc_url` | string | `""` | Primary gRPC URL (set automatically in `auto` mode; required for `replica`). In CDC-native mode (`cdc_experimental = true`) this field carries the primary's **cluster channel address** (e.g. `10.0.0.1:7948` — the channel defaults to the gossip port + 2) instead of a gRPC URL. |
| `cdc_experimental` | bool | `false` | **Experimental** — opt in to Phase 2 CDC-native replication (`engine = "turso"` only). Setting this to `true` also implicitly enables the [cluster channel](#clusterchannel) on this node. See the [Turso engine roadmap](/roadmap/turso-engine/#phase-2--cdc-native-replication-experimental-implementation-available-gated-on-ga-for-default) and the [cluster channel design](/roadmap/cluster-channel/). Without this flag, `engine = "turso"` + clustered mode is a hard startup error. |
| `max_snapshot_bytes` | u64 (bytes) | `1073741824` (1 GiB) | Largest snapshot-bootstrap payload a cold replica will accept from the primary. Only used on the CDC-native path. Both the length the primary advertises and the running total of received chunks are checked against it, so a peer cannot exhaust the replica's memory by claiming an absurd size or streaming without an end marker. Bootstrap fails with a message naming this knob when a legitimate dump is larger. |

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
| `query_stats` | bool | `true` | Track per-digest timing/throughput metrics. Applies to the embedded SQLite paths **and** the MySQL/PostgreSQL proxies — see the coverage table below. |
| `slow_query_threshold` | duration string | `"1s"` | Queries exceeding this are logged at WARN. |
| `auto_explain` | bool | `false` | **Not yet implemented** — parsed but unused. |
| `auto_explain_target` | string | `"stderr"` | **Not yet implemented**. |
| `digest_store_max_entries` | usize | `100_000` | Max in-memory query digests; oldest evicted on overflow. |
| `metric_label_series_max` | usize | `1000` | Max distinct `digest` label values emitted to Prometheus; overflow folds into `digest="__other__"`. `0` = unlimited. |

#### What query stats cover

One collector serves every database path, so proxied and embedded queries land on the same metric names with no extra label to distinguish them. What each path can *see* differs, because the proxy only observes statements at the points where it already parses the wire protocol:

| Path | Statements recorded | Duration measured | Row counts |
|------|--------------------|-------------------|------------|
| Embedded SQLite (`[db.sqlite]`, any engine or replication mode) | All queries and mutations | In-process execution time | Rows returned / rows affected |
| MySQL proxy, single-backend path (any reset strategy) | `COM_QUERY` only | Wire round trip: command written to the backend → last response byte read back | Not available |
| MySQL proxy, R/W splitting enabled with replicas | `COM_QUERY` and `COM_STMT_EXECUTE` | Same wire round trip | Rows returned, or affected rows from the `OK` packet |
| PostgreSQL proxy, `reset_strategy = "smart"` (default) or R/W splitting | Simple `Query` messages | Wire round trip: message written → `ReadyForQuery` | Rows returned (`DataRow` count); mutations record `0` |
| PostgreSQL proxy, `reset_strategy = "never"`/`"always"` without replicas | None | — | — |

Notable gaps, all deliberate:

- **Proxy durations include the network.** The embedded-SQLite numbers are in-process execution time; the proxy numbers are a round trip to your database server. Comparing them directly is comparing two different things.
- **`COM_STMT_PREPARE` is never recorded.** Preparing is a metadata round trip, not an execution — recording it under the statement's digest would publish parse latency as query latency.
- **`COM_STMT_EXECUTE` is invisible on the default MySQL path.** Attributing an execute to its SQL requires having parsed the *prepare response*, which only the R/W-split routing loop does. Applications that turn off PDO's emulated prepares (`PDO::ATTR_EMULATE_PREPARES = false`) will therefore see little or no proxy query-stat traffic unless R/W splitting is on.
- **PostgreSQL extended-protocol traffic is invisible.** `Parse`/`Bind`/`Execute` pin the session to one backend and splice it verbatim, because message boundaries stop lining up with turn boundaries. Same for `COPY`, and for PostgreSQL sessions using `reset_strategy = "never"`/`"always"` without replicas.
- **Rows the proxy cannot count are reported as `0`, never estimated.**

## `[kv]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `memory_limit` | string | `"256MB"` | Max memory for **stored key/value payloads**. Per-connection RESP protocol buffers are NOT counted here — bound those with `[kv.redis_compat]` `max_connections` / `max_input_buffer`. |
| `eviction_policy` | string | `"allkeys-lru"` | `noeviction`, `allkeys-lru`, `volatile-lru`, `allkeys-random`. |
| `compression` | string | `"none"` | `none`, `gzip`, `brotli`, `zstd`. |
| `compression_level` | u32 | `6` | 1=fastest, 9=best. |
| `compression_min_size` | usize (bytes) | `1024` | Values below this are stored uncompressed. |
| `secret` | string | (none) | Master secret for per-site RESP AUTH. Not auto-generated — if unset, multi-tenant HMAC AUTH is disabled. |

### `[kv.redis_compat]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable the RESP listener. Off by default; in multi-tenant mode keep it off. |
| `listen` | string | `"127.0.0.1:6379"` | RESP listener address (TCP only). |
| `socket` | string | (none) | **Not yet implemented** — parsed but unused; startup logs a warning if set. |
| `password` | string | (none) | RESP `AUTH` password. |
| `max_connections` | usize | `1000` | Max concurrent RESP connections; excess clients get `ERR max number of clients reached` (like Redis `maxclients`). `0` = unlimited. |
| `max_input_buffer` | usize (bytes) | `1048576` (1 MiB) | Per-connection input buffer cap (like Redis `client-query-buffer-limit`). Not counted against `[kv] memory_limit`. |
| `idle_timeout_secs` | u64 | `300` | Close RESP connections idle this long, freeing their buffers. `0` = never. |

## `[cluster]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable gossip clustering. |
| `bind` | string | `"0.0.0.0:7946"` | Gossip UDP listener. |
| `join` | array of strings | `[]` | Seed addresses for initial cluster join. |
| `secret` | string | `""` | Shared secret for cluster transport security. When set, gossip UDP and the KV TCP data plane are encrypted and authenticated (ChaCha20-Poly1305, keys derived via HKDF-SHA256); nodes without it cannot join, read, or inject. **Required when `enabled = true`:** an empty secret is a hard startup error unless `allow_insecure_no_auth = true`. |
| `allow_insecure_no_auth` | bool | `false` | Opt in to running clustering with an empty `secret` (unauthenticated plaintext gossip + KV data plane). Off by default so clustering fails closed. Set `true` only on a fully trusted private network with ports 7946/7947 firewalled from untrusted hosts; a loud warning is still logged. Not recommended. |
| `node_id` | string | (auto) | Unique node identifier. Auto-generated if empty. |
| `cluster_id` | string | `"ephpm"` | Nodes with different `cluster_id`s ignore each other. |

### `[cluster.channel]`

**Experimental-adjacent.** The cluster channel is a single,
authenticated, `yamux`-multiplexed TCP listener that opt-in cluster
features share (Turso CDC replication and its snapshot bootstrap
today). It is **only bound when at least one feature asks for it**: a
config that ships no channel feature is byte-identical to a config
without this section — no socket, no task, no startup log noise above
`debug!`. Adding `[cluster.channel]` to a config is not itself an
opt-in; a feature elsewhere (today just `[db.sqlite.replication]
cdc_experimental = true` in clustered mode) has to ask. See the
[cluster channel roadmap](/roadmap/cluster-channel/) for the design.

**Security posture.** Connections complete a mutual challenge/response
handshake in which both peers prove possession of the shared secret and
both contribute fresh randomness, so a recorded handshake cannot be
replayed. Both ends then derive a per-connection key from the secret
salted with the handshake transcript, and every subsequent byte —
yamux framing included — is sealed with ChaCha20-Poly1305. Inbound
connections must additionally come from an IP that gossip currently
knows as a cluster member. There is **no TLS and no certificate-based
peer identity**: authentication is "holds the shared cluster secret",
and the membership check is per-host and trusts the TCP source address.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `listen` | string, optional | *(derived: gossip bind IP with port `bind_port + 2` — `+ 2` because the KV data plane already claims gossip + 1 (7947), so defaults land on 7948)* | TCP listen address for the channel. Ignored when no channel feature is enabled. |
| `secret` | string, optional | *(fall back to `[cluster] secret`)* | Shared secret for the channel handshake and per-connection frame keys (distinct HKDF domains from gossip/KV and from each other). When neither this nor `[cluster] secret` is set, the channel refuses to bind — channel features require authentication (fail-closed). |

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

## `[[middleware]]`

Native middleware mounts — repeatable array-of-tables. Each mount resolves
against the **builtin registry first**: the four in-tree modules (`jwt`,
`cors`, `ratelimit`, `security-headers`) are compiled into every binary and
run in-process — no shared library on disk, no `dlopen`. Any other name
loads a shared library (`.so`/`.dylib`/`.dll`) at startup. Loading is
fail-fast: a builtin rejecting its config, an unresolvable library, a
missing ABI symbol, or a failing module `init` aborts server startup. The
chain is evaluated on every PHP-bound request, before the request body is
read. Mounts apply globally, not per vhost — a module can discriminate by
vhost via the request's server name. See the
[Native Middleware guide](/guides/native-middleware/).

**Built-ins work in every binary.** Shared-library mounts (custom
out-of-tree modules) work out of the box with the stock release binaries
on all platforms — the Linux release is glibc-dynamic
(`<arch>-unknown-linux-gnu`), so `dlopen` is available. Only a self-built
fully static musl binary lacks `dlopen` (`Dynamic loading not supported`
at startup) — see the guide's
[dynamic-lane section](/guides/native-middleware/#the-dynamic-lane).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `library` | string | **required** | Builtin name (`jwt`, `cors`, `ratelimit`/`rate-limit`, `security-headers`, or their `ephpm-middleware-*` long forms; `-`/`_` interchangeable), a bare module name resolved through the middleware search path (`<name>.<os>-<arch>.<ext>`, `lib<name>.<ext>`, `<name>.<ext>` in the working directory, `$EPHPM_MIDDLEWARE_DIR`, then `/usr/local/lib/ephpm/middleware`), or an explicit path (any value containing a path separator or file extension). Must not be empty. |
| `match` | string | (none) | Glob the request path must match for the mount to run. `*` matches any character sequence, including `/`. Unset = every PHP-bound request. |
| `order` | u32 | **required** | Chain position; lower runs first. Equal orders keep declaration order. |
| `config` | inline table | (none) | Arbitrary module configuration, serialised to JSON and passed to the module's `init`. |

## `[opcache]`

Governs the cluster-wide OPcache invalidation watcher (Phase 1 of the
[OPcache clustering roadmap](/roadmap/opcache-clustering/)). When enabled,
every PHP request checks `opcache:version:<vhost>` in the in-process KV
store and, when the value has advanced since this node last saw it, runs
`opcache_invalidate()` for every cached script under the vhost's docroot
before executing the request. The lookup is one atomic load plus one
`DashMap::get` — sub-microsecond in the fast path.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cluster_invalidation` | bool | (auto) | Watch the KV store for invalidation events. Unset defaults to `true` when `[cluster] enabled = true`, `false` otherwise. Applies to fpm mode only (`[php] mode = "fpm"`); worker mode logs a WARN at startup and skips the watcher. |

The companion CLI is `ephpm deploy` / `ephpm cache reset` — both write
the version key via the RESP listener, so `[kv.redis_compat] enabled = true`
is required for the CLI to reach the running server. See the roadmap
page for the wire semantics.

## See also

- [Environment variables](environment-variables/) — how to override any of these via `EPHPM_*`
- [`crates/ephpm-config/src/lib.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — definitive source

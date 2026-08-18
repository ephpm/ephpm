# ePHPm — Embedded PHP Manager

Run PHP applications without the infrastructure. No PHP-FPM, no MySQL server, no Redis, no reverse proxy, no certbot. One binary, one config file. Drop in WordPress or Laravel and go. When you need more, it's already built in: MySQL connection pooling, read/write splitting, a Redis-compatible KV store, clustered SQLite with automatic failover, TLS, and Prometheus metrics. One binary from `localhost` to production — same runtime in development, CI, staging, and prod. No environment drift, no deployment surprises.

Designed by [@luthermonson](https://github.com/luthermonson) in Arizona 🌵 Assembled in [Claude Opus 4.6](https://claude.ai).

## Why ePHPm?

How ePHPm compares to other ways of running PHP with a webserver.

| | ePHPm | FrankenPHP | RoadRunner | Swoole | Apache + mod_php | Nginx + php-fpm |
|---|---|---|---|---|---|---|
| Language | Rust | Go (CGO) | Go | PHP + C | C | C |
| Dispatch to PHP | <1 μs (in-process C call) | ~2–3 μs (CGO crossings) | ~10–50 μs (IPC to worker) | <1 μs (in-process) | <1 μs (in-process) | ~50–100 μs (FastCGI socket) |
| Worker mode (boot app once) | Built-in (`mode = "worker"`; native Octane + WordPress adapters) | Built-in | Built-in (core model) | Built-in (requires app rewrite) | No | No |
| Server GC pauses | None | Go GC | Go GC | None | None | None |
| Binary | Single static binary | Caddy module | Go binary + PHP workers | PHP + extension | Apache + modules | Nginx + separate FPM |
| DB proxy + connection pooling | Built-in (MySQL wire, R/W split) | No | No | No | No | No |
| Embedded DB | SQLite via litewire | No | No | No | No | No |
| Built-in KV store | Yes (RESP compatible, in-process) | No | No | No | No | No |
| Query stats (Prometheus) | Built-in | No | No | No | No | No |
| Auto TLS (ACME) | Built-in | Via Caddy | No | No | No | No |
| Clustering | Gossip (SWIM) | No | No | Multi-process (single node) | No | No |
| Virtual hosts | Built-in (directory-based) | Via Caddy | No | No | `<VirtualHost>` | `server` blocks |
| Install size | ~40 MB (varies by PHP extensions) | ~150 MB | ~60–70 MB (rr + PHP) | ~35–45 MB (PHP + .so) | ~50–60 MB (Apache + PHP) | ~40–50 MB (Nginx + PHP) |
| PHP compatibility | Drop-in | Drop-in | Drop-in (worker mode requires PSR-7) | Drop-in (async features require rewrite) | Native (100%) | Native (100%) |
| Deployment | Single binary | Requires Caddy | Multi-process | Requires PHP + Swoole extension | Apache + modules | Separate services |
| Container-friendly | ✓ (single binary) | ✓ (Caddy module) | ✓ | ⚠️ (PHP + extension) | ⚠️ (heavier) | ⚠️ (two services) |

## Install

ePHPm is a single self-managing binary — it registers and controls its own system service. There is no install script.

> Prefer containers? `docker run -p 8080:8080 ephpm/ephpm:latest`. See the [Docker section of the install docs](https://ephpm.dev/getting-started/install/#docker) for the full tag scheme.

### Linux / macOS

Download the latest binary from [Releases](https://github.com/ephpm/ephpm/releases), unpack, then:

```bash
sudo ./ephpm install
```

This copies the binary to `/usr/local/bin/ephpm`, writes a default config to `/etc/ephpm/ephpm.toml`, registers a systemd service (Linux) or launchd plist (macOS), and starts it. The server listens on `http://localhost:8080` by default.

### Windows

Download `ephpm.exe` from [Releases](https://github.com/ephpm/ephpm/releases). In an Administrator PowerShell:

```powershell
.\ephpm.exe install
```

Installs to `C:\Program Files\ephpm\`, adds to `PATH`, registers a Windows service, and starts it.

> **Note:** Single-node Turso and the DB proxy work fully on Windows. Clustered (Turso CDC replication) mode is experimental and not validated on Windows.

### Manage the service

The same subcommands work on every platform — they wrap systemd / launchd / the Windows service controller:

```bash
sudo ephpm start          # start the service
sudo ephpm stop           # stop
sudo ephpm restart        # restart (e.g. after editing the config)
sudo ephpm status         # PID, uptime, listen address
sudo ephpm logs           # tail the service log (--follow to follow)
```

### Uninstall

```bash
sudo ephpm uninstall              # remove binary, service, and data dir
sudo ephpm uninstall --keep-data  # keep config and SQLite databases
```

### Build from Source

For contributors or custom builds. Requires Rust 1.88+.

```bash
# Stub mode (no PHP, fast iteration on HTTP/routing logic)
cargo build
cargo run -- serve --config ephpm.toml

# Release binary with PHP embedded
# Prerequisites: git, curl, tar, build-essential, pkg-config, libclang-dev
# The PHP SDK (libphp.a + headers) is downloaded from github.com/ephpm/php-sdk releases —
# no PHP CLI, Composer, or static-php-cli toolchain required.
cargo xtask release       # → target/release/ephpm

# A locally built binary can self-install too
sudo ./target/release/ephpm install
```

## Configuration

```toml
# ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
index_files = ["index.php", "index.html"]

[php]
mode = "fpm"
memory_limit = "128M"
max_execution_time = 30       # inner PHP deadline (default). Natively enforced
                              # on Linux ZTS builds (catchable fatal → HTTP 500,
                              # wall-clock, set_time_limit()-able); not enforced
                              # on macOS/Windows. Keep it below the outer 504
                              # backstop, [server.timeouts] request.

# Load a custom php.ini before applying overrides (optional)
# ini_file = "/etc/php/php.ini"

# INI directive overrides (applied AFTER ini_file)
ini_overrides = [
    ["display_errors", "Off"],
    ["error_reporting", "E_ALL"],
]

# Prometheus metrics endpoint
[server.metrics]
enabled = true
# path = "/metrics"   # default

# Embedded SQLite (via litewire). The presence of this section IS the
# enable switch — there is no `enabled` key.
[db.sqlite]
path = "/var/lib/ephpm/app.db"
# engine = "turso"   # the default and only engine as of v0.7.0 — the Turso Database engine (Beta upstream); may be omitted
```

Any TOML key can be overridden with an `EPHPM_` prefixed environment variable — e.g. `EPHPM_SERVER__LISTEN=0.0.0.0:9090`, `EPHPM_PHP__MEMORY_LIMIT=256M`. Nesting uses `__`. Arrays use JSON syntax: `EPHPM_CLUSTER__JOIN='["a:7946","b:7946"]'`. See [Environment Variables](https://ephpm.dev/reference/environment-variables/) for the full mapping rules.

## Database: Three Options, Zero Code Changes

ePHPm gives you three database strategies. PHP apps keep their existing `pdo_mysql` configuration in all cases — no code changes needed.

### 1. Already have a database? Use the built-in proxy

If you have a MySQL or PostgreSQL server, ePHPm's DB proxy sits between PHP and your database with connection pooling, read/write splitting, and health checks. PHP connects to `localhost:3306` (or `localhost:5432` for Postgres) — the proxy handles the rest. The PostgreSQL proxy supports trust, md5, and SCRAM-SHA-256 authentication. SQL Server (TDS) proxying is not implemented.

```toml
[db.mysql]
url = "mysql://user:pass@db-server:3306/myapp"

# or PostgreSQL
[db.postgres]
url = "postgres://user:pass@db-server:5432/myapp"
```

### 2. Small site? Use embedded SQLite

No external database needed. ePHPm embeds SQLite and exposes it via MySQL wire protocol through **[litewire](https://github.com/ephpm/litewire)**. Your PHP app thinks it's talking to MySQL — it's actually talking to SQLite. One binary, one `.db` file, done.

Back up with cloud volume snapshots (Kubernetes PVCs, EBS snapshots, disk images) or any file-level backup tool.

```toml
[db.sqlite]
path = "app.db"
```

### 3. Need HA? Use clustered Turso replication (experimental)

For multi-node high availability, ePHPm replicates the embedded Turso database in-process — no sidecar, no extra binary. The primary's change-data-capture (CDC) stream is tailed and per-transaction batches are shipped to replicas over ePHPm's authenticated cluster channel; cold replicas bootstrap from a logical snapshot. The single-binary model is preserved end to end.

- **Primary node** — accepts writes, ships CDC batches to replicas
- **Replica nodes** — serve reads locally, apply the primary's CDC stream
- **Primary election** — automatic via ePHPm's gossip layer (lowest-ordinal live node wins)
- **Failover** — gossip detects failure, the next node promotes and begins shipping CDC

> The Turso engine is Beta upstream, so clustered mode is **experimental**. Single-node Turso is the stable default.

```toml
[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.replication]
role = "auto"

[cluster]
enabled = true
join = ["ephpm-headless.default.svc.cluster.local"]
```

### How it works under the hood

```
PHP (pdo_mysql) → litewire (MySQL wire :3306) → SQL Translator → Turso engine (in-process)
```

[litewire](https://github.com/ephpm/litewire) translates MySQL wire protocol and SQL dialect to SQLite on the fly using `sqlparser-rs`. It's a standalone open-source project — works outside of ePHPm too.

The backend is the in-process [Turso](https://github.com/tursodatabase/turso) engine (a pure-Rust SQLite rewrite) in both single-node and clustered mode — PHP always sees a MySQL server at `127.0.0.1:3306`. Existing rusqlite/SQLite `.db` files open in place (a cleanly-shut-down database upgrades with no dump/reload).

See the [database architecture docs](site/content/architecture/database/_index.md) for the full architecture, failover details, and configuration reference.

## KV Store: Three Ways to Use It, Zero External Services

ePHPm ships a `DashMap`-backed in-process key-value store with TTLs, atomic counters, hashes, LRU eviction, and optional value compression (gzip/zstd/brotli). No Redis server, no extension to install. Like the database, you pick the access pattern — the data lives in the same binary either way.

### 1. Already use phpredis / predis? Speak RESP

The KV store speaks Redis RESP2 on `127.0.0.1:6379`. Existing PHP code using `phpredis`, `predis`, or any other Redis client connects unchanged. Commands implemented: `GET` / `SET` / `SETEX` / `SETNX` / `MGET` / `MSET` / `INCR` / `DECR` / `INCRBY` / `DECRBY` / `APPEND` / `STRLEN` / `GETSET` / `DEL` / `EXISTS` / `EXPIRE` / `PEXPIRE` / `TTL` / `PTTL` / `PERSIST` / `TYPE` / `RENAME` / `KEYS` / `DBSIZE` / `FLUSHDB` / `FLUSHALL` / `HSET` / `HGET` / `HDEL` / `HGETALL` / `HKEYS` / `HVALS` / `HLEN` / `HEXISTS` / `AUTH` / `PING` / `ECHO` / `INFO`.

```toml
[kv]
memory_limit = "256MB"
eviction_policy = "allkeys-lru"   # noeviction | allkeys-lru | volatile-lru | allkeys-random
compression = "zstd"              # none | gzip | brotli | zstd  (transparent, per-value)

[kv.redis_compat]
enabled = true                    # off by default — multi-tenant deployments should keep it off
listen = "127.0.0.1:6379"
```

### 2. Want zero round-trips? Use the native PHP functions

Every request gets a set of `ephpm_kv_*` functions registered as part of the ePHPm SAPI — they call directly into the in-process store with no socket, no protocol parse, no serialization. No extension to install, no client library to configure. In multi-tenant (`sites_dir`) mode they are automatically namespaced per virtual host — each site sees its own keyspace.

```php
ephpm_kv_set('cart:42', $json, 3600);   // value, TTL seconds
$cart = ephpm_kv_get('cart:42');
ephpm_kv_incr_by('views:home', 1);
ephpm_kv_expire('session:abc', 1800);
ephpm_kv_del('cart:42');
```

Available: `ephpm_kv_get`, `set`, `setnx`, `del`, `exists`, `incr`, `decr`, `incr_by`, `expire`, `ttl`, `pttl`, `flush_all`, `wait`. These are the lowest-latency API — a direct FFI call, no socket. The RESP listener is also safe for multi-tenant use: set `[kv] secret` and each connection is scoped to a single site's keyspace by HMAC AUTH (see below).

### 3. Need HA? Use the clustered KV tier

In a cluster, the KV store becomes a two-tier distributed store with no extra moving parts — it piggybacks on the same SWIM gossip layer used for SQLite primary election.

- **Small values** (< 512 bytes by default) ride the **gossip tier** — eventually consistent, replicated to every node, sub-millisecond reads everywhere.
- **Large values** live on a **hashed data plane** — the owner is `hash(key)` modulo the sorted alive-node list (no consistent-hash ring), replicated to N nodes (configurable replication factor), fetched on demand via TCP, with optional hot-key promotion that caches frequently-fetched remote values locally.
- **Failover** — when a node leaves the gossip view, the hash ring rebalances and owned keys migrate to the next replicas. No primary, no election — every node can read and write.

```toml
[cluster]
enabled = true
join = ["ephpm-headless.default.svc.cluster.local"]

[cluster.kv]
small_key_threshold = 512         # bytes — under this, replicate via gossip (default)
replication_factor = 3            # large keys: 3 copies (owner + 2 replicas)
replication_mode = "async"        # async | sync
hot_key_cache = true              # cache hot remote values locally
hot_key_max_memory = "64MB"
```

### Multi-tenant security

When `sites_dir` is set, each virtual host gets its own isolated keyspace. The `ephpm_kv_*` PHP functions are namespaced automatically. For the RESP endpoint, ePHPm derives a per-site password from `HMAC-SHA256(kv.secret, hostname)` and injects it into PHP `$_SERVER` as `EPHPM_REDIS_PASSWORD` for each request — so `phpredis` can `AUTH` without any per-site config in your code.

### How it works under the hood

```
PHP → ephpm_kv_*  (in-process function call, ~ns)
PHP → phpredis → :6379 (RESP2)  →  DashMap store
            cluster mode ↓
        gossip tier (small values, eventually consistent)
        data plane  (large values, consistent-hash, replicated)
```

The store is a single `DashMap<String, Entry>` with concurrent reads/writes, async TTL expiry, and an approximate-memory tracker driving eviction. Compression is applied per-value above a size threshold and is transparent on read. In clustered mode the same `Store` is wrapped with a routing layer that consults the hash ring for non-local keys.

## Query Stats & Observability

Every SQL query — whether it goes through the DB proxy to a real MySQL server or through litewire to SQLite — is tracked automatically. ePHPm normalizes queries (replacing literal values with `?`), groups them by digest, and records timing, throughput, and error rates.

Metrics are exported in Prometheus format at `/metrics` once you enable the endpoint (`[server.metrics] enabled = true` — it is **off** by default):

```
# Histogram of query execution times, by digest and kind (query/mutation)
ephpm_query_duration_seconds_bucket{digest="SELECT * FROM users WHERE id = ?",kind="query",le="0.01"} 4521

# Total query count by status
ephpm_query_total{digest="SELECT * FROM users WHERE id = ?",kind="query",status="ok"} 4520
ephpm_query_total{digest="SELECT * FROM users WHERE id = ?",kind="query",status="error"} 1

# Rows returned/affected
ephpm_query_rows_total{digest="SELECT * FROM users WHERE id = ?",kind="query"} 4520

# Slow query counter (exceeds threshold)
ephpm_query_slow_total 3

# Active digest count
ephpm_query_active_digests 47
```

Slow queries (default: > 1s) are logged at WARN level with the normalized SQL and digest ID. Query stats are on by default but fully configurable:

```toml
[db.analysis]
query_stats = true            # on by default

[server.metrics]
enabled = true                # OFF by default — /metrics 404s without this
```

Point Grafana, Datadog, or any Prometheus-compatible tool at `http://your-ephpm:8080/metrics` to chart query latency, throughput, error rates, and identify slow queries — no APM agent or database plugin needed.

Query stats are collected by default, but **the `/metrics` endpoint itself is opt-in**: `[server.metrics] enabled` defaults to `false`, and with no recorder installed the metric calls are no-ops.

See [site/content/architecture/query-stats.md](site/content/architecture/query-stats.md) for the full design.

## Virtual Hosts: Multi-Tenant Hosting

Run multiple WordPress sites on a single ePHPm instance. The directory structure IS the config — each subdirectory is named after a domain.

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/marketing"   # fallback for unmatched domains
sites_dir = "/var/www/sites"           # vhost directory
```

```
/var/www/
  marketing/                  # signup page (fallback for unknown domains)
  sites/
    alice-blog.com/           # served when Host: alice-blog.com
      index.php
      ephpm.db
    bobs-recipes.com/         # served when Host: bobs-recipes.com
      index.php
      ephpm.db
```

- **Add a site:** create a directory, drop in WordPress
- **Remove a site:** delete the directory — traffic falls back to your marketing page
- **No per-site config needed:** sites inherit global PHP settings, timeouts, and security rules
- **Shared thread pool:** all sites share tokio's `spawn_blocking` pool — 20 blogs don't need 20x the memory

A $3.69/mo Hetzner VM (2 ARM cores, 4 GB RAM) comfortably runs 20 WordPress blogs at ~$0.18/site. See [site/content/guides/virtual-hosts.md](site/content/guides/virtual-hosts.md) and [site/content/roadmap/hosting.md](site/content/roadmap/hosting.md) for full details.

## Project Structure

```
crates/
├── ephpm/           CLI binary — clap args, config loading, server boot
├── ephpm-server/    HTTP server — hyper + tokio, routing, static files, metrics
├── ephpm-php/       PHP embedding — FFI bindings, SAPI, request/response
├── ephpm-config/    Configuration — figment, TOML + env var overrides
├── ephpm-kv/        Embedded KV store — DashMap, RESP2 protocol, TTL/expiry, compression
├── ephpm-db/        DB proxy — MySQL wire protocol, connection pooling
└── ephpm-cluster/   Clustering — SWIM gossip (chitchat), hashed key ownership, SQLite election
```

Key design decisions:
- **Conditional compilation** — All PHP FFI code is gated behind `#[cfg(php_linked)]`. Stub mode compiles and tests without a PHP SDK.
- **C wrapper for safety** — PHP uses `setjmp`/`longjmp` for error handling. All Rust→PHP calls go through `ephpm_wrapper.c` with `zend_try`/`zend_catch` guards to prevent stack corruption.
- **Async I/O, blocking PHP** — tokio handles HTTP connections. PHP execution runs on `spawn_blocking` threads (ZTS).
- **litewire for SQL** — wire protocol translation is a separate concern; litewire handles it as a library (MySQL/PG/TDS wire → the in-process Turso engine), ePHPm manages the database lifecycle, replication, and config.

## Contributing

### Prerequisites

- **Rust 1.88+** — https://rustup.rs (on Windows, also install [C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/))
- **Nightly Rust** — `rustup toolchain install nightly` (required for `cargo +nightly fmt`)
- **cargo-nextest** — `cargo install cargo-nextest --locked`
- **cargo-deny** — `cargo install cargo-deny --locked`
- **WSL + Ubuntu** (Windows only) — needed for `cargo xtask release` (see Quick Start above)

See [site/content/developer/getting-started.md](site/content/developer/getting-started.md) for detailed setup instructions including per-platform Rust installation.

### Workflow

Most development uses stub mode — no PHP SDK or container engine needed:

```bash
# Build (stub mode)
cargo build

# Run tests (prefer single-crate runs)
cargo nextest run -p ephpm-server

# Lint (must pass with zero warnings)
cargo clippy --workspace --all-targets -- -D warnings

# Format (requires nightly)
cargo +nightly fmt --all

# Dependency audit
cargo deny check
```

### Build & test tooling (xtask)

```bash
cargo xtask doctor          # Check build prerequisites (toolchains, PHP SDK cache, optional tools)
cargo xtask release         # Download PHP SDK + build ephpm binary (release mode)
cargo xtask php-sdk         # Download only the prebuilt PHP SDK for the host platform
cargo xtask e2e             # Run bare-process E2E tests (spawns ephpm on 127.0.0.1, no Kind/Tilt)
cargo xtask k8s-e2e-install # Opt-in: download kind, tilt, kubectl to ./bin
cargo xtask k8s-e2e         # Opt-in: run E2E in a Kind cluster via tilt ci
cargo xtask k8s-e2e-up      # Opt-in: start K8s dev env (tilt dashboard at localhost:10350)
cargo xtask k8s-e2e-down    # Opt-in: tear down Kind cluster
```

On Windows, `release` re-invokes itself inside WSL (building a Linux binary from native Windows isn't supported). `php-sdk` is a plain tarball download and works directly on any platform with curl + tar. The PHP SDK is cached at `php-sdk/<version>-<os>-<arch>[-gnu]/` (the `-gnu` libc suffix applies on Linux) — delete that directory to force a re-download.

The default `cargo xtask e2e` spawns bare ephpm processes on 127.0.0.1 — no container engine required. The opt-in `k8s-e2e*` commands require Podman or Docker; run `cargo xtask k8s-e2e-install` to download kind/tilt/kubectl to `./bin/`. See [site/content/developer/testing.md](site/content/developer/testing.md) for details.

### Code conventions

- **Clippy**: Pedantic + all warnings denied. Zero warnings policy.
- **Formatting**: 2024 edition style, grouped imports. Run `cargo +nightly fmt --all`.
- **Error handling**: `thiserror` in library crates, `anyhow` in the binary. Always add `.context()`.
- **Logging**: `tracing` crate — debug for requests, info for lifecycle, warn/error for problems.
- **Unsafe code**: Safety comment (`// SAFETY:`) before every `unsafe` block explaining invariants.
- **Documentation**: `///` on public items, `//!` at module level.

## Docs

- [Getting started](https://ephpm.dev/developer/getting-started/) — Prerequisites, building, IDE setup
- [Testing strategy](https://ephpm.dev/developer/testing/) — Unit tests, Tilt + Kind E2E, database testing
- [E2E test coverage](https://ephpm.dev/testing/e2e/) — 170+ tests across single-node and cluster
- [Architecture decisions](https://ephpm.dev/architecture/) — Language choice, crate design, PHP execution modes
- [Implementation guide](https://ephpm.dev/architecture/implementation/) — Build system, CI, MVP spec
- [CLI design](https://ephpm.dev/architecture/cli/) — Command structure, UX principles
- [Security model](https://ephpm.dev/architecture/security/) — Threat model, FFI safety, trust boundaries
- [Clustering](https://ephpm.dev/architecture/clustering/) — SWIM gossip, hashed key ownership, two-tier KV
- [DB proxy](https://ephpm.dev/architecture/db-proxy/) — MySQL wire protocol, connection pooling, query analysis
- [Kubernetes deployment](https://ephpm.dev/architecture/kubernetes/) — Helm chart, StatefulSet, gossip DNS
- [Observability](https://ephpm.dev/architecture/metrics/) — Prometheus metrics, histogram buckets, phased rollout
- [Embedded SQL](https://ephpm.dev/architecture/sql/) — litewire integration, Turso engine, single-node vs clustered CDC
- [Competitive analysis](https://ephpm.dev/analysis/) — FrankenPHP, RoadRunner, Swoole comparisons

## Related Projects

- **[litewire](https://github.com/ephpm/litewire)** — MySQL/PG/TDS wire protocol → SQLite translation proxy. Used by ePHPm for embedded SQL, also works standalone.

## License

MIT

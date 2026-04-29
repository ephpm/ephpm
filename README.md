# ePHPm — Embedded PHP Manager

Run PHP applications without the infrastructure. No PHP-FPM, no MySQL server, no Redis, no reverse proxy, no certbot. One binary, one config file. Drop in WordPress or Laravel and go. When you need more, it's already built in: MySQL connection pooling, read/write splitting, a Redis-compatible KV store, clustered SQLite with automatic failover, TLS, and Prometheus metrics. One binary from `localhost` to production — same runtime in development, CI, staging, and prod. No environment drift, no deployment surprises.

## Why ePHPm?

How ePHPm compares to other ways of running PHP with a webserver.

| | ePHPm | FrankenPHP | RoadRunner | Swoole | Apache + mod_php | Nginx + php-fpm |
|---|---|---|---|---|---|---|
| Language | Rust | Go (CGO) | Go | PHP + C | C | C |
| Dispatch to PHP | <1 μs (in-process C call) | ~2–3 μs (CGO crossings) | ~10–50 μs (IPC to worker) | <1 μs (in-process) | <1 μs (in-process) | ~50–100 μs (FastCGI socket) |
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

## Feature Status

| Feature | Status |
|---------|--------|
| HTTP/1.1 + HTTP/2 serving | **Implemented** |
| Static file serving | **Implemented** |
| PHP embedding (ZTS) | **Implemented** |
| Request routing (pretty permalinks) | **Implemented** |
| Configuration (TOML + env vars) | **Implemented** |
| Embedded KV store (strings, TTL, counters) | **Implemented** |
| KV store value compression (gzip/zstd/brotli) | **Implemented** |
| KV store CLI debugging (`ephpm kv`) | **Implemented** |
| SAPI functions (`ephpm_kv_*` in PHP) | **Implemented** |
| Prometheus metrics + query stats | **Implemented** |
| Gossip clustering (SWIM via chitchat) | **Implemented** |
| Embedded SQLite — single-node (litewire + rusqlite) | **Implemented** |
| Embedded SQLite — clustered HA (litewire + sqld) | **Implemented** |
| TLS (manual cert/key + ACME/Let's Encrypt) | **Implemented** |
| Virtual hosts (directory-based, multi-tenant) | **Implemented** |
| Admin UI / API | Planned |
| OpenTelemetry export | Planned |

## Install

ePHPm is a single self-managing binary — it registers and controls its own system service. There is no install script.

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

> **Note:** Clustered SQLite (sqld) is not available on Windows. Single-node SQLite and the DB proxy work fully.

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

For contributors or custom builds. Requires Rust 1.85+.

```bash
# Stub mode (no PHP, fast iteration on HTTP/routing logic)
cargo build
cargo run -- --config ephpm.toml

# Release binary with PHP embedded
# Prerequisites: git, curl, tar, build-essential, pkg-config, libclang-dev, musl-tools (Linux only)
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
mode = "embedded"
max_execution_time = 30
memory_limit = "128M"

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

# Embedded SQLite (via litewire)
[db.sqlite]
enabled = true
path = "/var/lib/ephpm/app.db"
```

Any TOML key can be overridden with an `EPHPM_` prefixed environment variable — e.g. `EPHPM_SERVER__LISTEN=0.0.0.0:9090`, `EPHPM_PHP__MEMORY_LIMIT=256M`. Nesting uses `__`. Arrays use JSON syntax: `EPHPM_CLUSTER__JOIN='["a:7946","b:7946"]'`. See [Environment Variables](https://ephpm.dev/reference/environment-variables/) for the full mapping rules.

## Database: Three Options, Zero Code Changes

ePHPm gives you three database strategies. PHP apps keep their existing `pdo_mysql` configuration in all cases — no code changes needed.

### 1. Already have a database? Use the built-in proxy

If you have a MySQL server, ePHPm's DB proxy sits between PHP and your database with connection pooling, read/write splitting, and health checks. PHP connects to `localhost:3306` — the proxy handles the rest. PostgreSQL proxy support is planned but not yet implemented.

```toml
[db.mysql]
url = "mysql://user:pass@db-server:3306/myapp"
```

### 2. Small site? Use embedded SQLite

No external database needed. ePHPm embeds SQLite and exposes it via MySQL wire protocol through **[litewire](https://github.com/ephpm/litewire)**. Your PHP app thinks it's talking to MySQL — it's actually talking to SQLite. One binary, one `.db` file, done.

Back up with cloud volume snapshots (Kubernetes PVCs, EBS snapshots, disk images) or any file-level backup tool.

```toml
[db.sqlite]
path = "app.db"
```

### 3. Need HA? Use clustered SQLite

For multi-node high availability, ePHPm embeds [sqld](https://github.com/tursodatabase/libsql) (Turso's SQLite server) inside the binary. sqld is extracted and spawned as a managed child process at startup — the single-binary model is preserved. Replication happens automatically via WAL frame streaming over gRPC.

- **Primary node** — accepts writes, streams WAL frames to replicas
- **Replica nodes** — serve reads locally, forward writes to primary
- **Primary election** — automatic via ePHPm's gossip layer (lowest-ordinal live node wins)
- **Failover** — gossip detects failure, next node promotes, sqld restarts in primary mode

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
PHP (pdo_mysql) → litewire (MySQL wire :3306) → SQL Translator → SQLite backend
```

[litewire](https://github.com/ephpm/litewire) translates MySQL wire protocol and SQL dialect to SQLite on the fly using `sqlparser-rs`. It's a standalone open-source project — works outside of ePHPm too.

In single-node mode, the backend is `rusqlite` (in-process, zero overhead). In clustered mode, it switches to an HTTP client talking to the local sqld instance. Either way, PHP sees a MySQL server at `127.0.0.1:3306`.

See [docs/architecture/sql.md](docs/architecture/sql.md) for the full architecture, failover details, and configuration reference.

## Query Stats & Observability

Every SQL query — whether it goes through the DB proxy to a real MySQL server or through litewire to SQLite — is tracked automatically. ePHPm normalizes queries (replacing literal values with `?`), groups them by digest, and records timing, throughput, and error rates.

Metrics are emitted via Prometheus at `/metrics`:

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
query_stats = true            # set to false to disable (zero overhead)
slow_query_threshold = "500ms"
```

Point Grafana, Datadog, or any Prometheus-compatible tool at `http://your-ephpm:8080/metrics` to chart query latency, throughput, error rates, and identify slow queries — no APM agent or database plugin needed.

See [docs/architecture/query-stats.md](docs/architecture/query-stats.md) for the full design.

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

A $3.69/mo Hetzner VM (2 ARM cores, 4 GB RAM) comfortably runs 20 WordPress blogs at ~$0.18/site. See [docs/architecture/vhosts.md](docs/architecture/vhosts.md) and [docs/architecture/hosting.md](docs/architecture/hosting.md) for full details.

## Project Structure

```
crates/
├── ephpm/           CLI binary — clap args, config loading, server boot
├── ephpm-server/    HTTP server — hyper + tokio, routing, static files, metrics
├── ephpm-php/       PHP embedding — FFI bindings, SAPI, request/response
├── ephpm-config/    Configuration — figment, TOML + env var overrides
├── ephpm-kv/        Embedded KV store — DashMap, RESP2 protocol, TTL/expiry, compression
├── ephpm-db/        DB proxy — MySQL wire protocol, connection pooling
├── ephpm-sqld/      sqld embedding — binary extraction, process lifecycle, health checks
└── ephpm-cluster/   Clustering — SWIM gossip (chitchat), consistent hash ring, SQLite election
```

Key design decisions:
- **Conditional compilation** — All PHP FFI code is gated behind `#[cfg(php_linked)]`. Stub mode compiles and tests without a PHP SDK.
- **C wrapper for safety** — PHP uses `setjmp`/`longjmp` for error handling. All Rust→PHP calls go through `ephpm_wrapper.c` with `zend_try`/`zend_catch` guards to prevent stack corruption.
- **Async I/O, blocking PHP** — tokio handles HTTP connections. PHP execution runs on `spawn_blocking` threads (ZTS).
- **litewire for SQL** — wire protocol translation is a separate concern; litewire handles it as a library, ePHPm manages the sqld lifecycle and config.

## Contributing

### Prerequisites

- **Rust 1.85+** — https://rustup.rs (on Windows, also install [C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/))
- **Nightly Rust** — `rustup toolchain install nightly` (required for `cargo +nightly fmt`)
- **cargo-nextest** — `cargo install cargo-nextest --locked`
- **cargo-deny** — `cargo install cargo-deny --locked`
- **WSL + Ubuntu** (Windows only) — needed for `cargo xtask release` (see Quick Start above)

See [docs/developer/getting-started.md](docs/developer/getting-started.md) for detailed setup instructions including per-platform Rust installation.

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
cargo xtask release     # Download PHP SDK + build ephpm binary (release mode)
cargo xtask php-sdk     # Download only the prebuilt PHP SDK for the host platform
cargo xtask e2e-install # Download kind, tilt, kubectl to ./bin (no global install)
cargo xtask e2e         # Run E2E tests (creates Kind cluster, builds images, tilt ci)
cargo xtask e2e-up      # Start E2E dev env (tilt dashboard at localhost:10350)
cargo xtask e2e-down    # Tear down Kind cluster
```

On Windows, `release` re-invokes itself inside WSL (cross-compiling to musl from native Windows isn't supported). `php-sdk` is a plain tarball download and works directly on any platform with curl + tar. The PHP SDK is cached at `php-sdk/<version>-<os>-<arch>/` — delete that directory to force a re-download.

E2E commands require Podman or Docker. Run `cargo xtask e2e-install` to download kind/tilt/kubectl to `./bin/` — no global install needed. See [docs/developer/testing.md](docs/developer/testing.md) for details.

### Code conventions

- **Clippy**: Pedantic + all warnings denied. Zero warnings policy.
- **Formatting**: 2024 edition style, grouped imports. Run `cargo +nightly fmt --all`.
- **Error handling**: `thiserror` in library crates, `anyhow` in the binary. Always add `.context()`.
- **Logging**: `tracing` crate — debug for requests, info for lifecycle, warn/error for problems.
- **Unsafe code**: Safety comment (`// SAFETY:`) before every `unsafe` block explaining invariants.
- **Documentation**: `///` on public items, `//!` at module level.

## Docs

- [Getting started](docs/developer/getting-started.md) — Prerequisites, building, IDE setup
- [Testing strategy](docs/developer/testing.md) — Unit tests, Tilt + Kind E2E, database testing
- [E2E test coverage](docs/testing/e2e.md) — 170+ tests across single-node and cluster
- [Architecture decisions](docs/architecture/architecture.md) — Language choice, crate design, PHP execution modes
- [Implementation guide](docs/architecture/implementation.md) — Build system, CI, MVP spec
- [CLI design](docs/architecture/cli.md) — Command structure, UX principles
- [Security model](docs/architecture/security.md) — Threat model, FFI safety, trust boundaries
- [Clustering](docs/architecture/clustering.md) — SWIM gossip, consistent hash ring, two-tier KV
- [DB proxy](docs/architecture/db-proxy.md) — MySQL wire protocol, connection pooling, query analysis
- [Kubernetes deployment](docs/architecture/kubernetes.md) — Helm chart, StatefulSet, gossip DNS
- [Observability](docs/architecture/metrics.md) — Prometheus metrics, histogram buckets, phased rollout
- [Embedded SQL](docs/architecture/sql.md) — litewire integration, sqld lifecycle, single-node vs HA
- [Competitive analysis](docs/analysis/) — FrankenPHP, RoadRunner, Swoole comparisons

## Related Projects

- **[litewire](https://github.com/ephpm/litewire)** — MySQL/PG/TDS wire protocol → SQLite translation proxy. Used by ePHPm for embedded SQL, also works standalone.

## License

MIT

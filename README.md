# ePHPm (Embedded PHP Manager)

Designed by [@luthermonson](https://github.com/luthermonson) in Arizona 🌵 Assembled in [Claude Opus](https://claude.ai).

I began my software career using PHP and like many I wasn't skilled enough about systems to know how to improve the speed in my PHP applications and just used common practices in the community to bolt on extra products and use shared memory caches. I moved on from PHP and entered cloud-native and have been writing a lot of go and working on massive products like managed kubernetes services. Now, 10yrs later I'm stretching my legs with AI and combining the last 20yrs of my software career to try and improve the things I didn't like about the bolt on deployments in PHP and combine it with the simplicity I enjoyed producing go binaries.

### What ePHPm is NOT:
Core functionality is an opinionated stack with sane defaults but it's not an opinionated project, everything can be overridden using config and php.ini to tune your PHP applications as they all have their own nuances. ePHPm will never be closed source or under a business license and all repos in the organization will remain 100% MIT, the PHP community is very opensource first and that is a position this project will respect.

### What ePHPm IS:
The simplest way to convey this project is to name what it is trying to replace and outline our primary engineering goals. 

First, it needed to be written in Rust to completely avoid the heavy `cgo` execution tax that platforms like FrankenPHP face when embedding `libphp.a`. Crossing the Go-to-C runtime boundary introduces structural latency and if you want to achieve peak performance then Rust is the ideal modern language to combine with C using zero-overhead and native FFI pointers. 

Second, it completely eliminates localhost loopback network hops. While local TCP connections and Unix sockets (used by PHP-FPM and RoadRunner) are fast, they still incur unavoidable OS kernel context-switching and protocol serialization taxes. 

* **Web Server:** Replaces Nginx by binding an in-process, high-concurrency [Tokio](https://tokio.rs) + [Hyper](https://docs.rs) network stack.
* **Process Manager:** Replaces PHP-FPM with our custom, resource-aware `fpm_engine` thread manager.
* **Shared Memory Cache:** Replaces standalone Redis with a concurrent, thread-safe [DashMap](https://docs.rs) key-value store (supports clustering)
* **Database:** Replaces external MySQL with an embedded, zero-dependency combination of [Turso](https://turso.tech/) and its underlying [libSQL](https://github.com/tursodatabase/libsql) relational engine (supports clustering)

ePHPm has also packed a highly robust and cloud-native feature set by drawing direct inspiration from other modern runtimes. This includes automatic ACME TLS certificate management and server-level middleware handlers (similar to RoadRunner and FrankenPHP), alongside an in-process SQL proxy with connection pooling and slow query logging inspired directly by ProxySQL. Combined with native cluster synchronization, OpenTelemetry tracing (OTLP), and container-aware runtime autotuning, ePHPm is fully prepared for modern cloud environments.

### How to use it:
Run `ephpm dev` locally from your laptop (with native macOS and Windows support) and drop it straight into your CI/CD pipelines using our off-the-shelf OCI container images. You can also safely alias the binary to act as your global system PHP CLI with 100% scripting compatibility:

```bash
alias php="ephpm php"
```

When you are ready to scale to production, you can deploy high-availability clusters using the exact same code paths running seamlessly from your local development machine, through CI, and straight into production. Need to use an external database just configure the SQL proxy and point your app to localhost:3306 to gain performance boosts from connection pooling.

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
cluster_id = "ephpm-prod"  # Required: Only nodes with matching IDs will pair
secret = "YOUR_HIGH_ENTROPY_BASE64_SECRET_HERE"  # Required: Enforces encrypted transport
join = ["ephpm.default.svc.cluster.local"]
```

### How the in-process database works under the hood
Single node is very simple and accessed via SAPI functions added to the runtime by ePHPm.

```
PHP Framework (ephpm driver) ──(Direct FFI)──> SAPI Functions (ephpm_db_query) ──> libSQL/Turso (In-Process)
```

In a distributed ePHPm cluster, relational data replication is handled at the engine layer by leveraging libSQL’s built-in Change Data Capture (CDC) architecture, completely removing the need for a separate heavy database replication service. Here is exactly how the CDC pipeline coordinates with the gossip protocol and the TCP data plane to maintain a high-availability database cluster and the request with the write shows up on the primary Turso node.

```
[ PHP Write Query ] ──(SAPI/Proxy)──> [ Node A (Elected Primary) ]
                                                │
                                       (Local WAL Commit)
                                                │
                                     [ libSQL CDC Generator ]
                                                │
                                                ▼  (Binary Transaction Log Stream)
                                       [ Cluster TCP Plane ]
                                          /           \
                                         ▼             ▼
                           [ Node B (Replica) ]   [ Node C (Replica) ]
                             (Apply Log Page)       (Apply Log Page)
```

If the request is to a read replica Turso node the write will fix itself by syncing the data to the primary.

```
[ PHP SAPI Write Call ]
          │
          ▼
┌─────────────────────────────────┐
│        Node B (Replica)         │
│  ┌───────────────────────────┐  │
│  │   ePHPm Gossip Layer      │  │ ◄─── Says: "Node A is Primary"
│  └─────────────┬─────────────┘  │
│                ▼                │
│  ┌───────────────────────────┐  │
│  │ libSQL Client (Replica)   │  │
│  │  • url: "local.db"        │  │
│  │  • sync_url: "Node A"     │  │
│  └─────────────┬─────────────┘  │
└────────────────┼────────────────┘
                 │
                 ▼ (Natively forwarded via libSQL gRPC)
┌─────────────────────────────────┐
│        Node A (Primary)         │
│  ┌───────────────────────────┐  │
│  │ libSQL Server (Primary)   │  │ ───► Commits to local disk
│  └───────────────────────────┘  │
└─────────────────────────────────┘
```

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

## Docs

- [Getting started](https://ephpm.dev/developer/getting-started/) — Prerequisites, building, IDE setup
- [Architecture decisions](https://ephpm.dev/architecture/) — Language choice, crate design, PHP execution modes
- [HTTP Server Architecture](https://ephpm.dev) — ZTS concurrency model, Tokio thread integration
- [Clustering](https://ephpm.dev/architecture/clustering/) — SWIM gossip, hashed key ownership, two-tier KV
- [KV Store](https://ephpm.dev/architecture/kv-store/) — DashMap design, RESP2 integration, eviction loops
- [Embedded SQL](https://ephpm.dev) — Litewire integration, Turso engine, zero-dependency data files
- [DB Proxy & Pooling](https://ephpm.devdb-proxy/) — MySQL wire protocol routing, connection metrics
- [CLI design](https://ephpm.dev/reference/cli/) — Command structure, clap routing, subcommand definitions
- [Configuration Reference](https://ephpm.dev) — Exhaustive ephpm.toml key mapping and overrides
- [Metrics Reference](https://ephpm.dev) — Prometheus endpoint specs and histogram allocation
- [Competitive Analysis](https://ephpm.dev/analysis/) — Feature map versus historical execution runtimes
- [Performance Comparison](https://ephpm.dev/analysis/performance-comparison/) — In-process latency models vs FPM, RoadRunner, and Swoole

## Related Projects

- **[litewire](https://github.com/ephpm/litewire)** — MySQL/PG/TDS wire protocol → SQLite translation proxy. Used by ePHPm for embedded SQL, also works standalone.

## License
MIT

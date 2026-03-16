# ePHPm CLI Architecture

Single binary, all commands. Built with `clap` (Rust).

```
ephpm <command> [subcommand] [flags]
```

---

## Core Commands

### `ephpm serve`

Start the PHP application server. This is the primary command — what runs in production.

```bash
# Start with config file (default: ./ephpm.toml)
ephpm serve

# Explicit config path
ephpm serve --config /etc/ephpm/ephpm.toml

# Override listen address
ephpm serve --listen 0.0.0.0:443

# Embed admin UI on this node (dev convenience)
ephpm serve --admin

# Foreground with log level
ephpm serve --log-level debug

# Specific PHP worker count (overrides config)
ephpm serve --workers 16

# Daemonize (background, writes PID file)
ephpm serve --daemon --pid-file /var/run/ephpm.pid
```

**What it starts:**
- HTTP server (`:443` by default, `:80` for HTTP→HTTPS redirect)
- PHP worker pool
- DB proxy (if configured)
- KV store (if configured)
- OTLP receiver (`:4317` gRPC, `:4318` HTTP — if configured)
- Gossip listener (`:7946` — if clustering configured)
- Node API (`:9090` — always)
- Admin UI (`:8080` — only with `--admin`)

**Graceful shutdown:** `SIGTERM` or `SIGINT` → drains in-flight requests, closes DB connections, leaves cluster gracefully, writes KV snapshot (if persistence enabled).

**Graceful reload:** `SIGHUP` → reloads `ephpm.toml`, restarts PHP workers (rolling — no dropped requests), updates DB pool sizes, refreshes TLS config. Does NOT restart the Rust process.

---

### `ephpm admin`

Start the admin UI as a standalone instance. Connects to one or more serving nodes via their Node API.

```bash
# Connect to specific nodes
ephpm admin --nodes 10.0.1.1:9090,10.0.1.2:9090,10.0.1.3:9090

# With config file (nodes listed in [admin] section)
ephpm admin --config /etc/ephpm/admin.toml

# Custom listen address
ephpm admin --listen 0.0.0.0:8080

# With Node API auth
ephpm admin --nodes 10.0.1.1:9090 --secret your-shared-secret
```

**What it starts:**
- Admin web UI (`:8080` by default)
- Node connector (polls/streams Node API from each configured node)

**Does NOT start:** PHP workers, DB proxy, KV store, HTTP server, OTLP receiver. Zero PHP-related resource usage.

---

### `ephpm stop`

Signal a running ePHPm instance to shut down gracefully.

```bash
# Stop via PID file
ephpm stop --pid-file /var/run/ephpm.pid

# Stop via signal to process
ephpm stop --pid 12345
```

Sends `SIGTERM`. The running instance drains requests and exits cleanly.

---

### `ephpm reload`

Signal a running instance to reload configuration without downtime.

```bash
ephpm reload --pid-file /var/run/ephpm.pid
```

Sends `SIGHUP`. The running instance reloads `ephpm.toml` and performs a rolling restart of PHP workers.

---

## Configuration Commands

### `ephpm init`

Scaffold a new `ephpm.toml` with sensible defaults and commented documentation.

```bash
# Interactive — asks about DB, clustering, etc.
ephpm init

# Generate minimal config
ephpm init --minimal

# Generate full config with all options documented
ephpm init --full

# Specify output path
ephpm init --output /etc/ephpm/ephpm.toml
```

Generates something like:

```toml
# ePHPm Configuration
# Docs: https://ephpm.dev/docs/config

[server]
listen = "0.0.0.0:443"
http_redirect = true          # redirect :80 → :443
workers = 0                   # 0 = auto (num_cpus)
worker_max_requests = 0       # 0 = unlimited (restart after N requests for leak protection)

[php]
root = "./public"
entry = "index.php"           # for worker mode

[tls]
acme_email = ""               # required for auto TLS
# domains = ["example.com"]   # optional, auto-detected from requests

# [db.mysql]
# url = "mysql://user:pass@db:3306/myapp"
# max_connections = 50

# [cluster]
# enabled = false
# bind = "0.0.0.0:7946"
# join = ["10.0.1.2:7946"]

[node_api]
listen = "0.0.0.0:9090"
# secret = ""                 # set this in production
```

---

### `ephpm validate`

Check configuration for errors without starting the server.

```bash
ephpm validate
ephpm validate --config /etc/ephpm/ephpm.toml
```

Validates:
- TOML syntax
- Required fields present
- DB URLs parseable
- Port conflicts (HTTP, DB proxy, Node API, OTLP, gossip — all on different ports)
- PHP root directory exists
- TLS cert paths valid (if manual certs)
- Cluster seed nodes resolvable

```
$ ephpm validate
✓ Config loaded from ./ephpm.toml
✓ PHP root ./public exists
✓ DB MySQL URL valid
✓ No port conflicts
✓ Node API secret set
✗ TLS: acme_email is empty — auto TLS will not work
```

---

### `ephpm config`

Show the effective running configuration (with defaults applied, secrets redacted).

```bash
# Show effective config as TOML
ephpm config

# Show specific section
ephpm config server
ephpm config db

# Query from a running instance's Node API
ephpm config --node 10.0.1.1:9090
```

---

## Inspection Commands

These connect to the Node API of a running instance. Useful for debugging, monitoring, and scripting.

### `ephpm status`

Quick overview of a running node.

```bash
ephpm status
ephpm status --node 10.0.1.1:9090
```

```
$ ephpm status
ePHPm v0.1.0 (PHP 8.4.2 ZTS)
Uptime:     3d 14h 22m
Workers:    12/16 busy, 4 idle, 0 queued
HTTP:       1,247 req/s (p99: 12ms)
DB Pool:    38/50 active connections
KV Store:   124MB used, 89,421 keys, 98.7% hit rate
Cluster:    3 nodes healthy
TLS:        4 certs managed, next renewal in 23d
```

---

### `ephpm workers`

PHP worker pool details.

```bash
# List workers
ephpm workers
ephpm workers --node 10.0.1.1:9090

# Restart all workers (rolling, no dropped requests)
ephpm workers restart

# Restart specific worker
ephpm workers restart --id 3
```

```
$ ephpm workers
ID  STATUS   REQUESTS  MEMORY   UPTIME     LAST REQUEST
 0  busy     14,231    32MB     3d 14h     12ms ago
 1  idle     13,887    28MB     3d 14h     340ms ago
 2  busy     14,102    35MB     3d 14h     2ms ago
 3  busy     14,450    31MB     3d 14h     8ms ago
...
16 workers | 12 busy | 4 idle | 0 queued | 0 crashed
```

---

### `ephpm db`

DB proxy inspection.

```bash
# Pool status
ephpm db status

# Top query digests (by total time)
ephpm db digests
ephpm db digests --sort count     # by execution count
ephpm db digests --sort max-time  # by worst single execution
ephpm db digests --limit 20

# Slow query log
ephpm db slow
ephpm db slow --since 1h
ephpm db slow --with-explain      # include EXPLAIN output

# Reset digest stats
ephpm db digests reset
```

```
$ ephpm db digests --limit 5
DIGEST       QUERY                                           COUNT    AVG      MAX      TOTAL
0xa3f2b1c4   SELECT * FROM users WHERE id = ?                45,231   2.1ms    89ms     95.0s
0xb1c4d9e7   INSERT INTO orders (user_id, ...) VALUES (?)    12,089   5.3ms    210ms    64.1s
0xd9e7f2a3   SELECT * FROM products WHERE category = ?        8,445   45.2ms   1.2s     381.8s
0xf2a3b1c4   UPDATE users SET last_login = ? WHERE id = ?     6,721   1.8ms    45ms     12.1s
0x1234abcd   SELECT COUNT(*) FROM orders WHERE status = ?     3,211   12.4ms   340ms    39.8s
```

---

### `ephpm kv`

KV store inspection and operations.

```bash
# Stats
ephpm kv stats

# Get/set/delete (for debugging — not a production data path)
ephpm kv get session:abc
ephpm kv set mykey myvalue --ttl 3600
ephpm kv del mykey

# Cluster membership
ephpm kv cluster

# Key scan (pattern match, like Redis SCAN)
ephpm kv keys "session:*" --limit 100
```

```
$ ephpm kv stats
Memory:     124MB / 512MB (24%)
Keys:       89,421
Hit rate:   98.7% (last 5m)
Evictions:  0 (last 5m)
Policy:     allkeys-lru

$ ephpm kv cluster
NODE            STATUS    KEYS      MEMORY    VNODES
10.0.1.1:7946   healthy   31,204    42MB      150
10.0.1.2:7946   healthy   28,891    39MB      150
10.0.1.3:7946   healthy   29,326    43MB      150
Replication: async, factor=2
```

---

### `ephpm cluster`

Cluster management.

```bash
# Cluster status
ephpm cluster status

# Force a node to leave
ephpm cluster leave --node 10.0.1.3:7946

# Show hash ring
ephpm cluster ring

# Show replication status
ephpm cluster replication
```

---

### `ephpm traces`

View recent traces from the ring buffer.

```bash
# List recent traces
ephpm traces
ephpm traces --limit 50

# Filter by slow requests
ephpm traces --min-duration 500ms

# Filter by status code
ephpm traces --status 500

# Show trace detail
ephpm traces show <trace-id>

# Live tail
ephpm traces tail
```

```
$ ephpm traces --min-duration 500ms --limit 5
TRACE ID          METHOD  PATH              STATUS  DURATION  DB QUERIES  KV OPS
a1b2c3d4e5f6     GET     /api/products     200     892ms     12          3
f6e5d4c3b2a1     POST    /checkout         200     1,204ms   28          7
...

$ ephpm traces show a1b2c3d4e5f6
[HTTP GET /api/products 892ms]
  ├─ [PHP: App\Http\Controllers\ProductController@index 845ms]
  │    ├─ [DB: SELECT * FROM products WHERE category = ? 312ms] ← SLOW
  │    ├─ [DB: SELECT * FROM categories WHERE id IN (?, ?, ?) 8ms]
  │    ├─ [KV: GET cache:products:featured 0.2ms] HIT
  │    ├─ [DB: SELECT COUNT(*) FROM reviews WHERE product_id IN (...) 445ms] ← SLOW
  │    └─ [KV: SET cache:products:listing 0.4ms]
  └─ [Response: 200 OK, 12.4KB]
```

---

## Diagnostic Commands

### `ephpm version`

```bash
$ ephpm version
ephpm 0.1.0 (rustc 1.83.0, PHP 8.4.2 ZTS)
Built:   2026-03-15T10:30:00Z
Commit:  a1b2c3d
Target:  x86_64-unknown-linux-gnu
PHP Extensions: core, date, json, pcre, pdo, pdo_mysql, pdo_pgsql,
                mbstring, openssl, curl, xml, zip, opcache, sodium
```

Shows the embedded PHP version and compiled extensions. Important because the PHP version is baked into the binary.

---

### `ephpm php`

Interact with the embedded PHP interpreter directly.

```bash
# PHP version info (like php -v)
ephpm php version

# PHP info (like php -i, but from the embedded SAPI)
ephpm php info

# List compiled extensions
ephpm php extensions

# Run a PHP file with the embedded interpreter
ephpm php run script.php

# Evaluate PHP code
ephpm php eval "echo phpversion();"

# Interactive REPL (if feasible)
ephpm php repl
```

This is useful for verifying the embedded PHP works, checking which extensions are available, and debugging PHP issues without starting the full server.

---

### `ephpm doctor`

Run diagnostics to verify the system is ready.

```bash
$ ephpm doctor
Checking ePHPm environment...

✓ PHP 8.4.2 ZTS embedded and functional
✓ OPcache enabled
✓ Config ./ephpm.toml valid
✓ PHP root ./public/index.php exists
✓ Port 443 available
✓ Port 9090 available
✓ DB connection: mysql://...@db:3306/myapp — connected (5ms)
✓ DB user has PROCESS privilege (required for auto-EXPLAIN)
✗ Cluster: seed node 10.0.1.2:7946 unreachable
✓ TLS: ACME account registered with Let's Encrypt
✓ DNS: example.com resolves to this server (93.184.216.34)
✓ Memory: 16GB available, recommended min 512MB per worker × 16 workers = 8GB

1 issue found:
  ✗ Cluster seed node 10.0.1.2:7946 unreachable — check firewall or node status
```

---

## Command Summary

```
ephpm serve          Start the PHP application server
ephpm admin          Start the admin UI (standalone)
ephpm stop           Graceful shutdown of a running instance
ephpm reload         Reload config + rolling worker restart

ephpm init           Scaffold ephpm.toml
ephpm validate       Check config for errors
ephpm config         Show effective configuration

ephpm status         Quick overview of a running node
ephpm workers        PHP worker pool details + restart
ephpm db             DB proxy: pool stats, query digests, slow queries
ephpm kv             KV store: stats, get/set/del, cluster membership
ephpm cluster        Cluster management: status, ring, replication
ephpm traces         View/filter/tail distributed traces

ephpm version        Version, build info, embedded PHP version
ephpm php            Interact with embedded PHP (version, info, eval, run)
ephpm doctor         Run system diagnostics
```

---

## Design Principles

1. **Inspection commands connect to the Node API.** They don't read internal state directly — they're HTTP clients to `:9090`. This means they work locally (`ephpm status`) or remotely (`ephpm status --node 10.0.1.1:9090`).

2. **Everything works without a config file for basic usage.** `ephpm serve --listen :8080 --php-root ./public` should work with zero config. The config file is for production tuning.

3. **Machine-readable output.** All inspection commands support `--json` for scripting and automation:
   ```bash
   ephpm workers --json | jq '.[] | select(.status == "busy")'
   ephpm db digests --json --sort total-time --limit 10
   ```

4. **No interactive prompts in production commands.** `ephpm serve`, `ephpm admin`, `ephpm stop`, `ephpm reload` never prompt. Only `ephpm init` is interactive (and has `--minimal`/`--full` for non-interactive use).

5. **Consistent `--node` flag.** Any inspection command can target a remote node:
   ```bash
   ephpm status --node 10.0.1.1:9090
   ephpm workers --node 10.0.1.1:9090
   ephpm db digests --node 10.0.1.1:9090
   ```
   Without `--node`, commands connect to `localhost:9090` (assumes local instance).

6. **Exit codes matter.** `0` = success, `1` = error, `2` = validation failure. `ephpm validate` and `ephpm doctor` use this for CI/CD gating:
   ```bash
   ephpm validate && ephpm serve
   ```

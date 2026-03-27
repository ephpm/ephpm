# Database Proxy & Connection Pooling

This document covers the SQL connection pooling proxy, wire protocol implementation, query analysis, read/write splitting, and PHP integration.

---

## Quick Start Examples

### Minimal — Just Connection Pooling

One MySQL database, no replicas, no splitting. ePHPm pools connections and auto-wires PHP:

```toml
[db.mysql]
url = "mysql://appuser:secret@db-server:3306/myapp"
inject_env = true
```

That's it. ePHPm listens on `127.0.0.1:3306`, injects `DB_HOST=127.0.0.1` into PHP, and Laravel/WordPress connects through the proxy automatically. 200 PHP requests share 50 backend connections (default `max_connections`).

### WordPress Production

WordPress with a primary and read replica, slow query tracking:

```toml
[db.mysql]
url = "mysql://wordpress:secret@db-primary:3306/wordpress"
min_connections = 10
max_connections = 100
inject_env = true

[db.mysql.replicas]
urls = ["mysql://wordpress:secret@db-replica:3306/wordpress"]

[db.read_write_split]
enabled = true
strategy = "sticky-after-write"
sticky_duration = "2s"

[db.analysis]
slow_query_threshold = "200ms"
auto_explain = true
```

WordPress reads (most page loads) go to the replica. Writes (post saves, comment submissions) go to the primary. After a write, reads stick to the primary for 2s to avoid stale data. Queries over 200ms are logged with EXPLAIN output.

### Laravel with PostgreSQL

Laravel app on Postgres with read replicas and aggressive pooling:

```toml
[db.postgres]
url = "postgres://laravel:secret@pg-primary:5432/myapp"
socket = "/run/ephpm/pgsql.sock"
min_connections = 5
max_connections = 30
pool_timeout = "3s"
inject_env = true

[db.postgres.replicas]
urls = [
    "postgres://laravel:secret@pg-replica-1:5432/myapp",
    "postgres://laravel:secret@pg-replica-2:5432/myapp",
]

[db.read_write_split]
enabled = true
strategy = "lag-aware"
max_replica_lag = "500ms"

[db.analysis]
slow_query_threshold = "50ms"
auto_explain = true
auto_explain_target = "replica"
```

Uses Unix socket for lower latency. Lag-aware splitting monitors replica lag and only routes reads to replicas within 500ms of the primary. Slow query threshold at 50ms for a performance-sensitive app.

### Dual Database (MySQL + PostgreSQL)

Some apps use both — e.g., WordPress on MySQL plus a Postgres analytics database:

```toml
[db.mysql]
url = "mysql://wordpress:secret@mysql-primary:3306/wordpress"
listen = "127.0.0.1:3306"
inject_env = true

[db.postgres]
url = "postgres://analytics:secret@pg-primary:5432/analytics"
listen = "127.0.0.1:5432"
# inject_env = false — don't override MySQL env vars, configure Postgres connection in app
```

Each database gets its own proxy listener, connection pool, and query analysis. They operate independently.

### Tuning the Pool

The defaults work for most apps, but here's how to tune for specific scenarios:

```toml
[db.mysql]
url = "mysql://user:pass@db:3306/myapp"

# High-traffic site (many concurrent requests)
max_connections = 100        # more backend connections
pool_timeout = "10s"         # wait longer for a connection vs failing

# Low-traffic site (save database resources)
min_connections = 1          # don't hold idle connections
max_connections = 10
idle_timeout = "60s"         # close idle connections quickly

# Long-running queries (reports, exports)
max_lifetime = "3600s"       # 1 hour max connection age
pool_timeout = "30s"         # wait longer since queries are slow

# Connection reset strategy
reset_strategy = "smart"     # "smart" = reset only if session state changed (default)
                             # "always" = reset every time (safest, slight overhead)
                             # "never" = skip reset (fastest, only for stateless queries)
```

### Kubernetes Deployment

In Kubernetes, database credentials come from Secrets:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ephpm
spec:
  template:
    spec:
      containers:
        - name: ephpm
          env:
            # Override db.mysql.url from a Secret
            - name: EPHPM_DB__MYSQL__URL
              valueFrom:
                secretKeyRef:
                  name: db-credentials
                  key: mysql-url
            # Override replica URLs
            - name: EPHPM_DB__MYSQL__REPLICAS__URLS
              value: '["mysql://user:pass@replica-1:3306/myapp","mysql://user:pass@replica-2:3306/myapp"]'
```

All `[db.*]` config values can be overridden with `EPHPM_DB__*` environment variables using `__` as the nesting separator, same as every other ephpm config.

---

## Why a DB Proxy in a PHP Server

The traditional PHP stack opens a new database connection per request (or per worker in persistent mode). This creates several problems:

```
Without proxy (traditional):
  PHP Worker 1 ──► MySQL connection 1 ──┐
  PHP Worker 2 ──► MySQL connection 2 ──├──► MySQL (max_connections = 151 default)
  PHP Worker 3 ──► MySQL connection 3 ──┤
  ...                                   │
  PHP Worker 100 ──► MySQL conn 100 ────┘
  PHP Worker 101 ──► ERROR: Too many connections
```

```
With ePHPm's DB proxy:
  PHP Worker 1 ──┐                    ┌──► MySQL connection 1 ──┐
  PHP Worker 2 ──┤                    ├──► MySQL connection 2 ──├──► MySQL
  PHP Worker 3 ──┼──► ePHPm Proxy ────┼──► MySQL connection 3 ──┤
  ...            │    (multiplexing)  ├──► ...                  │
  PHP Worker 200 ┘                    └──► MySQL connection 20 ─┘
```

Because ePHPm controls both the PHP workers AND the proxy, the connection is never over TCP — it's an in-process function call from the PHP worker to the proxy pool. Zero network overhead.

**No competitor does this.** FrankenPHP and RoadRunner don't have connection pooling. Swoole has `PDOPool` but it's PHP-level (still TCP to the database, no query analysis). ProxySQL is a separate process requiring TCP between the app and the proxy.

### Inspiration: ProxySQL

[ProxySQL](https://proxysql.com/) (6.6k GitHub stars, C++, GPL-3.0) is the gold standard for MySQL proxying. It provides connection multiplexing (50:1 ratios in production), query digest and stats, query caching, read/write splitting, failover detection, and query mirroring. ProxySQL can't be embedded (GPL-3.0, standalone C++ server). But ePHPm can replicate its most valuable features at the wire protocol level.

### What ePHPm's DB Proxy Is NOT

- **Not a general-purpose database proxy** — purpose-built for the ePHPm PHP server. Only needs to handle PHP workers, not arbitrary clients.
- **Not a query rewriter** — observes and analyzes queries, doesn't modify them (avoids breaking application semantics).
- **Not a query cache** — the KV store handles caching. Mixing query caching into the proxy adds complexity and cache invalidation headaches.

---

## Architecture

```
PHP Worker calls mysql_connect("127.0.0.1:3306") or PDO("mysql:host=127.0.0.1")
       │
       │  (ePHPm intercepts — this is localhost, so it's the proxy)
       ▼
┌─────────────────────────────────────────────────────────┐
│                    ePHPm DB Proxy                       │
│                                                         │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────┐  │
│  │ Protocol    │  │ Query        │  │ Connection    │  │
│  │ Frontend    │  │ Analyzer     │  │ Pool          │  │
│  │             │  │              │  │               │  │
│  │ Accept      │  │ Parse SQL    │  │ Backend conns │  │
│  │ MySQL/PG    │  │ Compute      │  │ to real DB    │  │
│  │ wire proto  │  │ digest       │  │               │  │
│  │ from PHP    │  │ Classify     │  │ Min/max pool  │  │
│  │             │  │ R/W          │  │ size, idle    │  │
│  │             │  │ Track timing │  │ timeout,      │  │
│  │             │  │ Detect slow  │  │ health check  │  │
│  └──────┬──────┘  └──────┬───────┘  └───────┬───────┘  │
│         │                │                   │          │
│         ▼                ▼                   ▼          │
│  ┌─────────────────────────────────────────────────┐    │
│  │              Metrics / Trace Emitter            │    │
│  │  → Query digest stats (count, sum/min/max time) │    │
│  │  → Slow query log (with EXPLAIN output)         │    │
│  │  → OTel spans per query (for trace correlation) │    │
│  │  → Prometheus metrics (pool utilization, QPS)   │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
       │
       ▼
  Actual MySQL / PostgreSQL server(s)
```

### Crate Dependencies

| Crate | Purpose |
|-------|---------|
| [`pgwire`](https://github.com/sunng87/pgwire) | PostgreSQL wire protocol — server and client APIs, SSL, SCRAM-SHA-256 auth, simple + extended query protocol |
| [`sqlx`](https://github.com/launchbadge/sqlx) | Async Postgres/MySQL driver — backend connection pool |
| [`mysql_async`](https://github.com/blackbeam/mysql_async) | Tokio-based MySQL client driver — backend pool alternative |
| [`sqlparser-rs`](https://github.com/apache/datafusion-sqlparser-rs) | SQL parsing with MySQL and PostgreSQL dialect support — query digest, R/W classification |

`pgwire` is the standout for Postgres — explicitly designed for building proxies, handles auth, SSL, query protocol, and cancellation. For MySQL, the server-side wire protocol needs to be implemented from scratch (~1,000-2,000 lines of Rust), since no mature MySQL server-side crate exists. The MySQL protocol is simpler than Postgres — straightforward packet-based format.

Reference implementations: [PgDog](https://github.com/pgdogdev/pgdog) (4.1k stars, Rust, AGPL-3.0) — validates the approach. Can't embed (AGPL), but excellent reference architecture.

---

## Connection Pool

### Backend Pool

```rust
use sqlx::mysql::MySqlPool;
use sqlx::postgres::PgPool;

struct PoolConfig {
    url: String,
    min_connections: u32,
    max_connections: u32,
    idle_timeout: Duration,
    max_lifetime: Duration,
    health_check_interval: Duration,
}
```

The proxy maps N frontend connections (from PHP workers) to M backend connections (to the database), where N >> M. `sqlx::MySqlPool` or `mysql_async` handles the real TCP connections, keepalive, health checks, and reconnection.

### Connection Lifecycle

```
PHP: new PDO("mysql:host=127.0.0.1")
       │
       ▼
  1. Frontend accepts connection (MySQL/PG wire handshake)
  2. PHP sends authentication credentials
  3. Proxy validates credentials against config
       (NOT forwarded to real DB — proxy authenticates independently)
  4. Connection established — proxy creates a ConnectionState
       │
       ▼
  5. PHP sends COM_QUERY "SELECT * FROM users WHERE id = 5"
       │
       ▼
  6. Proxy acquires backend connection from pool
     (if none available, waits up to pool_timeout)
  7. Forwards query to real database
  8. Receives result set
  9. Returns backend connection to pool
  10. Forwards result set to PHP via wire protocol
       │
       ▼
  11. PHP sends COM_QUIT
  12. Frontend connection closed
      (backend connections remain pooled)
```

Key insight: the frontend connection (PHP → proxy) and backend connection (proxy → database) have **independent lifecycles**. A backend connection may serve hundreds of frontend queries from different PHP requests. This is the multiplexing that gives 50:1+ ratios.

### Connection State Tracking

Some SQL operations create session state that must be tracked per-frontend-connection:

```rust
struct ConnectionState {
    in_transaction: bool,
    pinned_to: Option<PoolTarget>,       // primary or specific replica
    sticky_until: Option<Instant>,       // sticky-after-write expiry
    session_vars: Vec<(String, String)>, // SET variable tracking
    prepared_stmts: HashMap<String, PreparedStatement>,
}
```

| State | Implication |
|-------|------------|
| `SET @var = value` | Must replay on backend connection before next query |
| `BEGIN` / `START TRANSACTION` | Pin to one backend connection until `COMMIT`/`ROLLBACK` |
| `PREPARE stmt` | Prepared statement lives on a specific backend connection |
| `SET NAMES utf8mb4` | Must propagate to backend |
| `USE database` | Must propagate to backend |

When a query requires a specific backend (transaction pinning, prepared statement), the proxy holds the backend connection instead of returning it to the pool. This reduces multiplexing efficiency during transactions, so short transactions are encouraged.

### Session State Isolation

When a backend connection returns to the pool after serving one frontend, it may carry leftover state (temporary tables, user variables, changed `sql_mode`). Before reuse, the proxy must reset the connection:

**MySQL:** `COM_RESET_CONNECTION` (fast, preserves the TCP connection but resets session state) or `COM_CHANGE_USER` (heavier, re-authenticates).

**PostgreSQL:** `DISCARD ALL` resets session state. Or use `RESET ALL` + `DEALLOCATE ALL` for finer control.

The proxy tracks which state changes were made during a frontend's use of the backend connection. If no state changes occurred (the common case — most queries are stateless SELECT/INSERT), no reset is needed. This avoids the overhead of resetting clean connections.

```rust
enum ConnectionResetStrategy {
    /// Reset only if session state was modified (optimal)
    Smart,
    /// Always reset on return to pool (safest)
    Always,
    /// Never reset (fastest, risky — only for trusted apps)
    Never,
}
```

---

## Wire Protocol Implementation

### MySQL Frontend

The MySQL wire protocol is packet-based. ePHPm implements the server side (accepting connections from PHP):

```
MySQL Client Protocol:
  1. Server sends Handshake (protocol version, capabilities, auth challenge)
  2. Client sends HandshakeResponse (username, auth data, database, capabilities)
  3. Server sends OK or ERR
  4. Command phase:
     - COM_QUERY: text query → ResultSet or OK/ERR
     - COM_STMT_PREPARE: prepared statement → StmtPrepareOK
     - COM_STMT_EXECUTE: execute prepared → ResultSet or OK/ERR
     - COM_STMT_CLOSE: close prepared statement
     - COM_INIT_DB: switch database (USE)
     - COM_PING: keepalive
     - COM_QUIT: disconnect
```

Commands the proxy must handle:

| Command | Action |
|---------|--------|
| `COM_QUERY` | Parse SQL, classify R/W, acquire backend, forward, return result |
| `COM_STMT_PREPARE` | Forward to backend, cache statement ID mapping |
| `COM_STMT_EXECUTE` | Route to backend that holds the prepared statement |
| `COM_STMT_CLOSE` | Close on backend, remove mapping |
| `COM_INIT_DB` | Record database change, propagate to backend |
| `COM_PING` | Respond directly (don't hit backend) |
| `COM_QUIT` | Close frontend connection, return backend to pool |
| `COM_RESET_CONNECTION` | Reset frontend state |

Estimated: ~1,000-2,000 lines of Rust for the MySQL protocol layer.

### PostgreSQL Frontend

Use `pgwire` crate which handles the protocol:

```
PostgreSQL Wire Protocol:
  1. Startup message (protocol version, user, database)
  2. Authentication (SCRAM-SHA-256, MD5, or trust)
  3. ReadyForQuery
  4. Query cycle:
     Simple query protocol:
       - Query message → RowDescription + DataRow* + CommandComplete
     Extended query protocol:
       - Parse → Bind → Describe → Execute → Sync
       - Supports prepared statements and portals
```

`pgwire` provides `SimpleQueryHandler` and `ExtendedQueryHandler` traits. ePHPm implements these to intercept queries, run them through the analyzer, and forward to the backend pool.

### Unix Socket Frontend

Like the KV RESP listener, the DB proxy can also listen on Unix sockets for lower latency:

```toml
[db.mysql]
listen = "127.0.0.1:3306"              # TCP (default, zero-config)
socket = "/run/ephpm/mysql.sock"        # Unix socket (~2-5x faster)

[db.postgres]
listen = "127.0.0.1:5432"
socket = "/run/ephpm/pgsql.sock"
```

PHP frameworks detect Unix sockets automatically:
- **MySQL**: `PDO("mysql:unix_socket=/run/ephpm/mysql.sock;dbname=myapp")`
- **PostgreSQL**: `PDO("pgsql:host=/run/ephpm;dbname=myapp")` (PG uses directory, not file)

---

## Query Analysis

### Query Digest

Inspired by ProxySQL's `stats_mysql_query_digest`. Every query is normalized by replacing literal values with placeholders, then hashed to produce a fingerprint:

```rust
use sqlparser::parser::Parser;
use sqlparser::dialect::MySqlDialect;

/// Normalize a query by replacing literal values with placeholders.
/// "SELECT * FROM users WHERE id = 42 AND name = 'alice'"
/// → "SELECT * FROM users WHERE id = ? AND name = ?"
fn normalize_query(sql: &str) -> String {
    // Use sqlparser to parse, walk the AST, replace Value nodes with "?"
    // Handles strings, numbers, booleans, NULL, etc.
}

/// Compute a digest hash from the normalized query.
fn query_digest(normalized: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}
```

Per-digest statistics:

```rust
struct DigestStats {
    digest: u64,
    digest_text: String,           // normalized query text
    schema: String,
    count: AtomicU64,              // total executions
    sum_time_us: AtomicU64,        // total execution time (microseconds)
    min_time_us: AtomicU64,
    max_time_us: AtomicU64,
    sum_rows_affected: AtomicU64,
    sum_rows_sent: AtomicU64,
    first_seen: Instant,
    last_seen: AtomicInstant,
}

struct DigestStore {
    digests: DashMap<u64, DigestStats>,
}
```

This gives the admin UI ProxySQL-grade query intelligence:

| digest | digest_text | count | avg_time | max_time |
|--------|-------------|-------|----------|----------|
| `0xa3f2...` | `SELECT * FROM users WHERE id = ?` | 45,231 | 2.1ms | 89ms |
| `0xb1c4...` | `INSERT INTO orders (user_id, ...) VALUES (?, ...)` | 12,089 | 5.3ms | 210ms |
| `0xd9e7...` | `SELECT * FROM products WHERE category = ? ORDER BY ? LIMIT ?` | 8,445 | 45.2ms | 1,200ms |

### Security: Digest Logging

Query digests must **never** log sensitive parameter values. The normalization step replaces all literals with `?` before storage. The raw query with actual values is only held in memory during execution and is never persisted. This is critical — query logs that contain `WHERE email = 'user@example.com'` or `WHERE credit_card = '4111...'` are a data breach waiting to happen.

### Slow Query Detection

When a query exceeds a configurable threshold, ePHPm captures it and optionally runs `EXPLAIN` on a replica:

```toml
[db.analysis]
slow_query_threshold = "100ms"
auto_explain = true              # run EXPLAIN on slow queries
auto_explain_target = "replica"  # don't hit primary with EXPLAINs
```

```rust
async fn handle_query_result(
    query: &str,
    digest: u64,
    duration: Duration,
    config: &AnalysisConfig,
    replica_pool: &Pool,
) {
    digest_store.record(digest, duration);

    if duration > config.slow_query_threshold {
        let slow_query = SlowQuery {
            sql: query.to_string(),
            digest,
            duration,
            timestamp: Instant::now(),
            explain: None,
        };

        // Auto-EXPLAIN on a replica (non-blocking, background task)
        if config.auto_explain {
            let explain_sql = format!("EXPLAIN ANALYZE {}", query);
            if let Ok(explain_result) = replica_pool.fetch_one(&explain_sql).await {
                slow_query.explain = Some(explain_result);
            }
        }

        slow_query_store.push(slow_query);
        // Also emits an OTel span event for trace correlation
    }
}
```

The admin UI surfaces this as a slow query dashboard with the EXPLAIN plan inline — no need for external tools like `pt-query-digest` or Percona Monitoring.

---

## Read/Write Splitting

`sqlparser-rs` classifies queries by type:

```rust
use sqlparser::ast::Statement;

fn classify_query(stmt: &Statement) -> QueryType {
    match stmt {
        Statement::Query(_) => {
            // Check for FOR UPDATE / FOR SHARE — these are writes (take locks)
            // Check for INTO OUTFILE — this is a write
            // Otherwise: read
            QueryType::Read
        }
        Statement::ShowTables { .. } |
        Statement::ShowColumns { .. } |
        Statement::Explain { .. } => QueryType::Read,

        Statement::Insert { .. } |
        Statement::Update { .. } |
        Statement::Delete { .. } |
        Statement::CreateTable { .. } |
        Statement::AlterTable { .. } |
        Statement::Drop { .. } |
        Statement::Truncate { .. } => QueryType::Write,

        Statement::StartTransaction { .. } => QueryType::TransactionBegin,
        Statement::Commit { .. } => QueryType::TransactionEnd,
        Statement::Rollback { .. } => QueryType::TransactionEnd,

        // Unknown — safe default is primary
        _ => QueryType::Write,
    }
}
```

Read-only queries route to the replica pool. Writes route to the primary. Unknown statement types default to primary (conservative).

```
PHP: SELECT * FROM users WHERE id = 5
  → ePHPm parses → Read → routes to replica pool

PHP: INSERT INTO orders (...) VALUES (...)
  → ePHPm parses → Write → routes to primary

PHP: SELECT * FROM inventory WHERE id = 5 FOR UPDATE
  → ePHPm parses → Write (FOR UPDATE takes locks) → routes to primary

PHP: CALL process_order(42)
  → ePHPm parses → Unknown → defaults to primary (safe)

PHP: BEGIN; SELECT ...; UPDATE ...; COMMIT;
  → ePHPm detects TransactionBegin → pins to primary until TransactionEnd
```

### Transaction Tracking

Inside an explicit transaction, all queries must go to the primary — even SELECTs. The proxy detects `BEGIN`/`START TRANSACTION` at the wire protocol level and pins the connection to the primary until `COMMIT` or `ROLLBACK`. Implicit transactions (autocommit queries) don't require pinning.

### Replication Lag Awareness

A write hits the primary, then 50ms later a read goes to a replica that hasn't replicated yet — stale data. Two strategies:

```toml
[db.read_write_split]
enabled = true
strategy = "sticky-after-write"  # or "lag-aware"
sticky_duration = "2s"           # for sticky-after-write
max_replica_lag = "500ms"        # for lag-aware
```

**Strategy A: Sticky-after-write** (simpler, recommended default)

After a connection performs a write, all subsequent reads from that connection go to the primary for `sticky_duration` seconds. Simple, conservative, slight primary load increase during the sticky window.

**Strategy B: Lag-aware routing** (more complex, better distribution)

ePHPm periodically monitors replica lag via `SHOW SLAVE STATUS` (MySQL) or `pg_stat_replication` (Postgres). Only routes reads to replicas with lag below `max_replica_lag`. Replicas that fall behind are temporarily removed from the read pool.

---

## PHP Integration

### Transparent to the Application

The PHP app connects to `127.0.0.1:3306` (MySQL) or `127.0.0.1:5432` (Postgres) — which is ePHPm's proxy, not the real database. From PHP's perspective, it's talking to a normal database:

```php
// No code changes. Standard PDO.
$pdo = new PDO('mysql:host=127.0.0.1;dbname=myapp', 'user', 'pass');
$stmt = $pdo->prepare('SELECT * FROM users WHERE id = ?');
$stmt->execute([42]);
```

### Auto-Configuration via Environment Injection

Since ePHPm owns both the PHP process and the DB proxy, it can auto-wire the connection — configure the real database once in `ephpm.toml`, and PHP picks it up automatically:

```toml
[db.mysql]
url = "mysql://appuser:secret@real-db-server:3306/myapp"
max_connections = 50
inject_env = true   # auto-set DB env vars for the PHP app
```

When `inject_env = true`, ePHPm injects environment variables into the PHP runtime:

| Variable | Value | Consumed by |
|----------|-------|-------------|
| `DB_CONNECTION` | `mysql` | Laravel |
| `DB_HOST` | `127.0.0.1` | Laravel, Symfony, generic |
| `DB_PORT` | `3306` | Laravel, Symfony, generic |
| `DB_DATABASE` | `myapp` | Laravel |
| `DB_USERNAME` | `appuser` | Laravel |
| `DB_PASSWORD` | `secret` | Laravel |
| `DATABASE_URL` | `mysql://appuser:secret@127.0.0.1:3306/myapp` | Symfony, Doctrine |

```
Developer configures:  ephpm.toml → url = "mysql://...@real-db:3306/myapp"
ePHPm injects:         DB_HOST=127.0.0.1, DB_PORT=3306, ...
Laravel reads:         config/database.php → env('DB_HOST') → 127.0.0.1
PDO connects to:       127.0.0.1:3306 (ePHPm proxy)
ePHPm routes to:       real-db:3306 (pooled, analyzed, split)
```

For Postgres, the same pattern applies:

```toml
[db.postgres]
url = "postgres://appuser:secret@real-pg:5432/myapp"
inject_env = true
# Injects: DB_HOST=127.0.0.1, DB_PORT=5432, DATABASE_URL=postgres://...@127.0.0.1:5432/myapp
```

When `inject_env` is disabled, the developer manually points their app at the proxy (e.g., `DB_HOST=127.0.0.1` in `.env`).

### Optional: SAPI Direct Access

For maximum performance, bypass TCP entirely with SAPI-level functions:

```php
// Zero-overhead path via SAPI (no TCP, no wire protocol)
$result = ephpm_db_query('SELECT * FROM users WHERE id = ?', [42]);
```

But TCP compatibility is the priority — it means **zero code changes** for existing apps.

---

## Sharding (Post-v1 Roadmap)

Sharding splits **data** across multiple independent database instances. Unlike read/write splitting (one database, multiple copies), each shard holds a subset of the data.

### Shard Key Routing

```toml
[db.sharding]
enabled = true
key_column = "tenant_id"
hash_function = "consistent"
shards = [
    { name = "shard-a", url = "mysql://shard-a:3306/app", range = "0x0000-0x5555" },
    { name = "shard-b", url = "mysql://shard-b:3306/app", range = "0x5556-0xAAAA" },
    { name = "shard-c", url = "mysql://shard-c:3306/app", range = "0xAAAB-0xFFFF" },
]
```

ePHPm parses the SQL, extracts the shard key from the WHERE clause, hashes it, and routes to the correct shard. When the shard key isn't present, the query scatters to all shards and results are merged.

### The Hard Problems

| Problem | Description | Approach |
|---------|-------------|----------|
| **Scatter-gather** | No shard key in WHERE — must query all shards and merge results, re-apply ORDER BY/LIMIT/aggregations | Parallel fan-out, merge strategy per query type |
| **Cross-shard JOINs** | Tables on different shards can't JOIN efficiently | Co-location by convention — tables that JOIN shard on the same key |
| **Cross-shard transactions** | Atomic commit across multiple databases requires 2PC | Prohibit in v1 (same as ProxySQL, PgDog). Use application-level sagas. |
| **Schema migrations** | DDL must apply to all shards, partial failure handling is tricky | Fan out DDL, but typically handled outside the proxy via migration tools |
| **Shard rebalancing** | Adding/removing shards requires data movement during transition period | Consistent hashing minimizes movement (~1/N of keys) |

### Sharding Implementation Roadmap

| Phase | Scope | Complexity |
|-------|-------|------------|
| **v2** | Single-key shard routing, scatter-gather for keyless queries, shard-aware connection pools | High |
| **v3** | Aggregation merging (COUNT/SUM/AVG across shards), ORDER BY + LIMIT rewriting, cross-shard JOIN detection/warnings | High |
| **v4** | Shard rebalancing, online shard addition/removal, 2PC (optional) | Very High |

---

## Feature Comparison

| Feature | ProxySQL | PgBouncer | PgDog | ePHPm Goal |
|---------|----------|-----------|-------|------------|
| Connection pooling | Yes | Yes | Yes | v1 |
| Connection multiplexing | Yes (50:1) | Yes | Yes | v1 |
| Query digest / stats | Yes | No | No | v1 |
| Slow query detection | Yes | No | No | v1 + auto-EXPLAIN |
| Read/write splitting | Yes | No | Yes | v1 |
| Replication lag awareness | No | No | Yes | v1 |
| Sharding (single-key) | Yes (regex rules) | No | Yes (wire protocol) | v2 |
| Scatter-gather queries | No | No | Yes | v2 |
| Cross-shard aggregation | No | No | Partial | v3 |
| Shard rebalancing | No | No | No | v4 |
| In-process with app | **No** (separate process) | **No** | **No** | **Yes** (zero TCP to proxy) |
| OTel trace integration | **No** | **No** | **No** | **Yes** (spans per query) |
| Admin UI for queries | **No** (CLI only) | **No** | **No** | **Yes** |
| MySQL support | Yes | No | No | Yes |
| PostgreSQL support | Partial | Yes | Yes | Yes |
| License | GPL-3.0 | ISC | AGPL-3.0 | — |

---

## Security

- Wire protocol parsing in Rust — memory-safe by default
- Connection credentials stored in config (same secret handling as TLS keys)
- Query digest logging must not log sensitive parameter values — normalization replaces all literals with `?` before storage
- Connection pooling must isolate session state between frontend connections (see Connection State Tracking)
- Backend credentials never exposed to PHP — PHP authenticates against the proxy, proxy authenticates against the real database independently

---

## Node API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/db/digests` | GET | Query digest table: digest hash, normalized SQL, count, sum/min/max/avg time, rows |
| `/api/db/slow` | GET | Slow query log: recent slow queries with EXPLAIN output |
| `/api/db/pool` | GET | Connection pool stats: active/idle/total connections, wait time, timeouts |

---

## Implementation Order

| Phase | Scope | Depends on |
|-------|-------|------------|
| **1. MySQL frontend** | Wire protocol (handshake, COM_QUERY, COM_QUIT) | Nothing — can start now |
| **2. Backend pool** | `sqlx`/`mysql_async` connection pool with config | Phase 1 |
| **3. Query passthrough** | Forward queries from frontend to backend, return results | Phase 1 + 2 |
| **4. Query digest** | `sqlparser-rs` normalization, DigestStore, stats tracking | Phase 3 |
| **5. Slow query detection** | Threshold-based capture, auto-EXPLAIN on replica | Phase 4 |
| **6. Read/write splitting** | Query classification, replica pool, transaction tracking | Phase 4 |
| **7. Replication lag awareness** | Periodic lag monitoring, replica health | Phase 6 |
| **8. PostgreSQL frontend** | `pgwire` integration, same pool/analyzer backend | Phase 3 |
| **9. Session state tracking** | SET variable replay, connection reset on return | Phase 3 |
| **10. Unix socket frontend** | Listen on socket path for MySQL and PG | Phase 1 |
| **11. Environment injection** | Auto-wire DB env vars into PHP runtime | Phase 2 |
| **12. SAPI direct access** | `ephpm_db_query()` bypassing TCP | Phase 2 |
| **13. Node API endpoints** | `/api/db/digests`, `/api/db/slow`, `/api/db/pool` | Phase 4 + 5 |

Phases 1-3 are the MVP: a working MySQL proxy with connection pooling. Phase 8 (PostgreSQL) can start in parallel once the shared analyzer and pool infrastructure exists from phases 4-7.

---

## Configuration Reference

### Planned

```toml
[db.mysql]
url = "mysql://user:pass@db-primary:3306/myapp"
listen = "127.0.0.1:3306"             # proxy frontend listener
socket = "/run/ephpm/mysql.sock"       # unix socket (optional, faster)
min_connections = 5                    # minimum idle backend connections
max_connections = 50                   # maximum backend connections
idle_timeout = "300s"                  # close idle backend connections after
max_lifetime = "1800s"                 # max backend connection age
health_check_interval = "30s"
pool_timeout = "5s"                    # max wait for available backend connection
inject_env = true                      # auto-set DB_HOST, DB_PORT, etc.
reset_strategy = "smart"               # smart, always, or never

[db.mysql.replicas]
urls = [
    "mysql://user:pass@db-replica-1:3306/myapp",
    "mysql://user:pass@db-replica-2:3306/myapp",
]

[db.postgres]
url = "postgres://user:pass@pg-primary:5432/myapp"
listen = "127.0.0.1:5432"
socket = "/run/ephpm/pgsql.sock"
min_connections = 5
max_connections = 30
inject_env = true

[db.postgres.replicas]
urls = [
    "postgres://user:pass@pg-replica-1:5432/myapp",
]

[db.read_write_split]
enabled = false
strategy = "sticky-after-write"        # sticky-after-write or lag-aware
sticky_duration = "2s"
max_replica_lag = "500ms"              # for lag-aware strategy

[db.analysis]
slow_query_threshold = "100ms"
auto_explain = true
auto_explain_target = "replica"        # don't hit primary with EXPLAINs
digest_store_max_entries = 10000       # max unique query digests to track
```

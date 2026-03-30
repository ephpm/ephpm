# SQL Architecture

ePHPm supports two SQL strategies that cover every deployment scenario from local development to multi-node production clusters.

| Strategy | Backend | Use case | Crate |
|----------|---------|----------|-------|
| **DB Proxy** | Real MySQL / PostgreSQL servers | Production with existing databases | `ephpm-db` |
| **LiteWire** | SQLite (in-process or via sqld) | Development, CI/CD, edge, single-node | `litewire` (external crate) |

Both strategies are transparent to the PHP application. PHP connects to `127.0.0.1:3306` (or `:5432` / `:1433`) and sees a standard database server. The only difference is the backend.

---

## Feature Status

| Feature | DB Proxy | LiteWire | Notes |
|---------|----------|----------|-------|
| MySQL wire protocol | **Implemented** | Planned | DB Proxy: hand-rolled. LiteWire: `opensrv-mysql` |
| PostgreSQL wire protocol | Placeholder | Planned | LiteWire: `pgwire` |
| TDS (SQL Server) wire protocol | -- | Planned | LiteWire: custom TDS 7.4 |
| Hrana HTTP (sqld-compatible) | -- | Planned | LiteWire: `axum` |
| Connection pooling | **Implemented** | N/A | DB Proxy only (SQLite is in-process) |
| R/W splitting | **Implemented** | N/A | DB Proxy only (SQLite is single-writer) |
| SQL dialect translation | N/A | Planned | LiteWire only (MySQL/PG/T-SQL -> SQLite) |
| Pluggable backends | N/A | Planned | `rusqlite` (in-process), `libsql` (HTTP to sqld) |
| Smart connection reset | **Implemented** | N/A | DB Proxy only |
| Query digest / stats | Planned | -- | DB Proxy |
| Environment injection | **Implemented** | Planned | Both auto-wire `DB_HOST`, `DB_PORT`, etc. |

---

## Strategy Selection

```toml
# Production: real MySQL
[db.mysql]
url = "mysql://user:pass@db-server:3306/myapp"
inject_env = true

# Development: SQLite via LiteWire
[db.litewire]
backend = "rusqlite"
path = "app.db"
mysql_listen = "127.0.0.1:3306"
inject_env = true

# Clustered: SQLite with replication via sqld
[db.litewire]
backend = "libsql"
sqld_url = "http://127.0.0.1:8081"
mysql_listen = "127.0.0.1:3306"
hrana_listen = "127.0.0.1:8080"
inject_env = true
```

The `[db.mysql]` and `[db.litewire]` sections are mutually exclusive for the same listen port. ePHPm starts whichever is configured.

---

## DB Proxy (ephpm-db)

See [db-proxy.md](db-proxy.md) for the full design. Summary:

- Transparent MySQL proxy with `HandshakeV10` wire protocol
- Connection pool (semaphore-bounded, background maintenance, health checks)
- Read/write splitting with sticky-after-write and replica pools
- Smart connection reset (dirty-bit tracking, `COM_RESET_CONNECTION` only when needed)
- ~1,500 lines of Rust, zero external database driver dependencies (raw wire protocol)

The DB Proxy is for production deployments that use real MySQL or PostgreSQL servers. It multiplexes N PHP workers over M backend connections, provides query-level routing, and will add query digest/stats in a future release.

---

## LiteWire

LiteWire is a protocol translation proxy: it accepts MySQL, PostgreSQL, TDS (SQL Server), and Hrana connections, translates the SQL dialect to SQLite on the fly, and executes against a pluggable backend. Your PHP app thinks it's talking to MySQL -- it's actually talking to SQLite.

```mermaid
graph TD
    subgraph clients["Clients"]
        php["PHP\n(pdo_mysql, pdo_pgsql, pdo_sqlsrv)"]
        libsql["libsql SDK\n(Rust, JS, Python, Go)"]
    end

    subgraph litewire["LiteWire"]
        mysql_fe["MySQL Wire\n:3306"]
        pg_fe["PG Wire\n:5432"]
        tds_fe["TDS Wire\n:1433"]
        hrana_fe["Hrana HTTP\n:8080"]
        translator["SQL Translator\n(sqlparser-rs)"]
    end

    sqlite[("SQLite")]

    php --> mysql_fe
    php --> pg_fe
    php --> tds_fe
    libsql --> hrana_fe
    mysql_fe --> translator
    pg_fe --> translator
    tds_fe --> translator
    translator --> sqlite
    hrana_fe --> sqlite

    style litewire fill:#f5f5f5,stroke:#333
    style translator fill:#e3f2fd,stroke:#1565c0
    style mysql_fe fill:#fff3e0,stroke:#ef6c00
    style pg_fe fill:#e8f5e9,stroke:#388e3c
    style tds_fe fill:#f3e5f5,stroke:#7b1fa2
    style hrana_fe fill:#fce4ec,stroke:#c62828
```

### Why

| Problem | How LiteWire solves it |
|---------|----------------------|
| Docker required for local dev | SQLite. No container, no server, one file. |
| CI/CD database setup is slow and flaky | `litewire --db test.db` -- instant, disposable |
| Edge/single-node deployments need a database server | SQLite in-process, zero external deps |
| App code changes for SQLite | None. LiteWire translates wire protocol + SQL dialect |

### Repository

LiteWire is developed as a standalone crate at [litewire](https://github.com/nicholasgasior/litewire) and used by ePHPm as a library dependency. It is not part of the ePHPm workspace -- it has its own release cycle, test suite, and users outside of ePHPm.

### Crate Structure

```
litewire/
├── litewire/              # main crate (LiteWire builder + CLI binary)
├── litewire-translate/    # SQL dialect translation (sqlparser-rs)
│   ├── mysql.rs           # MySQL -> SQLite rewrites
│   ├── postgres.rs        # PG -> SQLite rewrites
│   ├── tds.rs             # T-SQL -> SQLite rewrites
│   ├── common.rs          # shared rewrites (types, functions, expressions)
│   ├── metadata.rs        # SHOW/DESCRIBE/INFORMATION_SCHEMA/sys.*
│   └── emit.rs            # AST -> SQLite SQL string
├── litewire-mysql/        # MySQL wire frontend (opensrv-mysql)
├── litewire-postgres/     # PG wire frontend (pgwire)
├── litewire-tds/          # TDS wire frontend (custom TDS 7.4)
├── litewire-hrana/        # Hrana HTTP frontend (axum, sqld-compatible)
└── litewire-backend/      # Backend trait + rusqlite + libsql implementations
```

### SQL Translation

The translator is the core of LiteWire. It uses `sqlparser-rs` to parse input SQL into an AST, rewrites dialect-specific nodes via a visitor pattern, then emits SQLite-compatible SQL.

```mermaid
flowchart LR
    A["Input SQL\n(MySQL/PG/T-SQL)"] --> B["sqlparser-rs\n(dialect-aware)"]
    B --> C["AST"]
    C --> D["Rewrite pass\n(visitor pattern)"]
    D --> E["SQLite AST"]
    E --> F["Emit"]
    F --> G["SQLite SQL"]

    style D fill:#e3f2fd,stroke:#1565c0
```

#### Expression Rewrites

| MySQL / PG / T-SQL | SQLite |
|---------------------|--------|
| `NOW()` / `GETDATE()` | `datetime('now')` |
| `CURDATE()` | `date('now')` |
| `UNIX_TIMESTAMP()` | `strftime('%s', 'now')` |
| `NEWID()` | `lower(hex(randomblob(16)))` |
| `TRUE` / `FALSE` | `1` / `0` |
| `ISNULL(a, b)` | `IFNULL(a, b)` |
| `::type` casts | `CAST(... AS ...)` |
| `$1`, `$2` params | `?1`, `?2` |
| `@@IDENTITY` | `last_insert_rowid()` |
| `@@ROWCOUNT` | `changes()` |

#### DDL Rewrites

| MySQL / PG / T-SQL | SQLite |
|---------------------|--------|
| `AUTO_INCREMENT` / `SERIAL` / `IDENTITY(1,1)` | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `VARCHAR(n)`, `CHAR(n)`, `NVARCHAR(n)` | `TEXT` |
| `INT`, `BIGINT`, `SMALLINT`, `TINYINT` | `INTEGER` |
| `FLOAT`, `DOUBLE`, `DECIMAL`, `MONEY` | `REAL` |
| `BLOB`, `LONGBLOB`, `BYTEA`, `IMAGE` | `BLOB` |
| `BOOLEAN`, `BIT` | `INTEGER` |
| `DATETIME`, `TIMESTAMP` | `TEXT` |
| `UNIQUEIDENTIFIER` | `TEXT` |
| `ENGINE=InnoDB`, `DEFAULT CHARSET=...` | Stripped |

#### DML Rewrites

| MySQL | SQLite |
|-------|--------|
| `INSERT ... ON DUPLICATE KEY UPDATE` | `INSERT ... ON CONFLICT DO UPDATE` |
| `REPLACE INTO` | Passed through (SQLite supports it) |
| `LIMIT x, y` | `LIMIT y OFFSET x` |
| `TOP n` (T-SQL) | `LIMIT n` |

#### Metadata Rewrites

| Query | SQLite equivalent |
|-------|-------------------|
| `SHOW TABLES` | `SELECT name FROM sqlite_master WHERE type='table' ORDER BY name` |
| `SHOW DATABASES` | Synthetic single-row result |
| `SHOW COLUMNS FROM t` / `DESCRIBE t` | `PRAGMA table_info(t)` |
| `SHOW CREATE TABLE t` | Reconstructed from `sqlite_master` |
| `SHOW INDEX FROM t` | `PRAGMA index_list(t)` + `PRAGMA index_info(...)` |
| `INFORMATION_SCHEMA.TABLES` | `sqlite_master` query |
| `INFORMATION_SCHEMA.COLUMNS` | `PRAGMA table_info` per table |
| `pg_catalog.*` | Mapped to equivalent PRAGMAs |
| `sys.tables` / `sys.columns` (T-SQL) | `sqlite_master` + `PRAGMA` |
| `sp_tables` / `sp_columns` (T-SQL) | `sqlite_master` + `PRAGMA` |

#### No-ops (Swallowed Silently)

- `SET NAMES ...`, `SET CHARACTER SET ...`
- `SET SESSION ...` / `SET GLOBAL ...` (most)
- `SET time_zone = ...`, `SET sql_mode = ...`
- `SET NOCOUNT ON`
- `BEGIN TRY ... END TRY`

### Wire Protocol Frontends

#### MySQL (opensrv-mysql)

Built on [opensrv-mysql](https://github.com/datafuselabs/opensrv) (Databend's production MySQL protocol crate). Implements the `AsyncMysqlShim` trait.

```mermaid
sequenceDiagram
    participant PHP as PHP (pdo_mysql)
    participant FE as MySQL Frontend
    participant TX as Translator
    participant BE as Backend

    PHP->>FE: TCP connect
    FE->>PHP: Handshake (greeting, auth OK)

    PHP->>FE: COM_QUERY "SELECT NOW()"
    FE->>TX: translate("SELECT NOW()", MySQL)
    TX->>FE: "SELECT datetime('now')"
    FE->>BE: query("SELECT datetime('now')")
    BE->>FE: ResultSet
    FE->>PHP: MySQL result packets

    PHP->>FE: COM_STMT_PREPARE "SELECT * FROM users WHERE id = ?"
    FE->>TX: translate(...)
    TX->>FE: "SELECT * FROM users WHERE id = ?1"
    FE->>BE: prepare(...)
    BE->>FE: PreparedStatement
    FE->>PHP: STMT_PREPARE OK

    PHP->>FE: COM_STMT_EXECUTE (params=[42])
    FE->>BE: query(..., [42])
    BE->>FE: ResultSet
    FE->>PHP: MySQL result packets
```

- **COM_QUERY**: Parse MySQL SQL, translate to SQLite, execute, return result set packets
- **COM_STMT_PREPARE / COM_STMT_EXECUTE**: Prepared statement protocol. Required for `pdo_mysql` which uses prepared statements by default.
- **COM_INIT_DB**: `USE database` -- no-op (SQLite has one database)
- **COM_PING**: Always OK
- **COM_FIELD_LIST**: Column metadata backed by `PRAGMA`
- **Auth**: Accepts any credentials (loopback only)

#### PostgreSQL (pgwire)

Built on [pgwire](https://github.com/sunng87/pgwire). Implements `SimpleQueryHandler` and `ExtendedQueryHandler` traits.

- **Simple query protocol**: `Query` -> translate PG SQL -> execute -> `RowDescription` + `DataRow` + `CommandComplete`
- **Extended query protocol**: `Parse`/`Bind`/`Describe`/`Execute`/`Sync` -- required for `pdo_pgsql`
- **Type OIDs**: SQLite affinities mapped to PG type OIDs so drivers handle types correctly

#### TDS (SQL Server)

Custom implementation -- no Rust crate exists for the server side of TDS. `tiberius` implements only the client. Targets TDS 7.4 (SQL Server 2012+).

- **Pre-Login**: TLS negotiation and version exchange
- **Login7**: SQL auth (NTLM/Kerberos not supported)
- **SQL Batch**: Text query (like `COM_QUERY`)
- **RPC Request**: `sp_executesql` for parameterized queries
- **Response tokens**: `COLMETADATA` + `ROW` + `DONE`

PHP connects via `pdo_sqlsrv` or `pdo_dblib`. Laravel's `sqlsrv` driver works unchanged.

#### Hrana HTTP (sqld-Compatible)

LiteWire implements the server side of Turso's Hrana 3 HTTP protocol, making it a lightweight drop-in replacement for sqld. Apps using the `libsql` client SDK can point at LiteWire instead of sqld -- no replication server needed.

- **Endpoint**: `POST /v2/pipeline`
- **No SQL translation**: Hrana clients send SQLite SQL natively. Bypasses the translator entirely.
- **Baton-based sessions**: The `baton` token links requests to a connection/transaction.
- **Batch requests**: Multiple statements in a single HTTP request with transactional semantics.
- **No auth by default**: Same as sqld in dev mode. Optional JWT for production.

### Pluggable Backends

| Backend | Crate | Use case |
|---------|-------|----------|
| `rusqlite` | `rusqlite` | In-process SQLite. Zero network. Wraps queries in `spawn_blocking`. |
| `libsql` | `libsql` | Remote sqld via HTTP/Hrana. Enables embedded replicas and replication. |
| Custom | implement `Backend` trait | Bring your own storage. |

```rust
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<ResultSet>;
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<ExecuteResult>;
    async fn prepare(&self, sql: &str) -> Result<PreparedStatement> { ... }
}
```

### Result Set Type Mapping

SQLite returns untyped text values. The wire protocol frontends map them to typed values using column `decltype` hints.

| SQLite affinity | MySQL type | PG type | TDS type |
|----------------|------------|---------|----------|
| `INTEGER` | `MYSQL_TYPE_LONGLONG` | `INT8` (OID 20) | `BIGINTTYPE` |
| `REAL` | `MYSQL_TYPE_DOUBLE` | `FLOAT8` (OID 701) | `FLT8TYPE` |
| `TEXT` | `MYSQL_TYPE_VAR_STRING` | `TEXT` (OID 25) | `NVARCHARTYPE` |
| `BLOB` | `MYSQL_TYPE_BLOB` | `BYTEA` (OID 17) | `IMAGETYPE` |
| `NULL` | `MYSQL_TYPE_NULL` | null indicator | null flag in `ROW` token |

---

## ePHPm Integration

LiteWire is used as a Rust library -- not a child process. ePHPm calls `LiteWire::new(backend).mysql(...).serve()` and it runs inside the same tokio runtime as the HTTP server and PHP embedding.

### Single-Node / Development / CI

No external database server. LiteWire runs in-process with the `rusqlite` backend. PHP connects via the MySQL wire frontend. External tools (libsql SDK, Turso CLI) connect via the Hrana HTTP frontend.

```mermaid
graph TD
    subgraph ephpm["ePHPm (single-node)"]
        http["HTTP Server"]
        php["PHP Runtime"]
        mysql_fe["LiteWire\nMySQL Wire :3306"]
        hrana_fe["LiteWire\nHrana HTTP :8080"]
        translate["SQL Translator"]
        rusqlite["rusqlite\n(in-process)"]
        db[("app.db")]

        http --> php
        php -->|pdo_mysql| mysql_fe
        mysql_fe --> translate
        translate --> rusqlite
        hrana_fe --> rusqlite
        rusqlite --> db
    end

    ext["External tools\n(libsql SDK, Turso CLI)"] -->|HTTP/Hrana| hrana_fe

    style ephpm fill:#f5f5f5,stroke:#333
    style mysql_fe fill:#fff3e0,stroke:#ef6c00
    style hrana_fe fill:#e8f5e9,stroke:#388e3c
    style rusqlite fill:#e3f2fd,stroke:#1565c0
```

Configuration:

```toml
[db.litewire]
backend = "rusqlite"
path = "app.db"
mysql_listen = "127.0.0.1:3306"
hrana_listen = "127.0.0.1:8080"
inject_env = true
```

### Clustered (HA with Replication)

sqld is spawned as a child process for replication. LiteWire's MySQL wire frontend handles PHP connections, but the backend switches to the `libsql` HTTP client talking to the local sqld instance. sqld serves the Hrana API directly -- LiteWire's Hrana frontend is not needed.

```mermaid
graph TD
    subgraph ephpm["ePHPm (clustered node)"]
        http2["HTTP Server"]
        php2["PHP Runtime"]
        mysql_fe2["LiteWire\nMySQL Wire :3306"]
        translate2["SQL Translator"]
        libsql_be["libsql backend\n(HTTP/Hrana client)"]
        sqld_mgr["sqld Manager"]
    end

    subgraph sqld["sqld (child process)"]
        hrana2["Hrana HTTP :8081"]
        engine["libSQL Engine"]
        db2[("app.db")]
        hrana2 --> engine --> db2
    end

    http2 --> php2
    php2 -->|pdo_mysql| mysql_fe2
    mysql_fe2 --> translate2
    translate2 --> libsql_be
    libsql_be -->|HTTP/Hrana| hrana2
    sqld_mgr -.->|spawn/monitor| sqld

    style ephpm fill:#f5f5f5,stroke:#333
    style mysql_fe2 fill:#fff3e0,stroke:#ef6c00
    style libsql_be fill:#e3f2fd,stroke:#1565c0
    style sqld fill:#e8f5e9,stroke:#388e3c
```

Configuration:

```toml
[db.litewire]
backend = "libsql"
sqld_url = "http://127.0.0.1:8081"
mysql_listen = "127.0.0.1:3306"
inject_env = true
```

ePHPm handles sqld lifecycle (spawn, monitor, restart), primary election via gossip, and replication configuration. LiteWire handles wire protocol and SQL translation only.

### Production with Real Databases

No LiteWire. The DB Proxy (`ephpm-db`) proxies directly to MySQL or PostgreSQL. See [db-proxy.md](db-proxy.md).

```toml
[db.mysql]
url = "mysql://user:pass@db-primary:3306/myapp"
max_connections = 50
inject_env = true

[db.mysql.replicas]
urls = ["mysql://user:pass@db-replica:3306/myapp"]

[db.read_write_split]
enabled = true
strategy = "sticky-after-write"
```

---

## Deployment Matrix

| Scenario | SQL Strategy | Backend | Database server required | Replication |
|----------|-------------|---------|--------------------------|-------------|
| Local dev | LiteWire | rusqlite | No | No |
| CI/CD | LiteWire | rusqlite | No | No |
| Edge / single-node | LiteWire | rusqlite | No | No |
| Single-node + external tools | LiteWire | rusqlite + Hrana | No | No |
| Clustered SQLite (HA) | LiteWire | libsql -> sqld | sqld (child process) | Yes (libSQL) |
| Production MySQL | DB Proxy | Real MySQL | Yes | MySQL native |
| Production PostgreSQL | DB Proxy | Real PostgreSQL | Yes | PG native |

---

## Tested With

LiteWire targets compatibility with these PHP frameworks and tools:

- WordPress (via `pdo_mysql`)
- Laravel (via `pdo_mysql` / `pdo_pgsql` / `pdo_sqlsrv`)
- Drupal
- `mysql` CLI, `psql` CLI, `sqlcmd` CLI
- DBeaver, pgAdmin, SSMS, TablePlus

---

## Limitations (LiteWire)

- **Single-writer**: SQLite is single-writer. Concurrent writes are serialized. Not a problem for ePHPm's NTS PHP (already serialized via mutex).
- **No stored procedures**: SQLite doesn't support them.
- **No replication built-in**: Use sqld/libSQL for replication. LiteWire is the protocol layer only.
- **Translation coverage**: Not every MySQL/PG/T-SQL construct is translatable. Unsupported constructs return a clear error rather than silently producing wrong results.

---

## Prior Art

| Project | Language | What it does | Stars |
|---------|----------|-------------|-------|
| [Marmot](https://github.com/maxpert/marmot) | Go | MySQL wire -> SQLite, distributed. Runs WordPress. | 2.8k |
| [WP sqlite-database-integration](https://github.com/WordPress/sqlite-database-integration) | PHP | Intercepts MySQL queries in PHP, rewrites to SQLite | Official WP project |
| [Postlite](https://github.com/benbjohnson/postlite) | Go | PG wire -> SQLite | 1.2k (archived) |
| [opensrv-mysql](https://github.com/datafuselabs/opensrv) | Rust | MySQL wire protocol server (no SQL translation) | Active |
| [pgwire](https://github.com/sunng87/pgwire) | Rust | PG wire protocol server (no SQL translation) | 734 |
| [ProxySQL](https://github.com/sysown/proxysql) | C++ | MySQL proxy (connection pooling, query routing) | 6.6k |
| [PgDog](https://github.com/pgdogdev/pgdog) | Rust | PostgreSQL proxy (wire protocol, pooling) | 4.1k |

The Rust building blocks for wire protocol exist but nobody has assembled them into a complete multi-dialect translation proxy backed by SQLite. LiteWire is the first to combine MySQL, PostgreSQL, TDS, and Hrana frontends with AST-based SQL translation in a single Rust crate.

---

## Dependencies (LiteWire)

| Crate | Purpose |
|-------|---------|
| `opensrv-mysql` | MySQL wire protocol server |
| `pgwire` | PostgreSQL wire protocol server |
| `sqlparser` | SQL parsing and AST (MySQL, PG, T-SQL dialects) |
| `rusqlite` | In-process SQLite backend (feature-gated) |
| `libsql` | Remote sqld backend via HTTP/Hrana (feature-gated) |
| `axum` | HTTP server for Hrana endpoint |
| `tokio` | Async runtime |
| `thiserror` / `anyhow` | Error handling |
| `tracing` | Logging |

---

## Implementation Phases (LiteWire)

| Phase | Scope | Milestone |
|-------|-------|-----------|
| 1 | Backend trait + rusqlite | Foundation |
| 2 | Hrana HTTP frontend (sqld-compatible) | Usable product: lightweight sqld replacement for CI/dev |
| 3 | MySQL wire + passthrough (no translation) | Wire protocol plumbing validated |
| 4 | SQL translator core (expressions, DML) | Basic INSERT/SELECT/UPDATE/DELETE with MySQL syntax |
| 5 | DDL translation | `php artisan migrate` completes |
| 6 | Metadata queries (SHOW, DESCRIBE, INFORMATION_SCHEMA) | `php artisan migrate:status`, Doctrine introspection |
| 7 | Prepared statements (COM_STMT_PREPARE/EXECUTE) | Laravel ORM works (pdo_mysql uses prepared stmts) |
| 8 | PostgreSQL wire frontend | `psql` connects, `pdo_pgsql` works |
| 9 | TDS (SQL Server) wire frontend | `sqlcmd` connects, `pdo_sqlsrv` works |
| 10 | libsql backend | LiteWire -> sqld -> SQLite roundtrip for clustered mode |
| 11 | WordPress + Laravel test suites | Real-world validation, translation gap fixes |

Phase 2 alone is a usable product -- a lightweight sqld replacement. Phase 7 is the milestone where PHP apps work end-to-end through the MySQL frontend.

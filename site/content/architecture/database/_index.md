+++
title = "Database"
type = "docs"
weight = 4
+++

ePHPm bundles an embedded SQLite-compatible database for zero-dependency deployments. PHP apps connect using their existing `pdo_mysql` drivers — backed by the same in-process engine. No code changes, no external database server.

The protocol translation layer is provided by [litewire](https://github.com/ephpm/litewire), a standalone Rust project that translates MySQL, PostgreSQL, and SQL Server wire protocols to SQLite. The embedded engine is the [Turso Database](https://github.com/tursodatabase/turso) engine (a Rust rewrite of SQLite) — see [Database engines](/architecture/database/engines/).

## Why an embedded database

The primary use case is single-node deployments where running a separate MySQL or PostgreSQL server is overkill:

- **CI/CD and preview environments** — spin up a full app stack with one binary, tear it down when done
- **Edge/single-server deployments** — VPS, bare metal, IoT, kiosk
- **Development** — no Docker Compose, no database container, just `ephpm serve`
- **Small production sites** — blogs, landing pages, internal tools that don't need a database cluster

WordPress and Laravel both support SQLite natively. The ephpm binary ships with the Turso engine compiled in — PHP's existing MySQL drivers connect to it transparently through litewire's protocol translation.

## Two ways in, two modes of operation

Two things vary independently, and it helps to keep them apart.

**How PHP reaches the database:**

- **The wire path** — `pdo_mysql` connects to `127.0.0.1:3306` and litewire
  translates. **This is the default and remains fully supported.** Zero code
  changes.
- **The in-process bridge** — the native
  [`ephpm_db_query()` / `ephpm_db_execute()`](/guides/db-from-php/) functions
  (v0.6.3+) run SQL through a per-thread litewire session inside the server
  process, skipping the TCP round trip. Same backend, same dialect, same
  results. Available whenever `[db.sqlite]` is configured, in either mode
  below.

**Which topology backs it:**

- **[Engine](/architecture/database/engines/)** — as of v0.7.0 the embedded
  engine is **Turso only** (`[db.sqlite] engine` defaults to `"turso"` and
  `"turso"` is the only accepted value). The rusqlite (SQLite C engine)
  backend and the sqld sidecar were removed; legacy `engine = "sqlite"` /
  `"rusqlite"` is a hard startup error.
- **Topology** — single-node (Turso in-process) or clustered (Turso CDC
  replication over the cluster channel — **experimental**), as described next.

### Single-Node (CI / Dev / Small Production)

No child processes. litewire runs entirely in-process with the Turso backend. This is the lightest possible deployment — just ephpm and a `.db` file.

```
   ┌────────────────── ePHPm (single node) ──────────────────────┐
   │                                                             │
   │   HTTP Server                                                │
   │       │                                                      │
   │       ▼                                                      │
   │   PHP Runtime                                                │
   │       │                                                      │
   │       │ pdo_mysql                                            │
   │       ▼                                                      │
   │   ┌───────────────┐         ┌──────────────────────┐        │
   │   │ litewire      │         │ litewire             │        │
   │   │ MySQL :3306   │         │ Hrana HTTP :8080     │ ◄──────┼──── External tools
   │   └───────┬───────┘         └──────────┬───────────┘        │     (libsql SDK,
   │           │                            │                    │      Turso CLI,
   │           ▼                            │                    │      mysql CLI)
   │   ┌───────────────┐                    │                    │
   │   │ SQL Translator│                    │                    │
   │   └───────┬───────┘                    │                    │
   │           │                            │                    │
   │           └──────────┬─────────────────┘                    │
   │                      ▼                                      │
   │              ┌───────────────┐                              │
   │              │ Turso engine  │                              │
   │              │ (in-process)  │                              │
   │              └───────┬───────┘                              │
   │                      ▼                                      │
   │                ╭───────────╮                                │
   │                │ ephpm.db  │                                │
   │                ╰───────────╯                                │
   └─────────────────────────────────────────────────────────────┘
```

Configuration:

```toml
[db.sqlite]
path = "ephpm.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:3306"
hrana_listen = "127.0.0.1:8080"  # optional
```

### Clustered (Turso CDC — experimental)

Clustered SQLite is **experimental** in v0.7.0 and there is no sqld sidecar. Each node runs the Turso engine in-process; the primary tails its own change-data-capture (`turso_cdc`) stream and ships per-transaction batches to replicas over the [cluster channel](/roadmap/cluster-channel/). Primary election is via gossip KV. Tested on Linux/macOS.

```
   ePHPm Cluster (3 nodes — primary + 2 replicas)

   Node 0 (primary)              Node 1 (replica)              Node 2 (replica)
   ────────────────              ────────────────              ────────────────
   litewire (MySQL wire)         litewire (MySQL wire)         (same as Node 1)
        │                             │
        ▼                             ▼
   SQL Translator                SQL Translator
        │                             │
        ▼                             ▼
   Turso engine                  Turso engine
   (in-process)                  (in-process)
        │                             ▲
        │ turso_cdc stream            │ apply per-txn batches
        └──── cluster channel ────────┘
             (per-transaction CDC batches, primary → replicas)
        │                             │
        ▼                             ▼
   ╭────────╮                ╭─────────────────╮
   │ app.db │                │ app.db          │
   ╰────────╯                │ (local copy)    │
                             ╰─────────────────╯
```

Configuration:

```toml
[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:3306"

[db.sqlite.replication]
role = "auto"                     # elected via gossip

[cluster]
enabled = true
bind = "0.0.0.0:7946"
join = ["ephpm-headless.default.svc.cluster.local"]
```

There is no `cdc_experimental` opt-in flag anymore — enabling `[cluster]` with `[db.sqlite]` selects CDC replication unconditionally.

## Implementation Status

| Component | Status | Crate |
|-----------|--------|-------|
| MySQL wire protocol frontend | **Implemented** | `litewire-mysql` (opensrv-mysql) |
| Hrana HTTP frontend | **Implemented** | `litewire-hrana` (axum) |
| SQL dialect translation (MySQL → SQLite) | **Implemented** | `litewire-translate` (sqlparser-rs) |
| Turso backend (in-process) | **Implemented** | `litewire-turso` |
| Primary election via gossip KV | **Implemented** | `ephpm-cluster` (sqlite_election) |
| In-process PHP bridge (`ephpm_db_query` / `ephpm_db_execute`) | **Implemented** (v0.6.3) | `ephpm-php` (`db_bridge`) |
| Turso engine (`[db.sqlite]`, single-node) | **Implemented — Beta engine upstream** | `litewire-turso` |
| Clustered Turso CDC replication (no sqld sidecar) | **Implemented — experimental** | `ephpm-server` (`turso_cdc`) |
| PostgreSQL wire protocol frontend | Placeholder | `litewire-postgres` (pgwire) |
| TDS (SQL Server) wire protocol frontend | Placeholder | `litewire-tds` |

> **Removed in v0.7.0:** the rusqlite backend, the sqld sidecar (`ephpm-sqld` crate, binary embedding, `sqld` auto-download, `--no-sqld`/`--sqld-binary` xtask flags), the `HranaClient` backend that talked to it, the `[db.sqlite.sqld]` block and its `write_permits` knob, and the `cdc_experimental` flag. Clustered mode is now the in-process Turso CDC path unconditionally.

## litewire: Protocol Translation Layer

[litewire](https://github.com/ephpm/litewire) is a standalone open-source Rust project that provides MySQL, PostgreSQL, SQL Server, and Hrana protocol frontends backed by SQLite. ePHPm uses it as a library dependency.

### SQL Translation

litewire uses `sqlparser-rs` to parse MySQL into an AST, then rewrites dialect-specific constructs to SQLite equivalents:

| MySQL | SQLite |
|-------|--------|
| `AUTO_INCREMENT` | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `NOW()` | `datetime('now')` |
| `ON DUPLICATE KEY UPDATE` | `ON CONFLICT DO UPDATE` |
| `SHOW TABLES` | `SELECT name FROM sqlite_master WHERE type='table'` |
| `DESCRIBE table` | `PRAGMA table_info(table)` |
| `INFORMATION_SCHEMA.*` | `sqlite_master` + `PRAGMA` queries |
| `VARCHAR(n)` / `NVARCHAR(n)` | `TEXT` |
| `TRUE` / `FALSE` | `1` / `0` |
| `SET NAMES utf8mb4` | No-op |

### Backend

ePHPm builds litewire with the `turso` backend — the in-process Turso engine. Both the wire path and the in-process bridge run through the same backend. (litewire itself still ships other backends, e.g. `backend-rusqlite`, for other consumers; ePHPm no longer enables them.)

## Clustered replication: Turso CDC

Clustered mode replicates in-process — there is no separate database server binary. The primary's Turso engine exposes a change-data-capture stream (`turso_cdc`); ePHPm tails it and ships per-transaction batches to replicas over the multiplexed [cluster channel](/roadmap/cluster-channel/). A cold replica first bootstraps from a snapshot (a `CREATE`/`INSERT` statement stream validated against an allowlist), then resumes from the live CDC stream. This path is **experimental** and manually validated on Linux/macOS.

### How Reads and Writes Work

- **Reads** on any node: served from the local database file. Microsecond latency.
- **Writes** on the primary: committed locally, then the resulting CDC batch is shipped to replicas asynchronously.
- **Writes** on a replica: replicas are not yet read-only-enforced at the wire frontend — sending writes to a replica is unsupported; direct writes to the primary.

## Primary Election

ePHPm uses its gossip clustering (SWIM protocol via chitchat) to elect the primary:

1. On cluster formation, the lowest-ordinal alive node becomes primary
2. The primary's identity is stored in gossip KV (`kv:sqlite:primary`)
3. The primary heartbeats this key every 5s with a 10s TTL
4. If the primary dies, gossip detects it (phi-accrual failure detector)
5. Next lowest-ordinal node promotes itself and begins publishing its CDC stream
6. Replicas detect the KV change and reconnect to the new primary's cluster channel address

### Failover

When the role-change watcher detects a new election result, the node reconfigures its CDC role in-process (primary begins tailing/publishing; replicas re-point at the new primary's cluster channel address and resume). There is no child process to restart.

**Data loss on failover:** Any writes committed on the primary but not yet shipped to replicas are lost. In practice this is the last few hundred milliseconds (sub-ms network latency in k8s, async replication lag is small).

## Configuration Reference

```toml
[db.sqlite]
path = "ephpm.db"                         # SQLite database file path
# engine = "turso"                        # the only accepted value (the default)

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:3306"           # MySQL wire protocol (PHP connects here)
hrana_listen = "127.0.0.1:8080"           # Hrana HTTP (optional, for external tools)

# Clustered mode only (ignored in single-node)
[db.sqlite.replication]
role = "auto"                             # "auto" | "primary" | "replica"
primary_grpc_url = ""                     # primary's cluster channel address; required when role = "replica"
# max_snapshot_bytes = 1073741824         # cap on cold-replica snapshot bootstrap
```

Environment variable overrides:

```bash
EPHPM_DB__SQLITE__PATH=app.db
EPHPM_DB__SQLITE__PROXY__MYSQL_LISTEN=127.0.0.1:3306
EPHPM_DB__SQLITE__REPLICATION__ROLE=auto
```

### Mode Detection

```
   [db.sqlite] configured?
        │
        ├── no  ──► no SQLite (only DB proxy if [db.mysql])
        │
        └── yes ──► replication.role?
                       │
                       ├── primary | replica ──► clustered (Turso CDC)
                       │
                       └── auto ──► [cluster] enabled?
                                       │
                                       ├── yes ──► clustered (Turso CDC)
                                       │
                                       └── no  ──► single-node (Turso, in-process)
```

Full detail on the engine, including its Beta-upstream limits and file-format
compatibility, is in [Database engines](/architecture/database/engines/).

## Platform Support

| Platform | Single-node | Clustered (Turso CDC, experimental) |
|----------|-------------|-------------------------------------|
| Linux x86_64 | Yes | Yes |
| Linux aarch64 | Yes | Yes |
| macOS x86_64 | Yes | Yes |
| macOS aarch64 | Yes | Yes |
| Windows x86_64 | Yes | Untested |

## When to Use SQLite vs. External MySQL

| Scenario | Recommendation |
|----------|---------------|
| CI/CD, preview environments | SQLite (single-node) |
| Development | SQLite (single-node) |
| Single-server blog/CMS | SQLite (single-node) |
| Medium production with HA | External MySQL, or clustered SQLite (experimental) |
| High write throughput | External MySQL |
| Existing MySQL infrastructure | External MySQL (DB proxy) |
| Zero-data-loss requirement | External MySQL with semi-sync replication |

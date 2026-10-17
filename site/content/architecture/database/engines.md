+++
title = "Database Engines"
weight = 2
+++

`[db.sqlite]` decides *which file* holds your data. As of v0.7.0 there is
exactly one embedded engine that reads it: the **Turso** engine.

| `engine` | What it is | Status |
|---|---|---|
| `"turso"` (default, and the only accepted value) | [Turso Database](https://github.com/tursodatabase/turso) — a ground-up Rust rewrite of SQLite (MIT) | Shipped. **Beta upstream** |

The engine is reached through the same [litewire](https://github.com/ephpm/litewire)
wire translation as before, so PHP is unaffected: `pdo_mysql` still connects to
`127.0.0.1:3306`, and the [in-process bridge](/guides/db-from-php/) still works.
The engine is invisible to application code.

## `engine` is now Turso-only

In v0.7.0 the rusqlite backend (the genuine SQLite C engine) was de-linked from
ePHPm and the sqld sidecar was removed entirely. `engine` defaults to `"turso"`
and `"turso"` is the only accepted value.

Legacy `engine = "sqlite"` or `engine = "rusqlite"` is a **hard startup error**
with a migration message — it fails closed and never silently falls back to a
different engine:

```
[db.sqlite] engine = "sqlite" was removed in v0.7.0. The rusqlite (SQLite C
engine) backend and the sqld sidecar are gone; the embedded engine is now
Turso only. Remove the engine key (or set engine = "turso"). Existing .db
files open in place — see the migration note.
```

## The Turso engine

Turso Database is Turso's ground-up Rust rewrite of SQLite: native async
I/O instead of blocking C calls on a thread pool, and MVCC concurrent
writes instead of a single writer guarded by `busy_timeout`.

ePHPm reaches it through litewire's `turso` backend, which is compiled into
every standard ePHPm build — **there is no cargo feature, sqld binary, or
special build to obtain**. A minimal single-node config:

```toml
[db.sqlite]
path = "app.db"
# engine = "turso"   # the default; may be omitted
```

Because the engine is Beta upstream, take its limits literally:

- **Beta upstream.** Turso's own positioning is Beta; it is not yet presented
  as a production SQLite replacement.
- **`VACUUM` is unsupported.**
- **Multi-process access is unsupported.** One process opens the database
  file. Do not point a second ePHPm instance, a `sqlite3` shell, or a backup
  tool at the same file while the server is running.

## Opening existing SQLite / rusqlite `.db` files

The Turso engine opens existing SQLite3/rusqlite-created `.db` files **in
place** for cleanly-shut-down databases — both WAL and rollback-journal modes.
This was verified during the v0.7.0 engine swap: `PRAGMA integrity_check`
returns `ok` and rows are intact. So the normal 0.6.x → 0.7.0 upgrade of a
stopped node is seamless: **no dump/reload is required.**

Two honest caveats:

- **Shut down cleanly before upgrading.** A database left with an
  uncheckpointed hot `-wal` from a hard crash was *not* verified to replay
  through Turso. Stop the old node cleanly (which checkpoints the WAL) before
  starting the v0.7.0 binary on the same file.
- **Non-UTF-8 TEXT may not round-trip.** Turso surfaces `TEXT` as a Rust
  `String`, so cells holding invalid UTF-8 may not survive the round trip.
  This is an upstream Turso limitation.

## Turso 0.7.2 → 0.8.2 (v0.12.0)

v0.12.0 moves the embedded engine from Turso 0.7.2 to
0.8.2 ([ephpm#535](https://github.com/ephpm/ephpm/issues/535)). **No
dump/reload is needed, and you can roll back.** The file format did not
change, and neither did the CDC log schema: `turso_cdc` keeps its columns and
`turso_cdc_version` still reads `v2`.

What was tested, using one build on each engine version against the same
files:

- A database written and cleanly closed by 0.7.2 opens under 0.8.2.
  `PRAGMA integrity_check` returns `ok`, every row is present, and the
  table and its index agree. Writes continue, and `turso_cdc.change_id`
  carries on from where 0.7.2 stopped.
- A crash image taken while 0.7.2 still had the database open opens and
  recovers under 0.8.2. The image is the main file plus a 3.9 MB `-wal`
  that had never been checkpointed.
- The CDC log written partly by 0.7.2 and partly by 0.8.2 replays through
  the tail/apply path to an identical replica.
- Downgrade works: 0.7.2 opens files written by 0.8.2, including one it
  wrote before the upgrade, with `integrity_check` `ok`.

For clusters, the batches nodes exchange use ePHPm's own wire format, and
the `turso_cdc` rows those batches carry have the same shape on both engine
versions. Even so, a cluster running both versions at once was **not**
tested live. That is reasonable for a rolling upgrade, but do not leave a
cluster on mixed versions for long. Clustered mode is still experimental.

One behaviour changes for applications. On a session with CDC capture on,
which means every write session in clustered mode, `PRAGMA journal_mode =
mvcc` now fails with `cannot change journal_mode ... while CDC capture is
active`. Turso 0.7.2 accepted it, and according to upstream, capture then
stopped silently, so that node's later writes never replicated. Single-node
mode does not capture, so the pragma still works there. Turso accepts only
`wal` and `mvcc`; any other `journal_mode` value is still a silent no-op.

## Clustered mode (experimental)

Clustered SQLite is **experimental** in v0.7.0. It uses the in-process Turso
CDC path (`turso_cdc`) over the [cluster channel](/roadmap/cluster-channel/):
ePHPm tails the primary's own change-data-capture stream and ships
per-transaction batches to replicas — **no sqld sidecar, no child process, no
gRPC**. Primary election is still via the gossip KV tier.

```toml
[db.sqlite]
path = "app.db"

[cluster]
enabled = true
```

There is no `cdc_experimental` opt-in flag anymore — clustering enables CDC
replication unconditionally. Because the Turso engine is Beta upstream and the
CDC path is manually (not yet CI-) validated, treat clustered mode as
experimental and tested on Linux/macOS.

Metrics for this path — batches and rows shipped and applied, subscriber
count, applied watermark, reconnects, snapshot bytes, and a replication-lag
gauge measured in **change-log rows, not seconds** — are in the
[metrics reference](/reference/metrics/#cdc-native-turso-replication). The
full design, its verified findings, and everything still deferred (including
read-only enforcement on replica wire frontends and `turso_cdc` retention
pruning) are on the [Turso engine roadmap](/roadmap/turso-engine/).

## How the modes line up

| Mode | Engine | Backend |
|---|---|---|
| Single-node (`[db.sqlite]`, no cluster) | Turso, in-process | litewire `turso` backend, one process opens the file |
| Clustered (`[db.sqlite]` + `[cluster]`) | Turso, in-process | Turso CDC replication over the cluster channel. **Experimental** |

The [DB proxy](/architecture/database/db-proxy/) modes (`[db.mysql]`,
`[db.postgres]`) are unaffected — they forward to a real database server and
have no embedded engine to choose.

## See also

- [Turso engine roadmap](/roadmap/turso-engine/) — the full plan, phase status, gates, and risks
- [Configuration reference → `[db.sqlite]`](/reference/config/#dbsqlite) — every key
- [Database from PHP](/guides/db-from-php/) — the in-process `ephpm_db_*` bridge
- [Cluster channel](/roadmap/cluster-channel/) — the transport CDC replication rides on

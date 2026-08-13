+++
title = "Database Engines"
weight = 2
+++

`[db.sqlite]` decides *which file* holds your data. `[db.sqlite] engine`
decides *what reads it*.

| `engine` | What it is | Status |
|---|---|---|
| `"sqlite"` (default) | The genuine SQLite C engine, compiled into the binary via rusqlite's `bundled` feature | Production-supported |
| `"turso"` | [Turso Database](https://github.com/tursodatabase/turso) — a ground-up Rust rewrite of SQLite (MIT) | **Experimental. Beta upstream.** Not for data you cannot recreate |

Both sit behind the same [litewire](https://github.com/ephpm/litewire) wire
translation, so PHP is unaffected either way: `pdo_mysql` still connects to
`127.0.0.1:3306`, and the [in-process bridge](/guides/db-from-php/) still
works. The engine choice is invisible to application code.

Anything other than these two values is a hard startup error — a typo can
never silently fall back to a different engine.

## The default: the SQLite C engine

With `engine` unset (or `"sqlite"`), ePHPm opens the database with
rusqlite's bundled SQLite. This is the engine the project supports for
production, and there is no plan to ship anything less proven under a
default. Its properties are SQLite's: one writer at a time, WAL journaling,
and the file format every other SQLite tool on earth already reads.

Single-node mode additionally opts in to litewire's connection handle
reuse (idle cap 16), because PHP's connect-per-request pattern otherwise
pays a fresh `sqlite3_open` plus WAL-index attach on every request.

## The Turso engine (experimental)

Turso Database is Turso's ground-up Rust rewrite of SQLite: native async
I/O instead of blocking C calls on a thread pool, and MVCC concurrent
writes instead of a single writer guarded by `busy_timeout`.

ePHPm reaches it through litewire's `turso` backend. The backend is
compiled into every standard ePHPm build — **there is no cargo feature or
special binary to obtain**. Selecting it is purely a configuration change:

```toml
[db.sqlite]
path = "app.db"
engine = "turso"
```

### The startup warning

Selecting the Turso engine logs a `WARN` line at startup. This is its exact
content:

```
[db.sqlite] engine = "turso" is EXPERIMENTAL: the Turso Database engine is
Beta upstream and not yet a production SQLite replacement. Do not use it for
data you cannot recreate. VACUUM and multi-process access are unsupported.
See the Turso engine roadmap page.
```

The warning is not boilerplate. Take each clause literally:

- **Beta upstream.** Turso's own positioning is Beta; they do not yet
  present it as a production SQLite replacement.
- **Do not use it for data you cannot recreate.** ePHPm has not completed
  a crash-recovery soak or a file-format round-trip verification against
  this engine (both are open [decision gates](/roadmap/turso-engine/#decision-gates--all-of-them-no-exceptions)).
- **`VACUUM` is unsupported.**
- **Multi-process access is unsupported.** One process opens the database
  file. Do not point a second ePHPm instance, a `sqlite3` shell, or a
  backup tool at the same file while the server is running.

### Clustered mode

By default the Turso engine is **single-node only**. Combining
`engine = "turso"` with clustered SQLite is refused at startup:

```
[db.sqlite] engine = "turso" is not supported in clustered mode without the
experimental CDC replication opt-in. Set [db.sqlite.replication]
cdc_experimental = true to enable Phase 2 CDC-native replication
(EXPERIMENTAL — Turso engine is Beta upstream, sqld remains the production
clustered default for engine = "sqlite"), or set engine = "sqlite" to use
the production sqld path.
```

The one way through that door is the experimental CDC-native replication
path:

```toml
[db.sqlite]
engine = "turso"

[db.sqlite.replication]
cdc_experimental = true

[cluster]
enabled = true
```

With all three set, ePHPm replicates by tailing the engine's own
change-data-capture stream and shipping per-transaction batches to replicas
over the [cluster channel](/roadmap/cluster-channel/) — **no sqld sidecar,
no child process, no gRPC**. The flag is deliberately not optional: it is
what distinguishes "I know this is experimental" from a config typo.

`cdc_experimental` has no effect in single-node mode (there are no peers to
ship batches to); setting it there logs a warning and is otherwise ignored.

Metrics for this path — batches and rows shipped and applied, subscriber
count, applied watermark, reconnects, snapshot bytes, and a replication-lag
gauge measured in **change-log rows, not seconds** — are in the
[metrics reference](/reference/metrics/#cdc-native-turso-replication). The
full design, its verified findings, and everything still deferred (including
read-only enforcement on replica wire frontends and `turso_cdc` retention
pruning) are on the [Turso engine roadmap](/roadmap/turso-engine/).

## How the modes line up

| Mode | `engine = "sqlite"` | `engine = "turso"` |
|---|---|---|
| Single-node (`[db.sqlite]`, no cluster) | rusqlite, in-process. **Default.** | Turso, in-process. Experimental |
| Clustered (`[db.sqlite]` + `[cluster]`) | sqld child process, WAL replication over gRPC. **Production default** | Startup error unless `cdc_experimental = true`; then CDC-native replication, no sidecar. Experimental |

The [DB proxy](/architecture/database/db-proxy/) modes (`[db.mysql]`,
`[db.postgres]`) are unaffected by this knob — they forward to a real
database server and have no embedded engine to choose.

## Direction — not a commitment

The project's stated intent is to converge on **one** engine for both
single-node and clustered modes, retiring the current split between the
SQLite C engine and the sqld sidecar. sqld is a sunset dependency upstream:
libSQL/sqld remain maintained but feature-frozen.

Nothing about that is scheduled, and nothing is promised here. Making Turso
the *default* engine is explicitly a new-minor-or-larger event under the
versioning policy, and it does not happen before five
[decision gates](/roadmap/turso-engine/#decision-gates--all-of-them-no-exceptions)
close — upstream GA (with multiprocess and `VACUUM` landed), benchmark
parity including tails, a file-format round-trip verified by this project,
a clean crash-recovery soak, and green WordPress + Laravel e2e suites on the
Turso backend. **As of v0.6.3 all five remain open**, and the default engine
is unchanged.

Until then, rusqlite shipping the genuine SQLite C engine as the default is
a feature, not a compromise.

## See also

- [Turso engine roadmap](/roadmap/turso-engine/) — the full plan, phase status, gates, and risks
- [Configuration reference → `[db.sqlite]`](/reference/config/#dbsqlite) — every key
- [Database from PHP](/guides/db-from-php/) — the in-process `ephpm_db_*` bridge
- [Cluster channel](/roadmap/cluster-channel/) — the transport CDC replication rides on

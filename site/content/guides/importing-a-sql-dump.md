+++
title = "Importing a SQL dump"
weight = 12
+++

You have a `mysqldump` (or `pg_dump`) file from an existing MySQL/PostgreSQL
app and you want it running on ePHPm's embedded database. `ephpm db import`
loads the dump into a virtual host's [per-site Turso database](/architecture/database/),
translating the dialect on the way in and telling you — honestly — what the
embedded engine could not keep.

This guide walks a WordPress migration end to end, then lays out the full
compatibility matrix.

## The short version

```bash
# 1. Export from the old host (standard mysqldump)
mysqldump --single-transaction --default-character-set=utf8mb4 blog > blog.sql

# 2. Import into the `blog` vhost's database (server not running → offline)
ephpm db import blog.sql --site blog --config /etc/ephpm/ephpm.toml

# 3. Point WordPress at the embedded DB with the db-wordpress drop-in, then check
ephpm php --site blog --config /etc/ephpm/ephpm.toml -- wp db query 'SELECT COUNT(*) FROM wp_posts'
```

The import prints a compatibility report; read it before you trust the result.

## Step by step

### 1. Prepare the site

`db import` needs multi-tenant per-site mode — one database file per vhost,
which is the tenant-isolation boundary. A minimal config:

```toml
[server]
sites_dir = "/srv/sites"      # each subdirectory is a vhost

[db.sqlite]
dir = "/var/lib/ephpm/db"     # each vhost gets <key>.db here

[kv]
secret = "…"                  # required only for the wire path (see below)
```

Create the vhost directory (`/srv/sites/blog/`) and drop the app's code in it as
usual. The database file `/var/lib/ephpm/db/blog.db` is created by the import.

### 2. Run the import

```bash
ephpm db import blog.sql --site blog --config /etc/ephpm/ephpm.toml
```

With **no server running**, the import opens `blog.db` directly — the preferred
path, because an import is bulk and long-running and should not compete with
live traffic. The dump can be a path, `-` for stdin, or a `.sql.gz` (decompressed
transparently):

```bash
gunzip -c blog.sql.gz | ephpm db import - --site blog --config ephpm.toml
```

> **Import before enabling clustering.** Do the import against a single-node
> (non-clustered) configuration. In **per-site clustered** mode a standalone
> `db import` with no server running is *refused* — a CLI cannot reach the
> site's HRW owner, and writing the local file could land on a replica whose
> writes never replicate. Import first, verify, then turn clustering on; the
> data then replicates through the normal CDC path.

If a server **is** running, the import forwards statements over its MySQL
listener instead (authenticating as the site, so `[kv] secret` must be set and
match the server). This is correct while the server holds the file open, but a
PostgreSQL dump cannot go this way — the listener speaks MySQL. For a `pg_dump`,
stop the server and re-run for an offline direct-open.

### 3. Read the compatibility report

Every run ends with a summary and the findings that matter:

```text
db import: 2451 statements executed, 2451 emitted, 8 dropped, 0 indexes deferred, 3 batches committed
compatibility: 1 unsupported, 2 downgraded, 0 error(s), 8 dropped
  [downgraded] line 142 wp_posts.post_status: ENUM/SET column stored as TEXT (no value constraint enforced)
  [unsupported] line 8831 wp_posts_bi: trigger is not supported by SQLite/Turso; skipped
```

Add `--report import.json` to also write the full report as JSON for a CI gate
or a migration record. `--verbose` additionally lists the `dropped` safe-no-op
findings (`LOCK TABLES`, session `SET`s) that are normally hidden.

### 4. Preview first with `--dry-run`

To see exactly what a dump will do without changing anything — no execution, no
database opened:

```bash
ephpm db import blog.sql --site blog --config ephpm.toml --dry-run
```

The report is identical to a real run's, so this is the safe way to scout an
unfamiliar dump.

### 5. Choose an error policy

- `--on-error stop` (default) — halt at the first unsupported construct or SQL
  error and exit non-zero *before touching data past it*. Use this for a clean,
  all-or-nothing migration you will inspect.
- `--on-error skip` — record each problem as a finding and keep going, exiting
  zero-with-warnings. Use this to get everything importable in, then review the
  findings.

### 6. Large dumps

`db import` streams the dump — it never loads the whole file into memory, so
file size is not a limit. Two knobs help a big import:

- `--batch-size <N>` (default `1000`) — statements per transaction.
- `--defer-indexes` — hold `CREATE INDEX` statements and replay them after the
  data has loaded, which is often faster for large tables.

## Compatibility matrix

`db import` handles the dump **file format**; litewire translates the SQL
**dialect**. Between them:

### Kept, verbatim or translated

- `CREATE TABLE` (storage options stripped), `INSERT` (including multi-row
  extended inserts), `CREATE INDEX`, `CREATE VIEW` (MySQL `DEFINER`/`ALGORITHM`
  stripped), ordinary secondary `KEY`/`UNIQUE KEY` (litewire expands these to
  `CREATE INDEX`).
- PostgreSQL `COPY tbl (cols) FROM stdin` data blocks → batched `INSERT`s, with
  `\N` → `NULL` and `\t \n \r \\` unescaped.

### Downgraded (kept, with a finding)

| Construct | Becomes | Caveat |
|-----------|---------|--------|
| `ENUM(...)` / `SET(...)` column | `TEXT` | the value constraint is not enforced |
| `CREATE SEQUENCE` / `setval` (PG) | dropped in favour of SQLite `AUTOINCREMENT` | semantics differ; the current value is not carried over |

### Dropped (safe no-ops)

`LOCK TABLES` / `UNLOCK TABLES`, `ALTER TABLE … {DISABLE|ENABLE} KEYS`, session
`SET`s, MySQL `/*! … */` version-gate setters, and (PostgreSQL) `SET`,
`SELECT pg_catalog.set_config`, `\connect`, `COMMENT ON`, `GRANT`/`REVOKE`,
`… OWNER TO`, `CREATE EXTENSION`.

### Unsupported (not applied)

Stored procedures and functions, triggers, events, and `FULLTEXT`/`SPATIAL`
indexes. These are listed in the report so you can decide what to do about them
— typically reimplement the logic in the application, or (for full-text search)
use an application-level index.

## Notes

- The embedded engine opens existing SQLite/rusqlite `.db` files in place, so if
  you already have a SQLite database you do not need to dump and reimport it —
  just point `[db.sqlite] dir` at it. `db import` is for dumps *from another
  engine*.
- The importer never translates SQL types or expressions itself — that is
  litewire's job. If a statement fails to translate, it surfaces as an `error`
  finding (and, under `--on-error skip`, the rest of the import still runs). See
  the [MySQL compatibility guide](/guides/mysql-compatibility/) for what the
  translator does and does not cover.

## See also

- [`ephpm db import` reference](/reference/cli/db/) — every flag
- [`ephpm php`](/reference/cli/php/) — run `wp` / `artisan` / migrations against a site's database
- [db-wordpress](https://github.com/ephpm/db-wordpress) — the WordPress `db.php` drop-in over the embedded database

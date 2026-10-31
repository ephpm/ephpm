+++
title = "ephpm db"
weight = 8
+++

Database tooling for a virtual host's embedded (Turso) database. Three
subcommands:

- [`db import`](#ephpm-db-import) — load a `mysqldump` / `pg_dump` SQL file,
  with an honest compatibility report.
- [`db query`](#ephpm-db-query) — run one SQL statement (or a `;`-separated
  batch) and print the result set.
- [`db shell`](#ephpm-db-shell) — an interactive SQL REPL with introspection and
  cache meta-commands.

All three resolve the target the same way [`ephpm php --site`](../php/#binding-a-virtual-hosts-database-site)
does and obey the same single-writer rule (see [Transport & mode](#transport--mode-shared)).
`db export` is **planned — not yet implemented**.

## `ephpm db import`

```bash
ephpm db import <DUMP> --site <KEY> --config <FILE> [options]
```

`<DUMP>` is a path to a `.sql` (or `.sql.gz`) file, or `-` to read the dump
from **stdin**.

### What it does

The hard part of importing a dump is not moving SQL — ePHPm already moves SQL
three ways — it is the dump **file format** and being honest about what the
embedded engine cannot keep. `db import`:

1. **Detects the dialect** from the dump header (`--dialect auto`), or takes it
   from `--dialect mysql` / `--dialect pg`.
2. **Splits the dump into statements**, streaming — it never loads the whole
   file into memory. The splitter respects string literals (with `\` and
   doubled-quote escapes), backtick identifiers, `--`/`#`/`/* */` comments,
   `DELIMITER` changes (so a `CREATE PROCEDURE … BEGIN … ; … END $$` body stays
   one statement), and the PostgreSQL `COPY … FROM stdin` … `\.` data block.
3. **Sanitizes each statement** for SQLite/Turso: strips MySQL `/*! … */`
   version gates, drops `LOCK TABLES` and session `SET`s, strips `CREATE TABLE`
   storage options (`ENGINE=`, `DEFAULT CHARSET=`, `COLLATE=`, `ROW_FORMAT=`,
   `AUTO_INCREMENT=`), rewrites `ENUM`/`SET` columns to `TEXT`, converts
   `COPY FROM stdin` blocks to batched `INSERT`s (`\N` → `NULL`), and classifies
   stored routines / triggers / events / full-text indexes / sequences.
4. **Executes** the sanitized statements against the site's database, batching
   them into transactions. The SQL *type/dialect translation* (MySQL/PostgreSQL
   → SQLite) is done by litewire, the same translator the `ephpm_db_*` bridge
   and `pdo_mysql` already use — `db import` does the file-format work and hands
   litewire clean statements.
5. **Prints a compatibility report** of everything it changed, dropped, or
   could not apply.

### How the target is resolved (transport)

Identical to [`ephpm php --site`](../php/#binding-a-virtual-hosts-database-site):

- **No server running, single-node / per-site-single** → **offline
  direct-open** of the site's own `<key>.db`. This is the preferred path for an
  import: it is bulk, long-running, and must not compete with live traffic.
- **A server is running** (its `[db.sqlite.proxy] mysql_listen` answers) → the
  statements are forwarded over the server's MySQL listener, authenticating as
  the site (requires `[kv] secret`). The server does the translation and
  screening. Importing a **PostgreSQL** dump over this path logs a warning — the
  server's listener speaks MySQL, so stop the server and re-run for an offline
  direct-open (which uses the PostgreSQL translator).
- **Per-site clustered with no server running** → **refused**, with the same
  message `ephpm php --site` gives: a standalone CLI cannot reach the site's
  owner or safely write a replica.

`--site` is validated fail-closed: an unknown or malformed key is rejected and
nothing is created.

### The compatibility report

Every statement that is not passed through verbatim produces a **finding** of
one of four kinds:

| Kind | Meaning | Shown by default |
|------|---------|------------------|
| `dropped` | A safe no-op was removed (`LOCK TABLES`, a session `SET`, a `COMMENT ON`). Nothing is lost. | No — only with `--verbose` |
| `downgraded` | Kept, but not exactly equivalent — an `ENUM`/`SET` column stored as `TEXT`, or a sequence that becomes `AUTOINCREMENT` with different semantics. | Yes |
| `unsupported` | A construct SQLite/Turso cannot represent and the importer did not apply — stored procedures/functions, triggers, events, full-text/spatial indexes. | Yes |
| `error` | A statement the backend rejected at execution time (recorded under `--on-error skip`). | Yes |

A headline summary (counts per kind, plus how many statements executed) is
always printed. `--report <FILE>` additionally writes the full report as JSON:

```json
{
  "summary": { "dropped": 8, "downgraded": 2, "unsupported": 1, "error": 0 },
  "findings": [
    { "kind": "downgraded", "object": "wp_posts.post_status",
      "detail": "ENUM/SET column stored as TEXT (no value constraint enforced)", "line": 17 },
    { "kind": "unsupported", "object": "wp_posts_bi",
      "detail": "trigger is not supported by SQLite/Turso; skipped", "line": 38 }
  ]
}
```

### Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--site <KEY>` | (required) | Vhost directory name under `[server] sites_dir` |
| `-c`, `--config <FILE>` | (required) | Config with `[server] sites_dir`, `[db.sqlite]`, and (for the wire path) `[kv] secret` |
| `--dialect <auto\|mysql\|pg>` | `auto` | Dump dialect. `auto` sniffs the header and fails loudly if it is ambiguous |
| `--gzip` | off | Force gzip decompression (otherwise inferred from a `.gz` extension; required for a gzipped stdin stream) |
| `--on-error <stop\|skip>` | `stop` | `stop` halts at the first unsupported construct or SQL error and exits non-zero before touching data past it; `skip` records a finding and continues, exiting zero-with-warnings |
| `--dry-run` | off | Parse, sanitize, and report only — execute nothing and bind no backend |
| `--batch-size <N>` | `1000` | Statements per transaction |
| `--report <FILE>` | — | Write the compatibility report as JSON to `FILE` |
| `--defer-indexes` | off | Hold `CREATE INDEX` statements and replay them after the data has loaded (often faster for large tables) |
| `--verbose` | off | Also list `dropped` safe-no-op findings |

### Exit codes

- `0` — the import completed. Under `--on-error skip` this holds even when the
  report has warnings (count them in the summary).
- non-zero — under `--on-error stop`, any `unsupported` or `error` finding, or a
  statement that aborted the run.
- non-zero — the config or dump could not be read, the `--site` key was invalid,
  or the transport could not be established.

### Examples

```bash
# Load a WordPress mysqldump offline, then check it
ephpm db import blog.sql --site blog --config /etc/ephpm/ephpm.toml
ephpm php --site blog --config /etc/ephpm/ephpm.toml -- wp db query 'SELECT COUNT(*) FROM wp_posts'

# Preview what a dump would do without changing anything
ephpm db import blog.sql --site blog --config ephpm.toml --dry-run

# A gzipped pg_dump from stdin, keeping everything it can
gunzip -c shop.sql.gz | ephpm db import - --site shop --config ephpm.toml --dialect pg --on-error skip --report import.json
```

## Transport & mode (shared) {#transport--mode-shared}

`db query` and `db shell` reach a site's database over exactly the transport
`ephpm php --site` uses, decided automatically by probing the server's MySQL
listener (`[db.sqlite.proxy] mysql_listen`):

| State | Mode | What it does | Dialects |
|-------|------|--------------|----------|
| A server is running | **live** | Forward each statement to the server's MySQL listener, authenticating **as the site** (needs `[kv] secret`). The server does translation, screening, owner-forwarding, and query-stats. | `mysql` **only** |
| No server is running (single-node / per-site-single) | **offline** | Open the site's own `<key>.db` directly. | `sqlite`, `mysql`, `pg` |
| Per-site **clustered**, no server | — | **Refused** — a standalone CLI cannot reach the site's owner or safely write a replica (same message `ephpm php --site` gives). | — |

**The single-writer rule, fail-closed.** Two processes must never hold the same
Turso file open at once, and `sqlite`-native access means opening the file
directly. So **a `sqlite` (or `pg`) request while a server is up is a hard
error, never a silent fallback**:

```
a server is serving this site on 127.0.0.1:3306; sqlite needs offline access
(direct file open) — use --dialect mysql for the live DB, or stop the server
and re-run for sqlite access.
```

Use `--dialect mysql` to work against the live database through the running
server, or stop the server for `sqlite`-native / `pg` offline access.

### Dialects

- **`sqlite`** (the default for `query`/`shell`) — statements run straight
  against the Turso engine with **no** litewire translation. This is the
  engine's own SQL; use it to read `sqlite_master`, run `PRAGMA`s, etc.
- **`mysql`** / **`pg`** — statements are translated by litewire (the same
  translator `pdo_mysql` and the `ephpm_db_*` bridge use), so MySQL- or
  PostgreSQL-flavoured SQL works. Live mode is always `mysql`.

`db import` is different: it defaults to **`auto`** (sniff the dump header),
because it loads a dump, not interactive SQL.

## `ephpm db query`

```bash
ephpm db query --site <KEY> --config <FILE> [options] ["<SQL>"]
```

Run one SQL statement — or a `;`-separated batch — and print each result set.
The SQL comes from (in order of precedence) the `-e`/`--execute` option, the
positional argument, or **stdin**:

```bash
# Positional
ephpm db query --site blog --config ephpm.toml "SELECT COUNT(*) FROM wp_posts"

# -e
ephpm db query --site blog --config ephpm.toml -e "SELECT 1 + 1 AS two"

# stdin
echo "SELECT option_name FROM wp_options LIMIT 3" | ephpm db query --site blog --config ephpm.toml
```

Statements that return rows (a `SELECT`, a row-returning `PRAGMA`, `SHOW`,
`DESCRIBE`) are printed in the chosen `--format`; a write or DDL prints nothing
to stdout. A statement error stops the batch and exits non-zero.

### Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--site <KEY>` | (required) | Vhost directory name under `[server] sites_dir` |
| `-c`, `--config <FILE>` | (required) | Config with `[server] sites_dir`, `[db.sqlite]`, and (for live mode / `/kv`) `[kv] secret` |
| `--dialect <sqlite\|mysql\|pg>` | `sqlite` | SQL dialect (see [Dialects](#dialects)). Live mode is MySQL-only |
| `--format <table\|tsv\|csv\|json>` | `table` | Output format (see below) |
| `--read-only` | off | Refuse any statement that is not a pure read (`SELECT` / `WITH` / `VALUES` / `EXPLAIN` / `SHOW` / `DESCRIBE`) |
| `--maintenance` | off | **Offline-only, unscreened.** Drop statement screening so engine operations (`PRAGMA integrity_check`, `PRAGMA wal_checkpoint`, path-PRAGMAs) can run. Refused while a server is up or in per-site clustered mode (see below) |
| `-e`, `--execute <SQL>` | — | The SQL to run (wins over the positional argument) |
| `[SQL]` | — | The SQL to run (positional). Omit to use `-e` or stdin |

### `--format` outputs

For `SELECT id, name FROM customers ORDER BY id`:

**`table`** (default — aligned, a header rule, and a row count):

```
id | name
---+---------------
1  | Ada Lovelace
2  | Alan Turing
(2 rows)
```

**`tsv`** (header row, tab-separated; NULL is an empty field, tabs/newlines in a
value become spaces):

```
id	name
1	Ada Lovelace
2	Alan Turing
```

**`csv`** (RFC 4180 — fields with a comma, quote, CR or LF are quoted and
embedded quotes doubled; NULL is empty):

```
id,name
1,Ada Lovelace
2,Alan Turing
```

**`json`** (a JSON array of objects; integers/floats are numbers, NULL is
`null`, a blob is a lowercase-hex string):

```json
[{"id":1,"name":"Ada Lovelace"},{"id":2,"name":"Alan Turing"}]
```

### Examples

```bash
# Count posts in the live database (server running → mysql dialect)
ephpm db query --site blog --config ephpm.toml --dialect mysql "SELECT COUNT(*) FROM wp_posts"

# Export a table as CSV for a spreadsheet (offline, sqlite-native)
ephpm db query --site shop --config ephpm.toml --format csv "SELECT * FROM orders" > orders.csv

# JSON for a script, read-only so a stray write can't slip through
ephpm db query --site shop --config ephpm.toml --read-only --format json \
  "SELECT id, total FROM orders WHERE total > 100"

# Engine maintenance (server must be stopped)
ephpm db query --site shop --config ephpm.toml --maintenance "PRAGMA integrity_check"
```

## `ephpm db shell`

```bash
ephpm db shell --site <KEY> --config <FILE> [options]
```

An interactive SQL REPL. The prompt shows the site, the mode, and the dialect:

```
  blog ‹offline · sqlite› >
  blog ‹live · mysql› >
```

Type SQL ending in `;` (it may span lines; the continuation prompt is `->`).
Lines beginning with `/` are **meta-commands**. End with `/quit`, `/exit`, or
Ctrl-D. Result sets, `/tables` / `/schema` output and `/kv` replies go to
**stdout**; the banner, prompt and status lines go to **stderr**, so a piped
session's stdout stays clean.

### Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--site <KEY>` | (required) | Vhost directory name under `[server] sites_dir` |
| `-c`, `--config <FILE>` | (required) | Config (as for `db query`) |
| `--dialect <sqlite\|mysql\|pg>` | `sqlite` | Initial dialect; switch at runtime with `/dialect`. Live mode is MySQL-only |
| `--read-only` | off | Refuse any statement that is not a pure read |
| `--maintenance` | off | Offline-only, unscreened (as for `db query`) |

There is no `--format` flag — the shell starts in `table` and `/format` changes
it at runtime.

### Meta-commands

| Command | Example | What it does |
|---------|---------|--------------|
| `/dialect [D]` | `/dialect mysql` | Show the current dialect (no arg), or switch to `sqlite`/`mysql`/`pg`. **Live mode is MySQL-only** and refuses the others; offline, switching rebinds the session over the same open file |
| `/format [F]` | `/format json` | Show the current format (no arg), or set it to `table`/`tsv`/`csv`/`json` |
| `/tables` | `/tables` | List the user tables (`sqlite_master` in `sqlite`, `SHOW TABLES` in `mysql`/`pg`) |
| `/schema <table>` | `/schema wp_posts` | Describe one table (its stored `CREATE` statements in `sqlite`, `DESCRIBE` in `mysql`/`pg`). The table name must be `[A-Za-z0-9_]+` |
| `/kv <ARGS...>` | `/kv GET session:42` | Run a RESP command against the **live** KV store (per-site authenticated). **Live mode only** |
| `/kv-put '<tmpl>' [col]` | `/kv-put 'cust:${id}' name` | Pipe the **last** result set into the live KV: for each row `SET` the interpolated key to `col` (or the whole row as JSON when no column is given). **Live mode only.** This is the **client-side** loop — see the note below |
| `/help` | `/help` | Command help |
| `/quit`, `/exit` | `/quit` | Leave (Ctrl-D also works) |

`/kv-put` key templates interpolate `${column}` from each row (`$$` is a literal
`$`); a `${column}` naming a column not in the result set is an error, so a typo
fails loudly rather than writing a literal `${col}`.

> **`/kv-put` is the client-side path, and it is a different thing from the
> in-SQL `kv_set()`.** `/kv-put` loops over a result set **in the CLI** and
> issues one RESP `SET` per row to the live KV. A future `kv_set()` SQL function
> — writing the KV from **inside** a `SELECT` executed by the engine — is a
> **separate, coming feature** and is documented elsewhere when it ships. They
> are not the same mechanism; do not conflate them.

### `--read-only` and `--maintenance`

- **`--read-only`** refuses any statement whose leading keyword is not a pure
  read (`SELECT`, `WITH`, `VALUES`, `EXPLAIN`, `SHOW`, `DESCRIBE`). A write is
  rejected before it reaches the engine:

  ```
  --read-only: refusing to run 'UPDATE' (only SELECT / WITH / VALUES / EXPLAIN / SHOW / DESCRIBE statements are allowed)
  ```

- **`--maintenance`** is **offline-only** and drops the statement screen that
  normally rejects engine-level operations on the tenant path. It unlocks
  `PRAGMA integrity_check`, `PRAGMA wal_checkpoint(...)`, and path-PRAGMAs — the
  things an operator may need for inspection or repair but a tenant must never
  run. It is refused while a server is up, and in per-site clustered mode:

  ```
  a server is serving this site on 127.0.0.1:3306, so --maintenance is refused:
  it is offline-only (it drops statement screening and needs exclusive access to
  the engine). Stop the server and re-run offline.
  ```

  Normal mode (no `--maintenance`) always screens, exactly like the
  `ephpm_db_*` bridge and the per-site `pdo_mysql` listener. Note `VACUUM`
  remains unsupported by the Turso engine itself, even under `--maintenance`.

### Example session

```text
$ ephpm db shell --site blog --config ephpm.toml

  ePHPm db shell — site blog
    connected: offline (sqlite)
    type SQL ending in ';', or /help for commands, /quit to exit

  blog ‹offline · sqlite› > /tables
name
--------
wp_options
wp_posts
(2 rows)
  blog ‹offline · sqlite› > SELECT ID, post_title FROM wp_posts LIMIT 2;
ID | post_title
---+-------------
1  | Hello world
2  | Sample Page
(2 rows)
  blog ‹offline · sqlite› > /format json
  blog ‹offline · sqlite› > SELECT COUNT(*) AS n FROM wp_posts;
[{"n":2}]
  blog ‹offline · sqlite› > /quit
```

See the [REPL guide](/guides/using-the-db-repl/) for a from-zero walkthrough,
including a worked `/kv-put` cache pre-warm.

## See also

- [Using the db REPL](/guides/using-the-db-repl/) — a from-zero walkthrough of `db query` and `db shell`
- [Importing a SQL dump](/guides/importing-a-sql-dump/) — a worked WordPress migration and the full compatibility matrix
- [`ephpm php`](../php/) — run `wp` / `artisan` against a site's database
- [Database architecture](/architecture/database/) — how the embedded engine and litewire fit together

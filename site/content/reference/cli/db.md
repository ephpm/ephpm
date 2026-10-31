+++
title = "ephpm db"
weight = 8
+++

Database tooling. The one subcommand today is `db import`, which loads a
`mysqldump` or `pg_dump` SQL file into a virtual host's embedded (Turso)
database and reports — honestly — everything SQLite/Turso could not represent.

The `db` group exists so later database verbs (`db export`, `db shell`) have a
home; only `db import` is implemented.

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

## See also

- [Importing a SQL dump](/guides/importing-a-sql-dump/) — a worked WordPress migration and the full compatibility matrix
- [`ephpm php`](../php/) — run `wp` / `artisan` against a site's database
- [Database architecture](/architecture/database/) — how the embedded engine and litewire fit together

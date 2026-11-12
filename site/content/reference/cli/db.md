+++
title = "ephpm db"
weight = 8
+++

Database tooling for a virtual host's embedded (Turso) database. Five
subcommands:

- [`db import`](#ephpm-db-import) — load a `mysqldump` / `pg_dump` SQL file,
  with an honest compatibility report.
- [`db query`](#ephpm-db-query) — run one SQL statement (or a `;`-separated
  batch) and print the result set.
- [`db shell`](#ephpm-db-shell) — an interactive SQL REPL with introspection and
  cache meta-commands.
- [`db restore`](#ephpm-db-restore) — install a scheduled backup
  (`[db.sqlite.backup]`) or a local `.db`/`.db.gz` over a site's database.
- [`db backup list`](#ephpm-db-backup-list) — list a site's backup objects.
- [`db dump`](#ephpm-db-dump) — snapshot a site's database to a local file on
  demand (offline `VACUUM INTO` → gzip).

`import`, `query` and `shell` resolve the target the same way [`ephpm php --site`](../php/#binding-a-virtual-hosts-database-site)
does and obey the same single-writer rule (see [Transport & mode](#transport--mode-shared));
`restore` and `dump` are offline-only. `db export` is **planned — not yet implemented**.

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
| `--source-charset <CHARSET>` | `utf8mb4` | What the dump's bytes are encoded in (see [Character encoding](#character-encoding)). `utf8mb4` (also `utf8`): the stream is UTF-8 and passes through byte-identically. A legacy single-byte charset (`windows-1252`, `latin1`, `iso-8859-15`, `windows-1251`, `koi8-r`, …) transcodes only the byte runs that are **not** valid UTF-8 from it |
| `--on-invalid-utf8 <reject\|strip\|replace\|transcode>` | follows `--on-error` | What to do with `TEXT` bytes that are not valid UTF-8. Default: `--on-error stop` ⇒ `reject` (the whole dump is validated first; **nothing** is written if any is found), `--on-error skip` ⇒ `strip`, and a legacy `--source-charset` ⇒ `transcode`. An explicit value overrides, but must agree with the charset (`transcode` needs a legacy charset; the others cannot be combined with one) |

### Character encoding

`db import` parses the dump file itself — it never shells out to a `mysql`
client — and reads it as **bytes**, so there is no charset-negotiating hop
between the file and the engine. With the default `--source-charset utf8mb4`
every valid UTF-8 sequence, 4-byte emoji included, lands byte-identical; nothing
on the path can narrow `⏰` to `?`.

What MySQL *does* leave in a dump is **invalid** UTF-8: it never validates the
bytes in a `TEXT` column, so a `latin1` or `binary` column holds whatever was
written (cp1252 smart quotes `0x92`/`0x93`/`0x94`, an em-dash `0x97`, lone high
bytes), and `mysqldump` writes those bytes out verbatim. The embedded engine
validates `TEXT` when it is **read**, so such a value would import and then make
every `SELECT` touching it fail with `TEXT value contains invalid UTF-8`. `db
import` therefore never writes one; what it does instead is the
`--on-invalid-utf8` policy, whose default follows the flags you already set:

- **`reject`** — the default under `--on-error stop` (itself the default). The
  whole dump is scanned before the database is opened; if any invalid sequence
  is found, one `error` finding per `table.column` (with how many values are
  affected) is printed, nothing is written, and the exit is non-zero with the
  way forward. Because this needs a second pass over the source, `reject`
  refuses a dump on **stdin** (`-`); save it to a file first or pick a streaming
  policy.
- **`transcode`** — the default when `--source-charset` names a legacy charset.
  Decodes *only the invalid byte runs* from that charset: bytes that already
  form valid UTF-8 are never re-decoded, so a `café` written as UTF-8 stays
  `café` while a raw `0x92` becomes `’` (U+2019). Lossless when the non-UTF-8
  columns really are in that charset. MySQL's `latin1` is cp1252, so
  `windows-1252` is the right choice for a `latin1` dump (the `latin1` label
  maps to it too). Only single-byte charsets are accepted.
- **`strip`** — the default under `--on-error skip`: the invalid bytes are
  dropped and the import continues. **`replace`** substitutes U+FFFD per
  invalid sequence. Both are lossy and both report what they changed.

Under every policy the run reports what happened, per column:

```text
  [downgraded] line 142 wp_posts.post_content: 318 TEXT value(s) not valid UTF-8; transcoded from windows-1252
invalid UTF-8: 331 TEXT value(s) across 320 row(s) — transcoded from windows-1252
```

`--dry-run` produces the same findings (as `error`s under `reject`), so it is
the way to find out whether a dump needs a policy before touching anything.

### Exit codes

- `0` — the import completed. Under `--on-error skip` this holds even when the
  report has warnings (count them in the summary).
- non-zero — under `--on-error stop`, any `unsupported` or `error` finding, or a
  statement that aborted the run.
- non-zero — the dump holds invalid UTF-8 under `--on-invalid-utf8 reject` (the
  default under `--on-error stop`); nothing is written.
- non-zero — the config or dump could not be read, the `--site` key was invalid,
  or the transport could not be established.

### Examples

```bash
# Load a WordPress mysqldump offline, then check it
ephpm db import blog.sql --site blog --config /etc/ephpm/ephpm.toml
ephpm php --site blog --config /etc/ephpm/ephpm.toml -- wp db query 'SELECT COUNT(*) FROM wp_posts'

# Preview what a dump would do without changing anything
ephpm db import blog.sql --site blog --config ephpm.toml --dry-run

# A gzipped pg_dump from stdin, keeping everything it can. --on-error skip
# also selects the streaming `strip` UTF-8 policy stdin needs.
gunzip -c shop.sql.gz | ephpm db import - --site shop --config ephpm.toml --dialect pg --on-error skip --report import.json

# A utf8mb4 dump, byte-faithful (this is the default; spelled out for clarity)
ephpm db import blog.sql --site blog --config ephpm.toml --source-charset utf8mb4

# A latin1 (cp1252) mysqldump: transcode the non-UTF-8 bytes losslessly
ephpm db import legacy.sql --site shop --config ephpm.toml --source-charset windows-1252
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
Turso file open at once, and `sqlite`-native / `pg` access means opening the
file directly, so neither can run through the live server. How that resolves
depends on whether you *chose* the dialect:

- **No `--dialect` (the default).** Live mode auto-selects `mysql` so `db
  query` / `db shell` just work against a running server; stop the server and
  the same command opens the file offline as `sqlite`. You never have to think
  about it.
- **An explicit `--dialect sqlite` (or `pg`) while a server is up** is a hard
  error, never a silent override of your choice:

  ```
  a server is serving this site on 127.0.0.1:3306; sqlite needs offline access
  (direct file open) — use --dialect mysql for the live DB, or stop the server
  and re-run for sqlite access.
  ```

  Pass `--dialect mysql` to work against the live database, or stop the server
  for `sqlite`-native / `pg` offline access.

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
| `--dialect <sqlite\|mysql\|pg>` | `sqlite` | SQL dialect (see [Dialects](#dialects)). Omitted, the default auto-promotes to `mysql` in live mode; an explicit `sqlite`/`pg` is refused there |
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
| `--dialect <sqlite\|mysql\|pg>` | `sqlite` | Initial dialect; switch at runtime with `/dialect`. Omitted, the default auto-promotes to `mysql` in live mode; an explicit `sqlite`/`pg` is refused there |
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
| `/kv <ARGS...> [\| <fmt> <path>]` | `/kv GET widget:42 \| json .price` | Run a RESP command against the **live** KV store (per-site authenticated), optionally piping the reply through a formatter (see [The `/kv` output pipe](#kv-output-pipe)). **Live mode only** |
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

### The `/kv` output pipe {#kv-output-pipe}

`/kv` accepts an optional output-formatting **pipe** after the RESP command:

```
/kv <resp-command…> [ | <format> <path> ]
```

Everything after the first **top-level** `|` is the pipe (a `|` inside a quoted
RESP argument, `/kv SET k "a|b"`, is part of the value, not a separator). The
pipe carries an optional **format** keyword and an optional **path**; **at least
one must be present**, otherwise it is an error:

```
pipe needs a format or a path, e.g. | json .price
```

The pipe applies to `/kv` only — never to `/kv-put` (which writes and returns no
value) or to the SQL-result-set `--format` / `/format`.

#### Model: normalize → path → format

The reply is first **normalized to a JSON value**, then the path selects part of
it, then the format renders it:

| RESP reply | JSON value |
|------------|-----------|
| `Simple` / `Error` string | a JSON string |
| integer | a JSON number |
| nil | `null` |
| bulk | the parsed JSON if the bytes are valid JSON; else the UTF-8 text as a string; else a lowercase-hex string (non-UTF-8) |
| array (`LRANGE`, `HGETALL`, …) | a JSON array of recursively-normalized items |

#### Formats

| Format | Renders |
|--------|---------|
| `raw` | The classic reply rendering (the default when there is **no** pipe). With a path, a selected scalar prints bare and a selected object/array prints as compact JSON. |
| `json` | Pretty-printed JSON, 2-space indent. |
| `yaml` | A YAML block (via `serde_yaml_ng`). |
| `table` | An object → a two-column `key \| value` table; an array of objects → a multi-column table (columns are the union of keys, first-seen order; a missing key is empty). A nested object/array in a cell renders as compact JSON. A scalar prints as itself. |

With a path but **no** format keyword, the output is **auto**: a scalar prints
raw (bare), an object or array prints as pretty JSON.

#### Path (jq-lite field selector)

The path operates on the normalized JSON value:

| Path | Meaning |
|------|---------|
| `.` | identity — the whole value |
| `.field` | an object field; nestable (`.a.b.c`) |
| `.arr[2]` | an array element by index |
| `.arr[-1]` | a negative index counts from the end |
| `.arr[]` | **fan out** — each element is formatted and printed on its own line |
| `.a,.b` | a comma list — build one **reduced object** keyed by each path's final field segment (a deliberate simplification of jq's comma stream) |

Field names in a path are `[A-Za-z0-9_-]`. A path that does not resolve (a
missing field or an out-of-range index) is a clear error naming the path, not a
crash.

#### Fail-soft on non-JSON

When the reply is a **top-level bulk value that is not JSON** and the pipe asks
for `json` / `yaml` / `table` or a path, the value is printed **raw** with a
one-line note on stderr — never quoted, never a path error, never a panic:

```
value is not JSON; printed raw
```

(A non-JSON bulk *inside* an array still normalizes to a JSON string, so an
array reply formats cleanly.)

#### Examples

For `widget:42` holding `{"id":42,"slug":"blue-hat","price":1900}`:

```
/kv GET widget:42                      → {"id":42,"slug":"blue-hat","price":1900}   (raw)
/kv GET widget:42 | json               → pretty JSON of all fields
/kv GET widget:42 | yaml               → a YAML block
/kv GET widget:42 | table              → a key | value table
/kv GET widget:42 | .price             → 1900          (scalar, bare)
/kv GET widget:42 | json .price        → 1900
/kv GET widget:42 | .slug,.price       → pretty JSON of {"slug":"blue-hat","price":1900}
/kv GET widget:42 | table .slug,.price → a key | value table of those two
/kv LRANGE recent 0 4 | json           → pretty JSON array
/kv GET missing | json                 → null
```

### `--read-only` and `--maintenance`

- **`--read-only`** refuses any statement whose leading keyword is not a pure
  read (`SELECT`, `WITH`, `VALUES`, `EXPLAIN`, `SHOW`, `DESCRIBE`). A `WITH` is
  additionally inspected so a data-modifying CTE (`WITH … DELETE …`, or a
  Postgres `WITH t AS (DELETE … RETURNING …)`) is still refused. A write is
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

## `ephpm db restore`

```bash
ephpm db restore --site <KEY> --config <FILE> --from s3://bucket[/prefix] [--at <TIME>] [--yes]
ephpm db restore --site <KEY> --config <FILE> --from <local .db or .db.gz> [--yes]
```

Installs a backup written by [`[db.sqlite.backup]`](/reference/config/#dbsqlitebackup)
— a gzip of a standard SQLite main file taken with `VACUUM INTO` — over a site's
live database. It is a **file install**, not an SQL replay, and it is
**offline only**: the command refuses while a server is running.

### What it does, in order

1. **Resolves the target** fail-closed, as `db import` does: `--site` must name
   a vhost directory under `[server] sites_dir` (the file is
   `[db.sqlite] dir/<key>.db`); in a single-node or whole-database config the
   one database is addressed as `--site default` (its backups are stored under
   that key). A **clustered** config resolves the same local file and the
   restore additionally **strips the CDC replication bookkeeping** the image
   carries (step 5a) so the installed file is clean origin data. Restoring into a
   cluster installs the file on **one** node and is **experimental and manual for
   both topologies**: replicas do **not** auto-converge onto it — a replica
   restarted with its old database tails the restored log *on top of* its old
   rows. So the whole cluster must be stopped and every other node's
   `.db`/`-wal`/`-shm` **wiped** so they cold-bootstrap. The command prints the
   full procedure (restore on the lowest-ordinal node, wipe the others, start the
   restored node first, verify each re-bootstrapped) and asks you to confirm; it
   can only check the local node and says so. **Per-site clustered** adds a
   hazard: a returning peer with a higher `SiteVersion` epoch could overwrite the
   restore silently. The automatic `--reseed` (which performs the wipe) is **not
   yet implemented**. See the
   [clustered-backup design note](/roadmap/clustered-db-backups/).
2. **Picks the object.** With `--from s3://bucket[/prefix]` the objects under
   `<prefix>/<site>/` are listed and the newest with a timestamp **at or
   before `--at`** is chosen (default: the newest of all). `--at` takes the
   compact form the objects are named with (`20260301T120000Z`),
   `YYYY-MM-DDTHH:MM:SSZ`, or a date (`2026-03-01` = midnight UTC). The
   endpoint, region and addressing style come from `[db.sqlite.backup]` in
   `--config` (the bucket may differ from the configured one); the credentials
   from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`. A local `--from` path is
   used as is (`.gz` is gunzipped).
3. **Prints the plan** — site, source (object, time taken, size), target and
   whether it exists — and asks for a typed `yes` on a terminal. Without a
   terminal it refuses unless `--yes` is given. Nothing has been touched yet.
4. **Checks the server is stopped**: both `[server] listen` and (when the wire
   listener is on) `[db.sqlite.proxy] mysql_listen` are probed; if either
   answers, the command stops with "stop the server first". A database file
   cannot be replaced under an open engine handle (the rename would be ignored
   on Linux and fail on Windows), so there is no live path with a physical
   artifact. A live restore is **planned — not yet implemented**.
5. **Downloads and verifies.** The object is downloaded to
   `<target>.restore-tmp.gz` next to the target (so the final rename stays on
   one filesystem) and gunzipped to `<target>.restore-tmp`.
   **5a. Reconciles (clustered images only).** The CDC replication bookkeeping a
   clustered snapshot carries (`turso_cdc`, `__ephpm_cdc_*`,
   `__litewire_cdc_watermark`) is dropped from the staged file — a no-op on a
   single-node image, which carries none — so the restored node mints a fresh log
   identity on open and serves clean origin data. (This does not by itself make a
   running cluster converge: peers still holding old data must be wiped so they
   cold-bootstrap — see step 1.) Then the staged file is opened with the engine
   and checked with
   `PRAGMA integrity_check`. Anything but `ok` deletes the temporary file and
   exits `2` — **the live database is never touched by a file that does not
   verify**.
6. **Installs.** The verified file is renamed over `<key>.db` (atomic replace),
   and a stale `<key>.db-wal` / `-shm` from the stopped server is deleted so it
   can never be replayed onto the new file. Start the server; the site serves
   the restored data.

### Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--site <KEY>` | (required) | Vhost directory name under `[server] sites_dir`, or `default` for a single-node config |
| `-c`, `--config <FILE>` | (required) | Config with `[server] sites_dir`, `[db.sqlite]` and the `[db.sqlite.backup]` endpoint/region |
| `--from <SOURCE>` | (required) | `s3://bucket[/prefix]` (the prefix is the configured `s3_path`), or a local `.db` / `.db.gz` |
| `--at <TIME>` | newest | Restore the newest object taken at or before this UTC time; `s3://` sources only |
| `-y`, `--yes` | off | Skip the confirmation (required when stdin is not a terminal) |

### Exit codes

- `0` — restored.
- `2` — the backup failed `PRAGMA integrity_check`; nothing was changed.
- `1` — anything else: a server is running, no object at or before `--at`, an
  invalid `--site`, a download or install error, or the confirmation was
  declined.

### Examples

```bash
# Roll a site back to the newest backup
ephpm db restore --site blog --config /etc/ephpm/ephpm.toml --from s3://ephpm-backups/ephpm/db

# The newest backup taken before a bad deploy at 09:30 UTC
ephpm db restore --site blog --config ephpm.toml --from s3://ephpm-backups/ephpm/db --at 2026-03-01T09:30:00Z --yes

# A single-node config's one database, from a file copied off another host
ephpm db restore --site default --config ephpm.toml --from /backups/20260301T090000Z.db.gz
```

## `ephpm db backup list`

```bash
ephpm db backup list --site <KEY> --config <FILE> [--from s3://bucket[/prefix]] [--json]
```

Lists a site's backup objects — time taken (UTC), compressed size and key —
oldest first, from the configured `[db.sqlite.backup]` bucket and `s3_path`
(or `--from`). The site key resolves as for `db restore`. `--json` prints a
document with `site`, `location` and a `backups` array (`key`, `taken_at`,
`taken_at_unix`, `bytes`). Credentials come from the environment, as for the
server. Nothing else is implemented under `db backup`: there is no `now`
(the server has no "run now" endpoint in this release) and no `rm` — delete
objects with your bucket tooling; the retention sweep keeps the newest one
regardless.

```text
$ ephpm db backup list --site blog --config ephpm.toml
site:     blog
location: s3://ephpm-backups/ephpm/db/blog/
taken (UTC)                     bytes key
2026-03-01T08:45:00Z          1048231 ephpm/db/blog/20260301T084500Z.db.gz
2026-03-01T09:00:00Z          1049920 ephpm/db/blog/20260301T090000Z.db.gz
```

## `ephpm db dump`

```bash
ephpm db dump --site <KEY> --config <FILE> --out <FILE> [--no-gzip]
```

Snapshots a virtual host's embedded database to a local file on demand — a
`VACUUM INTO` of the live file, gzipped by default (`--no-gzip` writes a raw
`.db`). For ad-hoc backups or seeding a blue-green / failover test. The site key
resolves as for `db restore` (`default` for a single-node or whole-database
config, the vhost directory name in per-site mode).

**Offline only** — refused while a server holds the database open (the same
single-writer rule as `db restore`), so a second process cannot `VACUUM INTO`
the file. For snapshots of a *running* node, use scheduled backups
([`[db.sqlite.backup]`](/reference/config/#dbsqlitebackup)), which can write to a
local `dir` as well as S3. The output is a plain gzip of a standard SQLite file
— restore it with `ephpm db restore --from <file>` — and is **not** encrypted by
ePHPm; protect it yourself.

```text
$ ephpm db dump --site blog --config ephpm.toml --out blog-20260301.db.gz
dumped site "blog" → blog-20260301.db.gz (1048231 bytes gzipped)
```

## See also

- [Using the db REPL](/guides/using-the-db-repl/) — a from-zero walkthrough of `db query` and `db shell`
- [Importing a SQL dump](/guides/importing-a-sql-dump/) — a worked WordPress migration and the full compatibility matrix
- [Database backups to object storage](/guides/database-backups/) — enabling `[db.sqlite.backup]`, credentials for S3/MinIO/R2, and a worked `db restore --at`
- [`ephpm php`](../php/) — run `wp` / `artisan` against a site's database
- [Database architecture](/architecture/database/) — how the embedded engine and litewire fit together

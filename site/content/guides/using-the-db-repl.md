+++
title = "Using the db REPL"
weight = 13
+++

ePHPm ships a SQL client for your site's embedded database built into the same
binary that serves it: `ephpm db query` for one-shot statements and
`ephpm db shell` for an interactive REPL. No `mysql` client, no `sqlite3`, no
PDO — the CLI talks to the engine the same way your PHP does.

This guide starts from zero: connecting, reading the prompt, switching SQL
dialects, introspecting the schema, choosing an output format, and using the
`/kv` cache commands — ending with a worked example that pre-warms the KV cache
from a query.

For the exhaustive flag-by-flag reference see [`ephpm db`](/reference/cli/db/).

## What works today vs coming

**Works today:**

- `ephpm db query` — run one statement (or a `;`-separated batch) and print the
  rows.
- `ephpm db shell` — an interactive REPL with `/dialect`, `/format`, `/tables`,
  `/schema`, `/kv`, `/kv-put`, `/help`, `/quit`.
- Three SQL dialects: `sqlite` (engine-native, the default), `mysql`, `pg`.
- `--read-only` and an offline-only `--maintenance` mode.
- `/kv` and `/kv-put` reach the **live** KV store over RESP, scoped to the site
  — a **client-side** loop.

**Coming later (not in this REPL):**

- An in-SQL `kv_set()` function that writes the KV from *inside* a `SELECT`
  executed by the engine. That is a **different mechanism** from `/kv-put` (see
  [`/kv-put` vs `kv_set()`](#kv-put-vs-kv-set)) and will be documented when it
  ships.
- `ephpm db export`.

## Before you start

You need a config with multi-tenant per-site mode — `[server] sites_dir` plus
`[db.sqlite]` — the same mode `ephpm php --site` and `ephpm db import` require.
A minimal one:

```toml
[server]
listen = "127.0.0.1:8080"
document_root = "/srv/sites"
sites_dir = "/srv/sites"

[kv]
secret = "change-me-openssl-rand-base64-32"

[db.sqlite]
dir = "/var/lib/ephpm/db"

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:3306"
```

Every example below uses `--site blog --config ephpm.toml`; swap in your own
vhost key (the directory name under `sites_dir`) and config path.

## One-shot queries with `db query`

The quickest way to ask the database one thing:

```bash
ephpm db query --site blog --config ephpm.toml "SELECT COUNT(*) AS posts FROM wp_posts"
```

```
posts
-----
42
(1 row)
```

The SQL can come from the positional argument above, from `-e`, or from stdin:

```bash
ephpm db query --site blog --config ephpm.toml -e "SELECT 1 + 1 AS two"
echo "SELECT option_name FROM wp_options LIMIT 3" | ephpm db query --site blog --config ephpm.toml
```

Pick a machine-readable format with `--format` (`table` is the default; the
others are `tsv`, `csv`, `json`):

```bash
# CSV straight into a file for a spreadsheet
ephpm db query --site shop --config ephpm.toml --format csv \
  "SELECT id, total, status FROM orders" > orders.csv

# JSON for a script
ephpm db query --site shop --config ephpm.toml --format json \
  "SELECT id, total FROM orders WHERE total > 100"
```

```json
[{"id":7,"total":129.5},{"id":9,"total":240}]
```

## The REPL: `db shell`

For more than one statement, open the shell:

```bash
ephpm db shell --site blog --config ephpm.toml
```

```
  ePHPm db shell — site blog
    connected: offline (sqlite)
    type SQL ending in ';', or /help for commands, /quit to exit

  blog ‹offline · sqlite› >
```

Type SQL ending in a `;` (statements can span several lines — the continuation
prompt is `->`). Lines starting with `/` are meta-commands. Leave with `/quit`,
`/exit`, or Ctrl-D.

### Reading the prompt

The prompt is `  <site> ‹<mode> · <dialect>› > `, and both middle words matter:

```
  blog ‹offline · sqlite› >      ← no server running; the file is open directly
  blog ‹live · mysql› >          ← a server is serving this site
```

- **`offline`** — no ePHPm server is running for this site, so the shell opened
  the site's database file directly. You can use any dialect, including
  `sqlite`-native.
- **`live`** — a server *is* running, so the shell forwards every statement to
  it over the MySQL listener and the server does the work (translation,
  screening, clustered owner-forwarding). Live mode is **MySQL-only**.

Why the distinction is load-bearing: the embedded engine is single-writer — two
processes must never hold the same file open at once. So the shell picks the
transport automatically, and if you ask for `sqlite`-native access while a
server is up it refuses rather than corrupt the file:

```
  blog ‹live · mysql› > /dialect sqlite
error: live mode talks to the server's MySQL listener, so it is MySQL-only;
'sqlite' is not available. Stop the server for offline access to sqlite/pg.
```

To work against the live database, use `mysql`. To use `sqlite`-native, stop
the server first.

### Switching dialects

`/dialect` with no argument shows the current one; with an argument it switches
(offline only):

```
  blog ‹offline · sqlite› > /dialect
dialect: sqlite
  blog ‹offline · sqlite› > SELECT name FROM sqlite_master WHERE type='table';
name
--------
wp_options
wp_posts
(2 rows)
  blog ‹offline · sqlite› > /dialect mysql
dialect: mysql
  blog ‹offline · mysql› > SHOW TABLES;
Tables_in_blog
--------------
wp_options
wp_posts
(2 rows)
```

- **`sqlite`** runs straight against the engine — this is where `sqlite_master`
  and raw `PRAGMA`s work.
- **`mysql`** / **`pg`** translate the SQL through litewire, so your app's own
  MySQL/PostgreSQL-flavoured queries run as written.

### Introspection: `/tables` and `/schema`

```
  blog ‹offline · sqlite› > /tables
name
--------
wp_options
wp_posts
(2 rows)
  blog ‹offline · sqlite› > /schema wp_posts
type  | name              | sql
------+-------------------+-----------------------------------------
table | wp_posts          | CREATE TABLE wp_posts ( ID INTEGER ... )
index | idx_wp_posts_date | CREATE INDEX idx_wp_posts_date ON ...
(2 rows)
```

`/tables` uses `sqlite_master` in `sqlite` and `SHOW TABLES` in `mysql`/`pg`.
`/schema <table>` shows the stored `CREATE` statements in `sqlite` and runs
`DESCRIBE` in `mysql`/`pg`. The table name must be a plain identifier
(`[A-Za-z0-9_]+`).

### Output format: `/format`

```
  blog ‹offline · sqlite› > /format json
  blog ‹offline · sqlite› > SELECT ID, post_title FROM wp_posts LIMIT 2;
[{"ID":1,"post_title":"Hello world"},{"ID":2,"post_title":"Sample Page"}]
  blog ‹offline · sqlite› > /format table
```

The four formats are `table` (aligned, the default), `tsv`, `csv`, and `json` —
the same set `db query --format` offers.

## Guard rails: `--read-only` and `--maintenance`

Start the shell (or a query) with `--read-only` when you only mean to look:

```
$ ephpm db shell --site blog --config ephpm.toml --read-only
  blog ‹offline · sqlite› > DELETE FROM wp_posts;
error: --read-only: refusing to run 'DELETE' (only SELECT / WITH / VALUES / EXPLAIN / SHOW / DESCRIBE statements are allowed)
```

`--maintenance` is the opposite end: **offline only**, it drops the screening
that normally protects a tenant database so you can run engine operations like
`PRAGMA integrity_check`. It is refused while a server is up:

```bash
# Server stopped:
ephpm db query --site blog --config ephpm.toml --maintenance "PRAGMA integrity_check"
```

```
integrity_check
---------------
ok
(1 row)
```

(`VACUUM` is not supported by the Turso engine itself, even under
`--maintenance`.)

## Caching from the shell: `/kv` and `/kv-put`

The `/kv` commands reach ePHPm's built-in KV store — the same one your app sees
through the `ephpm_kv_*` functions and the `cache`/`session-handler` packages.
They need **live mode** (a running server) and the server's RESP listener
enabled:

```toml
[kv.redis_compat]
enabled = true
```

Each connection is authenticated **as the site**, so you only ever see your own
vhost's keys.

`/kv` is a RESP passthrough — anything the KV speaks:

```
  blog ‹live · mysql› > /kv SET greeting "hello world"
OK
  blog ‹live · mysql› > /kv GET greeting
hello world
  blog ‹live · mysql› > /kv TTL greeting
-1
```

### Worked example: pre-warm the cache from a query {#pre-warm}

`/kv-put` pipes the **last result set** into the KV, one key per row. Say you
want a lookup table of product names cached so the app can skip a query on the
hot path. Run the query, then put it:

```
  blog ‹live · mysql› > SELECT id, name FROM products;
id | name
---+-------------
1  | Widget
2  | Gadget
3  | Sprocket
(3 rows)
  blog ‹live · mysql› > /kv-put 'product:${id}:name' name
/kv-put: wrote 3 key(s)
  blog ‹live · mysql› > /kv GET product:2:name
Gadget
```

For each row, `/kv-put` builds the key by interpolating `${column}` from that
row (`product:2:name` for row 2) and `SET`s it to the named value column
(`name`). Omit the value column to store the **whole row as JSON** instead:

```
  blog ‹live · mysql› > SELECT id, name, price FROM products;
  ... (3 rows) ...
  blog ‹live · mysql› > /kv-put 'product:${id}'
/kv-put: wrote 3 key(s)
  blog ‹live · mysql› > /kv GET product:2
{"id":2,"name":"Gadget","price":19.99}
```

A `${column}` that names a column not in the result set is an error, so a typo
fails loudly rather than writing a literal `${col}`. Use `$$` for a literal `$`.

### `/kv-put` vs `kv_set()` {#kv-put-vs-kv-set}

`/kv-put` is a **client-side loop**: the CLI reads the rows you just selected
and issues one RESP `SET` per row. It is useful for ad-hoc cache warming and
one-off backfills from the shell.

It is **not** the same thing as a future in-SQL `kv_set()` function, which would
write the KV from *inside* a `SELECT` run by the engine (e.g.
`SELECT kv_set('product:' || id, name) FROM products`). That is a separate,
coming feature, and when it ships it will be documented on its own. Don't reach
for `kv_set()` in queries yet — it isn't there; use `/kv-put` from the shell
instead.

## See also

- [`ephpm db`](/reference/cli/db/) — the full `query` / `shell` / `import` reference
- [Importing a SQL dump](/guides/importing-a-sql-dump/) — get an existing database in first
- [`ephpm php`](/reference/cli/php/) — run `wp` / `artisan` against the same database
- [Database architecture](/architecture/database/) — the embedded engine and litewire

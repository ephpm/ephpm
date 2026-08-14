+++
title = "Database from PHP"
weight = 6
+++

When ePHPm has an embedded database configured, PHP can reach it two ways:

1. **The wire path** — `pdo_mysql` (or any MySQL client) connects to
   `127.0.0.1:3306` and litewire translates. **This is the default and
   remains fully supported.** Zero code changes; every framework driver,
   ORM, and tool already speaks it.
2. **The in-process bridge** — the native `ephpm_db_query()` /
   `ephpm_db_execute()` functions run SQL through a per-thread litewire
   session *inside the server process*.

Both hit the **same** backend object. MySQL-dialect SQL, `SHOW`/`DESCRIBE`
emulation, `SET NAMES` no-ops, and `BEGIN`/`COMMIT`/`ROLLBACK` behave
identically on both. The bridge simply skips the TCP round trip and the
per-request connection setup that `pdo_mysql` pays; it is not a different
database, a different dialect, or a different result.

Queries made through the bridge are recorded in
[query stats](/architecture/query-stats/) exactly like wire queries — the
bridge is handed the same stats-wrapping backend the wire frontends serve.

> **Requires ePHPm v0.6.3 or newer.** The `ephpm_db_*` functions first
> shipped in v0.6.3.

## When the functions exist

The functions are registered by the SAPI unconditionally, so
`function_exists('ephpm_db_query')` is `true` for any script running inside
ePHPm — it tells you that you are on ePHPm, **not** that a database is
available.

A backend is wired to the bridge whenever **`[db.sqlite]` is configured**,
in every one of its modes:

| Configuration | Bridge available |
|---|---|
| `[db.sqlite]`, single-node (Turso, in-process) | Yes |
| `[db.sqlite]` + `[server] sites_dir` (per-site, multi-tenant) | Yes — each vhost gets its **own** database |
| `[db.sqlite]` + `[cluster]`, clustered Turso CDC path | Yes |
| `[db.mysql]` / `[db.postgres]` proxy only | **No** |
| No `[db.*]` block at all | **No** |

Where it is not available, both functions throw:

```
Exception: ephpm_db: no embedded database is active (requires [db.sqlite])
```

with exception code `0`. The bridge does not proxy to `[db.mysql]` or
`[db.postgres]`; for those, use the wire path.

## `ephpm_db_query()`

```php
ephpm_db_query(string $sql, array $params = []): array
```

Returns the rows as a **list of associative arrays** keyed by column name.

```php
$rows = ephpm_db_query(
    'SELECT id, name, score FROM users WHERE score > ? ORDER BY score DESC',
    [50]
);

// [
//   ['id' => 7, 'name' => 'alice', 'score' => 91.5],
//   ['id' => 3, 'name' => 'bob',   'score' => 62.0],
// ]
```

Value mapping into PHP:

| Backend value | PHP |
|---|---|
| INTEGER | `int` |
| REAL | `float` |
| NULL | `null` |
| TEXT | `string` |
| BLOB | `string` (PHP strings are binary-safe) |

Two shapes to know about:

- **A duplicate column name keeps the last value.** `SELECT a, a` yields
  one `a` key — the same behavior as `mysqli_fetch_assoc()`.
- **A statement with no result set returns `[]`, not an error.** Routing
  `SET NAMES utf8mb4` through `ephpm_db_query()` gives you an empty array.

## `ephpm_db_execute()`

```php
ephpm_db_execute(string $sql, array $params = []): array
```

Returns `['affected_rows' => int, 'last_insert_id' => int]`.

```php
$ok = ephpm_db_execute(
    'INSERT INTO users (name, score) VALUES (?, ?)',
    ['carol', 77.5]
);
$id = $ok['last_insert_id'];   // int
$n  = $ok['affected_rows'];    // int
```

A `SELECT` routed through `ephpm_db_execute()` returns **zeros** rather than
throwing — it is defined that way deliberately, so an adapter that
misclassifies a statement degrades instead of failing.

## Parameter binding

Placeholders are positional `?`. Parameters are bound, never interpolated —
there is no escaping function to call and no string concatenation to get
wrong.

Only the types the MySQL binary protocol can carry will bind:

| PHP type | Binds as |
|---|---|
| `null` | NULL |
| `bool` | integer `1` / `0` |
| `int` | INTEGER |
| `float` | REAL |
| `string` | TEXT if the bytes are valid UTF-8, otherwise BLOB |

Anything else — an array, an object, a resource — throws immediately,
before the statement runs:

```
Exception: ephpm_db: unsupported parameter type array (only null, bool,
int, float, and string parameters bind)
```

with exception code `0`. Convert objects yourself (`DateTimeInterface` →
formatted string, enum → its backing value) before binding.

## Errors

SQL failures throw a plain `\Exception` — **not** `PDOException`; the bridge
has no PDO involvement at all. The shape follows PDO's convention:

- **Message**: `SQLSTATE[xxxxx]: <backend message>`
- **Code**: the mapped **MySQL error number**

```php
try {
    ephpm_db_execute('INSERT INTO users (id) VALUES (?)', [1]);
    ephpm_db_execute('INSERT INTO users (id) VALUES (?)', [1]);
} catch (\Exception $e) {
    $e->getCode();      // 1062  (ER_DUP_ENTRY)
    $e->getMessage();   // SQLSTATE[23000]: UNIQUE constraint failed: users.id
}
```

Familiar codes come through mapped: `1062` duplicate key, `1064` parse
error, `1205` lock timeout, `1290` read-only, `1452` foreign key. Anything
the mapper cannot classify arrives as `1105` with SQLSTATE `HY000`.

Catch `\Exception` (or `\Throwable`); do not catch `PDOException`.

## The session model

There is **no connection object**. Nothing to open, close, pool, or pass
around.

Each OS thread that executes PHP gets its own litewire session, created
lazily on that thread's first `ephpm_db_*` call and reused thereafter. Every
call from that thread goes through that session.

The consequence that matters: **transaction state belongs to the worker
thread, not to the request.**

### Per-site databases (multi-tenant mode)

When `[server] sites_dir` is set and `[db.sqlite] dir` points at a directory,
each virtual host gets its **own** database file at `<dir>/<site-key>.db`, opened
lazily on that site's first query. `ephpm_db_query()` / `ephpm_db_execute()`
automatically resolve to the database of the site that served the current
request — a script on `site-a.example` and a script on `site-b.example` see
two physically separate databases and cannot read or write each other's data.
This is the tenant-isolation boundary (Turso has no per-schema ACL, so the
file is the boundary).

**Which name is the site key.** It is the vhost key that selected the document
root — the same one that names the directory under `sites_dir`. The `Host`
header is normalized (port stripped, trailing dot stripped, lowercased) and, if
`[server] sites_domain_suffix` is configured, the suffix is stripped, so a site
served from `sites/shop/` uses `shop.db` whether the client sent
`Host: shop.local`, `Host: shop`, or `Host: SHOP.LOCAL:8080`. One tenant has one
database, reachable by every name that reaches its code.

**A `Host` that matches no site gets no database.** A well-formed but unknown
host still falls back to `[server] document_root`, but it has no tenant
identity: `ephpm_db_*` returns `no per-site database context for this request`
rather than creating a database named after the header. Deploy the site (a
directory under `sites_dir`) and its database appears on the first query.

A worker thread that serves site A and is then dispatched a request for site B
swaps its session to B's database — it never runs B's query on A's connection.
Two extra guarantees apply on this path:

- **`ATTACH` / `DETACH` / `VACUUM`, and path-bearing `PRAGMA`s, are rejected**
  before reaching the engine (they would be a cross-database escape). This
  screening is on regardless of mode.
- **`pdo_mysql` works here too**, against each site's own database — see
  [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/). The bridge is
  not the only way in.

Per-site isolation is single-node only; see the
[`[db.sqlite]` reference](/reference/config/#dbsqlite) for `dir` and
`max_open_dbs`.

### Transactions

Transactions are plain SQL. There is no `beginTransaction()` — you issue
`BEGIN`, `COMMIT`, and `ROLLBACK` through either function, exactly as you
would over the wire, and the session tracks the state:

```php
ephpm_db_execute('BEGIN');
try {
    ephpm_db_execute('UPDATE accounts SET balance = balance - ? WHERE id = ?', [100, 1]);
    ephpm_db_execute('UPDATE accounts SET balance = balance + ? WHERE id = ?', [100, 2]);
    ephpm_db_execute('COMMIT');
} catch (\Throwable $e) {
    ephpm_db_execute('ROLLBACK');
    throw $e;
}
```

### The request-end rollback safety net

If a script reaches the end of a request with a transaction still open —
because it forgot to commit, or because it fatalled mid-transaction — ePHPm
issues a `ROLLBACK` on that thread's session and logs a warning
server-side:

```
WARN  PHP script left a database transaction open at request end — rolling
it back (scripts must COMMIT or ROLLBACK before the request finishes)
```

This runs at the end of **every** request in both execution modes: in
fpm mode after the script returns (success, `exit`, or fatal alike), and in
worker mode at response end, again before the next request is taken (so a
framework `terminate` hook that touches the database is covered), and on
worker-thread recycle after a fatal.

Without it, an abandoned transaction would sit open on that worker thread
and the **next, unrelated request** dispatched to the same thread would
silently join it. That is the bug this prevents.

Treat it as a safety net, not an API. Abandoned writes are lost, and you get
a warning in the log every time. Commit or roll back explicitly.

### Recovery from a dead connection

If the backend connection dies underneath a thread — during a clustered
failover, say — the failing call surfaces the error to PHP, and the session
is dropped so the *next* call reconnects. This mirrors how a wire client
recovers by reconnecting.

Ordinary SQL errors never recycle the session; that would discard live
transaction state on every constraint violation. Nor does ePHPm recycle
mid-transaction, so a misclassified error can never silently throw away a
transaction you still own — an open transaction on a dead connection is
cleaned up by the request-end rollback above instead.

## What the bridge does not do

These are real, current limitations. Each has an open issue.

- **No streaming cursor.** `ephpm_db_query()` buffers the entire result set
  before returning it. Correct, but not constant-memory — a Laravel
  `cursor()`/`lazy()` call over the bridge is buffered underneath. Do not
  stream a million rows through it.
  ([#264](https://github.com/ephpm/ephpm/issues/264))
- **No column metadata on empty results.** A zero-row `SELECT` returns `[]`,
  which cannot carry column names. Column-introspection APIs
  (`wpdb::get_col_info()`, `mysqli_result::fetch_fields()`, DBAL's
  `columnCount()`) therefore cannot report anything after a zero-row query.
  ([#262](https://github.com/ephpm/ephpm/issues/262))
- **No has-rowset signal.** Rows and OK metadata are split across two
  functions, so an adapter implementing a *unified* `query()` API must
  decide which one to call by inspecting the SQL's first significant
  keyword. That heuristic has edges — `WITH … INSERT` hybrids and
  `INSERT … RETURNING` are the known ones. If you write your own adapter,
  be aware you are inheriting this; the maintained adapters below already
  handle the common cases.
  ([#263](https://github.com/ephpm/ephpm/issues/263))
- **No named placeholders**, no prepared-statement handles, no multi-result
  iteration API.

## Performance, stated honestly

The in-process path avoids the MySQL wire protocol round trip and the
per-request connection setup `pdo_mysql` pays. That is a structural
difference, not a tuning trick, and it is the entire reason the bridge
exists.

No speedup figure is published here. The measurements that exist were taken
on a developer machine and have not been verified on production hardware,
and this project does not publish performance numbers without the hardware
and methodology behind them. See [Benchmarking](/benchmarking/) for what has
been measured under controlled conditions.

## Should you use it directly?

Usually not. Most applications should install the adapter for their
framework and keep writing ordinary framework code — the adapters below
route through these functions for you.

Call `ephpm_db_query()` / `ephpm_db_execute()` directly when you are writing
an adapter, a migration script, or a hot path where a framework's query
builder is the overhead you are trying to remove.

Either way, **the wire path stays fully supported and remains the default.**
Nothing about the bridge deprecates `pdo_mysql` on `127.0.0.1:3306`.

## See also

- [PHP packages](/reference/php-packages/) — the DB and cache adapters, drop-ins, and shims
- [Database engines](/architecture/database/engines/) — the embedded Turso engine
- [KV from PHP](/guides/kv-from-php/) — the sibling `ephpm_kv_*` functions
- [Configuration reference → `[db.sqlite]`](/reference/config/#dbsqlite)
- [Query stats with Prometheus](/guides/query-stats-prometheus/) — bridge queries are recorded too

+++
title = "Database from PHP"
weight = 6
+++

When ePHPm has an embedded database configured, PHP can reach it two ways:

1. **The wire path** — `pdo_mysql` (or any MySQL client) connects to
   `127.0.0.1:3306` and litewire translates. **This is the default and
   remains fully supported.** Zero code changes; every framework driver,
   ORM, and tool already speaks it.
2. **The in-process bridge** — the native `ephpm_db_*` functions run SQL
   through a per-thread litewire session *inside the server process*.

Both hit the **same** backend object. MySQL-dialect SQL, `SHOW`/`DESCRIBE`
emulation, `SET NAMES` no-ops, and `BEGIN`/`COMMIT`/`ROLLBACK` behave
identically on both. The bridge simply skips the TCP round trip and the
per-request connection setup that `pdo_mysql` pays; it is not a different
database, a different dialect, or a different result.

Queries made through the bridge are recorded in
[query stats](/architecture/query-stats/) exactly like wire queries — the
bridge is handed the same stats-wrapping backend the wire frontends serve.

> **Requires ePHPm v0.6.3 or newer.** The `ephpm_db_*` functions first
> shipped in v0.6.3. `ephpm_db_run()`, `ephpm_db_columns()`,
> `ephpm_db_in_transaction()`, `ephpm_db_available()`, `ephpm_db_errno()`
> and `ephpm_db_error()` were added in **v0.7.4**; use
> `function_exists()` to feature-detect them if you support older servers.

## The function surface

| Function | Returns | Runs SQL |
|---|---|---|
| `ephpm_db_query(string $sql, array $params = [])` | list of assoc rows | yes |
| `ephpm_db_execute(string $sql, array $params = [])` | `affected_rows` / `last_insert_id` | yes |
| `ephpm_db_run(string $sql, array $params = [])` | rows **and** OK metadata **and** `has_rowset` | yes |
| `ephpm_db_columns()` | last statement's column metadata | no |
| `ephpm_db_in_transaction()` | `bool` | no |
| `ephpm_db_available()` | `bool` | no |
| `ephpm_db_errno()` | `int` (0 = last statement succeeded) | no |
| `ephpm_db_error()` | `?array{code, sqlstate, message}` | no |

The five introspection functions never throw, never run SQL, and never
disturb what the executing functions left behind. They describe the last
statement **on the current worker thread**, and are reset at the end of every
request.

## When the functions exist

The functions are registered by the SAPI unconditionally, so
`function_exists('ephpm_db_query')` is `true` for any script running inside
ePHPm — it tells you that you are on ePHPm, **not** that a database is
available.

`ephpm_db_available()` is the question you actually want answered:

```php
if (function_exists('ephpm_db_available') && ephpm_db_available()) {
    // a statement issued now will reach a database
}
```

It returns `true` when a backend is wired up **and**, in per-site mode, the
current request has a tenant identity. It does not open the database, so a
`true` can still be followed by a connection failure if the storage
underneath is broken — but it rules out the two conditions you can act on
in advance (no `[db.sqlite]` at all; a `Host` that matches no site).

A backend is wired to the bridge whenever **`[db.sqlite]` is configured**,
in every one of its modes:

| Configuration | Bridge available |
|---|---|
| `[db.sqlite]`, single-node (Turso, in-process) | Yes |
| `[db.sqlite]` + `[server] sites_dir` (per-site, multi-tenant) | Yes — each vhost gets its **own** database |
| `[db.sqlite]` + `[cluster]`, clustered Turso CDC path | Yes |
| `[db.mysql]` / `[db.postgres]` proxy only | **No** |
| No `[db.*]` block at all | **No** |

Where it is not available, the executing functions throw:

```
Exception: ephpm_db: no embedded database is active (requires [db.sqlite])
```

with exception code **`2000`** (`ephpm_db_errno()` reports the same). The
bridge does not proxy to `[db.mysql]` or `[db.postgres]`; for those, use the
wire path.

> **Changed in v0.7.4.** This exception previously carried code `0`. The
> message text is unchanged — adapters that match on the wording are
> unaffected — but the code is now a documented, reserved value so it can be
> told apart from a SQL error without parsing text. See
> [Errors](#errors).

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

`last_insert_id` is the **connection's** most recent insert rowid, not
"what this statement inserted" — exactly like `mysqli_insert_id()` on the
wire. After an `UPDATE`, a `COMMIT`, or any other non-`INSERT` OK it still
reports the id of the last `INSERT` on that thread's session. Read
`affected_rows` to learn whether the statement changed anything, and capture
`last_insert_id` immediately after the `INSERT` that produced it. A statement
that returned a **result set** reports zero for both.

## `ephpm_db_run()`

```php
ephpm_db_run(string $sql, array $params = []): array{
    has_rowset: bool,
    rows: array,
    columns: array,
    affected_rows: int,
    last_insert_id: int,
}
```

**Added in v0.7.4.** The unified entry point: it runs the statement and tells
you what the statement actually did.

```php
$r = ephpm_db_run($sql, $params);

if ($r['has_rowset']) {
    foreach ($r['rows'] as $row) { /* ... */ }
} else {
    $n = $r['affected_rows'];
}
```

This exists so an adapter implementing a *single* `query()` API —
`mysqli::query()`, `wpdb::query()`, Laravel's
`statement()`/`affectingStatement()`, DBAL — never has to guess which of
`ephpm_db_query()` / `ephpm_db_execute()` to call by inspecting the SQL's
first significant keyword. `has_rowset` comes from the executed statement,
not from the SQL text.

Shapes worth knowing:

- **`rows` is always an array**, never `null`. When `has_rowset` is `false`
  it is `[]`, so you can `foreach` unconditionally. `has_rowset` is the
  discriminator — a zero-row `SELECT` also gives you `[]`.
- **`columns` is present even for a zero-row result set** (see
  [`ephpm_db_columns()`](#ephpm_db_columns)). It is `[]` when the statement
  produced no result set.
- **`affected_rows` / `last_insert_id` are zero for a result set**, matching
  `ephpm_db_execute()`'s long-standing contract.
- Errors throw exactly as the other two functions do.

`ephpm_db_query()` and `ephpm_db_execute()` are unchanged and are not
deprecated. Use them when you already know the statement's shape;
`ephpm_db_run()` is for when you do not.

## `ephpm_db_columns()`

```php
ephpm_db_columns(): array   // list of ['name' => string, 'type' => ?string]
```

**Added in v0.7.4.** Column metadata of the last `ephpm_db_*` statement on
this thread.

```php
$rows = ephpm_db_query('SELECT id, label FROM t WHERE id = ?', [999]);
// $rows === []  — no rows, and therefore no column names in the result

$cols = ephpm_db_columns();
// [ ['name' => 'id', 'type' => 'INTEGER'], ['name' => 'label', 'type' => 'TEXT'] ]
```

That is the point: a zero-row result has no rows to carry its column names,
so `wpdb::get_col_info()`, `mysqli_result::fetch_fields()` and DBAL's
`columnCount()` had nothing to report after a `SELECT` that matched nothing.

- Returns `[]` when the last statement produced no result set, when nothing
  has run yet, or when no embedded database is active.
- `type` is the column's **declared schema type**. It is `null` both for a
  column with no declared type and for an expression (`SELECT a + 1`) —
  SQLite draws no distinction between those, so neither can this.
- It is not affected by reading the rows first; call it before or after.

## `ephpm_db_in_transaction()`

```php
ephpm_db_in_transaction(): bool
```

**Added in v0.7.4.** Whether **this worker thread's** session is inside an
explicit transaction — read from the session's own flag, the same state the
MySQL wire frontend reports as `SERVER_STATUS_IN_TRANS`.

```php
ephpm_db_execute('BEGIN');
try {
    // ...
    ephpm_db_execute('COMMIT');
} catch (\Throwable $e) {
    if (ephpm_db_in_transaction()) {
        ephpm_db_execute('ROLLBACK');
    }
    throw $e;
}
```

Without it, a `transaction()` helper has to fire `ROLLBACK` blind after any
failure and swallow the resulting error, because it cannot know whether a
transaction is actually open — the failing statement may have been the
`BEGIN` itself, or the [request-end rollback](#the-request-end-rollback-safety-net)
may already have run.

Returns `false` when the thread has no session yet or no embedded database is
active. In both cases nothing can be open, so it is an answer and not a
guess. Never throws.

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

with exception code **`2003`** (changed from `0` in v0.7.4; the message is
unchanged). Convert objects yourself (`DateTimeInterface` → formatted string,
enum → its backing value) before binding.

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

### Telling a bridge problem from a SQL problem

**Since v0.7.4**, every `ephpm_db_*` exception carries a **nonzero** code, and
infrastructure failures use reserved values so they can be distinguished from
a SQL error without matching message text:

| Code | Meaning | SQLSTATE |
|---|---|---|
| `2000` | No embedded database is active (`[db.sqlite]` not configured) | `HY000` |
| `2001` | Per-site mode, and this request has no tenant identity | `HY000` |
| `2002` | The database for this request could not be opened | `HY000` |
| `2003` | A parameter's PHP type cannot bind (thrown before the statement runs) | — |

These sit in MySQL's **client**-error range (2000–2999, the `CR_*` codes),
which a server never emits. A SQL error always carries a **server**-range
code, so the two can never collide:

```php
try {
    ephpm_db_run($sql, $params);
} catch (\Exception $e) {
    if ($e->getCode() >= 2000 && $e->getCode() < 3000) {
        // the bridge could not run this — infrastructure, not your SQL
    } else {
        // a genuine SQL error; $e->getCode() is the MySQL error number
    }
}
```

Codes `2000` and `2003` were `0` before v0.7.4, and `2001`/`2002` were the
generic `1105`. **All four messages are unchanged**, so adapters that
currently detect these cases by matching the message text keep working.

### `ephpm_db_errno()` and `ephpm_db_error()`

```php
ephpm_db_errno(): int
ephpm_db_error(): ?array{code: int, sqlstate: string, message: string}
```

**Added in v0.7.4.** The last error on this thread, in parts — for
`mysqli_errno()` / `mysqli_error()` / `mysqli_sqlstate()` /
`PDO::errorInfo()`-shaped adapter APIs. Both survive the exception being
caught:

```php
try {
    ephpm_db_query($sql);
} catch (\Exception $e) {
    ephpm_db_errno();   // 1062
    ephpm_db_error();   // ['code' => 1062, 'sqlstate' => '23000',
                        //  'message' => 'UNIQUE constraint failed: users.id']
}
```

- `ephpm_db_errno()` returns `0` and `ephpm_db_error()` returns `null` when
  the last statement **succeeded** — not "when no error has ever occurred".
  Both are cleared by the next statement on the thread and at request end.
- They report on the last statement that *reached the bridge*. A
  parameter-binding refusal (code `2003`) is thrown before anything runs, so
  it leaves them untouched; the exception's own code is the signal there.
- `message` is the backend message alone. The `SQLSTATE[xxxxx]: ` prefix
  belongs to the exception's composed message, not to this array.
- Neither ever throws.

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

Once it has run, `ephpm_db_in_transaction()` reports `false` — so a wrapper
that checks before rolling back will not fire a second, pointless `ROLLBACK`
against a session that has already been cleaned up.

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

These are real, current limitations.

- **No streaming cursor.** `ephpm_db_query()` buffers the entire result set
  before returning it. Correct, but not constant-memory — a Laravel
  `cursor()`/`lazy()` call over the bridge is buffered underneath. Do not
  stream a million rows through it.
  ([#264](https://github.com/ephpm/ephpm/issues/264))
- **`WITH … SELECT` and `INSERT … RETURNING` do not work.** Not a bridge
  limitation — litewire routes a statement to the engine's query or execute
  path by matching its first keyword, and neither of these matches. Both
  fail with `SQLSTATE[HY000]: SQLite error: unexpected row during execution`,
  on the **wire path too**. Two things to know:

  - `ephpm_db_run()`'s `has_rowset` cannot rescue them. It reports what the
    statement *did*, and these statements do not run.
  - **`INSERT … RETURNING` still writes its row** before failing. Retrying
    on that error double-inserts. Split it into an `INSERT` followed by a
    `SELECT` instead.

  Fixing this means teaching litewire's statement classifier about CTEs and
  `RETURNING`.
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

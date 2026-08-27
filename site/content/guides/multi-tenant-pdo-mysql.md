+++
title = "Multi-tenant pdo_mysql"
weight = 7
+++

In multi-site mode every virtual host gets its **own** database file. This page
is about reaching it the ordinary way — `pdo_mysql`, `mysqli`, Eloquent,
`wpdb`, whatever your framework already uses — rather than through the
[`ephpm_db_*` bridge](/guides/db-from-php/).

It works, each site reaches only its own database, and a hostile tenant cannot
reach its neighbour's. The rest of this page explains how, because "trust us"
is not a security property.

## The short version

Each site gets its own MySQL **account**. The username is the site's hostname;
the password is minted per site by ePHPm and injected into that site's requests
as `$_SERVER['DB_PASSWORD']`. One listener serves everybody, and a connection's
database is decided by the credential it authenticates with.

```php
// wp-config.php, .env, config/database.php — wherever your creds live
define('DB_HOST', $_SERVER['DB_HOST']);      // 127.0.0.1
define('DB_NAME', $_SERVER['DB_NAME']);      // this site's hostname
define('DB_USER', $_SERVER['DB_USER']);      // this site's hostname
define('DB_PASSWORD', $_SERVER['DB_PASSWORD']);
```

Or with a DSN:

```php
$pdo = new PDO(
    "mysql:host={$_SERVER['DB_HOST']};port={$_SERVER['DB_PORT']};dbname={$_SERVER['DB_NAME']}",
    $_SERVER['DB_USER'],
    $_SERVER['DB_PASSWORD'],
);
```

Frameworks that read `DATABASE_URL` (Symfony, Doctrine) or Laravel's
`DB_CONNECTION`/`DB_HOST`/`DB_DATABASE`/`DB_USERNAME`/`DB_PASSWORD` get those
too, populated per request.

### Two things that will bite you

1. **Read `$_SERVER`, not `getenv()`.** ePHPm injects these into `$_SERVER`
   only. It deliberately installs no `sapi_module.getenv` handler: the process
   environment is shared by every worker thread, so putting tenant credentials
   there would be the exact cross-tenant leak this design avoids. Laravel and
   Symfony's `Env` repositories already read `$_SERVER`, so `env('DB_PASSWORD')`
   works; a bare `getenv('DB_PASSWORD')` returns `false`.

2. **The password changes on every restart.** It is derived from a secret
   generated in memory at startup and never written to disk. Do not paste it
   into `wp-config.php` — read it from `$_SERVER` every request. This is a
   deliberate trade: a stable password would have to be persisted somewhere,
   and a per-tenant secret at rest is a much bigger surface than one that only
   ever lives in memory.

## Configuration

Nothing new to set. Multi-tenant `pdo_mysql` turns on with per-site databases:

```toml
[server]
sites_dir = "/srv/sites"      # multi-site mode

[db.sqlite]
dir = "/var/lib/ephpm/dbs"    # one <site-key>.db per virtual host
max_open_dbs = 256            # LRU bound on simultaneously-open databases

[db.sqlite.proxy]
mysql_listen = "127.0.0.1:3306"   # the one listener every tenant connects to
max_connections = 0               # 0 = unlimited; see "Noisy neighbours" below
```

`dir` is required in multi-site mode — ePHPm refuses to start without it rather
than share one database between tenants. Per-site isolation is **single-node
only**; with `[cluster]` enabled the database is clustered and shared, and
startup warns about it.

At startup you will see:

```
per-site database mode: MySQL wire listener enabled with per-site credentials.
Each virtual host connects with DB_USER = its own hostname and the DB_PASSWORD
injected into its requests, and reaches ONLY its own database.
```

`hrana_listen`, `postgres_listen`, and `tds_listen` are **not** served in
multi-site mode (warned about at startup if configured). Only the MySQL
frontend can bind a database per connection; the others would have to serve one
shared backend to everyone.

## Why it is safe: the threat model

The tenant we defend against is **a site's own PHP code**. In shared hosting
that code is not trusted — it is the customer's. It can open arbitrary sockets
(`fsockopen`, `stream_socket_client`, a `PDO` DSN of its choosing), read its own
`$_SERVER`, and read its own files.

### Why not a port or a socket per site

Two designs suggest themselves, and both fail — for the same reason.

**A listener per site, each on its own port.** Ports are enumerable. Tenant B
loops `fsockopen('127.0.0.1', $p)` and finds tenant A's listener in under a
second. Nothing about the connection distinguishes the two callers.

**A listener per site, each on its own unix socket.** Every tenant's PHP runs
**in one process as one OS user**, so file permissions cannot separate them —
the same uid owns, and can open, every socket. `open_basedir` does not help
either: it restricts the plain-files wrapper, not the `unix://` transport or
PDO's `unix_socket` DSN parameter. And an unguessable path is obscurity, not a
permission.

There is no socket-level primitive that can tell tenant A's PHP from tenant B's
PHP inside one process. So the boundary has to be **a secret one tenant holds
and its neighbour does not** — which is how every real database separates
accounts, and what ePHPm already does for the
[multi-tenant KV listener](/guides/kv-from-php/).

### Why the username is safe to route on

The username in a MySQL handshake is client-asserted. Tenant B can type
`site-a.test` — site names are public. On its own the username is a *claim*,
and routing on it alone would be the shared-database hole with extra steps.

It becomes an identity when it is paired with a password only that tenant can
produce. ePHPm verifies the `mysql_native_password` challenge response **before**
it touches the database registry: a caller that cannot answer the challenge for
`site-a.test` never causes `site-a.test.db` to be opened, let alone read. It
gets `ERROR 1698 (28000)` and a closed connection.

Concretely, on a connection attempt:

1. The username must normalize to a valid site key (`[a-z0-9._-]`) — this also
   bounds the filename that will be derived from it.
2. `password = HMAC-SHA256(master_secret, site_key)`. The master secret is 32
   random bytes drawn at startup; it never touches disk, never reaches PHP, and
   cannot be recovered from any site's password.
3. The client's challenge response is verified against that password in
   constant time.
4. **Only then** is the site's backend resolved and bound to the connection,
   for the connection's whole lifetime.

A connection that fails any step gets no backend at all. There is no default or
shared backend on this path to fall back to.

The challenge is freshly random per connection, so a response captured from one
connection cannot be replayed against another.

### Where each site's password comes from

The router mints it per request from the **canonical site key** — the single
identity a request resolves to, and the same value that picks the site's
document root, its private temp/session directory, its KV keyspace, and its
database file. There is one derivation (`Router::resolve_site`) and everything
downstream consumes its result, so those cannot disagree.

That matters concretely with `[server] sites_domain_suffix`: `Host: shop.local`
and `Host: shop` are one tenant, so they get one document root, one session
directory, one `shop.db`, and one `DB_USER = shop`. Before issue #290 the
database key was derived separately and did *not* strip the suffix, so the same
tenant reached `shop.local.db` or `shop.db` depending on how it was addressed.

A `Host` that resolves to no site has no identity at all: it serves the default
document root and gets **no** `DB_*` variables and no database (issue #291) —
an unknown name cannot mint `<name>.db`.

`$_SERVER` is rebuilt per request from a thread-local table, so site B's PHP
never observes the credential injected for site A.

## What this does *not* protect against

Stated plainly, because a security page that only lists wins is not useful.

- **Noisy neighbours.** One listener means `[db.sqlite.proxy] max_connections`
  is a **global** cap. A tenant that opens connections greedily can crowd out
  its neighbours. This is an availability problem, not a confidentiality one —
  no tenant reaches another's data — but per-tenant connection caps are not
  implemented. Leave `max_connections` generous, or at `0`, unless you have a
  reason.
- **A tenant that can read another tenant's `$_SERVER`.** Nothing in ePHPm
  exposes it, but a PHP extension or a debugging tool that dumps another
  thread's request state would hand over the credential. Do not load such
  things in a multi-tenant deployment.
- **Anything on the box that is not a tenant.** `mysql_listen` is a real TCP
  port. Keep it on `127.0.0.1` (the default). A process on the same host that
  can read the derived password can use it — but so could it read the database
  files directly.
- **Clustered mode.** Per-site isolation is single-node only. With `[cluster]`
  enabled the database is shared across tenants and ePHPm warns loudly at
  startup.

## Resource cost

| Resource | Cost | Bound |
|---|---|---|
| Listeners / ports | **1**, regardless of site count | The configured `mysql_listen` |
| File descriptors for listeners | 1 | — |
| Open databases | Lazy, on a site's first query | `[db.sqlite] max_open_dbs` (default 256), LRU |
| Per-site credentials | ~64 bytes per active vhost | One cache entry per validated site key |
| Wire connections | As the tenants open them | `[db.sqlite.proxy] max_connections` (global) |

The listener count does not grow with the number of sites — that is the main
practical reason for one listener rather than N. A per-site listener would cost
N descriptors and N ports, would need the port to change across restarts (or a
port allocator), and — per the threat model above — would buy no isolation at
all, since the credential has to do that work either way.

Open **databases** are the resource that does scale with tenants, and that is
bounded by `max_open_dbs`: when the cache is full the least-recently-used
**idle** database is closed, and a later request re-opens it. A database with a
live session (a bridge session or an open wire connection) is never evicted, so
the cap is a *soft* bound — size it with headroom under `RLIMIT_NOFILE`
(roughly `max_open_dbs × 3 + sockets`).

## Failure modes

Every one of these fails closed — no tenant is ever handed a database that is
not its own.

| Situation | Result |
|---|---|
| `[db.sqlite] dir` unset in multi-site mode | **Startup fails** with a message naming the fix |
| `mysql_listen` unparseable or its port in use | **Startup fails** — no listener, no credentials injected |
| Wrong / missing password | `ERROR 1698 (28000)`, connection closed, no database opened |
| Username is not a valid site key | Refused before any path is derived from it |
| Request `Host` matches no vhost | No `DB_*` injected and no database context — the default docroot cannot mint `<host>.db` |
| A site's database cannot be opened | That connection is refused — never a fallback to another site's |
| Per-site wire not active | No `DB_*` in `$_SERVER` at all — a visibly absent config, not a shared one |

## Interaction with the `ephpm_db_*` bridge

Both paths resolve through the *same* per-site registry, so a site's
`pdo_mysql` connections and its bridge queries land on one backend instance and
one LRU entry — not two handles on one file. Both are recorded in
[query stats](/architecture/query-stats/).

Use whichever suits the code. `pdo_mysql` is the compatible one (WordPress,
Laravel, every ORM); the bridge skips the wire round trip and the per-request
connection setup.

One current difference worth knowing: the bridge resolves its tenant from a
per-request thread-local that only the default (non-worker) execution path
sets, whereas a wire connection carries its tenant in its own credential.
`pdo_mysql` therefore works in worker mode; the bridge does not yet.

### Turning the wire listener off

If every app on the box uses the bridge and nothing uses `pdo_mysql`, you can
drop the wire frontend entirely:

```toml
[db.sqlite.proxy]
mysql_wire_enabled = false   # default: true
```

With this set, ePHPm does **not** bind `mysql_listen` (no `:3306`) and injects
no `DB_HOST`/`DB_PORT`/`DB_USER`/`DB_PASSWORD` into requests — one fewer local
attack surface on a hardened preview host. The per-site registry and the
`ephpm_db_*` bridge stay wired up, so in-process database access is unchanged;
only the wire frontend is skipped. Startup logs that the listener is disabled.
Leave it at the default `true` for any deployment where an app uses stock
`pdo_mysql`.

## See also

- [Database from PHP](/guides/db-from-php/) — the `ephpm_db_*` bridge
- [KV from PHP](/guides/kv-from-php/) — the same per-site credential pattern for the RESP listener
- [Configuration reference → `[db.sqlite]`](/reference/config/#dbsqlite)
- [Query stats with Prometheus](/guides/query-stats-prometheus/)

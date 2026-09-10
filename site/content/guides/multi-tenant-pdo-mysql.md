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

2. **Read the password from `$_SERVER` every request; don't hard-code it.**
   Without `[kv] secret` it is derived from a random secret generated in memory
   at startup, so it *changes on every restart*. With `[kv] secret` set (see
   [Running wp-cli against a site](#running-wp-cli-artisan-against-a-site) below)
   it is stable across restarts, but reading it from `$_SERVER` is still the
   right habit — it works under both, and it is the only place ePHPm injects it.

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
2. `password = HMAC-SHA256(master_secret, site_key)`. The master secret is
   either the configured `[kv] secret` (when set — stable across restarts) or 32
   random bytes drawn at startup (when unset); either way it never touches disk,
   never reaches PHP, and cannot be recovered from any site's password.
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
- **Clustered mode.** With `[cluster]` enabled and the default
  `[db.sqlite.replication] per_site = false`, the database is shared across
  tenants and ePHPm warns loudly at startup. Setting `per_site = true` gives
  each tenant its own replicated database (**experimental**). `pdo_mysql`
  writes are forwarded to the site's owner on that path — see
  [Clustered mode](#clustered-mode) below.

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

## Clustered mode

With `[db.sqlite.replication] per_site = true` (**experimental**), each site's
database replicates across the cluster and one node *owns* each site by
rendezvous hashing. Only writes made on the owner are captured into the change
log that replicates, so a write made anywhere else has to get to the owner.

Both routes handle that identically: a connection on a node that does not own
the site — `pdo_mysql` or bridge — **forwards** its statements to the owner over
the cluster channel. Reads and writes work on any node, and you do not have to
route traffic to a particular one.

Two things worth knowing:

- **A connection is routed once, when it is opened.** Ownership is recomputed
  from live membership, so it can move while a connection is open. When it does,
  the old owner refuses the forwarded statements and the tenant sees an ordinary
  connection error until it reconnects — loudly, rather than writing somewhere
  the write would not replicate. Non-persistent `pdo_mysql` (the default) opens
  a connection per request, so the blast radius is one request;
  `PDO::ATTR_PERSISTENT` widens it until PHP drops the handle.
- **A forwarded statement is measured on the owner.** Its
  [query stats](/architecture/query-stats/) are recorded there, not on the node
  the request hit. `ATTACH` / `VACUUM` / path-`PRAGMA` rejections are unchanged
  — those are screened locally, at the frontend, before anything is forwarded.

Per-site clustered mode is **experimental** (the Turso engine is Beta upstream)
and its multi-node behaviour is unit-tested rather than validated on a live
cluster.

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

## Running wp-cli / artisan against a site

`wp`, `artisan`, migrations, and seeders run through the PHP **CLI**, not an
HTTP request — so there is no `Host` header to pick a tenant, and (for apps
built on the [`db-*` packages](/guides/db-from-php/)) no `ephpm_db_*` backend
bound. `ephpm php --site <key> --config <path>` supplies both:

```bash
# Offline (no server running) — opens the site's own database file directly:
ephpm php --site shop --config /etc/ephpm/ephpm.toml -- wp --path=/srv/sites/shop db query "SELECT COUNT(*) FROM wp_posts"

# Against a running server — forwards to it over the wire, so the single live
# writer stays the server (safe even while it is serving the site):
ephpm php --site shop --config /etc/ephpm/ephpm.toml -- vendor/bin/wp option get siteurl
```

`--config` and `--site` are ePHPm's own options and must come **before** the
PHP program/args; everything after (including a `--` separator) is passed to PHP
untouched. Without `--site`, `ephpm php` behaves exactly as before (no database
bound).

At startup it picks a strategy automatically:

| Situation | What it does |
|---|---|
| A server answers on `[db.sqlite.proxy] mysql_listen` | Connects to it authenticating as the site and forwards **raw** SQL — the server does all translation, screening, query-stats, and (clustered) owner-forwarding, exactly as for an HTTP request |
| No server, single-node / per-site-single | Opens the site's own `<key>.db` file directly (offline seeding) |
| No server, per-site **clustered** | **Refuses** — a standalone CLI cannot reach the site's HRW owner, and opening the local file would write to a replica whose writes never replicate |

Unknown or malformed `--site` keys fail closed (the same validation a request
uses), and a config that is not multi-tenant per-site is rejected rather than
silently binding a shared database.

### The wire path needs `[kv] secret`

To reach a **running** server, the CLI must present the site's MySQL password —
which it can only derive if the server's per-site credentials come from a
**stable, shared** secret rather than the per-process random one. Set `[kv]
secret` (the CLI reads the same config, so it derives the identical password):

```toml
[kv]
secret = "a-long-random-string-kept-in-a-0600-file-or-secret-mount"
```

Without it, the server still runs (random per-restart credentials) and the CLI's
**offline** path still works, but the wire path fails closed with a message
telling you to set `[kv] secret`.

> **Security implication — state it out loud.** With `[kv] secret` set, every
> tenant's database password is `HMAC-SHA256(secret, site_key)` over a *public*
> site name. So **read access to the config file is enough to impersonate any
> tenant's database connection.** That is the same trust boundary
> [`ephpm exec`](/guides/multi-tenant-hardening/) already assumes ("whoever can
> read this config can already impersonate any tenant"); keep the config
> `0600` / in a secret mount, exactly as you would the secret itself. If you do
> not need the CLI wire path, leave `[kv] secret` unset and the property never
> applies.

### Sandboxed on Linux: compose with `ephpm exec`

On Linux, nest the two subcommands so the database-touching tool runs inside the
tenant's Landlock + uid sandbox:

```bash
ephpm exec --site shop -- ephpm php --site shop --config /etc/ephpm/ephpm.toml -- wp core version
```

The outer `ephpm exec` establishes the filesystem/uid containment; the inner
`ephpm php --site` binds the database. See
[Multi-tenant hardening](/guides/multi-tenant-hardening/).

## See also

- [Database from PHP](/guides/db-from-php/) — the `ephpm_db_*` bridge
- [KV from PHP](/guides/kv-from-php/) — the same per-site credential pattern for the RESP listener
- [Configuration reference → `[db.sqlite]`](/reference/config/#dbsqlite)
- [Query stats with Prometheus](/guides/query-stats-prometheus/)

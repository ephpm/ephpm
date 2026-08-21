+++
title = "PHP Packages"
weight = 5
+++

ePHPm exposes native SAPI functions — `ephpm_kv_*` for the embedded KV store,
`ephpm_db_*` for the embedded database, and the `\Ephpm\Worker\*` primitives
for worker mode. You *can* call them directly. Most applications shouldn't:
the packages below wrap them in the API your framework already speaks, so you
keep writing ordinary framework code.

All of them are maintained in the [ephpm GitHub organization](https://github.com/orgs/ephpm/repositories)
and are MIT licensed.

## Installing

**These packages are not on Packagist.** Distribution is via each package's
GitHub repository as a Composer `vcs` repository.

Composer does **not** resolve a VCS dependency's own VCS repositories
transitively, so **every ePHPm package in the tree needs its own
`repositories` entry** — including packages you did not name in `require`.

```json
{
  "repositories": [
    { "type": "vcs", "url": "https://github.com/ephpm/db-laravel" },
    { "type": "vcs", "url": "https://github.com/ephpm/db" }
  ],
  "require": {
    "ephpm/db-laravel": "^0.1"
  }
}
```

Two things to check on the repository before you write the constraint:

- **The repo name is not always the package name.** The worker base package
  `ephpm/worker`, for example, lives at
  [github.com/ephpm/php-worker](https://github.com/ephpm/php-worker). Use the
  repository URL in `repositories` and the package name in `require`.
- **Not every package has a tagged release yet.** A `^0.1` constraint only
  resolves against a tag; for an untagged package use `dev-main` (with an
  appropriate `minimum-stability`). Check the repository's releases page.

Every package requires **PHP 8.2+**.

## Database adapters

These route SQL through the in-process
[`ephpm_db_*`](/guides/db-from-php/) bridge — no socket, no wire protocol, no
PDO. They need an ePHPm build that provides those functions (**v0.6.3 or
newer**) with `[db.sqlite]` configured. The adapter-facing additions of
**v0.7.4** — `ephpm_db_run()`, `ephpm_db_columns()`,
`ephpm_db_in_transaction()`, `ephpm_db_available()`, `ephpm_db_errno()`,
`ephpm_db_error()` — are additive; an adapter written against v0.6.3 keeps
working unchanged.

| Package | Repository | What it is |
|---|---|---|
| `ephpm/db` | [ephpm/db](https://github.com/ephpm/db) | The base library the others build on: a typed `Connection` facade, an exception hierarchy, and IDE/static-analysis stubs for the native functions. Start here if you are writing your own integration. |
| `ephpm/db-wordpress` | [ephpm/db-wordpress](https://github.com/ephpm/db-wordpress) | WordPress database drop-in (`wp-content/db.php`) — `wpdb` without `mysqli`. |
| `ephpm/db-laravel` | [ephpm/db-laravel](https://github.com/ephpm/db-laravel) | Laravel database driver (`'driver' => 'ephpm'`). Eloquent, the query builder, and migrations without PDO. |
| `ephpm/db-doctrine` | [ephpm/db-doctrine](https://github.com/ephpm/db-doctrine) | Doctrine DBAL 4 driver. |
| `ephpm/mysqli-shim` | [ephpm/mysqli-shim](https://github.com/ephpm/mysqli-shim) | Userland `mysqli` compatibility shim — the `mysqli` API your legacy code already calls, over the bridge. The global `mysqli` surface activates only when `ext-mysqli` is absent; the namespaced API works everywhere. |

> The stubs shipped by `ephpm/db` are for your IDE and static analyzer only.
> They are deliberately **not** in `autoload.files` — at runtime the global
> functions are provided natively by the engine, and autoloading the stub
> would redefine them and fatal. Point PhpStorm/Psalm/PHPStan at the
> `stubs/` directory instead.

The [wire path](/guides/db-from-php/) — `pdo_mysql` against
`127.0.0.1:3306` — remains fully supported and is still the default. These
adapters are an optimization, not a migration you are obliged to make.

## Cache, KV, and session packages

These route through the `ephpm_kv_*` SAPI functions into the embedded,
cluster-replicated KV store. See [KV from PHP](/guides/kv-from-php/) for the
underlying API.

| Package | Repository | What it is |
|---|---|---|
| `ephpm/cache` | [ephpm/cache](https://github.com/ephpm/cache) | PSR-16 (SimpleCache) and PSR-6 cache implementations. The framework-agnostic base. |
| `ephpm/cache-wordpress` | [ephpm/cache-wordpress](https://github.com/ephpm/cache-wordpress) | WordPress object-cache drop-in (`wp-content/object-cache.php`), including a real `wp_cache_flush()`. |
| `ephpm/cache-laravel` | [ephpm/cache-laravel](https://github.com/ephpm/cache-laravel) | Laravel cache store with an auto-discovered service provider. |
| `ephpm/cache-symfony` | [ephpm/cache-symfony](https://github.com/ephpm/cache-symfony) | Symfony Cache adapter (PSR-6 / PSR-16 compatible). |
| `ephpm/predis-connection` | [ephpm/predis-connection](https://github.com/ephpm/predis-connection) | A Predis `Connection` that routes Redis-shaped calls into the in-process store — a drop-in for `Predis\Client` with no socket. |
| `ephpm/session-handler` | [ephpm/session-handler](https://github.com/ephpm/session-handler) | A PHP session save handler backed by the KV store — replaces the Files / Memcached / Redis handlers with no daemon. |

Each of these requires the `ephpm_kv_*` functions, which means running under
ePHPm; check the individual repository's README for its own minimum ePHPm
version.

> ePHPm also has a **native** session save handler built into the SAPI
> (`session.save_handler = ephpm`), which needs no Composer package at all.
> `ephpm/session-handler` is the userland alternative for cases where you
> want the handler under your own control.

## Worker-mode adapters

These drive [worker mode](/guides/worker-mode/) — boot the framework once per
worker thread, then loop on requests.

| Package | Repository | What it is |
|---|---|---|
| `ephpm/worker` | [ephpm/php-worker](https://github.com/ephpm/php-worker) | Base SDK for worker mode: the primitives and IDE stubs the adapters below build on. Note the repository name. |
| `ephpm/octane-driver` | [ephpm/octane-driver](https://github.com/ephpm/octane-driver) | Laravel Octane driver. See [Laravel Octane (Worker Mode)](/guides/laravel-octane/). |
| `ephpm/wordpress-worker` | [ephpm/wordpress-worker](https://github.com/ephpm/wordpress-worker) | WordPress under worker mode (classic themes; block themes are a documented limitation). See [WordPress Worker Mode](/guides/wordpress-worker/). |
| `ephpm/psr15-worker` | [ephpm/psr15-worker](https://github.com/ephpm/psr15-worker) | Any PSR-15 handler — Slim, Mezzio, and friends. See [PSR-15 Apps (Worker Mode)](/guides/psr15-worker/). |

The three adapters all depend on `ephpm/worker`, so its repository needs its
own `repositories` entry alongside theirs.

## See also

- [Database from PHP](/guides/db-from-php/) — the `ephpm_db_*` bridge these DB adapters wrap
- [KV from PHP](/guides/kv-from-php/) — the `ephpm_kv_*` functions the cache packages wrap
- [Worker Mode](/guides/worker-mode/) — the engine primitives the worker adapters wrap

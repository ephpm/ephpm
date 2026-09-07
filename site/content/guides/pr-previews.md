+++
title = "PR Previews for Your App"
weight = 6
+++

Every pull request gets a live URL, with its own database and its own KV
keyspace, deployed in seconds and torn down when the PR closes. A bot leaves a
single sticky comment with the link.

This page is the orientation: what the environment is, what you put in your
repository, and where the depth lives. It is deliberately short and it does not
restate the two reference documents — it points at them, because a fact restated
in two places is a fact that will drift.

- **[Making your PHP app work as a preview](https://github.com/ephpm/switchboard/blob/main/docs/preview-app-guide.md)**
  — the full app-developer contract: every manifest field, the `$_SERVER`
  rules, `open_basedir`, disabled functions, recipes for WordPress, Laravel and
  Symfony. Read it once before your first deploy.
- **[PR Preview Bot](/guides/preview-bot/)** — the operator side: how the
  webhook receiver and the deploy daemon fit together, wildcard TLS, and how to
  stand the whole system up yourself.

## What you get

A preview is a **virtual host inside one shared ePHPm process** — not a
container, not a VM, not a chroot. Other people's previews run in the same
process, on the same uid. Everything per-tenant hangs off one
[canonical site key](/guides/virtual-hosts/): your document root, your database
file, your KV keyspace, your private temp and session directory.

Concretely, per preview:

| | |
|---|---|
| A hostname | `<owner>-<repo>-pr-<N>.<preview-domain>`, behind a wildcard certificate |
| A database | Its own embedded, SQLite-compatible Turso file. Nothing shared with other previews. There is no MySQL or PostgreSQL server on the host. |
| A KV store | Its own keyspace in ePHPm's in-process KV. There is no Redis server. |
| Isolation | `open_basedir` confined to your checkout plus your private temp root, plus a function denylist (no `exec`, no `mail`, no persistent connections) |
| A sticky PR comment | One comment, updated in place on every push, rewritten on close |

What you do **not** get, and should plan around: no shelling out at request
time, no `dl()`, no `queue:work` worker, and no authentication in front of the
preview URL (see [Private repositories](#private-repositories) below).

## The GitHub App

There is **no public marketplace App to install.** The App that comments on
`ephpm/*` pull requests as `ephpm[bot]` is a *private* GitHub App — visit
[github.com/apps/ephpm](https://github.com/apps/ephpm) and GitHub says so
plainly. A private App can only be installed on the account that owns it, so it
cannot be installed on a repository outside the ePHPm organization.

So there are two real situations:

**Your repo is in the `ephpm` org.** The App is already installed. Add an
`ephpm.yaml`, open a pull request, and the bot comments. Nothing else to do.

**Your repo is anywhere else.** You run the system yourself, and part of that is
creating your own GitHub App — one App per preview deployment, pointed at your
own webhook endpoint. The permissions, the webhook event to subscribe to, and
the rest of the standing-up work are in
[PR Preview Bot → Install it on your repo](/guides/preview-bot/#install-it-on-your-repo).
Do not wait for a hosted service; there is not one today.

## What happens on a pull request

The short version, in the order you will see it happen:

1. GitHub sends a `pull_request` webhook (`opened`, `synchronize`, `reopened`
   → deploy; `closed` → teardown).
2. The deploy daemon fetches `refs/pull/<N>/head` at the recorded head SHA from
   the **base** repository — which resolves fork PRs, and still resolves after
   the fork is deleted, without ever trusting a third-party clone URL.
3. It reads your `ephpm.yaml`, runs your `build:` steps, and moves the manifest
   out of the served tree.
4. It swaps the built checkout into place atomically and publishes your
   `docroot:` as ePHPm's per-site document-root override.
5. It runs your `seed:` steps, polls your `health:` path for a 200, and posts or
   updates the comment.

Two consequences worth internalising now, because they surprise everyone once:

**`build:` and `seed:` run outside ePHPm.** They are plain `sh -c` children of
the daemon, not HTTP requests, so they get no `$_SERVER['DB_*']` and cannot
reach your preview's database. `composer install` is fine; `php artisan migrate`
and `wp core install` are not. Anything that must touch the database has to
arrive as an HTTP request to your own preview — that is what the worked example
does.

**`build:` and `seed:` failures are logged, not fatal.** Your real gate is
`health:` — make it a path that only returns 200 once the app is genuinely
usable.

## Your manifest

Put an `ephpm.yaml` at your repository root. Everything except `version` is
optional, and a repo with no manifest still gets a preview (one is synthesized
from the detected framework).

```yaml
version: 1                 # required; only version 1 is understood
php: "8.5"
docroot: "public"          # the web root; default "." — see the warning below
build:
  - "composer install --no-dev --optimize-autoloader --no-interaction"
services:
  database: "turso"        # "turso" (default) or false
  kv: true                 # default true
  websocket: true          # default: auto-detect websocket.php at the docroot
seed:
  - "curl -fsS \"$PREVIEW_URL/preview-migrate.php?token=$MIGRATE_TOKEN\""
env:
  APP_ENV: "staging"
health: "/"
```

Field-by-field semantics, defaults, and the framework-detection table are in the
[app guide](https://github.com/ephpm/switchboard/blob/main/docs/preview-app-guide.md#1-ephpmyaml--the-manifest).
Three things are worth stating here because they change what you write:

**`services:` records intent; it does not provision anything.** The values are
type-checked — `database: "banana"` fails the parse with `unknown database
service "banana" (expected "turso" or false)` — but beyond that the deploy only
logs them and writes them into a sidecar file, and an unrecognised *key* under
`services:` is ignored rather than rejected. ePHPm never reads your manifest at
all. Your per-site database and KV come from the *operator's*
ePHPm configuration, and they are there whether or not you declare them. Setting
`kv: true` does not turn anything on; setting `database: false` does not turn
anything off. Treat the block as documentation of what your app uses.

**`docroot: "."` publishes your entire repository.** It is the default and it is
genuinely right for WordPress, whose repository root *is* its web root. But
every non-dot-prefixed file in your checkout becomes public — `composer.json`,
`README.md`, a stray `*.sql` dump, whatever a build step left behind. The deploy
warns on every such deploy, and your `ephpm.yaml` is moved to `.switchboard/`
before the site goes live so it is not served
([switchboard#16](https://github.com/ephpm/switchboard/issues/16)), but nothing
can vet the rest of your files. Declare a subdirectory if you have anything at
the root you would not paste into a public issue.

**`env:` may not reach your app yet.** The values are written to a `.env` at
your repository root, which is exactly right for Laravel, Symfony, Drupal, and
anything else built on `vlucas/phpdotenv` or `symfony/dotenv`. For an app that
does not read a `.env` — WordPress, most bespoke apps — the deploy also writes
`.ephpm-preview-prepend.php`, but nothing loads it for you yet. ePHPm shipped
the server half of the fix
([#463](https://github.com/ephpm/ephpm/issues/463), merged as
[#472](https://github.com/ephpm/ephpm/pull/472)): a per-site override file can
now carry an `auto_prepend_file` alongside `document_root`, applied as a
per-request INI directive. The deploy daemon does not generate that key yet
([switchboard#4](https://github.com/ephpm/switchboard/issues/4)). Until it does,
the workaround is one line at the top of your front controller:

```php
$__preview = __DIR__ . '/.ephpm-preview-prepend.php';
if (is_file($__preview)) {
    require_once $__preview;   // no-op off the preview host
}
```

## Database

Your preview gets its own Turso file. Two ways to reach it, both landing on the
same database.

**Stock `pdo_mysql`.** ePHPm runs one MySQL-wire listener and injects a per-site
credential into each request. Your ORM does not know anything changed. The
credentials arrive in **`$_SERVER`** — not `$_ENV`, not `getenv()` — because the
process environment is shared by every tenant in the process:

```php
$pdo = new PDO(
    "mysql:host={$_SERVER['DB_HOST']};port={$_SERVER['DB_PORT']};dbname={$_SERVER['DB_NAME']}",
    $_SERVER['DB_USER'],
    $_SERVER['DB_PASSWORD'],
);
```

Both naming conventions are injected in per-site mode (`DB_DATABASE`/`DB_USERNAME`
as well as `DB_NAME`/`DB_USER`, plus `DB_CONNECTION` and `DATABASE_URL`), so
Laravel's default `mysql` connection works unmodified. The password is
HMAC-derived from a master secret held only in memory, so it **rotates on every
restart of the host** — read it per request and never bake it into a cached
config. The username is a claim; the credential is the identity, and
authentication happens before the backend is resolved. See
[Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/).

**The native bridge.** `ephpm_db_*` runs SQL in-process: no wire protocol, no
connection setup, nothing to authenticate. Install the adapter for your stack
rather than calling the raw functions — [`ephpm/db`](https://github.com/ephpm/db)
(base), [`ephpm/db-wordpress`](https://github.com/ephpm/db-wordpress),
[`ephpm/db-laravel`](https://github.com/ephpm/db-laravel),
[`ephpm/db-doctrine`](https://github.com/ephpm/db-doctrine) (DBAL 4),
[`ephpm/mysqli-shim`](https://github.com/ephpm/mysqli-shim). These are
distributed from their GitHub repos as Composer `vcs` repositories, **not
Packagist**. Details in [Database from PHP](/guides/db-from-php/).

It is SQLite underneath either way. MySQL-only SQL — stored procedures, `ENUM`
semantics, vendor functions — is the thing most likely to fail first.

## KV

You get an in-process KV store. There is **no Redis server**; there is a
RESP-compatible listener in front of the store for clients that speak Redis.

The native path is less work on a preview: install
[`ephpm/cache`](https://github.com/ephpm/cache) (PSR-6/PSR-16),
[`ephpm/cache-laravel`](https://github.com/ephpm/cache-laravel),
[`ephpm/cache-symfony`](https://github.com/ephpm/cache-symfony),
[`ephpm/cache-wordpress`](https://github.com/ephpm/cache-wordpress),
[`ephpm/predis-connection`](https://github.com/ephpm/predis-connection), or
[`ephpm/session-handler`](https://github.com/ephpm/session-handler), and forget
about connections entirely.

If you insist on the RESP path, be ready for one asymmetry that catches
everybody: the database variables use the conventional names your framework
already reads, but the KV variables are `EPHPM_`-prefixed —
`EPHPM_REDIS_HOST`, `EPHPM_REDIS_PORT`, `EPHPM_REDIS_USERNAME`,
`EPHPM_REDIS_PASSWORD`. Laravel reads `REDIS_HOST`/`REDIS_PORT`/`REDIS_USERNAME`/
`REDIS_PASSWORD`, and nothing bridges the two for you. The listener also requires
a two-argument `AUTH <username> <password>` in multi-tenant mode; a
password-only client will fail to authenticate. The mapping snippet is in the
[app guide](https://github.com/ephpm/switchboard/blob/main/docs/preview-app-guide.md#10-kv--redis-and-the-naming-asymmetry);
sessions and the store itself are covered in [KV from PHP](/guides/kv-from-php/).

Note also that persistent connections are disabled on a multi-tenant host, so
phpredis `pconnect` will not work regardless of which path you choose.

## Private repositories

Be clear-eyed about this one: **preview deployments of private repositories are
not supported today.** Two independent reasons, both verified in source rather
than inferred.

**The checkout is unauthenticated.** The daemon mints a GitHub App installation
token, but it uses it only to post the comment and create the Deployment record.
The `git fetch` of `refs/pull/<N>/head` runs as an ordinary `git` child process
against the plain `https://github.com/<owner>/<repo>.git` clone URL, with no
credential helper, no `http.extraheader`, and no token in the URL. A public repo
fetches fine; a private one fails. (An operator could in principle give the
daemon's OS user ambient git credentials — that is outside switchboard's
contract, is not documented by it, and is not tested.)

**The preview URL has no gate.** Even with the checkout solved, a preview is
served at a guessable public hostname with no authentication in front of it.
Middleware to gate previews behind HTTP Basic, signed session cookies, or GitHub
OAuth was proposed as ePHPm PRs
[#387](https://github.com/ephpm/ephpm/pull/387),
[#388](https://github.com/ephpm/ephpm/pull/388) and
[#389](https://github.com/ephpm/ephpm/pull/389), and all three were **closed
unmerged**. There is no such builtin. So a private repository's code and seeded
data would be published to anyone who guesses the URL.

The related, separable fact — and the one that *is* about degrading gracefully —
is that GitHub **reporting** is optional. Configure both `--app-id` and
`--app-key` and previews are reported on the PR; configure neither and deploys
still run, they just say nothing. Configuring exactly one is a startup error, so
a half-configured App cannot silently never report.

If you need previews of private code today, run the whole system on
infrastructure you control and put your own authentication in front of the
preview domain. Treat anything you seed into a preview as public.

## The worked example

[`ephpm/wordpress-sample`](https://github.com/ephpm/wordpress-sample) is the
reference: real WordPress on a per-site Turso database, with the KV object cache
and a native WebSocket activity ticker. Read its
[`ephpm.yaml`](https://github.com/ephpm/wordpress-sample/blob/main/ephpm.yaml)
and its [`wp-config.php`](https://github.com/ephpm/wordpress-sample/blob/main/wp-config.php)
together — that pair is the concrete answer to "how do I wire the database and
KV properly."

The one thing about it that is not obvious from the packages' own READMEs, and
that will cost you an afternoon if you get it wrong: **the drop-ins and their
classes must be real files inside the document root.** Multi-tenant mode sets
`open_basedir` to your site container, so a `wp-content/db.php` symlinked to a
shared checkout outside it is denied — and WordPress does not fail loudly, it
silently falls back to stock mysqli and you get "Error establishing a database
connection." The sample vendors the files and points each drop-in at an
autoloader living under the docroot:

```php
// wp-config.php
define('EPHPM_DB_AUTOLOAD',    __DIR__ . '/ephpm-db/autoload.php');
define('EPHPM_CACHE_AUTOLOAD', __DIR__ . '/ephpm-cache/autoload.php');
```

Both drop-ins degrade rather than fatal when the SAPI functions are absent —
`db.php` hands WordPress back to mysqli, `object-cache.php` falls back to the
built-in non-persistent cache — which is what keeps the same checkout working on
ordinary hosting.

The other detail worth copying is the seed strategy. Because `seed:` runs
outside ePHPm, the sample cannot use `wp core install`; it drives WordPress's own
web installer and its in-docroot generator scripts **over HTTP**, so every insert
runs through the drop-in and lands in the per-site database.

A live preview of that repository is usually running at
`ephpm-wordpress-sample-pr-6.preview.ephpm.dev`. Previews are ephemeral by
definition — treat any specific URL as a demo that may be gone.

## Where to go deeper

- [Making your PHP app work as a preview](https://github.com/ephpm/switchboard/blob/main/docs/preview-app-guide.md)
  — the complete app-facing contract. The single most useful next click.
- [PR Preview Bot](/guides/preview-bot/) — running the system yourself.
- [Virtual Hosts](/guides/virtual-hosts/) — the multi-tenant runtime underneath.
- [Database from PHP](/guides/db-from-php/) · [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/) · [KV from PHP](/guides/kv-from-php/)
- [Native WebSockets](/guides/websockets/) — opt-in on the server, HTTP/1.1 only.
- [PHP packages](/reference/php-packages/) — every Composer adapter.
- [Preview Deployments roadmap](/roadmap/preview/) — what is still design, not shipped.

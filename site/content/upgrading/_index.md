+++
title = "Upgrading"
type = "docs"
weight = 4
+++

# Upgrading ePHPm

How to upgrade an existing ePHPm deployment from one version to the next — what changed, what you'll see, and what to do. This is **not** a feature changelog: for everything new in a release, read the [GitHub release notes](https://github.com/ephpm/ephpm/releases). It is also **not** about moving an app onto ePHPm from another stack — for Apache, Nginx + PHP-FPM, Docker Compose, shared hosting or Laravel Forge, see [Migration](/migration/).

ePHPm ships as a single binary and its embedded database files are format-compatible across versions, so **most upgrades are a drop-in binary (or Docker image) swap**. Only the releases listed below need you to do anything; crossing a release that isn't listed means it introduced no operator-visible breaking change. **When upgrading across several versions, read every section between your current version and your target.**

## How to upgrade (any version)

1. **Binary / service:** replace the binary — or re-run `ephpm install` with the new one, which keeps your existing config (it writes a default config only if none exists) — and restart the service.
2. **Docker:** bump the image tag. The rolling tags `:latest` and `:<minor>` move on each stable release; pin `:vX.Y.Z` if you want to control exactly when you move.
3. **Stop the old version cleanly first.** A graceful stop (`SIGTERM` / `systemctl stop` / container stop) checkpoints the embedded database's write-ahead log. A database left with a hot `-wal` from a hard crash should be started once on the old version and stopped cleanly *before* you upgrade.
4. **`.db` files open in place** — no dump/reload between versions.

Then read the per-version notes below for anything that needs action.

---

## v0.20.0

### PHP no longer sees the process environment by default (environment scrub)

- **What changed:** `ephpm serve` now **scrubs the process environment before PHP starts** (`[php] scrub_environment`, default `true`; audit finding F1). A tenant's PHP reads the process environment through `getenv()`, `$_ENV` and `phpinfo()`, so in a multi-tenant deployment the shared environment would otherwise let any tenant read the operator's `EPHPM_KV__SECRET` / `EPHPM_CLUSTER__SECRET`, `AWS_*` credentials and any other exported secret. The scrub keeps only `[php] env_allowlist` (default `PATH`, `TZ`, `LANG`, `LC_*`) plus ePHPm's own small operational keep-set. On glibc Linux the scrubbed values are also zeroed in place so `/proc/self/environ` cannot leak them. The same scrub is applied by the `ephpm php --site` tenant CLI lane (cron / `wp` / `artisan`). This follows the v0.12.0 precedent where `ephpm exec` began running with a cleared environment.
- **You'll see:** an app that read a non-allowlisted variable from `getenv()` / `$_ENV` (e.g. `APP_ENV`, a third-party API key you exported) now finds it unset. Secrets ePHPm itself consumes are unaffected (figment read them into config first; `AWS_*` are cached before the scrub), so the server, DB, KV and cloud backups keep working. On Windows, core OS variables (`SystemRoot`, `PATH`, `TEMP`, `COMPUTERNAME`, …) are kept automatically.
- **Do:** list the exact variables your app legitimately reads in `[php] env_allowlist` — e.g. `env_allowlist = ["PATH", "TZ", "LANG", "LC_*", "APP_ENV", "AWS_REGION"]`; **never put a secret there** (hand per-vhost values to a tenant via the override file's `[env]` table instead, injected into that tenant's `$_SERVER`). A single-site deployment that genuinely relies on an inherited environment may set `[php] scrub_environment = false` (not permitted under `[server] sites_dir`). See [What PHP can see: the environment scrub](/reference/environment-variables/#what-php-can-see-the-environment-scrub) and [Multi-tenant hardening → Environment scrubbing](/guides/multi-tenant-hardening/#environment-scrubbing-finding-f1).

---

## v0.13.1

### PHP `memory_limit` is now bounded, and an explicit value is honoured

- **What changed:** an explicit `[php] memory_limit` is now honoured under `ephpm serve` (through v0.13.0 serve mode silently replaced it with the autotuned value); the autotuned per-request limit is capped at `1024M`; and a runtime raise above `[php] memory_limit_max` (default: the `memory_limit` PHP started with) is refused.
- **You'll see:** a config with `memory_limit = "128M"` — including the one `ephpm install` wrote on older versions, Windows services among them — now really runs every request at 128M. `ini_set('memory_limit', …)` above the ceiling, including `-1`, returns `false` and the limit stays (WordPress raising admin requests to `256M` fails under a 128M limit), and each refusal logs one WARN (`ePHPm refused memory_limit=…`). On a large host the autotuned limit is now `1024M` rather than a share of all memory.
- **Do:** remove `[php] memory_limit` to keep autotuning, or set the value you want; raise `[php] memory_limit_max` (e.g. `"512M"`) to allow runtime raises; set `memory_limit` explicitly if an app genuinely needs more than `1024M`. See [`[php]` memory settings](/reference/config/#php).

### HTTP request framing is stricter (request smuggling)

- **What changed:** ambiguous HTTP/1.1 request framing is now rejected.
- **You'll see:** `400` (with `Connection: close`) for `Content-Length` together with `Transfer-Encoding`, any `Transfer-Encoding` other than a single `chunked`, a request with no `Host` or two, or an absolute-form target whose authority differs from `Host`. A malformed chunked body is `400` (was `413`), and any request sent with `Transfer-Encoding` closes its connection after the response.
- **Do:** nothing for well-behaved clients and proxies. If a client or proxy relied on one of these, fix its framing. See [HTTP request framing](/architecture/security/#http-request-framing-request-smuggling).

---

## v0.13.0

### Per-site clustered nodes need a full-cluster restart (HRW scheme change)

*Only affects experimental per-site clustered deployments (`[db.sqlite.replication] per_site = true`).*

- **What changed:** the ownership hash that maps sites to nodes moved from scheme 1 to a better-mixed scheme 2, so a site's owning node changes.
- **You'll see:** a node running a different scheme logs an ERROR for every peer advertising another scheme, and refuses to serve as owner (reads included) while schemes are mixed — it fails closed rather than forking a tenant's data.
- **Do:** upgrade this as a **full-cluster restart**, not a rolling one — stop every node, upgrade, then start them. Serving resumes on its own once all nodes agree on scheme 2. See [Clustering setup](/guides/clustering-setup/).

### KV snapshot controls now require a credential

*Only affects nodes using `[kv.snapshot]`.*

- **What changed:** the `POST /_ephpm/kv/snapshot` save and `/load` controls now require `[server.diagnostics] token` or an mTLS listener on **every** node, single-site included (the read-only `GET` status is unchanged).
- **You'll see:** `401`/`403` on those POSTs where they previously succeeded.
- **Do:** configure a token or mTLS — see [access to internal endpoints](/reference/config/#access-to-internal-endpoints).

### Clean-shutdown caveat before the Turso 0.8.3 upgrade

- **What changed:** the embedded engine moved to Turso 0.8.3. Existing database files upgrade in place.
- **You'll see:** nothing, in the normal case. The one exception is a database left with a hot, uncheckpointed `-wal` from a **hard crash** (not a graceful stop).
- **Do:** let the old version shut down cleanly once before upgrading, so its write-ahead log is checkpointed. A graceful `SIGTERM` stop already does this.

---

## v0.12.0

### Config now fails closed on unknown keys and mode typos

- **What changed:** an unknown key in any config section, and a typo in a mode-selecting enum (e.g. `replication.role`, `[php] mode`, `[kv] compression`), now **abort startup** naming the offender instead of being silently ignored. These are the errors that otherwise pass every health check while running the wrong thing.
- **You'll see:** startup fails with a message naming the key or the accepted values — where the old binary booted and quietly ran a default.
- **Do:** fix or remove the flagged key. Knobs that were *removed but still honoured* for upgrades (`[db.sqlite.sqld]`, `cdc_experimental`, `engine`) still parse and warn. See [unknown keys are a startup error](/reference/config/#unknown-keys-are-a-startup-error).

### Metrics / diagnostics endpoints require auth on multi-site nodes

*Only affects multi-tenant nodes (`[server] sites_dir` set).*

- **What changed:** on a multi-site node, `/metrics` and the diagnostics endpoints answer `403` until a `[server.metrics]`/`[server.diagnostics]` `token` or an mTLS listener is configured. (Single-site nodes are unchanged.)
- **You'll see:** your Prometheus scrape or diagnostics probe against a multi-tenant node starts returning `403`.
- **Do:** set a `token` (or point scraping at an mTLS listener), then give the scraper that credential. See [access to internal endpoints](/reference/config/#access-to-internal-endpoints).

### `display_errors` defaults to `Off` under `ephpm serve`

- **What changed:** in `serve` mode PHP errors no longer render into the HTTP response by default; they go to ePHPm's log instead.
- **You'll see:** a blank page or generic 500 where a PHP error used to print into the response body in production.
- **Do:** read errors from the log, use `ephpm dev` (which keeps them visible) for local work, or set `display_errors = On` via `[php] ini_overrides` if you genuinely want them in responses. See [configuration](/getting-started/configuration/).

### KV RESP listener enforces the password on single-site nodes

- **What changed:** a single-site node with `[kv] secret` set previously accepted *any* password on the RESP listener; the password is now enforced.
- **You'll see:** a RESP client authenticating with a wrong or placeholder password now fails `AUTH` where it used to connect.
- **Do:** authenticate with the correct secret. See [`[kv]` reference](/reference/config/).

### `ephpm exec` runs with a cleared environment

- **What changed:** the sandboxed command launched by `ephpm exec` no longer inherits the parent process environment.
- **You'll see:** a script run through `exec` that relied on an inherited environment variable finds it unset.
- **Do:** pass the variables the command needs explicitly rather than depending on inheritance.

---

## v0.9.0

### The `[php]` config keys were renamed; old keys fail startup

- **What changed:** v0.9.0 removed the `spawn_blocking` execution engine (the dedicated pool is the only per-request engine) and renamed or collapsed the `[php]` concurrency knobs. Every old key is now a hard startup error naming the key, so an old config fails fast rather than silently dropping your concurrency cap.
- **You'll see:** startup fails naming an offending key — `mode = "fpm"`, `fpm_engine`, `workers`, `worker_count`, `worker_backlog`, `overload_policy`, or a `worker_*` key.
- **Do:** rename per the mapping — `mode = "fpm"` → `"per_request"`, `workers`/`worker_count` → `concurrency`, `worker_backlog` → `queue_depth`, `overload_policy` → `overload`, and the `worker_*` keys move under `[php.worker]`. Note `workers = 0` meant *unlimited*; `concurrency = 0` means *derived* (clamped `[2, 32]`), so an unbounded PHP concurrency setting no longer exists. Full table: [Migration from the pre-v0.9.0 `[php]` keys](/reference/config/#migration-from-the-pre-v090-php-keys).

---

## v0.7.0 (upgrading from 0.6.x)

### The embedded SQLite engine changed to Turso

- **What changed:** v0.7.0 removed the rusqlite (SQLite C engine) backend and the sqld sidecar; the in-process [Turso engine](/architecture/database/engines/) is now the only embedded engine.
- **You'll see:** `[db.sqlite] engine = "sqlite"` (or `"rusqlite"`) is a hard startup error with a migration message; the removed `[db.sqlite.sqld]` block and `cdc_experimental` flag log deprecation warnings and have no effect.
- **Do:** remove the `engine` key or set it to `"turso"`. Your `.db` files open in place — a cleanly-shut-down 0.6.x database (WAL or rollback journal) needs no dump/reload, but **stop the old node cleanly before upgrading** (a hot `-wal` left by a hard crash was not verified to replay through Turso), and note that non-UTF-8 `TEXT` may not round-trip. Details: [Opening existing `.db` files](/architecture/database/engines/#opening-existing-sqlite--rusqlite-db-files).

---

*Releases not listed here introduced no operator-visible breaking change — see the [GitHub release notes](https://github.com/ephpm/ephpm/releases) for what's new in each.*

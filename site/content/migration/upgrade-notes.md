# Upgrade Notes

Breaking and behavior changes that need your attention when upgrading an existing ePHPm deployment. This is **not** a feature changelog — for what's new in each release, read the [GitHub release notes](https://github.com/ephpm/ephpm/releases). This page lists only the changes that can break a silent upgrade, as **what changed → what you'll see → what to do**, and links to the reference page that owns each setting.

ePHPm is single-binary and its embedded database files are format-compatible across versions, so most upgrades are drop-in. When upgrading across several releases, read **every** section between your current version and your target.

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

*Upgrading across a release not listed here means it introduced no operator-visible breaking change — check its [release notes](https://github.com/ephpm/ephpm/releases) for what's new.*

# ePHPm native scheduled jobs (`[cron]`)

A worked `ephpm.toml` for the built-in distributed scheduler added in **v0.16.0**.
Declare jobs in config; the running server fires them on a standard five-field
cron schedule. No systemd timer, Compose loop, or Kubernetes `CronJob` sidecar.

See [`ephpm.toml`](./ephpm.toml) in this directory for the fully-commented config.

## What's in the example

| Job | Kind | When | Runs as |
|-----|------|------|---------|
| `wp-cron` | per-site PHP (`ephpm php --site`) | every 5 min | `blog` tenant, with `DB_*` |
| `laravel-schedule` | per-site command (`ephpm exec --site`) | every minute | `shop` tenant |
| `shop-nightly-rollup` | per-site, timezone + catch-up | 02:00 America/Denver | `shop` tenant |
| `nightly-report` | siteless binary | 03:00 UTC | cluster leader, no tenant creds |
| `data-export` | suspended | — | parsed & listed, never fires |
| directory tier | `blog`'s `.ephpm/cron/*/` scripts | hourly/daily/weekly/monthly | `blog` tenant |

## How a job is dispatched

Decided by `command[0]` and whether `site` is set:

- first arg ends in **`.php`** + a `site` → `ephpm php --site <key>` (binds the
  tenant DB bridge, exports `DB_*` / KV credentials)
- any other command **with** a `site` → `ephpm exec --site <key>` (tenant sandbox)
- **no** `site` → the binary is spawned directly on the leader, **no** tenant
  credentials (a siteless job may not be a `.php` command)

## Per-job keys

| Key | Required | Default | Meaning |
|-----|----------|---------|---------|
| `name` | ✅ | — | Unique; it is the lease key, metric label and log key |
| `schedule` | ✅ | — | Five-field cron expression, 1-minute granularity |
| `command` | ✅ | — | **argv array**, not a shell string |
| `site` | — | *(siteless)* | Vhost to run as; omit for a cluster-leader job |
| `timezone` | — | `[cron].timezone` | Per-job IANA timezone override |
| `timeout` | — | `"1h"` | Max run time (`ms`/`s`/`m`/`h`/`d`) |
| `concurrency_policy` | — | `"forbid"` | `forbid` / `allow` / `replace` |
| `starting_deadline` | — | *(none)* | Catch-up window; **unset ⇒ only the current tick fires** |
| `suspend` | — | `false` | Parsed and listed, but never fires while true |

`[cron]` itself: `enabled` (off by default), `timezone`, `max_concurrent`,
`max_jobs_per_site`, and the directory tier (`directory_cron` /
`directory_cron_sites`).

## Try it

```bash
# Create the two vhost directories the per-site jobs name (fail-closed otherwise)
mkdir -p /srv/sites/blog /srv/sites/shop /srv/db

# Inspect the schedule without a running server — schedule + next fire time:
ephpm cron list -c ephpm.toml
ephpm cron list -c ephpm.toml --json      # machine-readable

# Start the server (the scheduler runs inside `serve`):
ephpm serve -c ephpm.toml

# Live scheduler state from the running server (GET /_ephpm/cron):
ephpm cron status -c ephpm.toml

# Fire a job right now (still honours the per-tick lease):
ephpm cron run-now wp-cron -c ephpm.toml
ephpm cron run-now wp-cron -c ephpm.toml --force   # true out-of-band, bypass the lease
```

`status` and `run-now` talk to the running server's internal endpoints
(`GET /_ephpm/cron`, `POST /_ephpm/cron/run/<name>`), which are gated like the
other `/_ephpm/*` control endpoints — set `[server.diagnostics] token` (or mTLS).

## Clustering — exactly-once, with a caveat

Run the same config on every node of a cluster. Each job runs on exactly one node:

- **per-site** jobs run on the site's **rendezvous-hash (HRW) owner** — the same
  node that owns that tenant's database, so the write stays local;
- **siteless** jobs run on the **lowest-ordinal alive node** (the leader).

A per-tick KV lease (`cron:lease:<job>:<minute>`) gates duplicate fires. The
guarantee is **exactly-once while cluster membership is stable and converged,
and at-least-once across membership changes** — during a partition or a
join/leave mid-tick, two nodes with divergent views of who's alive can each
fire. **Write your jobs to be idempotent.** Single-node mode (no `[cluster]`) is
trivially exactly-once: the one node owns everything. Clustered cron is
**experimental**.

## Migrating from Kubernetes `CronJob`

The full guide includes a `CronJob` → `[[cron.jobs]]` mapping table
(`schedule`, `.spec.timeZone` → `timezone`, the coordination Lease → per-tick KV
lease, `startingDeadlineSeconds` → `starting_deadline`, `concurrencyPolicy` →
`concurrency_policy`, `suspend` → `suspend`, `activeDeadlineSeconds` → `timeout`,
`kubectl create job --from` → `ephpm cron run-now`):

- Guide: `site/content/guides/cron-and-scheduled-scripts.md`
- Config reference: the `[cron]` / `[[cron.jobs]]` sections of `site/content/reference/config.md`

+++
title = "Distributed Per-Site Cron"
weight = 40
+++

> **Status: implemented for v0.16.0; clustered mode is experimental
> (lab-gated).** The scheduler, the two-layer exactly-once model, the operator
> and tenant job tiers, the `ephpm cron` CLI, the `/_ephpm/cron` endpoints and
> the metrics all exist in the binary (`[cron]` config in
> `crates/ephpm-config`, the scheduler in
> `crates/ephpm-server/src/cron_scheduler/`). Single-node firing and the
> exactly-once decision/gate/lease logic are covered by unit and integration
> tests that run in CI. **Multi-node cluster behaviour is proven at the
> integration level but has not been validated on a live multi-node cluster**
> (see [§9](#9-testing)); treat clustered cron as experimental, the same bar as
> clustered Turso replication. This page is the frozen design; it supersedes the
> earlier `clustered-cron.md` proposal, whose six open questions are now locked
> here.

## 1. Why this exists

ePHPm already runs scheduled work through an external scheduler (a crontab line,
a systemd timer, or a Kubernetes `CronJob`) calling
[`ephpm php --site`](/guides/cron-and-scheduled-scripts/) or
[`ephpm exec --site`](/roadmap/ephpm-exec-sandboxed-vhost/). That is the right
*execution* layer, but it leaves three gaps only ePHPm itself can close, because
only ePHPm knows the cluster's membership and who owns each site:

1. **No clustered single-run guarantee.** The same crontab line on three nodes
   fires three times. Operators migrating off Kubernetes `CronJob` (one run,
   guaranteed) have no equivalent without hand-rolling a distributed lock.
2. **No per-site fan-out.** In per-site clustered mode each site has an owner
   node; a job that touches a site's database should run *there*, where the data
   is local, and should move when the owner moves. An external scheduler cannot
   track that.
3. **No tenant self-service.** On shared hosting a tenant cannot edit
   `ephpm.toml`, so they cannot schedule anything without the operator wiring a
   crontab line per tenant.

The thesis mirrors `ephpm exec`: **ePHPm already decides, per request, who owns
a site and whether this node leads.** A native scheduler asks whether that same
ownership decision can gate a timer — and it reuses the primitives that already
exist (`hrw_owner`, the gossip primary election, the clustered KV's atomic
`SET … NX`).

## 2. Config schema

Two tiers, both honouring the same gate + lease.

### 2.1 Operator tier — `[cron]` + `[[cron.jobs]]`

```toml
[cron]
enabled        = true          # master switch; false ⇒ no scheduler runs
timezone       = "UTC"         # default IANA tz for jobs that omit one
max_concurrent = 4             # node-wide cap on simultaneously-running jobs

# Tenant directory tier (§2.2) — off unless opted in:
directory_cron       = false   # enable .ephpm/cron/ for every site
directory_cron_sites = []      # …or only these site keys
max_jobs_per_site    = 20      # cap on directory jobs discovered per site
# optional cadence-clock overrides (5-field cron), defaults shown in §2.2:
# directory_hourly / directory_daily / directory_weekly / directory_monthly

[[cron.jobs]]
name     = "catalog-refresh"   # unique; the lease key, metric label, log key
site     = "wholesale"         # OPTIONAL. present → runs as that vhost on the
                               #   site's HRW owner. absent → "siteless":
                               #   runs on the elected cluster leader only.
schedule = "0 3 * * *"         # five-field cron, one-minute granularity
timezone = "America/Chicago"   # optional per-job override of [cron].timezone
command  = ["bin/cake.php", "catalog", "refresh"]   # argv (never a shell string)
timeout  = "10m"               # wall-clock cap (ms/s/m/h/d); default "1h"
concurrency_policy = "forbid"  # forbid (default) | allow | replace
starting_deadline  = null      # unset (default) ⇒ NO catch-up; set ⇒ see §4
suspend  = false               # Kubernetes `suspend`: parsed, never fires
```

**Command routing** is by the argv's first element and the `site`:

| `site` | first element | runs as |
|--------|---------------|---------|
| set | ends in `.php` | `ephpm php --site <key> --config <path> -- <argv>` (DB bridge bound) |
| set | anything else | `ephpm exec --site <key> --config <path> -- <argv>` (tenant sandbox) |
| unset | anything (a `.php` first element is a **config error**) | the command spawned **directly** on the leader |

> **Note — siteless execution.** `ephpm exec` is a Linux-only *tenant* sandbox
> and requires a `--site`. A siteless (cluster-global) job has no tenant to
> sandbox, so the scheduler spawns its command **directly** on the leader
> (operator-trust, exactly as a systemd timer would), still supervising the
> child, its timeout and its kill switch. This differs from running it through
> `ephpm exec` but produces the same result — a plain binary on one node — and
> keeps the security-critical sandbox path untouched. `setrlimit`/cgroup
> confinement for these jobs is a documented follow-up ([§8](#8-follow-ups)).

**Validation** (fail-closed, at startup, `Config::validate`): unique names;
valid five-field cron; valid IANA timezone; `max_concurrent >= 1`; a `.php`
command with no `site` is rejected; a `site` (or the directory tier) requires
`[server] sites_dir` and the named vhost directory to exist; `timeout` must be
non-zero; `concurrency_policy` must be one of the three. Unknown keys under
`[cron]`/`[[cron.jobs]]` are a startup error. The one-minute floor is inherent —
five-field cron cannot express a sub-minute cadence.

### 2.2 Tenant tier — a per-site `.ephpm/cron/` directory

Self-service cron for shared hosting, **opt-in per site by the operator**
(`directory_cron` for all sites, or `directory_cron_sites` for specific ones; a
tenant can never self-enable). A tenant drops files into
`<site-root>/.ephpm/cron/`, re-scanned **live each tick** (no restart); the
`.ephpm/` prefix keeps it out of the document root and under the hidden-file
block. **PHP-only** — every file runs as `ephpm php --site <that site>`; a
tenant who can already run arbitrary PHP in its app gains nothing new.

* **Cadence subdirectories** — `hourly/ daily/ weekly/ monthly/`; every `*.php`
  runs on that cadence at the fixed clock (**hourly `:00`, daily `00:00`, weekly
  `Sun 00:00`, monthly `1st 00:00`**, operator-overridable via the `[cron]
  directory_*` knobs), in the site's/default timezone.
* **`jobs/`** — each `*.php` carries a header comment
  `// @schedule */15 * * * *` (and optional `// @timezone America/Chicago`;
  `#`/`*` comment prefixes also accepted). A file with no valid `@schedule` is
  ignored with a WARN.

A tenant may **tighten** (fewer files) but never loosen the operator's limits
(`max_jobs_per_site`, the one-minute floor). Directory jobs run through the
**same** gate + lease as operator jobs (the site's HRW owner), so they too fire
exactly once cluster-wide.

## 3. Exactly-once — two layers

1. **Ownership gate (primary guarantee).** A node only *considers* a job it
   owns: a site job on `hrw_owner(alive_nodes, site)`, a siteless job on the
   elected leader (lowest-ordinal alive, non-draining — the single-database
   primary rule). Single-node owns everything. **Fail-closed:** a node that
   cannot confirm ownership does not run (skip + WARN + `not_owner` metric).
2. **Per-tick lease (handoff backstop).** Before executing, a node acquires
   `cron:lease:<job>:<tick_ts>` in the clustered KV via the atomic conditional
   set (`SET key token NX EX ttl`, TTL ≈ `timeout` + margin). During a brief
   ownership-move overlap where two nodes both pass the gate, exactly one wins
   the tick. **Fail-closed** on an unreachable KV owner (skip, never "assume
   free"). The lease is **not released on success** — it TTLs out, so a
   crash-restart in the same tick cannot re-run it and a peer that has not yet
   seen the advanced cursor cannot steal it (at-most-once bias).

**Honest limit.** This is **exactly-once under stable membership and across
clean ownership moves; at-most-once-biased (skip on uncertainty) during a
partition.** A gossip KV cannot offer linearizable compare-and-set, so a network
partition that splits the lease owner from a would-be runner makes the runner
*skip*, not double-fire. That is the deliberate trade.

## 4. Scheduler mechanism (Kubernetes `CronJob` port)

Tick granularity is one minute, aligned to the wall-clock minute boundary. Each
tick, for each owned job, the scheduler evaluates the half-open window
`(lastScheduleTime, now]` and resolves the schedule in its timezone to **UTC
instants** (`tick_ts`), identical on every node — which is what the lease key is
built from.

* **DST** (mirrors Kubernetes 1.27+ `.spec.timeZone`): a spring-forward
  nonexistent local time is **skipped**; a fall-back ambiguous local time
  **runs once**, at the earliest occurrence.
* **Catch-up:** `starting_deadline` **unset (default) ⇒ no catch-up** — only a
  tick that is the current minute fires; an older missed tick is skipped (cursor
  advances past it). **Set ⇒** the single most-recent missed tick within the
  deadline fires; older ones are skipped.
* **The >100 valve:** more than 100 missed schedules in the window ⇒ give up on
  catch-up, log ERROR + `missed_valve` metric, jump the cursor to now, fire
  nothing. No stampede.
* **`lastScheduleTime`** lives in the clustered KV (`cron:state:<job>`), written
  on a successful start, so an ownership move resumes where the old owner left
  off. A lost cursor is re-derived as "window starts now" (safe; skips catch-up).
* **`concurrency_policy`:** `forbid` (default) skips while a previous run is
  active; `allow` runs regardless; `replace` terminates the running instance
  (SIGTERM → grace → SIGKILL) then starts. `max_concurrent` caps simultaneous
  runs per node (a due job at the cap is skipped, like a traditional cron).
* **`suspend = true`** never evaluates or fires.

**Mid-run ownership move:** the in-flight job finishes on the old owner (bounded
by its timeout); the lease stops the new owner re-firing that tick; the next
tick goes to the new owner, which reads the shared cursor and resumes. In
per-site clustered mode a finishing job's DB writes forward to the new owner via
the existing [`sql_forward`](/roadmap/) non-owner write path — no new machinery.

## 5. Execution

A cron fire is a child process the scheduler times, gates and leases — never a
run inside ePHPm's own PHP-linked, multi-threaded runtime. stdout/stderr are
inherited to the server's logs. The parent enforces the timeout (SIGTERM →
10 s grace → SIGKILL on Unix; terminate on other platforms). Platform: the
per-site sandboxed form is Linux-only (inherited from `ephpm exec`); the
scheduler still fires and leases on every platform.

## 6. CLI & observability

`ephpm cron list` (configured jobs, schedule, timezone, next fire, site —
standalone, no server needed), `ephpm cron status` (`GET /_ephpm/cron` on a
running server), `ephpm cron run-now <name>` (`POST /_ephpm/cron/run/<name>` —
**takes the per-tick lease** so it cannot double-fire with the scheduled run;
`--force` bypasses the lease for a true ad-hoc run).

**Endpoints:** `GET /_ephpm/cron` (status JSON, gated like `/_ephpm/db/backup`:
read-only, follows `[server.metrics] listen`); `POST /_ephpm/cron/run/<name>`
(credential required on **every** node — `[server.diagnostics] token` or mTLS —
since it runs a command).

**Metrics** (`metrics` facade, labelled `job` + `site` + `tier`):

| Metric | What |
|--------|------|
| `ephpm_cron_runs_started_total{job,site,tier}` | runs started |
| `ephpm_cron_runs_total{job,site,tier,outcome}` | runs finished — `outcome` ∈ `ok`/`failed`/`timeout`/`killed`/`spawn_error` |
| `ephpm_cron_duration_seconds{job,site}` | run wall-clock duration |
| `ephpm_cron_skipped_total{job,site,reason}` | skips — `reason` ∈ `overlap`/`lease_lost`/`not_owner`/`missed_valve`/`at_cap` (its presence on a node is how exactly-once coverage is confirmed) |
| `ephpm_cron_last_schedule_timestamp_seconds{job,site}` | last fired tick (unix seconds) |

## 7. Security

* **Operator tier = config-file trust.** A `[[cron.jobs]]` entry runs arbitrary
  commands as any tenant; the boundary is identical to `ephpm exec` — whoever
  can write `ephpm.toml` can already impersonate any tenant. Every fire is
  logged (job, site, tier, node, `tick_ts`).
* **Tenant tier = zero new privilege.** A `.ephpm/cron/` job runs only in its
  own tenant's sandbox with its own credentials, behind the operator opt-in and
  the `[cron]` resource limits; the directory is never web-served.
* **Lease/state keys** (`cron:lease:*`, `cron:state:*`) live in the cluster
  coordination keyspace, not a tenant's per-site store, so a tenant's own KV
  access cannot forge or steal a job's lease.

## 8. Follow-ups

* `setrlimit` (`RLIMIT_AS`/`RLIMIT_CPU`) and a transient cgroup per run — the
  same follow-up `ephpm exec` defers; a recurring fleet is a stronger reason to
  land them. **Planned — not yet implemented.**
* Live multi-node cluster validation (below).

## 9. Testing

Covered by unit + integration tests that run in CI (deterministic, with an
injected clock and membership — no wall-clock sleeps on the decision path):

1. **Single-node** — a job fires on schedule and its command executes
   (observable side effect asserted).
2. **Clustered exactly-once (stable membership)** — a per-site job fires exactly
   once across a two-node membership (the HRW owner), the other node records
   `not_owner`.
3. **Exactly-once during an ownership-move overlap** — when both nodes believe
   they own the site, the shared per-tick lease still yields exactly one fire.
4. **Clean ownership move** — when the owner drains, the next tick fires exactly
   once on the new owner — no double, no drop.
5. **Directory tier** — a dropped `daily/x.php` and a `jobs/y.php` with a header
   are discovered live and scheduled; the operator opt-in gates discovery.
6. **Unit** — DST skip/ambiguous resolution, the catch-up/no-catch-up/>100
   valve logic, the concurrency policies, and all schedule/tz/command/site
   validation.

**Live multi-node validation is lab-pending** (the same bar as clustered Turso
replication). The manual lab step: start ≥ 2 ePHPm nodes with `[cluster]
enabled = true`, `[db.sqlite] dir` + `[db.sqlite.replication] per_site = true`,
and a `[[cron.jobs]]` with `site = "<a site>"` and `schedule = "* * * * *"`
whose command appends to a per-site marker; after several minutes confirm the
marker gained exactly one line per minute cluster-wide, and that
`ephpm_cron_skipped_total{reason="not_owner"}` advances on the non-owner nodes.
Kill the owner and confirm the marker keeps gaining exactly one line per minute
on the new owner.

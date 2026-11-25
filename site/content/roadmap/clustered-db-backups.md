+++
title = "Clustered database backups"
type = "docs"
weight = 24
+++

> **Status: shipped in v0.16.1.** This page is the design note the
> clustered-backup work was built against; it is kept as the architecture
> reference. Clustered embedded-SQLite mode is itself **experimental** (Turso is
> Beta upstream, per-site CDC layers on top), so clustered backup inherits that
> label — whole-database clustered backup is supported, per-site clustered
> backup is experimental (see below). Operational usage — including single-node
> and per-site backups — is in the
> [Database backups guide](/guides/database-backups/) and
> [Engines → Backups](/architecture/database/engines/#supported-backups).

## Maturity, up front

Separate the two halves. **Taking the backup** — snapshotting the right database
from exactly one node, exactly once per interval, into a valid standalone
artifact — is the supported, shipped part (whole-database) / experimental part
(per-site). **Restoring *into* a running cluster** is experimental for **both**
topologies, because of a correction to the original design (see the box below):
a replica does **not** auto-converge onto a restored node.

> **Correction (v0.16.1).** This note was written assuming that a restored
> node's **fresh log identity** would make every replica "detect a different log
> id → re-bootstrap → converge". That is **not** how the engine behaves. A
> replica that still holds its old database treats a new log id as an ordinary
> **failover** (`maybe_bootstrap_cold_replica` re-bootstraps only a *cold*
> replica; a non-owner per-site replica returns `TailOnly` while a lineage is
> recorded), so it keeps its old rows and tails the restored log on top of them.
> Convergence therefore requires **manually wiping every other node's `.db`**
> before restart — or the `--reseed` flag, which automates exactly that: for
> whole-database clustered restores since **v0.16.2**, and for per-site clustered
> restores since **v0.20.0** (#707; see below). The passages
> below that describe "log-id change → proven divergence → re-bootstrap" are
> retained as the *original design intent*; the implemented behaviour is the
> correction here.

- **Whole-database clustered backup — supported; restore experimental.** The
  snapshot is deterministic and exactly-once. **Since v0.16.2, `ephpm db restore
  --reseed` converges the cluster automatically:** it stamps a monotonic reseed
  generation into the restored image, which the restored node advertises in its
  CDC `Hello`/snapshot header; every replica that sees a newer generation
  force-discards its own database and cold re-bootstraps from the restored node.
  The procedure is then: stop every node, `ephpm db restore --reseed` on the
  lowest-ordinal node, start it **first**, then the others (which auto-converge),
  and verify each via `ephpm_cdc_bootstrap_total{outcome="ok"}`. `--reseed`
  requires **every node on v0.16.2+** — an older replica ignores the signal and
  stays divergent (fail-safe, not corruption). Without `--reseed` the original
  manual procedure still applies: wipe every other node's `.db`/`-wal`/`-shm`
  before restart.
- **Per-site clustered backup — EXPERIMENTAL; restore converges with `--reseed`
  since v0.20.0 (#707).** The backup half snapshots each site once by its HRW
  owner (`source = "replica"` is not yet supported here and falls back to the
  owner with a `WARN`). **`ephpm db restore --reseed --site <key>` now converges
  a per-site clustered deployment automatically**, the same way the whole-database
  path does: the restore stamps a per-site reseed generation into the site's
  `.db`, the site's owner advertises it, and it **dominates the per-site
  `SiteVersion` order** — so a peer that still holds the site force-discards it
  and cold re-bootstraps from the owner, and the former `SiteVersion`
  epoch-dominance hazard (a stale higher-epoch peer silently overwriting the
  restore) is prevented in the ownership-handoff pull itself — the pull is ranked
  by durable version and a discard is only ever justified by a strictly newer
  copy (#707; implemented and unit-tested, **not yet live-cluster-validated** —
  see [Validation boundary](#validation-boundary)). The procedure mirrors
  whole-database: stop every node, `ephpm db restore --reseed --site <key>` on one
  node, start it **first**, then the others. Requires **every node on v0.20.0+**;
  until a node is upgraded to v0.20.0 and the reseed re-run it does not honor the
  signal, and if it is the site's HRW owner it **actively serves its stale copy**
  (fail-safe against corruption, but divergent). Without `--reseed` the
  manual wipe procedure still applies. Reachable only through the experimental
  `[db.sqlite.replication] per_site = true` opt-in; both **startup** and **`ephpm
  db restore`** still warn that per-site clustering is experimental.

## Where we are

Scheduled backups ([`[db.sqlite.backup]`](/reference/config/#dbsqlitebackup),
`crates/ephpm-server/src/db_backup.rs`) snapshot every embedded database with
the engine's own memory-bounded `VACUUM INTO`, gzip it, and upload it to object
storage; `ephpm db restore` installs one offline after a
`PRAGMA integrity_check`. This works in **single-node** and **per-site (single)**
modes only. In any clustered mode `Config::validate` **refuses** `enabled = true`
at startup, and `ephpm db restore` refuses a clustered config, for two concrete
reasons:

1. **No per-node scheduling.** Every node holds a copy of every clustered
   database (whole-DB clustered) or of the sites it serves (per-site clustered).
   With no owner/primary gate, every node would run `VACUUM INTO` on every
   database and upload it once per interval — N× the work, N× the object churn,
   N copies racing for the same object key.
2. **The artifact carries replication bookkeeping a restore cannot reconcile.**
   A `VACUUM INTO` is a *physical page image*, so it captures every table in the
   file — including the CDC bookkeeping tables (below) that only exist in
   clustered mode. Installing such an image back into a cluster re-introduces a
   stale log identity and a stale watermark, which makes a peer believe it is
   already caught up and silently diverge.

This note decides how to close both.

## The CDC bookkeeping a clustered `.db` carries

Every table below is a real SQL table created with `CREATE TABLE IF NOT EXISTS`
on the site's own `.db` file (`crates/ephpm-server/src/turso_cdc.rs`), so a
physical `VACUUM INTO` snapshot captures all of them. A single-node database has
none of them.

| Table | Purpose | Modes |
|---|---|---|
| `__ephpm_cdc_log_id` | This database file's own CDC log identity — 128-bit CSPRNG, generated once per file (`ensure_log_id`). | whole-DB + per-site |
| `__ephpm_cdc_source` | Which log the local watermark counts against — this database's **data lineage** (`site_data_lineage`). | whole-DB + per-site |
| `__ephpm_cdc_watermarks` | Per-source cursor history, saved across re-promotion cycles. | whole-DB + per-site |
| `__ephpm_cdc_epoch` | This site's ownership **epoch** (#654) — the high half of `SiteVersion`. | **per-site only** |
| `__litewire_cdc_watermark` | litewire's applied-change cursor for the current source log. | whole-DB + per-site |
| `turso_cdc` | The change-capture log itself; one row per captured local write. Grows on the owner/primary, stays empty on a replica (capture is off there). | whole-DB + per-site |
| `__turso_internal_seq_*` | Turso's own autoincrement backing tables for `turso_cdc`. **Engine-managed — valid physical state that must be preserved.** | whole-DB + per-site |

Two facts from this table drive the whole restore design:

- A watermark (`__litewire_cdc_watermark.applied_change_id`) is only meaningful
  **against the specific `__ephpm_cdc_log_id` it was read from**. `turso_cdc.change_id`
  is a per-database, per-ownership-stint sequence that resets each stint (issue
  #315). Carry a watermark across a restore without resetting the log identity
  and the number now points into a *different* log — the silent-divergence bug.
- The logical snapshot path already exists and already excludes exactly these
  tables — see below — but it is **not** a backup artifact (it drops triggers on
  purpose).

## Problem 1 — which node backs up (scheduling)

The ownership primitives already exist; the scheduler only has to consult them.
`ClusterHandle::nodes()` returns the membership view, `ephpm_cluster::hrw_owner(alive, site)`
is the per-site rendezvous-hash owner, and the whole-DB primary is the
`Arc<AtomicBool> primary_view` the CDC election already flips on every role
change (`crates/ephpm-server/src/turso_cdc.rs`). There is no "am I the owner?"
helper — the serving path recomputes HRW ad hoc each time (`settle_round`,
`sql_forward::plan_serve`), and the backup scheduler does the same.

**Each tick, the scheduler filters its candidate databases by responsibility
before taking a WAL-gate permit.** The candidate set is unchanged from today:
the one `[db.sqlite] path` in whole-DB mode, every `<dir>/<site>.db` on disk in
per-site mode (`enumerate_site_databases`). A node only ever considers databases
whose file it actually holds, so a per-site node never tries to back up a site
it does not replicate.

| Mode | `source = "primary"` (default) | `source = "replica"` |
|---|---|---|
| Whole-DB clustered | Back up iff this node is the current single-DB **primary** (`primary_view == true`). | Back up iff this node is the **lowest-ordinal alive non-primary** node. |
| Per-site clustered | For each local site, back up iff `hrw_owner(alive, site)` is **this node**. | For each local site, back up iff this node is the **highest-HRW-scoring alive holder that is not the owner** (the 2nd-ranked HRW node). |

Fallback: when `source = "replica"` finds no eligible non-owner holder (a
single-node-in-a-cluster, or a site only one node holds), the **owner/primary
backs it up** — coverage never silently drops to zero because a replica was
asked for and none exists. This fallback is logged.

**Exactly-once, and what breaks it.** Ownership is a pure function of the same
membership view on every node, so in steady state exactly one node satisfies the
predicate per database → exactly-once coverage, RPO = `interval`. A membership
change (a join is enough to move HRW; a primary failover) can make two nodes
briefly agree they are responsible, or make a freshly-promoted owner skip a tick
it is not yet settled for. The bounds:

- **Duplicate:** two nodes snapshot the same database in one tick → two objects
  with near-identical timestamps. Object keys are newest-wins and retention
  always keeps the newest, so a duplicate is harmless churn, not a correctness
  problem.
- **Gap:** a new owner that is still bootstrapping (its serving gate is
  `Blocked`/`Pending`) skips the tick. The next tick covers it. Worst-case RPO
  during a move is therefore `interval + one tick`, not unbounded.

The scheduler emits a tick summary at `DEBUG` and a counter
(`ephpm_db_backup_skipped_total{site,reason}` with `reason` ∈ `not_primary`
(whole-DB primary mode), `not_replica` (whole-DB replica mode), `not_owner`
(per-site), and `empty_snapshot`) so an operator can confirm each database is
backed up by exactly one node and see the skip reason on every other node.
`BACKUP_CONCURRENCY` (4) is unchanged; a per-site owner that owns many sites
still processes at most four at once. The scheduler gates on **ownership only**,
not the serving gate — so a not-yet-bootstrapped new owner (or a cold whole-DB
replica) *is* ownership-responsible and would otherwise snapshot an empty file.
Its on-disk `.db` exists (the mgmt registry creates it on announce), so the
snapshot is taken but the **staged artifact is checked for user tables and an
empty one is skipped** with `reason = empty_snapshot` — never uploaded, so an
empty image can never become the newest object and the last good backup stays.
This is the "gap" above, bounded by one tick.

A node that has announced a graceful drain (`NodeInfo::draining`) is already
skipped by `hrw_owner`, so a draining node stops being backup-responsible the
moment it announces, and its sites' new owners pick them up — the same re-homing
the serving path uses.

## Problem 2 — restore reconciliation (the crux)

A clustered backup is useless if it cannot be restored correctly, so this is the
decision the whole feature turns on. The hazard, stated once more precisely: a
physical `VACUUM INTO` image of a clustered database carries `__ephpm_cdc_log_id`,
`__ephpm_cdc_source`, `__litewire_cdc_watermark`, `__ephpm_cdc_watermarks`,
`__ephpm_cdc_epoch` and `turso_cdc`. If that image is installed back into a
cluster unchanged, the node reopens it, keeps the **old** log id (`ensure_log_id`
only generates one when absent), and keeps a watermark that says "I have applied
up to change N of log L". A peer tailing CDC then sees a log id and watermark it
recognizes, concludes nothing needs re-bootstrapping, and the two copies diverge
with every health check green. That is the exact failure the startup refusal
exists to prevent.

### Options considered

**(A) Strip the CDC bookkeeping on restore.** Keep the `VACUUM INTO` physical
artifact (one format for every mode). On restore, after gunzip and
`integrity_check` and before the install, drop the CDC/replication bookkeeping
tables so the file is reduced to clean user data with **no** replication
identity. When the cluster next opens it, `ensure_log_id` mints a **fresh** log
id, `site_data_lineage` finds no recorded source and stamps the node as the
site's **origin**, and any peer still holding the old copy sees a different log
id → proven divergence → re-bootstrap from the restored data → convergence.

**(B) Capture a clean logical dump at backup time** (schema + data excluding the
bookkeeping) instead of `VACUUM INTO`. The cold-replica/handoff path already
produces exactly this (`produce_snapshot`), and it excludes the bookkeeping for
free via `is_internal_object`.

**(C) Rewrite lineage on restore** — keep the physical image but overwrite
`__ephpm_cdc_log_id` with a fresh value and clear the watermark/`turso_cdc`.

### Recommendation: (A), strip on restore

Option **A** is the recommendation. The reasoning, against the invariants in
`turso_cdc.rs`/`handoff_gate.rs`:

- **It keeps one artifact format.** A clustered backup is byte-for-byte the same
  kind of object a single-node backup is — a gzipped standard SQLite file that
  `sqlite3` opens with no ePHPm, **triggers included**. `db backup list`,
  retention, the object layout, the memory-bounded batched `VACUUM INTO`, and
  the plaintext-plus-bucket-encryption model are all unchanged. Only the restore
  does something extra, and only when the image turns out to carry bookkeeping.
- **It reduces every mode to one restore rule.** After the strip, a clustered
  image is shaped exactly like a single-node file: user data, no replication
  identity. The restore then has a single reconciliation rule for all four
  modes, and the convergence is handled by machinery that already exists and is
  already tested (fresh-log-id-forces-rebootstrap; `SiteVersion` epoch
  dominance), not by new restore-time cluster logic.
- **It is the only option that forces convergence rather than risking silent
  divergence.** The fresh log id is what makes a peer's stale cursor
  *un-applicable* to the restored data (the issue #315 design) and so *mandates*
  a re-bootstrap. Keeping the old identity (do-nothing) is the divergence bug;
  rewriting it by hand (C) is strictly more fragile than letting the normal
  cold-open path regenerate it.

Why not **B**: the logical dump **drops triggers on purpose** — a replica must
not re-fire a trigger whose row effects CDC already ships. A *backup* must carry
triggers; dropping them makes it a replica-bootstrap, not a backup. It is also a
second artifact format (SQL text, not a `.db`) that `sqlite3` cannot open as a
file and that `db restore`'s existing physical-install path would have to special-case.
The Tier-0 design already rejected the logical dump as a backup artifact for
exactly this reason.

Why not **C**: it is A with a sharper knife and no upside. Letting the file come
up with the bookkeeping *absent* and regenerate through the standard cold-open
path exercises code that already runs on every fresh site; hand-writing specific
replacement values exercises a bespoke path that must be kept in lockstep with
the bookkeeping schema forever.

### The strip must be a curated list, not the `__` predicate

The logical dump excludes tables with `is_internal_object`:
`name.starts_with("sqlite_") || name.starts_with("__") || name == "turso_cdc"`.
The restore strip **must not** reuse that predicate, because a physical image
legitimately contains `__turso_internal_seq_*` (Turso's autoincrement backing
tables) which are **valid engine state that must be preserved** — dropping them
corrupts AUTOINCREMENT columns. The strip is the explicit CDC/replication set:

```
turso_cdc
__ephpm_cdc_log_id
__ephpm_cdc_source
__ephpm_cdc_watermarks
__ephpm_cdc_epoch
__litewire_cdc_watermark
```

These names already exist as constants in `turso_cdc.rs` (`LOG_ID_TABLE`,
`SOURCE_LOG_TABLE`, `LOG_WATERMARKS_TABLE`, `EPOCH_TABLE`, `WATERMARK_TABLE`).
The implementation exports one shared `CDC_BOOKKEEPING_TABLES: &[&str]` from
`turso_cdc.rs` so the capture side and the restore-strip side can never drift,
and the strip is `DROP TABLE IF EXISTS` for each, with CDC capture disabled
first so `turso_cdc` can be dropped. `__turso_internal_*` and `sqlite_*` are
deliberately left alone.

### Where convergence comes from, and the one hazard

Stripping makes the restored file a clean origin. It does **not** by itself
guarantee the *cluster* converges onto it, because of how a replica treats local
data (`plan_replica_attempt`): a node that holds data with a self-stamped
lineage and no *proven* divergence does `TailOnly` — it tails the owner on top
of its own data, destroying nothing. So if the restored data lands on a node
that ends up a **non-owner**, and some other node (still holding the old copy)
becomes owner, the restored node tails the old owner and **the restore is
silently lost.**

The restored data must therefore end up on the node that **serves the site as
owner**, *and* every other node must be prevented from tailing the restored log
on top of its old rows. Stripping + ownership placement is not enough on its own
— see the correction box at the top: a replica that still holds old data
`TailOnly`s rather than re-bootstraps. The operating procedure must also **wipe
the replicas** so they come up cold:

- **Whole-DB clustered — backup supported; restore experimental.** Restore the
  stripped image on the **lowest-ordinal node** (the one the election makes
  primary) with the cluster stopped. **Since v0.16.2, pass `--reseed`:** it
  stamps a monotonic reseed generation into the image, which the restored node
  advertises in its CDC `Hello`/snapshot header. Every replica that sees a newer
  generation than it has adopted force-discards its own database and cold
  re-bootstraps from the restored node — the automatic replacement for the manual
  wipe. Start the restored node first, then the others, and verify each converged
  (`ephpm_cdc_bootstrap_total{outcome="ok"}`). `--reseed` requires every node on
  **v0.16.2+**; an older replica ignores the signal and stays divergent
  (fail-safe, not corruption). *Without* `--reseed` the manual procedure still
  applies: **delete the `.db`/`-wal`/`-shm` on every other node** so each comes
  up *cold* and `maybe_bootstrap_cold_replica` fetches the restored primary's
  snapshot — skipping that wipe leaves a replica tailing the restore on top of
  its old rows (a mix), which a later failover then loses.
- **Per-site clustered — EXPERIMENTAL; `--reseed` converges since v0.20.0
  (#707).** The site's owner after restart is `hrw_owner`, which depends on which
  nodes are alive first — not predictable offline. `ephpm db restore --reseed
  --site <key>` removes the fragility that made this lab-pending: it stamps a
  per-site **reseed generation** into the site's `.db`, and that generation is now
  the **most-significant component of `SiteVersion`** (above `epoch` and `head`).
  So convergence no longer *relies* on the restored node's epoch happening to
  dominate a returning peer's: the reseed generation dominates unconditionally.
  The site's owner advertises it (gossip `sqlite:ver:<site>` and the CDC
  `Hello`/snapshot header). The generation is the tie-break in **every** direction
  a restore could be lost, so a stale peer cannot out-rank it by ownership epoch:
  a replica that sees a newer generation force-discards and cold re-bootstraps
  from the owner (`run_site_replication` → `ReseedRequired` → `force_reset`); a
  replica that itself holds the restore refuses to tail a staler owner rather than
  merging its pre-reseed rows back; the destructive snapshot install refuses a
  lower-generation header; the ownership-handoff pull is ranked by durable version
  and installs only a copy at or above the local generation (and refuses while a
  *known-fresher* peer is unreachable); `decide_warm_settle` blocks a stale warm
  owner by reseed dominance; an owner ships no CDC/snapshot for a site it has
  not settled for serving; and a **CDC log floor** closes the last replay path — a
  node that installs the restore over its own data keeps its existing `turso_cdc`
  log (which still holds pre-switch rows), so it records the head at the switch as
  a floor and answers any subscribe below it with a re-bootstrap signal, which
  sends a replica with a stale cursor to a fresh snapshot instead of replaying that
  pre-switch history. The procedure mirrors whole-database: stop every node,
  `ephpm db restore --reseed --site <key>` on one node, start it **first**, then
  the others; verify each converged. Each converging peer records
  `ephpm_cdc_bootstrap_total{outcome="ok"}` (the per-site re-bootstrap) alongside
  `ephpm_cdc_replica_connects_total{outcome="reseed"}` and the "force-discarding
  local data and cold re-bootstrapping" log line — **not** `{outcome="handoff_ok"}`,
  which fires only on owner-settle, not on a reseeded replica's convergence. Requires
  **every node on v0.20.0+**; until a node is upgraded to v0.20.0 and the reseed
  re-run it does not honor the signal, and an older HRW owner **actively serves
  its stale copy** (fail-safe against corruption, but divergent). Still reachable
  only through the experimental `[db.sqlite.replication] per_site = true` opt-in,
  and startup plus `ephpm db restore` still warn that per-site clustering is
  experimental; multi-node ownership churn under reseed is **unit-tested only and
  not yet live-cluster-validated** — the `reseed_converges_a_running_cluster` e2e
  harness covers the **whole-database** path only (see the Validation boundary
  below), and per-site has no live-cluster or long-run lab coverage yet.
  *Without* `--reseed` the manual wipe procedure still applies, and the
  `SiteVersion` epoch-dominance hazard it carried remains — use `--reseed`.

### Restore stays offline, and says what it does

`ephpm db restore` keeps its offline guarantee: it refuses while a local
listener answers. For a clustered restore the honest limitation is that the tool
can only probe the **local** node — it cannot verify the rest of the cluster is
stopped — so the confirmation prompt states plainly that a clustered restore
**re-seeds the whole cluster** and requires every node stopped, and the docs make
the whole-cluster stop a hard precondition. The `integrity_check` and
"a corrupt artifact never touches the live file" guarantees are unchanged; the
strip happens on the staged temp file, after verification, before the rename.

## Stability interactions

- **No deadlock with the CDC tailer.** The backup takes the WAL-gate's *shared*
  permit for the `VACUUM INTO` read and releases it before gzip/upload (today's
  behavior). `CdcTailer::poll_batch` also takes a *shared* permit. Shared permits
  do not exclude each other; only the gate's *exclusive* truncate window waits
  for both. The backup's short-lived factory goes through `builder_with_gate`
  keyed by canonical path, so it shares the serving factory's gate — the
  "both factories on one file must opt in" rule is satisfied.
- **WAL pin on a write-hot writer.** A long `VACUUM INTO` read pins the WAL
  (suppresses the truncate-checkpoint for its duration), so on a write-hot
  primary or per-site owner the `-wal` grows during the backup — the same cost a
  clustered snapshot dump already pays, already documented. `source = "replica"`
  is precisely the mitigation: run the `VACUUM` I/O and the WAL pin on a holder
  that is **not** the write-critical node. For a per-site owner — which serves
  forwarded writes from the whole cluster — that offload is most valuable, which
  is why `source` is a single knob that applies to both topologies.
- **No new DB-concurrency cap.** The backup uses the same blocking-pool and
  `offload::engine` discipline as today; it adds no semaphore that could
  interact with the engine's commit path.

## Config surface

One new knob under the existing section, `#[serde(deny_unknown_fields)]` like
every config struct:

```toml
[db.sqlite.backup]
enabled = true
s3_bucket = "ephpm-backups"
source  = "primary"   # "primary" (default) | "replica"
```

- `source = "primary"` (default) — back up from the write node (whole-DB:
  primary; per-site: each site's owner). Freshest RPO; the VACUUM I/O and WAL pin
  land on the write-critical node.
- `source = "replica"` — back up from a non-write holder (whole-DB: a non-primary
  node; per-site: a non-owner holder). RPO = `interval + replication lag`, and
  the write path is spared the VACUUM I/O and WAL pin.

Parsed as an enum, validated at startup (a typo is a hard error naming the
accepted values, like every other mode knob), and **ignored with no effect in
single-node / per-site-single mode** (there is only one holder) — documented, not
a silent no-op. The clustered guard in `Config::validate` is relaxed to allow
`enabled = true` under clustered modes; everything else it validates (bucket,
cadence, retention, endpoint, credentials) is unchanged.

## Validation boundary

Unit tests cover: the ownership/election gating (mock membership + HRW, both
`source` values, the no-replica fallback), the strip (a clustered-shaped file —
built with the real bookkeeping tables — restores to a clean, serveable,
`integrity_check`-ok file that carries its user data and triggers but none of the
CDC tables), and config validation (clustered now allowed; `source` parsed and
rejected on a typo; ignored in single modes). These prove the mechanism, **not**
multi-node behavior.

**Must be validated multi-node in the `lab` before this ships (claim:
unit-validated + lab-pending, never cluster-validated):**

1. **Exactly-once coverage** — a 3-node whole-DB cluster and a 3-node per-site
   cluster each produce exactly one object per database per interval in steady
   state; confirm via `ephpm_db_backup_skipped_total` on the non-responsible
   nodes.
2. **Ownership-move coverage** — add a node (HRW moves some sites) and confirm
   coverage stays exactly-once after convergence, with at most one duplicate /
   one skipped interval during the move.
3. **Failover backup handoff** — kill the whole-DB primary (and a per-site
   owner) and confirm the new owner picks up backup responsibility within one
   interval, and the drained/dead node stops.
4. **A real clustered backup → restore → re-serve round trip** — write data,
   back up, change the data, stop the cluster, restore the earlier backup on the
   lowest-ordinal node, **wipe every other node's `.db`/`-wal`/`-shm`** (or, on
   whole-database clustered, run `ephpm db restore --reseed`, which automates
   that wipe), start the restored node first then the wiped peers, and confirm
   **every node** serves the restored data (each peer cold-bootstrapped; no node
   silently kept the newer copy or tailed on top of it). This is the test that
   proves the reconciliation, and it is also what `--reseed` (shipped for
   whole-database clustered in v0.16.2, and for per-site clustered in v0.20.0)
   automates; the negative case (skip the wipe → a peer serves the old+new mix)
   is the regression guard. The whole-database half of this round trip is now
   covered live by the `reseed_converges_a_running_cluster` e2e harness
   (`crates/ephpm-e2e/tests/cluster_failover.rs`).
5. **WAL behavior on a write-hot primary during backup** — a `VACUUM INTO` on a
   write-hot whole-DB primary / per-site owner does not unbound the `-wal`, and
   `source = "replica"` moves the WAL pin off the write node (measure
   `ephpm_sqlite_wal_bytes` on both).
6. **`source = "replica"` RPO + offload** — confirm replica-sourced backups lag
   the primary by ~replication lag and that the VACUUM runs on the replica.

## Open design question to resolve before GA

**Resolved in v0.20.0 (#707): per-site clustered restore no longer depends on
`SiteVersion` epoch dominance.** The original concern was that a *plain* per-site
restore relied on the restored node settling first and publishing an ownership
**epoch** that dominates the epochs returning peers bring — and if gossip had
dropped the site's `kv:sqlite:ver:<site>` before the restored node settled, a
returning peer could carry a *higher* epoch and silently overwrite the restore.
`ephpm db restore --reseed --site <key>` removes that dependency: the reseed
generation it stamps is the **most-significant component of `SiteVersion`**
(above `epoch` and `head`), so a reseeded copy out-ranks any pre-restore copy
unconditionally, whatever epoch a returning peer brings. The restored owner
advertises the generation, and every path that could re-introduce the stale data
now defers to it: replicas with an older generation force-discard and
re-bootstrap; a replica that *holds* the restore refuses to tail a staler owner
(so it never merges pre-reseed rows back); the destructive snapshot install
refuses a lower-generation header; the ownership-handoff pull (ranked by durable
version) installs only a copy at or above the local generation and refuses while a
known-fresher peer is unreachable; `decide_warm_settle` blocks a stale warm owner;
and an owner ships no CDC/snapshot for a site it has not settled. None can prefer
a stale higher-epoch peer over the reseed.
`--reseed` requires every node on **v0.20.0+**. A *plain* per-site restore still
carries the old epoch-dominance hazard and is not recommended — use `--reseed`.
Whole-DB clustered restore converges the same way via `ephpm db restore --reseed`
(shipped v0.16.2). Per-site clustered remains **experimental** (the `per_site =
true` opt-in warns at startup and at restore); its per-site reseed convergence is
implemented and unit-tested but **not yet live-cluster-validated** (the
`reseed_converges_a_running_cluster` e2e harness covers the whole-database path
only), so its restore path is best described as convergent **by design and in
unit tests** with `--reseed` — pending live-cluster validation.

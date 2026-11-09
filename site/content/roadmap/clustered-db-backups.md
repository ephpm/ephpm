+++
title = "Clustered database backups"
type = "docs"
weight = 24
+++

> **Status: design — targeted for v0.16.0.** This page is the design note the
> clustered-backup work is built against; it is reviewed before feature code
> lands. Clustered embedded-SQLite mode is itself **experimental** (Turso is
> Beta upstream, per-site CDC layers on top), so clustered backup inherits that
> label. What is *shipped* today — single-node and per-site backups — is in the
> [Database backups guide](/guides/database-backups/) and
> [Engines → Backups](/architecture/database/engines/#supported-backups).

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

The scheduler emits a per-database decision at `DEBUG` and a counter
(`ephpm_db_backup_skipped_total{reason}` with `reason` ∈ `not_primary`,
`not_owner`, `blocked`) so an operator can confirm each database is backed up by
exactly one node and see the skip reason on every other node. `BACKUP_CONCURRENCY`
(4) is unchanged; a per-site owner that owns many sites still processes at most
four at once.

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
owner**. The mechanisms that make that happen are already in the engine, and the
operating procedure is what engages them:

- **Whole-DB clustered — supported and the higher-confidence path.** Restore the
  stripped image on the **lowest-ordinal node** (the one the election makes
  primary) with the cluster stopped, then start that node first. It comes up as
  primary with a fresh log id; every replica, on reconnect, sees a source-log
  change → proven divergence → re-bootstraps from the restored primary. The
  owner is predictable (lowest `node_id`), so the procedure is deterministic.
- **Per-site clustered — supported, but convergence is epoch-dependent and
  lab-gated.** The site's owner after restart is `hrw_owner`, which depends on
  which nodes are alive first — not predictable offline. Convergence relies on
  the restored node **settling first** and publishing a dominating `SiteVersion`
  (`next_owner_epoch` bumps the epoch above every peer's). When a peer later
  becomes HRW owner of the site, `decide_warm_settle` sees the restored node's
  higher `SiteVersion` in gossip and catches up from it before serving, so the
  restored data wins. This is correct *if* the restored node's epoch dominates
  the epochs returning peers bring — which is the one cluster-durability property
  unit tests cannot prove and the lab must (see below). The belt-and-suspenders
  alternative, should the lab show epoch dominance is not reliable across a
  whole-cluster restart, is a `--reseed` that removes the site's `.db` on every
  other node so they come up cold and bootstrap from the restored owner; the
  design keeps that option open but does not ship it unless the lab forces it.

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
   back up, change the data, stop the cluster, restore the earlier backup
   (whole-DB on the lowest-ordinal node; per-site per the procedure above), start
   the cluster, and confirm **every node** serves the restored data (the peers
   re-bootstrapped; no node silently kept the newer copy). This is the test that
   proves the reconciliation and the `SiteVersion`-epoch convergence for per-site.
5. **WAL behavior on a write-hot primary during backup** — a `VACUUM INTO` on a
   write-hot whole-DB primary / per-site owner does not unbound the `-wal`, and
   `source = "replica"` moves the WAL pin off the write node (measure
   `ephpm_sqlite_wal_bytes` on both).
6. **`source = "replica"` RPO + offload** — confirm replica-sourced backups lag
   the primary by ~replication lag and that the VACUUM runs on the replica.

## Open design question to resolve before GA

**Per-site clustered restore convergence depends on `SiteVersion` epoch
dominance surviving a whole-cluster restart** (open question, flagged rather than
guessed): the restored node must settle and publish an epoch that dominates the
epochs returning peers bring. If gossip has dropped the site's
`kv:sqlite:ver:<site>` before the restored node settles, it could compute a
*lower* epoch than a returning peer and lose. The lab round-trip (item 4) is the
gate on this; if it is not reliable, the fallback is the `--reseed` that
invalidates peer copies (removes their `<site>.db`) so the question does not
arise. Whole-DB clustered restore does not have this dependency (the log-id
change forces re-bootstrap directly), so it is the safer of the two and can ship
first if per-site convergence needs more lab time.

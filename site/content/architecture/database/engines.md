+++
title = "Database Engines"
weight = 2
+++

`[db.sqlite]` decides *which file* holds your data. As of v0.7.0 there is
exactly one embedded engine that reads it: the **Turso** engine.

| `engine` | What it is | Status |
|---|---|---|
| `"turso"` (default, and the only accepted value) | [Turso Database](https://github.com/tursodatabase/turso) — a ground-up Rust rewrite of SQLite (MIT) | Shipped. **Beta upstream** |

The engine is reached through the same [litewire](https://github.com/ephpm/litewire)
wire translation as before, so PHP is unaffected: `pdo_mysql` still connects to
`127.0.0.1:3306`, and the [in-process bridge](/guides/db-from-php/) still works.
The engine is invisible to application code.

## `engine` is now Turso-only

In v0.7.0 the rusqlite backend (the genuine SQLite C engine) was de-linked from
ePHPm and the sqld sidecar was removed entirely. `engine` defaults to `"turso"`
and `"turso"` is the only accepted value.

Legacy `engine = "sqlite"` or `engine = "rusqlite"` is a **hard startup error**
with a migration message — it fails closed and never silently falls back to a
different engine:

```
[db.sqlite] engine = "sqlite" was removed in v0.7.0. The rusqlite (SQLite C
engine) backend and the sqld sidecar are gone; the embedded engine is now
Turso only. Remove the engine key (or set engine = "turso"). Existing .db
files open in place — see the migration note.
```

## The Turso engine

Turso Database is Turso's ground-up Rust rewrite of SQLite, with an `async`
Rust API and an experimental MVCC mode for concurrent writers (ePHPm does
not enable it; writers are serialized by the write lock and `busy_timeout`,
as in SQLite).

The `async` API is async in name only. Each step of a statement — its VDBE
work, the page reads and writes it needs, and the `fsync` at commit — runs
as ordinary blocking calls inside `poll`; the future yields only between
steps, so a worker polling it is held for every step as long as the disk
takes. ePHPm therefore never awaits engine work on one of tokio's async
worker threads. Every engine call runs through
litewire's `offload::engine`, which moves it off the worker with
`block_in_place` before it starts (the worker's queued tasks continue on
another thread). Without that, a few dozen replicated sites' applies,
snapshot dumps and checkpoints could occupy every worker, and with no worker
free to poll sockets and timers the whole server stalled: HTTP connections,
cluster heartbeats, even the `SIGTERM` handler. PHP's `ephpm_db_*` calls
already ran the engine on the PHP thread itself and are unaffected.

One engine behaviour to know when sizing: a statement waiting for another
connection's write lock does not sleep. The engine re-polls it immediately
until the lock frees or the 5 s busy timeout expires, so each waiting
statement keeps one thread at 100% CPU for that time. Heavy write
contention on one database shows up as CPU, not as idle waiting.

ePHPm reaches it through litewire's `turso` backend, which is compiled into
every standard ePHPm build — **there is no cargo feature, sqld binary, or
special build to obtain**. A minimal single-node config:

```toml
[db.sqlite]
path = "app.db"
# engine = "turso"   # the default; may be omitted
```

Because the engine is Beta upstream, take its limits literally:

- **Beta upstream.** Turso's own positioning is Beta; it is not yet presented
  as a production SQLite replacement.
- **`VACUUM` is unsupported.**
- **Multi-process access is unsupported.** One process opens the database
  file. Do not point a second ePHPm instance, a `sqlite3` shell, or a
  file-tailing backup/replication tool (**Litestream, LiteFS**, or anything
  built for stock SQLite) at the same file while the server is running — see
  [Backups, and why external SQLite tools lock the file](#backups-and-why-external-sqlite-tools-lock-the-file)
  below. It is not a reader/writer coexistence story; the second opener and
  ePHPm fight over the file and one of them loses it.

## Opening existing SQLite / rusqlite `.db` files

The Turso engine opens existing SQLite3/rusqlite-created `.db` files **in
place** for cleanly-shut-down databases — both WAL and rollback-journal modes.
This was verified during the v0.7.0 engine swap: `PRAGMA integrity_check`
returns `ok` and rows are intact. So the normal 0.6.x → 0.7.0 upgrade of a
stopped node is seamless: **no dump/reload is required.**

Two honest caveats:

- **Shut down cleanly before upgrading.** A database left with an
  uncheckpointed hot `-wal` from a hard crash was *not* verified to replay
  through Turso. Stop the old node cleanly (which checkpoints the WAL) before
  starting the v0.7.0 binary on the same file.
- **Non-UTF-8 TEXT may not round-trip.** Turso surfaces `TEXT` as a Rust
  `String`, so cells holding invalid UTF-8 may not survive the round trip.
  This is an upstream Turso limitation.

## Turso 0.8.2 → 0.8.3

v0.13.0 moves the engine to Turso 0.8.3
([litewire#47](https://github.com/ephpm/litewire/pull/47)). This fixes a
whole-database-loss bug, so **upgrade off 0.8.2**. The file format and the CDC
log schema (`turso_cdc_version` = `v2`) did not change, so no dump/reload is
needed.

Turso 0.8.2 accepted `PRAGMA journal_mode = 'mvcc'` **inside a transaction**.
In this sequence:

```sql
BEGIN;
PRAGMA journal_mode = 'mvcc';
INSERT INTO t VALUES (1);
COMMIT;
```

`COMMIT` reported success. The file then could never be opened again
(`Missing MVCC metadata table while logical log state exists`), and every row in
it was lost. We reproduced this through ePHPm over both `pdo_mysql` and the
`ephpm_db_*` bridge. Afterwards a single-node server would not restart, and a
per-site tenant's database refused every connection. Turso 0.8.3 refuses the
switch inside a transaction (`cannot change into mvcc mode from within a
transaction`).

0.8.3 still lets a connection switch the file to the experimental MVCC mode
*outside* a transaction. So ePHPm's tenant SQL screen now refuses every
`PRAGMA journal_mode` **assignment**, in both the `= value` and `(value)` forms,
with any schema prefix or quoting, and including the harmless `= 'wal'`.
Reading the mode with `PRAGMA journal_mode` still works. The screen covers
every path that already refused `ATTACH`/`VACUUM`/path-`PRAGMA`s: the
`ephpm_db_*` bridge in every mode, the per-site `pdo_mysql` listener, and
`ephpm php --site`. The shared `pdo_mysql` listener of single-node and
whole-database clustered mode is **not** screened by ePHPm. In clustered mode
Turso itself refuses the switch while CDC capture is on (see below). On a
single-node `pdo_mysql` connection the switch outside a transaction is still
possible.

## Patched Turso 0.8.3: the page-1 allocation race and `VACUUM INTO` memory

From v0.13.0, ePHPm builds Turso 0.8.3 from a fork rather
than the crates.io release: the org mirror
[ephpm/turso](https://github.com/ephpm/turso), at tag
[`v0.8.3-ephpm.3`](https://github.com/ephpm/turso/releases/tag/v0.8.3-ephpm.3)
(branch `ephpm/v0.8.3-vacuum-harden`; pinned from v0.15.2 — v0.15.1 pinned
`v0.8.3-ephpm.2`, v0.13.0–v0.15.0 `v0.8.3-ephpm.1`) — the v0.8.3 tag plus a
backport of
[tursodatabase/turso#10117](https://github.com/tursodatabase/turso/pull/10117)
(the page-1 race below, not yet in a Turso release) plus a hardened, batched
`VACUUM INTO` target build (upstreamed as
[tursodatabase/turso#11503](https://github.com/tursodatabase/turso/pull/11503)).
The stock engine builds the whole copy in one write transaction whose commit
re-reads every spilled page and holds a WAL frame per page, so the statement
needed ~2× the database resident (1.4 GB for a 688 MB file, 11.6 GB for 5.6 GB)
and OOM-killed the scheduled backup on a 1.6 GB database. The fork builds the
copy in batches into a temp file that is durably renamed into place only after
its final checkpoint syncs — so a killed backup never leaves a half-written
`.db` — and builds each index *after* its rows rather than before (bulk-sorted,
the order SQLite itself uses; building indexes first turned every random or
UUID key into a random B-tree insert, ~3× slower on exactly the slug/UUID
shapes WordPress backups hit). The peak no longer grows with the database
size: it is bounded by `~66 MiB + 2× the largest single index` — a few tens of
MB for ordinary data, sized by the biggest index and never the file for the
rest (the operator-facing numbers are under
[memory headroom](/guides/database-backups/#memory-headroom)).
`v0.8.3-ephpm.2` was flatter still (~53 MB at every size) but built indexes
before rows; `.3` trades that flat profile for the bulk-sorted speed and for
atomic, crash-safe output. The crate versions stay 0.8.3; the swap is a
`[patch.crates-io]` entry in ePHPm's workspace `Cargo.toml`, and it goes away
once a Turso release carrying the fixes is pinned.

The bug: when several connections make the **first write to a fresh database
file** at the same time — typically a new per-site database receiving
concurrent traffic — two of them could both allocate page 1, and the engine
panicked. ePHPm catches engine panics per statement, so the process kept
running, but the affected writes failed with an SQL error. The fix holds the
engine's initialization lock across the whole page-1 write and re-checks under
it. The file format is unchanged. `crates/ephpm-server/tests/turso_page1_race.rs`
races 32 threads' first writes on 1000 fresh files; on Linux it fails against
the unpatched crates.io 0.8.3 and passes against the fork.

## WAL growth under load, and the checkpoint gate

Found by the WAL-growth stress runs of 2026-10-17/18 (ePHPm on Turso 0.8.3)
and fixed in v0.13.0 ([litewire#49](https://github.com/ephpm/litewire/pull/49)).

Turso checkpoints like SQLite: a passive auto-checkpoint copies
write-ahead-log frames into the main file once the log holds ~1000
un-backfilled frames, but the log is only ever *rewound* when a writer can
take every read-mark slot — that is, when there is no active reader at all.
The blocking checkpoint modes do not help: `PRAGMA wal_checkpoint(RESTART)`
and `(TRUNCATE)` report `busy` immediately when any reader holds a slot, and
the connection's busy handler is never consulted (Turso tries each lock once).
Under constant concurrent load some session is always inside a statement, so
the log is backfilled but never rewound and the `-wal` file grows by one frame
per write for as long as the process runs. Measured on the same harness:

- single node, 48 clients, 8 minutes, a 0.7 MB database: `-wal` reached
  **1.3 GB**, and the worst single request took 30 s;
- per-site clustered, 3 nodes, 10 minutes: the owner's `-wal` reached
  **6.5 GB**, growing linearly, because every CDC tailer is a continuous
  reader.

Idle connections hold no read slot, but nothing else rewinds the log either.
The engine has a truncating checkpoint that runs when a database's last
connection is closed explicitly, but the Turso Rust SDK has no `close()`:
dropping its handles (an LRU eviction of a per-site database, or process exit)
only releases locks and memory maps and never runs that checkpoint. So the
`-wal` file kept every frame until the process exited, and the next start had
to recover it.

Outside the gate below, ePHPm truncates logs explicitly at exit, in two
places. `ephpm serve`'s graceful shutdown runs a final
`PRAGMA wal_checkpoint(TRUNCATE)` on every database it has open. When
`ephpm php --site` opens a site database directly (offline, no server running),
it has no gate, but it runs the same checkpoint once PHP is done. It retries a
`busy` answer for up to 10 s, then exits with a WARN and leaves the log for the
next opener to recover.

**The workaround is litewire's WAL checkpoint gate**, which ePHPm installs on
every embedded-Turso factory. It is always on and **not configurable**: there
is no config section for it, and its settings (below) are fixed in the binary.
It is a stopgap — once Turso's `TRUNCATE` checkpoint can wait for readers
instead of failing `busy`, ePHPm will replace the gate with a plain periodic
`PRAGMA wal_checkpoint(TRUNCATE)`. Every
session statement — the wire listeners and the in-process `ephpm_db_*` bridge
alike — and every CDC tailer poll holds the shared side of a per-file
write-preferring lock; a statement that opens a transaction keeps its permit
until the connection is back in autocommit. A maintenance task per open
database samples the `-wal` size every 2 s and, once it is at least 64 MiB,
first backfills the log with `PRAGMA wal_checkpoint(PASSIVE)` rounds — outside
the lock, blocking nobody, reaching as far as the oldest live snapshot — and
then takes the exclusive side for at most 250 ms (new statements queue behind
it) and runs `PRAGMA wal_checkpoint(TRUNCATE)`. An open transaction that
outlives the wait makes the attempt give up and retry with exponential backoff
(capped at 60 s), so the gate never deadlocks and never stalls a long
transaction. While the log is under 4 × the threshold, the only cost it can
impose on traffic is a ≤ 250 ms delay for new statements per attempt. The
exclusive window itself is the checkpoint, which is milliseconds once the
passive phase has done the copying.

**Escalation.** That bounded wait has its own failure mode, found by a
mixed-workload stress run. 48 clients ran `COUNT`/`GROUP BY`/join reads over
a growing table, and those reads routinely took longer than 250 ms. Every
exclusive attempt timed out, the backoff stretched attempts to about one a
minute, and truncations froze at 17. The `-wal` file reached 1,225 MiB and p99
went from 241 ms to 1,045 ms. A bigger log makes reads slower, which makes
timeouts more likely: the loop the gate exists to break. So once the file is
at least 4 × the threshold (256 MiB), a timeout no longer backs off. The next
attempt, one tick later, waits twice as long (250 → 500 → 1000 → 2000 ms),
capped at 2 s. Only a timeout at that
ceiling backs off, and a truncate resets the schedule. The trade is explicit:
while an escalated attempt waits, new statements queue behind it. A request
can therefore stall for up to 2 s, though in practice only until the
statements already in flight finish, and only while the log is already
several times over its threshold. The alternative was a `-wal` file and read
latency that grow without limit.

The passive phase is not optional, and this is the detail that cost a run
during validation: a bare `TRUNCATE` inside the exclusive window has to copy
and fsync every frame the auto-checkpoint never reached (readers that
permanently pin old snapshots make that most of the 64 MiB), which stalled a
cluster owner for seconds — p999 of 4.6 s, forwarded statement streams dying
with "owner connection closed", new sessions seeing "database is locked" from
their setup pragma. Backfilling first made the window 5–7 ms and the clustered
run clean.

ePHPm's own raw-connection work — the replica's CDC `apply_batch`, snapshot
bootstrap and per-site ownership handoff, the primary's snapshot dump and head
sampler — takes the same permit explicitly (`Turso::wal_gate_permit`), because
a raw connection gets no busy timeout and a collision with the exclusive
window would otherwise fail the stream and cost a reconnect. The permit is
never held across a network wait.

**Measured with the gate on**, same harness, same commits: single node — peak
`-wal` 1,286 MiB → 68 MiB, requests served +7.9%, p99 unchanged, worst request
30 s → 1.3 s. Per-site clustered — owner's peak `-wal` 6,482 MiB → 363 MiB
(six sites × 64 MiB), requests +1%, zero errors, replicas converged; the cost
was about +100 ms at p999, from forwarded statements queueing behind an
exclusive attempt. That trade is why the gate is always on. What it does
**not** fix: a transaction that genuinely stays open keeps the WAL growing for
its duration — `ephpm_sqlite_wal_bytes` staying well over the threshold while
`ephpm_sqlite_wal_gate_timeouts_total` climbs is the operator signal. Both
series are in the
[metrics reference](/reference/metrics/#embedded-database-wal-checkpoint-gate).

## Turso 0.7.2 → 0.8.2 (v0.12.0)

v0.12.0 moves the embedded engine from Turso 0.7.2 to
0.8.2 ([ephpm#535](https://github.com/ephpm/ephpm/issues/535)). **No
dump/reload is needed, and you can roll back.** The file format did not
change, and neither did the CDC log schema: `turso_cdc` keeps its columns and
`turso_cdc_version` still reads `v2`.

What was tested, using one build on each engine version against the same
files:

- A database written and cleanly closed by 0.7.2 opens under 0.8.2.
  `PRAGMA integrity_check` returns `ok`, every row is present, and the
  table and its index agree. Writes continue, and `turso_cdc.change_id`
  carries on from where 0.7.2 stopped.
- A crash image taken while 0.7.2 still had the database open opens and
  recovers under 0.8.2. The image is the main file plus a 3.9 MB `-wal`
  that had never been checkpointed.
- The CDC log written partly by 0.7.2 and partly by 0.8.2 replays through
  the tail/apply path to an identical replica.
- Downgrade works: 0.7.2 opens files written by 0.8.2, including one it
  wrote before the upgrade, with `integrity_check` `ok`.

For clusters, the batches nodes exchange use ePHPm's own wire format, and
the `turso_cdc` rows those batches carry have the same shape on both engine
versions. Even so, a cluster running both versions at once was **not**
tested live. That is reasonable for a rolling upgrade, but do not leave a
cluster on mixed versions for long. Clustered mode is still experimental.

One behaviour changes for applications. On a session with CDC capture on,
which means every write session in clustered mode, `PRAGMA journal_mode =
mvcc` now fails with `cannot change journal_mode ... while CDC capture is
active`. Turso 0.7.2 accepted it, and according to upstream, capture then
stopped silently, so that node's later writes never replicated. Single-node
mode does not capture, so Turso itself still accepts the pragma there. Since
the 0.8.3 upgrade, ePHPm's tenant screen refuses it first on the bridge and
per-site paths (see above). Turso accepts only `wal` and `mvcc`; any other
`journal_mode` value is still a silent no-op.

## Clustered mode (experimental)

Clustered SQLite is **experimental** in v0.7.0. It uses the in-process Turso
CDC path (`turso_cdc`) over the [cluster channel](/roadmap/cluster-channel/):
ePHPm tails the primary's own change-data-capture stream and ships
per-transaction batches to replicas — **no sqld sidecar, no child process, no
gRPC**. Primary election is still via the gossip KV tier.

```toml
[db.sqlite]
path = "app.db"

[cluster]
enabled = true
```

There is no `cdc_experimental` opt-in flag anymore — clustering enables CDC
replication unconditionally. Because the Turso engine is Beta upstream and the
CDC path is manually (not yet CI-) validated, treat clustered mode as
experimental and tested on Linux/macOS.

### Primary failover and graceful drain

Replication is **asynchronous** — a write commits on the primary and ships to
replicas in the background. The whole-database primary is deterministically the
**lowest-node-id alive node**; there is no `promote`/`stepdown` command, so a
replica **auto-promotes** once the current primary's process exits and gossip
marks it dead. (Two primaries cannot coexist: lowest-id determinism plus the
incumbent-protection and log-identity guards.)

Because replication is async, a primary that is **abruptly killed** with
un-shipped or un-applied batches loses that tail — the promoted replica
continues from what it had. A **graceful drain is lossless**: as of **v0.16.1** a
draining whole-database primary waits (up to 30 s) for every attached replica to
apply its CDC head before it closes the cluster channel
(`ephpm_cdc_drain_catchup_seconds`; an `ephpm_cdc_drain_catchup_timeouts_total`
increment means a replica was still behind). So the safe way to retire a primary
— e.g. a **blue-green cutover**: add a node, let it cold-bootstrap, then retire
the old one — is to **drain it** (a normal graceful shutdown) and let a replica
promote, **not** to kill or delete it abruptly. If you will then delete the
node's volume, first confirm that drain did not increment
`ephpm_cdc_drain_catchup_timeouts_total`.

**Busy databases report as busy.** Every clustered wire factory captures its
sessions' writes into the change log, and Turso runs the capture pragma as a
*write* transaction. Until litewire#51 that pragma ran when a session opened.
While one connection held a long write transaction (a stress run used a
3,000-row import), every new session on that database waited out the 5 s busy
timeout and then failed. That covered readers, the owner side of forwarded
statements, and stock `pdo_mysql` connections. Each route mislabelled the
failure: the bridge said "failed to open embedded database session",
forwarding clients said "owner connection closed … early eof", and
`pdo_mysql` said `1698 Authenticate failed`. Capture is now switched on just
before a session's first statement that is not a plain `SELECT`. Opening a
session and reading never wait on the write lock. A write that cannot get the
lock fails with `1205` "database is locked" on every route: the bridge, a
forwarded statement (the owner answers with its error instead of closing the
stream), and the wire. A verified MySQL credential is never reported as an
authentication failure because its database is busy or cannot be opened.

**Reconnects back off, with jitter.** A replica whose CDC stream fails retries
after a random 1–3 s, then doubles the nominal delay on each consecutive failure
up to 20 s (±50 % jitter, so at most 30 s); a stream that stayed up for 30 s
starts over from 2 s, and a role change (failover) interrupts the wait. Before
this every stream retried on a fixed 2 s beat. When an owner stalled, all its
replica streams failed in the same instant and re-dialed in the same instant.
That burst overran the owner's 64-handshake admission cap, and every dropped
handshake failed another stream, which re-dialed 2 s later. A stress run with
200 sites logged 1,934 dropped handshakes in one second, and the storm continued
after the load stopped. The cap's drops are counted as
`ephpm_cluster_channel_handshakes_total{outcome="over_capacity"}`.

Metrics for this path — batches and rows shipped and applied, subscriber
count, applied watermark, reconnects, snapshot bytes, and a replication-lag
gauge measured in **change-log rows, not seconds** — are in the
[metrics reference](/reference/metrics/#cdc-native-turso-replication). The
full design, its verified findings, and everything still deferred (including
read-only enforcement on replica wire frontends and `turso_cdc` retention
pruning) are on the [Turso engine roadmap](/roadmap/turso-engine/).

## Backups, and why external SQLite tools lock the file

**Litestream, LiteFS, and every other tool built for stock SQLite are
unsupported on a Turso-backed database file, and will take the site down.**
This is not a tuning caveat; it is a hard incompatibility.

Turso is not a drop-in `sqlite3` library that happens to share a file format.
It has its **own write-ahead-log format and its own file locking**, and ePHPm's
always-on [WAL checkpoint gate](#wal-growth-under-load-and-the-checkpoint-gate)
assumes it is the only thing touching the `-wal`. A stock-SQLite tool does the
opposite of a passive reader: it opens the file as a SQLite database, forces its
own WAL mode, and holds read/write descriptors to tail the `-wal`. Pointed at a
Turso file it

1. cannot parse Turso's WAL (Litestream logs `primary wal header: EOF` and
   "clears the generation"),
2. imposes its own WAL/lock state on the file, and
3. locks ePHPm out — the next Turso open fails with
   `Locking error: Failed locking file '…'. File is locked by another process`.

On the per-site wire path that surfaces as a connection that **authenticates but
cannot be served**: ePHPm verifies the site credential, then refuses with
`could not open the database for site <key>: … File is locked by another
process` (a verified credential is never downgraded to an auth failure — see
[the clustered busy-database note](#clustered-mode-experimental)). Whichever
process opens the file first wins, so the symptom is timing-dependent: a site
whose file ePHPm opens before the sidecar may survive until the next restart,
while a larger file the sidecar snapshots first is dead from the start. Deleting
the `-wal`/`-shm` while ePHPm is stopped does not help — the sidecar re-grabs the
file on restart. There is no locking contract for an external opener; the only
supported state is *ePHPm is the sole process on the file*.

### Supported backups

**Built in: scheduled snapshots to object storage (`[db.sqlite.backup]`).**
Every `interval`, each embedded database — the one `[db.sqlite] path` in
single-node mode, every `<dir>/<site-key>.db` in per-site mode, open or not —
is snapshotted *inside the ePHPm process* with the engine's own `VACUUM INTO`
(memory-bounded in the engine as of the pinned Turso `v0.8.3-ephpm.3`, which
builds the copy in batches with each index after its rows: the peak is bounded
by `~66 MiB + 2× the largest single index`, not the database size, where stock
0.8.3 committed the whole copy at once and needed ~2× the file — see
[the patched-engine note](#patched-turso-083-the-page-1-allocation-race-and-vacuum-into-memory)),
gzipped and uploaded as `<s3_path>/<site-key>/<YYYYMMDDTHHMMSSZ>.db.gz`; objects
older than `retention` are deleted after each successful upload, always keeping
the newest. The snapshot is read through an in-process engine handle,
coordinated with the serving handle by the WAL checkpoint gate — which is the
whole point versus the external tools above: nothing else opens the file. The
artifact is a self-contained SQLite main file (no `-wal` sidecar; triggers
included) that `sqlite3` can open with no ePHPm at all.

What it is and is not:

- **RPO = `interval`.** A restore loses whatever was written after the chosen
  snapshot. There is no transaction-granularity point-in-time restore yet —
  continuous CDC archival on top of these snapshots is
  [planned — not yet implemented](/roadmap/turso-engine/#tier-1-point-in-time-restore-planned).
- **Restore is offline** (`ephpm db restore --site <key> --from s3://… [--at <ts>]`):
  the newest object at or before `--at` is downloaded, verified with
  `PRAGMA integrity_check`, and renamed over the site's `.db` — only when no
  server holds the file. A corrupt object never replaces the live file. See
  [`ephpm db restore`](/reference/cli/db/#ephpm-db-restore).
- **The object is plaintext** (a gzip of a standard SQLite file, not the
  `[kv.snapshot]` AEAD envelope — so it needs no `[kv] secret` and opens
  anywhere). The bucket must carry server-side encryption and a tight policy.
- **All modes.** In single-node and per-site-single mode the one holder backs
  each database up. In clustered mode the scheduler gates each database on this
  node's election/HRW ownership so it is snapshotted by exactly one node — the
  write node by default, or a non-write holder with `[db.sqlite.backup] source =
  "replica"` — and `ephpm db restore` strips the CDC replication bookkeeping the
  physical image carries so the installed file is clean origin data.
  **Whole-database clustered backup is supported** (exactly-once scheduling, a
  valid standalone artifact); **per-site clustered backup is experimental**
  (reachable only through the experimental `per_site = true`; `source = "replica"`
  there falls back to the owner with a `WARN`). **Restoring *into* a cluster is
  experimental and manual for both topologies**: replicas do not auto-converge
  onto a restored node (one still holding old data tails the restore on top of
  it), so every node must be stopped and every other node's `.db` wiped so they
  cold-bootstrap — the command prints the procedure, and the automatic `--reseed`
  is not yet built. See the
  [clustered-backup design note](/roadmap/clustered-db-backups/) for the
  exactly-once guarantees and the multi-node lab validation each topology needs.

The from-zero walkthrough — enabling it, credentials for S3/MinIO/R2, reading
the listing and the status endpoint, retention, and a worked restore — is the
[Database backups guide](/guides/database-backups/). Configuration and the
per-tick mechanics are in the
[config reference](/reference/config/#dbsqlitebackup); the metrics in
[Metrics → Database backups](/reference/metrics/#database-backups).

(`[kv.snapshot]`'s S3 mode backs up the **KV store**, not the database, and is a
cache warm-up rather than durability. `[db.sqlite.replication]` is experimental
cross-node **cluster replication** for high availability, not a backup to
external storage — a node loss keeps committed data that was already shipped to
a replica, but it gives no archive and no restore-to-a-point.)

The alternatives below still apply where object storage is not an option (or
alongside the built-in backup on a clustered node):

- **Stop-and-copy (the clean path).** A graceful `ephpm` shutdown runs
  `PRAGMA wal_checkpoint(TRUNCATE)` on every open database, so a cleanly stopped
  node leaves a single consolidated `.db` with an empty `-wal`. Copy that file.
- **Crash-consistent volume snapshots** — AWS EBS, Hetzner, DigitalOcean, LVM,
  ZFS. These are safe only when the snapshot is **atomic across the `.db`, `-wal`
  and `-shm` together** (a single-volume block snapshot is). The restored image
  is equivalent to a crash image, which Turso recovers on next open — the same
  recovery path verified for the [0.7.2 → 0.8.2 upgrade](#turso-072--082-v0120).
  A plain `cp`/`rsync` of a live `.db` alone is **not** consistent: it misses the
  `-wal` or captures a write in progress.
- **High availability** (not a backup): experimental clustered mode keeps a full
  local copy on every node — see [Clustered mode](#clustered-mode-experimental).

## How the modes line up

| Mode | Engine | Backend |
|---|---|---|
| Single-node (`[db.sqlite]`, no cluster) | Turso, in-process | litewire `turso` backend, one process opens the file |
| Clustered (`[db.sqlite]` + `[cluster]`) | Turso, in-process | Turso CDC replication over the cluster channel. **Experimental** |

The [DB proxy](/architecture/database/db-proxy/) modes (`[db.mysql]`,
`[db.postgres]`) are unaffected — they forward to a real database server and
have no embedded engine to choose.

## See also

- [Turso engine roadmap](/roadmap/turso-engine/) — the full plan, phase status, gates, and risks
- [MySQL compatibility: named locks](/reference/mysql-compatibility/) — `GET_LOCK()` & co., per site, in every mode above
- [Configuration reference → `[db.sqlite]`](/reference/config/#dbsqlite) — every key
- [Database from PHP](/guides/db-from-php/) — the in-process `ephpm_db_*` bridge
- [Cluster channel](/roadmap/cluster-channel/) — the transport CDC replication rides on

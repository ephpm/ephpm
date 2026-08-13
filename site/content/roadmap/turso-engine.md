# Turso Engine — One Database Engine for Both Modes

> **Status: SHIPPED in v0.7.0. Turso is now the only embedded engine.**
> Single-node Turso is the default; clustered replication is the
> in-process Turso CDC path (**experimental**). The rusqlite (SQLite C
> engine) backend and the sqld sidecar were **removed** — legacy
> `[db.sqlite] engine = "sqlite"` / `"rusqlite"` is now a hard startup
> error, and there is no longer a `cdc_experimental` opt-in flag
> (clustering always uses CDC).
>
> The default swap (Phase 3 below) was driven forward in v0.7.0 by the
> security case — removing the rusqlite backend removes the cross-tenant
> `ATTACH` primitive from the shipped binary. Turso Database is still Beta
> upstream (multiprocess and `VACUUM` remain unsupported), so single-node
> Turso ships "eyes open" and clustered CDC is explicitly experimental. Of
> the five [decision gates](#decision-gates--all-of-them-no-exceptions)
> below, the file-format round-trip (gate 3) is now **MET**; the rest are
> tracked there.
>
> For the user-facing description — how it behaves, its limits, and
> file-format compatibility — see
> [Database engines](/architecture/database/engines/). This page is the
> plan, the evidence, and the risks.

## The thesis

Before v0.7.0, ePHPm ran two SQLite lineages:

- **Single-node**: the genuine SQLite C engine, compiled into the binary
  via rusqlite's `bundled` feature, behind litewire's wire-protocol
  translation.
- **Clustered**: sqld (Turso's libSQL server) embedded as an extracted
  child process, doing page-level WAL replication over gRPC.

[Turso Database](https://github.com/tursodatabase/turso) — the ground-up
Rust rewrite of SQLite (MIT) — replaced **both** with one in-process
engine, which is what v0.7.0 ships:

| Before v0.7.0 | With the Turso engine (v0.7.0) |
|---|---|
| SQLite C via FFI, blocking calls on tokio's pool | Rust-native engine, native async I/O |
| Single writer; per-connection WAL + busy_timeout | MVCC concurrent writes |
| Clustering via the sqld **sidecar** (child process, health checks, binary extraction) | **In-process CDC** feeding ePHPm's own replication layer — no sidecar at all |

The last row is the strategic one. ePHPm already owns gossip membership,
primary election, and a replication data plane; sqld only ever supplied
the WAL-streaming leg. Turso's engine exposes change-data-capture
in-process, and its sync wire protocol is deliberately open (documented
endpoints, reference implementation in their repo). litewire consuming
the CDC stream and handing it to the existing cluster layer completes
the single-binary story: clustered SQLite with **no child processes**.

## Why now (the sqld sunset)

Turso has refocused on the rewrite: libSQL/sqld remain maintained but
feature-frozen, and page-level edge replicas are being discontinued for
new cloud users. The old pinned sqld (v0.24.32) still worked, but it was
a sunset dependency — so v0.7.0 removed it rather than deepening it. The
replacement primitive Turso built (CDC + open sync protocol) is a better
foundation for a project that wants to own its cluster layer than the
black-box sidecar ever was.

## Verified facts (2026-07-10)

- Engine, CDC, and the client-side sync engine are MIT, in the main
  repo (`core/`, `sync/engine`).
- The sync protocol is published as an open contract
  (`/v2/pipeline`, `/pull-updates`) with a reference local server —
  self-hosting is a supported premise, not a loophole.
- Near-complete SQLite surface compatibility; **missing: multiprocess
  support, vacuum**. Beta, `v0.7.0-pre` release line. (Update
  2026-07-14: `v0.7.0` non-pre is out on crates.io; upstream positioning
  is still Beta and multiprocess/vacuum are still experimental flags —
  gate 1 remains open.)
- SQLite file-format compatibility is claimed by upstream. **Verified by
  us (gate 3, now MET):** existing rusqlite/sqlite3 `.db` files open in
  place for cleanly-shut-down databases (WAL and rollback-journal), so the
  0.6.x → 0.7.0 upgrade needs no dump/reload. Caveats in the gates section.

## Plan

### Phase 1 — experimental backend (can start before GA)

> **Status: SHIPPED, 2026-07; became the default and only engine in
> v0.7.0.** litewire has a `litewire-turso` backend (engine pinned
> `turso =0.7.0`) and ePHPm builds it as the sole embedded engine.
> Gate 2–4 evidence lives in `docs/turso-phase1-results.md`; Phase 2
> design notes in `docs/turso-phase2-cdc-design.md`.

The `turso-backend` crate in litewire sits beside its other backends
(rusqlite stays in litewire for other consumers; ePHPm no longer enables
it). The `Backend`/`BackendConn` trait split shipped in July 2026 was
exactly the seam this needed. Deliverable was *data*, not adoption:

- The existing DB latency matrix (point SELECT, insert, connect) —
  Turso engine vs rusqlite vs MySQL baselines.
  **Partially done (2026-08-01):** SELECT and INSERT measured against
  rusqlite; **connect latency and the MySQL baselines are not yet
  measured.**
- A concurrent-writers benchmark (N wire connections inserting), where
  MVCC should beat WAL + busy_timeout — this is the headline claim to
  verify, not assume.
  **Done (2026-08-01) — claim verified.** At c=16 single-INSERT writes,
  CDC-native clustered Turso sustained 694 RPS with no `SQLITE_BUSY`,
  against 37 RPS for clustered SQLite via sqld, which surfaced
  `SQLITE_BUSY` as HTTP 500 and hung connections. Numbers and bounds in
  [Results](/benchmarking/results/#v060--the-turso-engine-measured-against-sqlite);
  the failure mode in
  [Findings](/benchmarking/findings/#sqlite_busy-is-the-clustered-write-ceiling).
- A durability/crash-recovery smoke (kill -9 mid-write, reopen,
  integrity check) — beta engines earn trust here or nowhere.
  **Not started.**

### Phase 2 — CDC-native replication (**shipped; became the only clustered path in v0.7.0**)

Replace the sqld sidecar: litewire tails the engine's CDC stream on the
primary; ePHPm's cluster layer ships changes to replicas (own transport
or Turso's open sync protocol — decide on measured simplicity). Election
and failover machinery is unchanged. In v0.7.0 sqld was removed outright
rather than run through a deprecation-with-overlap cycle, because dropping
the rusqlite backend in the same release already made `engine = "sqlite"`
(the config that selected sqld's clustered path) a hard error.

**Status (2026-07-14): experimental implementation landed; in v0.7.0 it
became the only clustered path.** Enabling clustering with `[db.sqlite]`
selects a CDC-native replication path that runs a
`litewire::litewire_turso::cdc::CdcTailer` on the primary and applies
batches to replicas via `apply_batch` — no sqld sidecar, no child
process, no gRPC. The old `cdc_experimental` opt-in flag was removed in
v0.7.0: with sqld gone there is no alternative clustered path to opt out
to.

**Headline empirical finding (from building this):** Turso 0.7.0 CDC
captures DDL. `CREATE TABLE`/`CREATE INDEX`/`ALTER TABLE ADD COLUMN`/
`DROP TABLE` all appear in the same `turso_cdc` stream as row DML,
encoded as mutations on `sqlite_schema`. This means the replication
path is a **single ordered stream** with no schema-sync side channel.

**Landed in this experimental cut:**

- litewire `CdcTailer` + `apply_batch` API (per-transaction batches,
  monotonic `__litewire_cdc_watermark` for exactly-once apply, SQLite
  record-format decoder for DML replay, sqlite_schema-SQL replay for
  DDL). 45 unit + integration tests in `litewire-turso`.
- ephpm `turso_cdc` module: two `Turso` factories per node (wire +
  mgmt); on the primary each inbound subscriber stream gets its **own**
  `CdcTailer`, anchored at the watermark the subscriber sends in its
  first frame; replica dial + subscribe + apply loop; JSON-framed
  protocol (base64 for record blobs). 2-node e2e integration test
  proves DDL + INSERT + UPDATE + DELETE land on the replica through a
  real authenticated, multiplexed cluster channel, and that a reconnect
  both resumes (rows written during the gap arrive) and does not
  double-apply.
- **Transport = the [cluster channel](/roadmap/cluster-channel/):**
  a single, lazy-bound, `yamux`-multiplexed TCP listener shared by all
  opt-in cluster features, with a mutual ChaCha20-Poly1305
  challenge/response handshake and per-connection sealing of every
  post-handshake byte. CDC is registered as stream type `cdc/<vhost>`
  and snapshot bootstrap as `snapshot/<vhost>`. The channel only binds
  when a feature asks — single-node configs (no clustering) open no extra
  port and are byte-identical to before.
- Cold-replica snapshot bootstrap over `snapshot/<vhost>`: an online
  logical dump captured under one read view, plus the aligned
  watermark. Served only by the elected primary, size-capped by
  `[db.sqlite.replication] max_snapshot_bytes`, and validated against a
  `CREATE`/`INSERT` statement allowlist before it is applied.
- Snapshot size cap: `[db.sqlite.replication] max_snapshot_bytes`
  bounds the cold-replica bootstrap payload (still used by the CDC
  path). *(The `cdc_experimental` opt-in knob that originally gated this
  path was removed in v0.7.0 — clustering always uses CDC now.)*
- **Schema replay is allowlisted (v0.6.2, litewire#17).** A CDC batch
  carrying a `sqlite_schema` row used to have its stored `sql` text run
  directly, so a peer whose frames reached `apply_batch` could run
  arbitrary SQL on a replica — including `ATTACH` and `PRAGMA`. That
  asymmetry with the snapshot path (dump checked, CDC not) is closed:
  `classify_replayed_ddl` now parses the text first and admits only
  what `sqlite_schema.sql` can hold — a **single** `CREATE TABLE`,
  `CREATE [UNIQUE] INDEX`, `CREATE VIEW` or `CREATE TRIGGER`. That is
  the whole legitimate set, because `ALTER TABLE ... ADD COLUMN`
  surfaces as an UPDATE whose text is the *rewritten* `CREATE TABLE`
  and `DROP` arrives as a DELETE that litewire answers with a
  `DROP ... IF EXISTS` of its own construction. `CREATE VIRTUAL TABLE`
  is refused as well, deliberately: it invokes a module with
  peer-chosen arguments, which is a larger capability than building a
  b-tree. The statement scan is quote- and comment-aware, so a `;`
  inside a literal, a quoted identifier or a comment is not a
  separator, and an unterminated quote or comment is refused rather
  than guessed at. Triggers are recognised and skipped rather than
  replayed (see the deferred note below). Reaching `apply_batch` still
  requires the cluster secret and gossip membership; what this removes
  is a trusted peer's ability to do more than corrupt rows.
- **Empty CDC batch frames no longer depend on a local workaround
  (v0.6.2, litewire#17).** `TxnBatch::commit_change_id()` returned
  `i64` and panicked on an empty batch, which a peer could reach by
  sending `{"rows":[]}` — taking down an applying task nothing joins.
  It now returns `Option<i64>`, and litewire's `apply_batch` reports an
  empty batch as an error. ePHPm keeps its frame-level rejection as
  defence in depth: validating a peer's frame is ePHPm's job rather
  than the pinned revision's, and rejecting at decode time keeps the
  diagnostics legible (every failure past that point is reported "at
  change_id N", which an empty batch has no value for).
- Prometheus instrumentation for the whole path — batches and rows
  shipped and applied, subscriber count, the applied watermark, apply and
  tail-poll errors, reconnects by outcome, snapshot bytes and outcome,
  and a replication-lag gauge. Full table in the
  [metrics reference](/reference/metrics/#cdc-native-turso-replication).
  The lag is measured in **change-log rows, not seconds**; a time-based
  lag is listed under "still deferred" below because it needs a wire
  change, not a calculation.

**Still deferred:**

- A **time-based** replication lag. `ephpm_cdc_replication_lag_changes`
  counts change-log rows, which answers "how far behind" but not "how
  many seconds behind". A seconds-valued lag needs a commit timestamp
  travelling with each change: `turso_cdc` stores one (`change_time`),
  but litewire's `CdcRow` does not expose it, so this needs a litewire
  PR adding the field plus a matching CDC wire-format change here. Not
  fakeable from row counts, so nothing is published in the meantime.
- Subscriber watermarks that survive a *primary change*. Resume works
  within one primary (the subscriber names its cursor); after a
  failover the new primary's `change_id` space is its own, so a
  cross-primary cursor needs a cluster-wide watermark scheme.
- TLS wrapping of the cluster channel. The channel is authenticated and
  encrypted with the operator's shared secret, but there is no PKI peer
  identity — see the [cluster channel
  roadmap](/roadmap/cluster-channel/).
- `turso_cdc` retention pruning (v1: table grows unbounded — no
  operational issue on the small-write experimental workloads Phase 2
  targets, but must be solved before Phase 3 default). Note this
  interacts with snapshot bootstrap: once the log is truncated, a
  snapshot watermark below the oldest retained `change_id` is not
  replayable.
- Read-only enforcement on replica wire frontends. litewire has no
  read-only mode, so a replica's MySQL/Hrana frontend accepts writes
  that nothing replicates; the replica logs a warning at startup.
- Triggers reach a replica by neither path. The snapshot deliberately
  does not ship them and CDC replay deliberately skips them — CDC
  already carries the rows a trigger produced on the primary, so a
  replica holding the trigger would fire it again on top of those
  replayed rows and diverge. That is correct for replay, but it means a
  replica promoted to primary has no triggers until an operator
  recreates them.
- 2-node podman/kind e2e test running the full ephpm binary against a
  real MySQL wire client. The in-process integration test proves the
  replication pipeline; the podman lift is largely test-orchestration.
  **Manually validated 2026-08-01, still not automated.** Two release
  containers on a podman network completed cold snapshot bootstrap
  (watermark 24) → CDC subscribe → convergence, driven by PHP `pdo_mysql`
  through litewire, and the replica held 57,634 rows after a ~57k-row
  write benchmark. That was a scripted one-off during benchmarking, not
  a checked-in test — nothing in CI covers it yet.
- Wire-frontend session capture without the factory-level flag: capture
  is enabled on every node's wire factory (`enable_cdc_on_connect =
  true`, so a node promoted mid-life still captures), but a session
  that uses `raw_connection()` bypasses it — a documented gotcha.

### Phase 3 — default engine (a major-version decision)

> **Status: SHIPPED in v0.7.0.** Turso is now the default and only
> embedded engine; the rusqlite backend and sqld sidecar were removed.

Swapping the single-node default off the genuine SQLite C engine changes
what user data sits on, so per the versioning policy it was a new-minor
event, not a patch. In v0.7.0 the swap was driven forward by the security
case (removing the rusqlite `ATTACH` cross-tenant primitive) rather than
waiting on every gate — single-node Turso ships GA-intent with the
crash-recovery soak (gate 4) and upstream GA (gate 1) still tracked as
open, and clustered CDC ships explicitly experimental. See the gates
below for exactly what is and isn't closed.

## Decision gates — all of them, no exceptions

1. Upstream GA: a stable (non-pre) release and upstream's own
   production-readiness statement; multiprocess + vacuum landed.
2. Phase 1 benchmarks at parity-or-better on our matrix, including
   tails. **Substantially evidenced (release build, 2026-08-02), gate
   still open.** Reads: Turso +15% RPS at c=1 with the steadier c=16
   tail (30.9 vs 46.0 ms p99). Writes: parity — 666 vs 648 RPS at c=1,
   and at c=16 rusqlite edges throughput (1130 vs 1120) with the better
   p99 (18.7 vs 20.8 ms); the mid-cycle run's "Turso write tail is
   worse" finding did not persist into the release build. What keeps the
   gate open: `connect` latency and the MySQL baselines are still
   unmeasured, and one c=16 write-p99 cell in rusqlite's favor is within
   this harness's run-to-run spread. See
   [Results](/benchmarking/results/#v060--the-turso-engine-measured-against-sqlite).
3. File-format round-trip verified by us (SQLite-written DB opened by
   Turso and back, checksummed). **MET.** The Turso engine opens existing
   rusqlite/sqlite3-created `.db` files **in place** for cleanly-shut-down
   databases — both WAL and rollback-journal modes; `PRAGMA
   integrity_check` returns `ok` and rows are intact. So the normal
   0.6.x → 0.7.0 upgrade of a stopped node is seamless, **no dump/reload**.
   Two caveats carried into the docs: a database left with an
   uncheckpointed hot `-wal` from a hard crash was not verified to replay
   (shut down cleanly before upgrading), and non-UTF-8 TEXT cells may not
   round-trip (Turso surfaces TEXT as `String` — an upstream limitation).
4. Crash-recovery soak clean. **Open** — this is why single-node Turso
   ships "eyes open" rather than declared GA.
5. WordPress + Laravel e2e suites green on the Turso backend. WordPress
   front-page + drop-in passed; **automate in CI and confirm Laravel**.

v0.7.0 shipped Turso-only despite gates 1 and 4 remaining open, because
removing rusqlite removes the cross-tenant `ATTACH` RCE primitive from
the binary — a more dangerous property to keep than a Beta engine is to
adopt. The remaining gates gate the *GA label*, not the ship.

## Risks, stated plainly

- **Beta engine under user data** is the whole risk; everything above
  is scaffolding to avoid finding its bugs in production.
- Semantic drift: MVCC concurrency changes locking-visible behavior
  vs SQLite's writer serialization; some apps observe `SQLITE_BUSY`
  semantics.
- Velocity risk: Turso the company is mid-pivot; the engine's roadmap
  is theirs, not ours. The mitigation is that everything we depend on
  (engine, CDC, protocol) is MIT — forkable at worst, and litewire's
  backend seam means reversing course is a feature flag.

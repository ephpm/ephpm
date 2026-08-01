# Turso Engine — One Database Engine for Both Modes

> **Status: DESIGN — gated on upstream GA.** Turso Database is in Beta
> (latest release `v0.7.0-pre.x` as of July 2026; multiprocess support
> and vacuum still missing; upstream explicitly does not yet position it
> as a production SQLite replacement). Nothing here ships until that
> changes. This page exists so the decision is pre-made and the
> evidence-gathering starts early.

## The thesis

ePHPm currently runs two SQLite lineages:

- **Single-node**: the genuine SQLite C engine, compiled into the binary
  via rusqlite's `bundled` feature, behind litewire's wire-protocol
  translation.
- **Clustered**: sqld (Turso's libSQL server) embedded as an extracted
  child process, doing page-level WAL replication over gRPC.

[Turso Database](https://github.com/tursodatabase/turso) — the ground-up
Rust rewrite of SQLite (MIT) — plausibly replaces **both** with one
in-process engine:

| Today | With the Turso engine |
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
new cloud users. Our pinned sqld (v0.24.32) keeps working, but it is a
sunset dependency — no new ePHPm feature should deepen it. The
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
- SQLite file-format compatibility is claimed; must be verified by us
  (see gates) before any migration story is written.

## Plan

### Phase 1 — experimental backend (can start before GA)

> **Status: SHIPPED (experimental), 2026-07.** litewire has a
> `litewire-turso` backend (facade feature `turso`, off by default;
> engine pinned `turso =0.7.0`) and ePHPm exposes it as
> `[db.sqlite] engine = "turso"` — single-node only, rejected in
> clustered mode, warns at startup. Gate 2–4 evidence lives in
> `docs/turso-phase1-results.md`; Phase 2 design notes in
> `docs/turso-phase2-cdc-design.md`. Gates 1 and 5 remain open and the
> default engine is unchanged.

A `turso-backend` crate in litewire beside `rusqlite-backend`, behind a
feature flag and an explicit opt-in config knob marked **experimental**
(additive knob: v0.4.x-compatible under the versioning policy). The
`Backend`/`BackendConn` trait split shipped in July 2026 is exactly the
seam this needs. Deliverable is *data*, not adoption:

- The existing DB latency matrix (point SELECT, insert, connect) —
  Turso engine vs rusqlite vs MySQL baselines.
- A concurrent-writers benchmark (N wire connections inserting), where
  MVCC should beat WAL + busy_timeout — this is the headline claim to
  verify, not assume.
- A durability/crash-recovery smoke (kill -9 mid-write, reopen,
  integrity check) — beta engines earn trust here or nowhere.

### Phase 2 — CDC-native replication (**experimental implementation available; gated on GA for default**)

Replace the sqld sidecar: litewire tails the engine's CDC stream on the
primary; ePHPm's cluster layer ships changes to replicas (own transport
or Turso's open sync protocol — decide on measured simplicity). Election
and failover machinery is unchanged. sqld support enters deprecation
with a full release cycle of overlap.

**Status (2026-07-14): experimental implementation landed behind
`[db.sqlite.replication] cdc_experimental = true`.** Enabling it (with
`engine = "turso"` + `[cluster] enabled = true`) selects a CDC-native
replication path that runs a `litewire::litewire_turso::cdc::CdcTailer`
on the primary and applies batches to replicas via `apply_batch` — no
sqld sidecar, no child process, no gRPC. sqld remains the production
clustered default for `engine = "sqlite"`.

**Headline empirical finding (from building this):** Turso 0.7.0 CDC
captures DDL. `CREATE TABLE`/`CREATE INDEX`/`ALTER TABLE ADD COLUMN`/
`DROP TABLE` all appear in the same `turso_cdc` stream as row DML,
encoded as mutations on `sqlite_schema`. This means the replication
path is a **single ordered stream** with no schema-sync side channel.

**Landed in this experimental cut:**

- litewire `CdcTailer` + `apply_batch` API (per-transaction batches,
  monotonic `__litewire_cdc_watermark` for exactly-once apply, SQLite
  record-format decoder for DML replay, sqlite_schema-SQL replay for
  DDL). 25 unit + integration tests in `litewire-turso`.
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
  when a feature asks — configs without `cdc_experimental` are
  byte-identical to before.
- Cold-replica snapshot bootstrap over `snapshot/<vhost>`: an online
  logical dump captured under one read view, plus the aligned
  watermark. Served only by the elected primary, size-capped by
  `[db.sqlite.replication] max_snapshot_bytes`, and validated against a
  `CREATE`/`INSERT` statement allowlist before it is applied.
- Additive config knob: `cdc_experimental` defaults to `false`;
  `engine = "turso"` + clustered mode without it is still a hard startup
  error pointing at the knob. v0.4.x-compatible under versioning policy.

**Still deferred:**

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
- Schema replay executes wire-supplied SQL. Note the asymmetry with the
  snapshot path above: the snapshot dump *is* checked against a
  `CREATE`/`INSERT` allowlist, but the CDC apply path is not. When a
  batch carries a `sqlite_schema` row, `apply_batch` runs its stored
  `sql` text directly (`litewire-turso/src/cdc.rs`) with no allowlist of
  statement kinds. The DML path is safe — values bind as parameters and
  identifiers go through `escape_ident` — but a peer whose frames reach
  `apply_batch` can run arbitrary SQL on a replica, including `ATTACH`
  and `PRAGMA`. Reaching it requires the cluster secret *and* passing
  the gossip-membership check, so the peer is already a trusted node
  that dictates replicated data anyway; the residual gap is that a
  compromised primary can do more than corrupt rows. Closing it needs a
  litewire PR to parse and allowlist the replayed DDL, plus a pin bump.
- Triggers in the snapshot. They are deliberately not shipped (CDC
  already carries the rows a trigger produced on the primary), which is
  correct for replay but means a replica promoted to primary has no
  triggers until an operator recreates them.
- 2-node podman/kind e2e test running the full ephpm binary against a
  real MySQL wire client. The in-process integration test proves the
  replication pipeline; the podman lift is largely test-orchestration.
- Wire-frontend session capture without the factory-level flag: capture
  is enabled on every node's wire factory (`enable_cdc_on_connect =
  true`, so a node promoted mid-life still captures), but a session
  that uses `raw_connection()` bypasses it — a documented gotcha.
- `TxnBatch::commit_change_id()` panics on an empty batch inside
  litewire; ephpm rejects empty batch frames on the wire instead.
  Making the litewire API return `Option<i64>` needs a litewire PR and
  a pin bump.

### Phase 3 — default engine (a major-version decision)

Swapping the single-node default off the genuine SQLite C engine is the
last step and the highest bar: it changes what user data sits on. It
does not happen before the gates below, and per the versioning policy it
is a new-minor (or larger) event, never a patch.

## Decision gates — all of them, no exceptions

1. Upstream GA: a stable (non-pre) release and upstream's own
   production-readiness statement; multiprocess + vacuum landed.
2. Phase 1 benchmarks at parity-or-better on our matrix, including
   tails.
3. File-format round-trip verified by us (SQLite-written DB opened by
   Turso and back, checksummed).
4. Crash-recovery soak clean.
5. WordPress + Laravel e2e suites green on the experimental backend.

Until all five: rusqlite ships the genuine SQLite C engine as the
default, and that is a feature, not a compromise — "the most-deployed
database engine on earth, compiled into the binary."

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

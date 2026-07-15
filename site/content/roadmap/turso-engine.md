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
  mgmt), primary tail loop → broadcast → **cluster channel handler**;
  replica dial + apply loop; JSON-framed protocol (base64 for record
  blobs). 2-node e2e integration test proves DDL + INSERT + UPDATE +
  DELETE land on the replica through a real authenticated, multiplexed
  cluster channel and that reconnect is idempotent.
- **Transport = the [cluster channel v1](/roadmap/cluster-channel/):**
  a single, lazy-bound, `yamux`-multiplexed, ChaCha20-Poly1305-
  authenticated TCP listener shared by all opt-in cluster features. CDC
  is registered as stream type `cdc/<vhost>`; snapshot bootstrap is
  RESERVED (`snapshot/<vhost>`). The channel only binds when a feature
  asks — configs without `cdc_experimental` are byte-identical to
  before.
- Additive config knob: `cdc_experimental` defaults to `false`;
  `engine = "turso"` + clustered mode without it is still a hard startup
  error pointing at the knob. v0.4.x-compatible under versioning policy.

**Deferred to Phase 2.1:**

- Fresh-replica snapshot bootstrap (v1: operator seeds the DB file
  before starting the replica, or accepts that replicas start empty).
- Persisted subscriber watermark across primary restart (v1: broadcast
  channel; new subscribers start from the current position, not from
  cursor 0 of the primary's history).
- TLS wrapping of the cluster channel (v1: the channel handshake and
  framing are ChaCha20-Poly1305-authenticated with the operator's
  shared secret, but not TLS. Per-plane PKI identity is a Phase 2.1
  item — see the [cluster channel roadmap](/roadmap/cluster-channel/)).
- `turso_cdc` retention pruning (v1: table grows unbounded — no
  operational issue on the small-write experimental workloads Phase 2
  targets, but must be solved before Phase 3 default).
- 2-node podman/kind e2e test running the full ephpm binary against a
  real MySQL wire client. The in-process integration test proves the
  replication pipeline; the podman lift is largely test-orchestration.
- Wire-frontend session capture without the factory-level flag (v1:
  every wire session that goes through the primary's wire factory gets
  CDC because `enable_cdc_on_connect = true`; a session that uses
  `raw_connection()` bypasses this — a documented gotcha).

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

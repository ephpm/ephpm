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
  support, vacuum**. Beta, `v0.7.0-pre` release line.
- SQLite file-format compatibility is claimed; must be verified by us
  (see gates) before any migration story is written.

## Plan

### Phase 1 — experimental backend (can start before GA)

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

### Phase 2 — CDC-native replication (gated on GA)

Replace the sqld sidecar: litewire tails the engine's CDC stream on the
primary; ePHPm's cluster layer ships changes to replicas (own transport
or Turso's open sync protocol — decide on measured simplicity). Election
and failover machinery is unchanged. sqld support enters deprecation
with a full release cycle of overlap.

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

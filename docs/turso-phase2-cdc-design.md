# Turso Engine Phase 2 — CDC-Native Replication (implementation notes)

Companion to [turso-phase1-results.md](turso-phase1-results.md) and the
[roadmap page](../site/content/roadmap/turso-engine.md). Phase 2 is
gated on upstream GA (roadmap gate 1, currently **not met**) for
promotion to a default; the **experimental** implementation described
below has landed behind an opt-in knob so we can gather the operational
evidence needed for gate 5 (WordPress/Laravel e2e).

## Empirical corrections to the original design (2026-07-14)

Phase 1 documented the CDC surface from a source-code read. Building the
actual tail/apply pipeline forced empirical verification (see
`crates/litewire-turso/tests/cdc_ddl_capture.rs` in litewire) and turned
up three corrections:

1. **DDL IS captured.** The headline blocking question — "does CDC
   capture `CREATE TABLE`/`ALTER TABLE`/`CREATE INDEX`/`DROP TABLE`?" —
   is **yes**. Turso 0.7.0 emits DDL as ordinary row mutations on
   `sqlite_schema` within the same `turso_cdc` stream. The `after`-image
   record's column 4 carries the CREATE statement text; the applier
   simply re-executes it. This means replication is a **single ordered
   stream**, not a data+schema pair — WordPress/Laravel's runtime DDL
   flows through the normal path with no side channel. Verified by 7
   integration tests against `turso = "=0.7.0"`.

2. **Column order in `turso_cdc` is not what the source-only read
   suggested.** The actual v2 schema (from
   `turso_core::translate::emitter::mod::emit_cdc_insns_v2`) is
   `(change_id, change_time, change_txn_id, change_type, table_name, id,
   before, after, updates)` — `change_txn_id` is **column 3**, not
   column 9. The tailer's SELECT list has been aligned accordingly.

3. **`change_type` numeric values are** `INSERT=1, UPDATE=0, DELETE=-1,
   COMMIT=2` (not the `0=DELETE, 1=INSERT` the doc previously suggested).
   COMMIT rows have `table_name = NULL` and `id = NULL`.

4. **`turso_cdc_version.version` is a TEXT tag `"v2"`**, not an
   integer. Version-detection code must match on text.

## The verified CDC API (turso 0.7.0 source + integration tests)

- Enablement is **per-connection**:
  `PRAGMA capture_data_changes_conn('<mode>[,<table>]')`. As of 0.7.0 this
  is the *stable* pragma name; `unstable_capture_data_changes_conn` remains
  as a deprecated alias. Modes: `off | id | before | after | full`
  (`full` = before-image + after-image + column-level updates).
- Captured changes are written **transactionally into a regular table**
  (default `turso_cdc`), so the log commits/rolls back atomically with the
  data it describes, and is readable with plain SQL. Schema is versioned
  via a `turso_cdc_version` table; current version **v2** (9 columns; see
  correction #2 above for the actual order), where v2 adds
  `change_txn_id` and explicit COMMIT records (`change_type = 2`) — i.e.
  transaction boundaries are in the stream.
- Stability: the API surface is young (v1→v2 schema bump already happened;
  the pragma was renamed within the 0.7 line). Pin exactly and
  version-detect via `turso_cdc_version` before tailing.
- Upstream's own sync engine (`turso_sync_engine`, same repo, MIT) is
  built on these primitives plus the open sync wire protocol
  (`/v2/pipeline`, `/pull-updates`). **We do not use it in Phase 2 v1**:
  its coroutine (`genawaiter`) execution model doesn't compose with
  tokio, it operates on `turso_core` primitives (not the async `turso`
  crate's `Connection`), and Turso 0.7.0's lack of multiprocess support
  means opening the same file through both handles is unsafe. Instead,
  litewire ships a small (~600 lines including tests) `cdc` module with
  a `CdcTailer` + `apply_batch` API and its own SQLite record-format
  decoder for DML replay.

## Proposed shape

```
            primary node                              replica nodes
┌─────────────────────────────────┐        ┌───────────────────────────────┐
│ PHP → wire → litewire → Turso   │        │ litewire → Turso engine (RO*) │
│  every write session:           │        │                               │
│  PRAGMA capture_data_changes_   │        │  apply loop: per-txn batches  │
│   conn('full') (Backend::connect)│        │  in order, one transaction    │
│  CdcTailer (litewire-turso):    │  ePHPm │  per change_txn_id; advance   │
│  SELECT * FROM turso_cdc        │ cluster│  watermark table atomically   │
│   WHERE change_id > ?cursor ────┼──data──┼─▶ with the applied rows       │
│  batch on COMMIT records        │  plane │                               │
└─────────────────────────────────┘        └───────────────────────────────┘
```

- **litewire side (new, small):** a `CdcTailer` in `litewire-turso` that
  (a) ensures every write session enables CDC in `Backend::connect`, and
  (b) polls `turso_cdc` past a cursor, emitting complete transactions
  (delimited by the v2 COMMIT records). This replaces nothing in the
  `Backend` trait — it is a sidecar API on the `Turso` factory.
- **ePHPm side (reuse):** the cluster layer already owns membership
  (chitchat gossip), primary election (`sqlite_election.rs` — unchanged),
  and a replication data plane (`KvReplicator`'s transport pattern). Phase
  2 adds a DB replication channel alongside KV replication: primary
  publishes CDC transaction batches; replicas apply them through their
  local Turso engine and record the applied `change_id` watermark in the
  same transaction (exactly-once apply by construction).
- **What dies:** the sqld sidecar — child-process management, binary
  embedding (`include_bytes!` of a 100MB+ binary), health polling, and the
  Hrana client hop on replicas. Failover becomes "stop applying, start
  capturing" with no process restarts.
- **Alternative transport:** self-host Turso's open sync protocol using
  `turso_sync_engine` instead of our own batching. Decide on measured
  simplicity (roadmap language): our transport is fewer new dependencies;
  theirs solves bootstrap + partial sync already.

## Open questions (must be answered before any Phase 2 code)

1. **DDL:** does CDC capture schema changes (`CREATE TABLE`/`ALTER`), or
   only row changes? WordPress installs/plugins run DDL at runtime. If
   uncaptured, replicas need a separate schema-sync path (the sync engine's
   answer here is the thing to study first).
2. **API churn:** the pragma was renamed and the schema bumped v1→v2
   within one minor line. What is our compatibility policy when v3 lands —
   dual-read, or hard-pin per litewire release?
3. **Replica writes:** how do we enforce read-only replicas at the
   litewire seam (reject non-SELECT on replicas vs forward-to-primary),
   and what do PHP apps see during failover?
4. **Bootstrap:** new replica joins — full-file snapshot (checkpoint +
   copy + cursor handoff) vs replaying the sync protocol from genesis.
   CDC alone can't bootstrap unless `turso_cdc` is retained forever.
5. **Retention/pruning:** `turso_cdc` is a regular table and grows
   unboundedly; pruning must wait for the slowest replica's watermark
   (gossip the min watermark, delete below it) — and what happens when a
   replica is offline for a week?
6. **Ordering under MVCC:** with concurrent writers, is `change_id` order
   guaranteed to be a valid serialization to replay? (COMMIT records
   suggest yes per-transaction; cross-transaction ordering needs a
   definitive upstream answer.)
7. **Failover watermark:** on primary death, the new primary's `turso_cdc`
   starts fresh — replicas promoted from different watermarks must agree
   on a common prefix. Likely needs the election to include "highest
   applied change_id wins" (same shape as Raft's log-completeness vote).
8. **Cost:** `full` mode roughly doubles write volume (data + before/after
   images). Phase 1's write-path advantage (4× concurrent throughput)
   needs re-measuring with CDC on before claiming the sidecar-free story
   is also the faster one.

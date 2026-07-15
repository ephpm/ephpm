# Turso Engine Phase 2 — CDC-Native Replication (design notes)

Design notes only — no code. Companion to
[turso-phase1-results.md](turso-phase1-results.md) and the
[roadmap page](../site/content/roadmap/turso-engine.md). Phase 2 is gated
on upstream GA (roadmap gate 1, currently **not met**).

## The verified CDC API (turso 0.7.0 source, not docs)

- Enablement is **per-connection**:
  `PRAGMA capture_data_changes_conn('<mode>[,<table>]')`. As of 0.7.0 this
  is the *stable* pragma name; `unstable_capture_data_changes_conn` remains
  as a deprecated alias. Modes: `off | id | before | after | full`
  (`full` = before-image + after-image + column-level updates).
- Captured changes are written **transactionally into a regular table**
  (default `turso_cdc`), so the log commits/rolls back atomically with the
  data it describes, and is readable with plain SQL. Schema is versioned
  via a `turso_cdc_version` table; current version **v2** (9 columns:
  `change_id, change_time, change_type, table_name, id, before, after,
  updates, change_txn_id`), where v2 adds `change_txn_id` and explicit
  COMMIT records (`change_type = 2`) — i.e. transaction boundaries are in
  the stream.
- Stability: the API surface is young (v1→v2 schema bump already happened;
  the pragma was renamed within the 0.7 line). Pin exactly and
  version-detect via `turso_cdc_version` before tailing.
- Upstream's own sync engine (`turso_sync_engine`, same repo, MIT) is
  built on these primitives plus the open sync wire protocol
  (`/v2/pipeline`, `/pull-updates`) — a reference consumer we can crib
  from or adopt outright.

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

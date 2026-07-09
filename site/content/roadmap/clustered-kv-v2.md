# Clustered KV v2 — Replicated Counters, Deletes, and Cluster-Aware Rate Limiting

> **Status: DESIGN — not yet implemented.** v0.4.0 shipped KV replication
> v1: `SET`/`DEL` route through a `Replicator` seam on the store, small
> values gossip to every node with write-timestamp ordering, large values
> replicate via the TCP data plane. This page designs what v1 deliberately
> left out.

## What v1 does not do (and warns about)

Three gaps shipped in v0.4.0, each documented and — where it bites —
guarded by a startup warning:

1. **`INCR`/`APPEND` in-place mutations do not replicate.** Only the
   new-key creation path routes through `Store::set`. Consequence: the
   rate-limit middleware's counters are **per-node** in a cluster — a
   client at the limit on node A is fresh on node B. Startup warns when
   `[cluster].enabled` coexists with a ratelimit mount.
2. **`EXPIRE` updates only the local copy.** Remote copies keep their
   write-time TTL.
3. **Deletes don't propagate to materialized copies.** `gossip_del`
   tombstones aren't applied by peers; a deleted key lingers remotely
   until TTL expiry or overwrite.

For v0.4.0's flagship consumer — the monotonic `opcache:version:*`
keys — none of this matters (pure `SET`, overwrite-only, no TTL games).
For general-purpose clustered KV it's the difference between "replicated
cache" and "replicated data structure".

## Design

### Replicated counters: owner-routed increments

CRDT G-counters would replicate increments without coordination, but
rate limiting needs *bounded* counters with TTL windows — merge
semantics get hairy. Simpler and sufficient: **route the increment to
the key's owner**.

- `hash(key) % alive_nodes` already defines an owner (the v1 data-plane
  placement function).
- `INCR` on a non-owner forwards over the existing TCP data plane
  (new `OP_INCR` frame: key, delta, optional TTL-on-create) and returns
  the owner's authoritative value. Owner applies locally, then
  gossips/replicates the new value using the v1 machinery.
- Owner-local `INCR` is what it is today, plus fan-out.
- One in-flight round trip per increment on non-owners (~data-plane
  RTT, sub-millisecond in-cluster). The ratelimit middleware's
  25-INCR-per-request bench pattern would amortize by batching
  (`OP_INCRBY` with delta 25 — the PHP `ephpm_kv_incr` loop collapses
  server-side).

**Failure mode:** owner unreachable → increment applies locally with a
`degraded` flag and a warning metric; the window self-heals on the next
ring change. Rate limiting degrades to per-node rather than failing
requests — the same behavior as v1, now as a *fallback* instead of the
steady state.

### Deletes: tombstone application

The gossip wire format v2 (`{expiry_ms}:{write_ms}:{b64}`) has room for
a tombstone marker (empty payload + `write_ms`). The applier that
materializes remote SETs also applies tombstones — `remove_local` when
the tombstone's `write_ms` beats the last applied write for that key.
The existing per-key `AppliedWriteMap` already provides the ordering;
this is the missing quarter of that design.

### `EXPIRE`: piggyback on the same ordering

`OP_EXPIRE` frame to the owner + gossip of a TTL-update event with
`write_ms`. Peers holding a materialized copy apply the shorter of
(current TTL, new TTL) when the event is fresher than their last write.

### Cluster-aware rate limiting (the user-facing payoff)

With owner-routed `INCR` + TTL-on-create, the ratelimit middleware
becomes cluster-correct with **zero configuration change** — the
middleware already calls `kv_incr_ttl`, which routes through the store.
Remove the startup warning; add
`ephpm_ratelimit_degraded_windows_total` for the fallback path.

## What this deliberately does not attempt

- **No quorum, no consensus.** Same posture as v1: best-effort
  replication with honest documentation. Bank-grade rate limiting needs
  a real coordination system; web-tier abuse throttling does not.
- **No cross-node transactions or multi-key atomicity.**
- **No pubsub.** Still unjustified by any current consumer.

## Sizing

| Piece | Effort |
|---|---|
| `OP_INCR`/`OP_INCRBY`/`OP_EXPIRE` data-plane frames + owner routing | the bulk |
| Tombstone gossip + applier | small (machinery exists) |
| Ratelimit warning removal + degraded metric | trivial |
| Two-node e2e: cross-node limit enforcement, delete propagation | moderate |

Roughly one focused week. Prerequisite for anyone selling "cluster" as
more than an OPcache feature; not a blocker for anything shipped today.

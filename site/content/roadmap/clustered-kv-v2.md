# Clustered KV v2 — Replicated Counters, Deletes, and Cluster-Aware Rate Limiting

> **Status: PARTIALLY SHIPPED.** v0.4.0 shipped KV replication v1 (SET
> routes through the `Replicator` seam, small values gossip with
> write-timestamp ordering, large values replicate via the TCP data
> plane). A follow-up ("v1.1") extended that machinery to **deletes**
> (write-stamped tombstones broadcast over the same gossip
> subscription — peers `remove_local` when the tombstone beats their
> last-applied write) and to **EXPIRE** (re-emit the value with the new
> expiry stamp; peers apply verbatim by last-arrival-wins). What remains
> is **owner-routed `INCR`** — the counter-replication piece needed for
> cluster-correct rate limiting. This page still designs that.

## What v1 does not do (and warns about)

One gap remains after v1.1:

1. **`INCR`/`APPEND` in-place mutations do not replicate.** Only the
   new-key creation path routes through `Store::set`. Consequence: the
   rate-limit middleware's counters are **per-node** in a cluster — a
   client at the limit on node A is fresh on node B. Startup warns when
   `[cluster].enabled` coexists with a ratelimit mount.

Two other v1 gaps have shipped as of v1.1 and are no longer roadmap
work:

- **`EXPIRE` now replicates.** `Store::expire` re-emits the current
  value on the gossip tier with a fresh `write_ms` and the new
  `expiry_ms`; peers apply verbatim by the same last-arrival-wins rule
  used for SETs. Extending a session's TTL (`session.lazy_write`)
  therefore propagates just like a shorten — the origin's newer
  timestamp wins.
- **Deletes now propagate.** `Store::remove` broadcasts a tombstone
  marker (`"TS:{write_ms}"`) into chitchat state — the same
  subscription the SET applier watches, since chitchat's real
  `state.delete()` does not fire subscribers. Peers call
  `Store::remove_local` when the tombstone's `write_ms` beats their
  last-applied write, which drops both the gossip-materialized copy
  and any locally-held data-plane replica of that key.

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

### Deletes: tombstone application — SHIPPED (v1.1)

The gossip wire format got a distinct tombstone payload
(`"TS:{write_ms}"`) — chitchat's own `state.delete()` does not fire
`subscribe_event`, so tombstones ride the same subscription as SETs to
be observable. The applier calls `Store::remove_local` when the
tombstone's `write_ms` beats the last-applied write for that key,
which also drops any locally-held data-plane replica (both tiers share
the local `Store`). No separate `OP_DELETE` data-plane frame — the
gossip broadcast reaches every node and each node cleans up whichever
tier holds its copy.

### `EXPIRE`: piggyback on the same ordering — SHIPPED (v1.1)

`Store::expire` on the origin updates the local copy, then re-emits the
current bytes on the gossip tier with the new `expiry_ms` and a fresh
`write_ms`. Peers apply the event verbatim by the SET applier's
last-arrival-wins rule — the origin's newer `write_ms` wins and the
peer takes whatever expiry the origin last stamped. This is what
`session.lazy_write` refresh needs (an *extension* of the TTL, not a
shorten); a shorter-wins rule would silently break session lifetimes
that legitimately grow. `write_ms` ordering already serializes intent.

Known scope limit: large-value TTL updates only touch the origin's
local copy. A `Store::expire` on a value above `small_key_threshold`
does not fan out to the value's data-plane replicas. That is the
`OP_EXPIRE` piece of the counter-replication design and lands together
with owner-routed `INCR`.

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

| Piece | Effort | Status |
|---|---|---|
| Tombstone gossip + applier | small (machinery existed) | shipped v1.1 |
| Gossip-tier TTL replication | small (re-encode + fresh write_ms) | shipped v1.1 |
| `OP_INCR`/`OP_INCRBY`/`OP_EXPIRE` data-plane frames + owner routing | the bulk | remaining |
| Ratelimit warning removal + degraded metric | trivial | after INCR |
| Two-node e2e: cross-node limit enforcement | moderate | after INCR |

The counter-replication work — the remaining bulk — is roughly one
focused week. Prerequisite for anyone selling "cluster" as more than an
OPcache + session feature; not a blocker for anything shipped today.

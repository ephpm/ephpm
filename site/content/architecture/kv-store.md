+++
title = "KV Store"
weight = 6
+++

Why a built-in KV? PHP apps want a cache. The default answer is "run Redis." That's another daemon, another network hop, another thing to monitor, another thing to back up. ePHPm bundles the cache in-process: a DashMap-backed store, RESP2 protocol on the wire, native PHP functions for hot paths.

## Two access paths, one store

```
                        ┌───────────────┐
                        │   PHP code    │
                        └───────┬───────┘
                                │
            ┌───────────────────┴───────────────────┐
            │                                       │
   ephpm_kv_* (in-process)              RESP2 over :6379
        ~100 ns/op                      Predis / phpredis
            │                              ~10–100 µs/op
            │                                       │
            └───────────────┬───────────────────────┘
                            ▼
                  ┌─────────────────────┐
                  │   ephpm-kv crate    │
                  └──────────┬──────────┘
                             │
       ┌─────────────────────┼─────────────────────┐
       ▼                     ▼                     ▼
┌──────────────┐      ┌──────────────┐    ┌──────────────────────┐
│ strings      │      │ hash         │    │ transparent          │
│ store        │      │ store        │    │ compression          │
│              │      │              │    │ gzip · zstd · brotli │
│ DashMap of   │      │ DashMap of   │    │ threshold-gated      │
│ String →     │      │ String →     │    │                      │
│ StringEntry  │      │ HashEntry    │    │                      │
└──────────────┘      └──────────────┘    └──────────────────────┘
```

Hash entries are kept in a separate map from string entries. Unlike Redis, the two do not exclude each other: there is no `WRONGTYPE` error, and one name can hold a string and a hash at the same time (`GET` on a hash name returns nil, `TYPE` reports `hash`). TTLs, `RENAME` and cluster replication apply to the string map only. See [KV RESP Protocol](/reference/kv-resp/#strings-and-hashes-do-not-collide).

## Storage

[DashMap](https://docs.rs/dashmap/) — lock-sharded concurrent hash map. Reads are wait-free for the common case; writes scale with the number of shards (default = `4 * num_cpus`). For ePHPm's workloads (cache + counters + sessions) this comfortably outpaces a hand-rolled `Mutex<HashMap>` and avoids the latency tail of a global lock.

Entries carry an optional expiry timestamp. Reads check it, so an expired key is never returned, and delete the expired entry they find. Keys that are never read again are reclaimed by **active expiry**: a dedicated `ephpm-kv-expiry` thread, started with the store whether or not the RESP listener is enabled, covering the default store and every per-site store. Ten times a second it examines TTL'd keys 20 at a time from a cursor that carries over between runs (so every TTL'd key is reached within one sweep), reaps the expired ones, and keeps going while more than 10% of a batch was expired, within a 25 ms budget shared by all stores. A store full of long-TTL keys therefore costs one small batch per run, and a burst of expiring keys is reclaimed as fast as the budget allows. Some expired keys can wait until the next sweep reaches them; they are never readable and never block a write:

Expired keys still count toward `memory_limit` until they are reclaimed, so a write that would exceed the limit first reclaims expired keys, and only then evicts or refuses.

## Compression

Configured via `[kv]`:

```toml
compression = "zstd"          # none / gzip / brotli / zstd
compression_level = 6
compression_min_size = 1024   # values smaller than this are stored raw
```

Compression is **transparent** — Get/Set don't know about it. The compression flag is per-entry: a small value stored uncompressed and a large value stored compressed coexist in the same store. Decompression happens on read; if it fails (corrupted entry), the read returns an error.

`zstd` is usually the right default — better ratio than gzip, faster than brotli at the same level. `brotli` wins when values are highly compressible text and you can spare the CPU.

## Eviction

Configured via `[kv]`:

```toml
memory_limit = "256MB"
eviction_policy = "allkeys-lru"   # noeviction / allkeys-lru / volatile-lru / allkeys-random
```

When memory exceeds the limit, the eviction policy picks victims:

- **noeviction** — refuse new writes with an error (`-OOM` over RESP). Every write that grows the store is checked: `SET`/`SETEX`/`MSET`/`GETSET`/`SETNX`, `INCR` creating a key, `APPEND`, and `HSET`.
- **allkeys-lru** — least recently used across all keys
- **volatile-lru** — least recently used among keys with a TTL only
- **allkeys-random** — uniformly random victim

LRU isn't strict — maintaining a true global LRU over a sharded map would be expensive. Instead, each eviction pass samples 16 candidates from map iteration, compares them by `last_accessed`, and evicts the best victim; passes repeat until usage drops back under the limit. Approximate but cheap.

## RESP2 protocol

The on-wire format is Redis RESP2. We don't fork a Redis-compatible parser — we have a tight implementation in [`crates/ephpm-kv/src/resp/`](https://github.com/ephpm/ephpm/tree/main/crates/ephpm-kv/src/resp). Connections are tokio tasks reading line-buffered RESP frames; one connection = one task, no thread pool.

Supported command groups: strings, hashes (`HSET`, `HGET`, `HDEL`, `HGETALL`, `HKEYS`, `HVALS`, `HLEN`, `HEXISTS`), keys, connection. See [KV RESP Protocol](/reference/kv-resp/) for every command, its deviations from Redis, and what is not implemented.

## Multi-tenant isolation

In vhost mode (`[server] sites_dir = ...`), the RESP listener requires `[kv] secret`.

**Without `[kv] secret`**, ePHPm refuses to start the RESP listener in vhost mode: `[kv.redis_compat] enabled = true` with `sites_dir` set and no secret is a **hard startup error**, because the listener would otherwise serve one shared store to every tenant. Either set the secret or leave `[kv.redis_compat] enabled = false` (the default) and let PHP use the SAPI functions, which are namespaced per site regardless of this listener.

**With `[kv] secret` set**, a connection must authenticate as a specific site (`AUTH <site> <password>`) and is then bound to that site's own store for its lifetime — separate `Store` instances, not a key-prefix convention, and the same store the site's `ephpm_kv_*` calls use. ePHPm derives per-site passwords from `[kv] secret`:

```
password = HMAC-SHA256(secret, site_key)
```

The derived password is injected into PHP's `$_SERVER` as `EPHPM_REDIS_PASSWORD` for each request, so each site's PHP code can authenticate to its own scope. `[kv.redis_compat] password` is ignored in this mode. The same secret also derives each site's MySQL wire password, so anyone who can read the config can impersonate any tenant — see [`[kv] secret`](/reference/config/#kv).

**Single-site mode** (no `sites_dir`) has one shared store. There, only `[kv.redis_compat] password` authenticates the listener. `[kv] secret` cannot (there is no per-site derivation without `sites_dir`), so the RESP listener on with `[kv] secret` set and no `password` is a **startup error**. See [KV RESP Protocol → Authentication](/reference/kv-resp/#authentication).

## Clustered KV (when `[cluster] enabled = true`)

The KV store goes from in-process to a distributed two-tier system:

### Tier 1 — gossip-backed (small values)

Values up to `[cluster.kv] small_key_threshold` (default 512 bytes) ride the gossip protocol via chitchat. They're encoded into the gossip state with base64 + an absolute millisecond expiry + the origin's **write stamp**. Convergence is fast (hundreds of ms typical). Eventually consistent.

Applies are **last-writer-wins** by stamp. A stamp is `(write, node)`: `write` is the origin's wall-clock millisecond with a per-millisecond sequence number packed into its low 20 bits (strictly increasing on each node, never reused), and `node` is a deterministic 64-bit tag of the origin node id. Stamps order by `write` first, then by `node`, so a write a millisecond later always wins, two writes from one node are ordered by their sequence, and two nodes writing one key in the **same** millisecond are broken by the node tag — identically on every node, so the cluster settles on one winner instead of each node keeping its own. Every write takes exactly one stamp, in the synchronous write path before the local copy is written, and that same stamp travels on the wire; each node keeps a per-key *last-applied* stamp shared between the gossip applier and its own write path and skips any incoming write whose stamp is not strictly greater. That is what stops a slow gossip echo of an older write (including the origin's echo of its own write) from clobbering a newer value already materialized locally. Clock skew between nodes biases the order toward the faster clock; run NTP.

**`SET`, `DEL`, and `EXPIRE` replicate today.** A `DEL` broadcasts a stamped tombstone marker over the same gossip subscription — peers apply it to their local copies (both gossip-materialized and any locally-held data-plane replica) when the tombstone's stamp beats their last-applied write. `EXPIRE` re-emits the existing value with the new expiry stamp, so a session TTL refresh (which *extends* the expiry) propagates just like a shorten. `INCR` and `DECR` replicate too (since #416): each increment is atomic on its own node and then publishes the counter's **new total**, with its TTL. Peers apply totals last-writer-wins, not deltas, so concurrent increments on different nodes overwrite each other instead of summing. The built-in `ratelimit` middleware uses `INCR`, so its windows are cluster-wide but **approximate**: they can under-count, up to roughly N times the allowance across N nodes in the worst case (see the [`clustered-kv-v2` roadmap](/roadmap/clustered-kv-v2/) for the owner-routed `INCR` design that would make them exact).

**Hashes and `FLUSHDB` do not replicate.** The replication tier carries string keys only, so `HSET`/`HDEL` stay on the node that ran them even in a cluster (a `WARN` is logged once per process the first time it happens), and hash values cannot be given a TTL. `FLUSHDB`/`FLUSHALL` empty only the local node's store, deliberately: replicating a flush would let one node erase the whole cluster's data. Use string keys for anything that must be cluster-visible.

**Conditional writes are owner-arbitrated, not last-arrival-wins.** `SETNX` / `SET … NX` and the compare-and-delete `DELIFEQ` (`ephpm_kv_setnx` / `ephpm_kv_del_if_eq` from PHP) cannot be decided on each node's own copy — two nodes would both see "absent" and both acquire a lock. They are routed over the KV data plane to the key's owner (`hash(key) % sorted alive nodes`, whichever tier the value lives in) and decided under that node's shard lock; the owner then replicates its decision like any other write. The guarantee holds while membership is stable: across a membership change two nodes can briefly name different owners, a partition lets each side decide on its own, a new owner only knows about locks that have replicated to it (lock values must fit this gossip tier for that), only these two operations are arbitrated (a plain `SET`/`EXPIRE`/`DEL` on a lock key is still last-arrival-wins and can clobber a grant), and an unreachable owner fails closed with an unknown outcome (`-CLUSTERDOWN` on RESP, `false` from PHP; 2s timeout — a timed-out request may still have been applied on the owner). See [KV from PHP — Locks in a cluster](/guides/kv-from-php/#locks-in-a-cluster).

This is where `kv:sqlite:primary` and other cluster-wide control state lives. Designed for small, frequently-read, eventually-consistent values.

### Tier 2 — TCP data plane (large values)

Values above the threshold live on a set of "owner" nodes. The primary owner is selected as `hash(key)` modulo the sorted alive-node list (no consistent hash ring — see `crates/ephpm-cluster/src/clustered_store.rs`). Requests reach the owners over the TCP data plane (`data_port`, default 7947).

Large-tier values are **replicated** to `[cluster.kv] replication_factor` nodes (default 2): the primary owner plus the next `replication_factor - 1` distinct nodes on the sorted alive-node ring, wrapping around. The factor is clamped to the number of alive nodes.

- **Writes** go to the whole replica set. `replication_mode = "async"` (default) returns after the primary copy is written and updates the other replicas in the background; `replication_mode = "sync"` also awaits every *reachable* replica before returning (best-effort — a down replica is logged, not fatal; this is not a quorum/consensus protocol).
- **Reads** try the primary owner first, then fall back to the other replicas in ring order. This is what lets a large value survive the loss of its owner — up to `replication_factor - 1` node failures.

Replication is **write-time only**: there is no active anti-entropy or rebalancing. A node that was down during a write does not hold that key until it is rewritten or fetched-through; when membership changes, existing keys are not retroactively re-replicated. Small (gossip-tier) values are still replicated to *every* node regardless of `replication_factor`.

### Hot-key promotion

If the same node fetches a remote value `[cluster.kv] hot_key_threshold` times (default 5) within `hot_key_window_secs` (default 10s), the value is promoted to a local cache with TTL `hot_key_local_ttl_secs` (default 30s). Subsequent reads hit the local cache until expiry. The cache is bounded by `hot_key_max_memory` (default 64MB).

This protects against thundering herds without requiring a separate "client cache" library — every node automatically caches its own hot reads.

### Versioning and invalidation

Cluster-tier writes carry a version number. Local hot-key cache entries store the version they were promoted at. When gossip notifies a node of a newer version (via small-tier metadata), the local cache entry is invalidated — the next read goes back to the owner.

## Failure modes

- **Eviction storm** — large bulk writes can push the store over `memory_limit`, triggering rapid eviction. Symptoms: p99 read latency rising as shards rebalance. The KV store exports no Prometheus metrics today (no eviction counter, no memory gauge; `ephpm_kv_evictions_total` is planned, not implemented). `INFO` over RESP reports `used_memory`. Mitigation: raise `memory_limit`, enable compression, or use TTLs more aggressively.
- **Compression failure on read** — corrupted entry. Read returns an error. Rare; usually indicates an upstream bug (e.g. compression algorithm changed without a flush).
- **Cluster split brain on writes** — async replication means a partitioned node can serve stale reads after a partition heals. The next gossip update invalidates the stale entry. Sync mode avoids this at the cost of write latency.

## Why not just use Redis?

We could. But:

- One process is much easier to operate than two, especially in containers.
- The SAPI path is ~100x faster than the network path. For hot counters and rate limiters, that matters.
- The whole stack (HTTP, PHP, KV, DB proxy) shares one tokio runtime — no inter-process coordination, no extra socket budget, no separate config.

If you outgrow it, the RESP listener means swapping back to standalone Redis is a one-line config change in your PHP app — for code that sticks to the commands the KV implements.

**It is a cache, not a broker.** The KV deliberately implements strings, hashes, TTLs and counters only. There are no lists, sets, sorted sets, streams, pub/sub, `MULTI`/`EXEC` or `EVAL`. Job queues (Laravel queues, Symfony Messenger's Redis transport, Sidekiq-style workers) need those, so run a real Redis for queues and keep the ePHPm KV for caching, sessions and counters.

## See also

- [KV from PHP](/guides/kv-from-php/) — the SAPI and RESP APIs
- [`ephpm kv` CLI](/reference/cli/kv/) — debug the live store
- [Architecture → Clustering](/architecture/clustering/) — gossip, key ownership, data plane replication
- [`crates/ephpm-kv/`](https://github.com/ephpm/ephpm/tree/main/crates/ephpm-kv) — implementation

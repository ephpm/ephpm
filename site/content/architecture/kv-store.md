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

Hash entries are kept in a separate map from string entries. Unlike Redis, the two do not exclude each other: there is no `WRONGTYPE` error, and one name can hold a string and a hash at the same time (`GET` on a hash name returns nil, `TYPE` reports `hash`). `RENAME` applies to the string map only. A hash has a whole-key TTL (`EXPIRE` on the hash) and every field can carry its own TTL (`HEXPIRE`); a hash's earliest deadline is kept in a TTL index of its own, which active expiry samples alongside the string TTL index, so expired fields are reclaimed without being read. In a cluster hashes replicate **per field** over the gossip tier — see [KV RESP Protocol → Hashes in a cluster](/reference/kv-resp/#hashes-in-a-cluster) and [Strings and hashes do not collide](/reference/kv-resp/#strings-and-hashes-do-not-collide).

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

Expired keys are reclaimed before any live key is evicted (see [Storage](#storage)). Eviction then repeats until the write fits, however many victims that takes — a large value written into a store full of small keys evicts as many as it needs. A write is refused only when it cannot fit at all: the value is larger than `memory_limit` itself, no eligible key is left (for example `volatile-lru` with no TTL'd keys), or concurrent writes are taking the freed room as fast as eviction frees it (the write has evicted its whole shortfall plus its own size again and still does not fit). There is no time limit: whether a write fits does not depend on how busy the machine is.

LRU isn't strict — maintaining a true global LRU over a sharded map would be expensive. It is Redis' approximated LRU instead: each eviction step samples 16 string keys (plus 4 hashes under `allkeys-lru`) from a random position in the map, merges them into a pool of the 16 best candidates kept across steps, and evicts the least recently used one. `volatile-lru` samples from the index of TTL'd keys, so persistent keys cost it nothing however many there are. `allkeys-random` takes the key at a random position (a string key, or a hash once no string keys remain). Each victim costs a few microseconds whatever the store size. The access clock has 100 ms granularity.

## What `memory_limit` counts

`memory_limit` caps the store's own accounting (`used_memory` in `INFO`). That counts, per key:

- the key and the value **as stored** — a compressed value counts its compressed size,
- a fixed per-entry estimate for the entry's metadata and its hash-table slot,
- for a key with a TTL, its entry in the TTL index (a second copy of the key plus a slot),
- for a hash, every field name and value plus a per-field and per-hash overhead.

Each per-site store of a multi-tenant node has its own `memory_limit`; the limit is not shared across sites.

That accounting tracks the heap the store actually holds to within about 10% (verified by `crates/ephpm-kv/tests/memory_accounting.rs` with a counting allocator, for small and large values, TTL'd keys, hashes and every compression algorithm). Stored buffers carry no spare capacity, and when expiry empties a store its hash tables are shrunk, so the slots of a burst of expired keys do not stay allocated.

**Resident memory (RSS) is still higher than `memory_limit`,** and the difference is outside what a byte counter in the store can see:

- **Allocator retention and fragmentation.** Freed memory (evicted, expired and overwritten values) is kept by the allocator for reuse and returned to the OS lazily, and values of many different sizes fragment it. This is most visible with compression, whose output sizes vary widely.
- **Compression working memory.** Compressing allocates scratch space that is freed straight away but adds to what the allocator retains: brotli at level 6 allocates about 2.4 MB per compressed value, gzip about 0.4 MB, zstd about 64 KB.
- **Everything else in the process:** the PHP runtime, connection buffers (see `[kv.redis_compat]` `max_input_buffer`) and so on.

Measured on a Linux stub build (glibc, mimalloc), a 256 MB store under sustained mixed writes of 2–128 KB values ran at about 330 MB RSS uncompressed (1.3×), 360 MB with gzip (1.4×), 740 MB with zstd (2.9×) and 1.0–1.1 GB with brotli (4×). Size the container for those factors rather than for `memory_limit` alone, and prefer gzip or no compression where memory is the tight resource. A store that has been emptied keeps some of its peak RSS until the allocator reuses or returns it.

## RESP2 protocol

The on-wire format is Redis RESP2. We don't fork a Redis-compatible parser — we have a tight implementation in [`crates/ephpm-kv/src/resp/`](https://github.com/ephpm/ephpm/tree/main/crates/ephpm-kv/src/resp). Connections are tokio tasks reading line-buffered RESP frames; one connection = one task, no thread pool.

Supported command groups: strings, hashes (including `HMGET`, `HINCRBY` and per-field TTLs via `HEXPIRE`), keys (including `SCAN`), connection. See [KV RESP Protocol](/reference/kv-resp/) for every command, its deviations from Redis, and what is not implemented.

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

**Gossip state is bounded.** Each node queues its gossip-tier writes in a coalescing, bounded queue (the newest write per key; up to 65,536 distinct keys in flight) that one task drains into chitchat in batches, instead of one task per write contending for the gossip lock. Past the cap a new key is **shed** — the local write succeeded, that key just does not replicate until it is written again — and a rate-limited `WARN` says so, because the gossip tier moves a few hundred entries per second per peer and a node writing distinct small keys faster than that cannot be replicated by gossip at all. The same task sweeps the node's own gossip state every 2 s: expired values are deleted once their TTL (plus a second of slack) has passed, a `DEL` marker is deleted 120 s after the delete, and a copy of a key that a peer has since overwritten is deleted as soon as the peer's write is applied (so exactly one node publishes each key). A chitchat delete lingers as a chitchat tombstone for 60 s before every node frees it. Steady-state gossip memory is therefore the live small keys plus a minute or two of churn, not every key ever written. The limit this buys: a node that is partitioned for more than 120 s while still publishing a stale copy of a key that was deleted meanwhile can resurrect that key on heal — the same trade every tombstone-collecting store makes. Each node also bounds its per-key last-applied-stamp map (entries older than 10 minutes are pruned, hard cap 1M); a write for a pruned key applies on sight and is then re-checked against every node's state.

**`SET`, `DEL`, and `EXPIRE` replicate today.** A `DEL` broadcasts a stamped tombstone marker over the same gossip subscription — peers apply it to their local copies (both gossip-materialized and any locally-held data-plane replica) when the tombstone's stamp beats their last-applied write. `EXPIRE` re-emits the existing value with the new expiry stamp, so a session TTL refresh (which *extends* the expiry) propagates just like a shorten. `INCR` and `DECR` replicate too (since #416): each increment is atomic on its own node and then publishes the counter's **new total**, with its TTL. Peers apply totals last-writer-wins, not deltas, so concurrent increments on different nodes overwrite each other instead of summing. The built-in `ratelimit` middleware uses `INCR`, so its windows are cluster-wide but **approximate**: they can under-count, up to roughly N times the allowance across N nodes in the worst case (see the [`clustered-kv-v2` roadmap](/roadmap/clustered-kv-v2/) for the owner-routed `INCR` design that would make them exact).

**Hashes replicate per field** (since v0.13; before that they stayed on the node that wrote them). Each field is its own gossip-tier item with its own stamp, so concurrent writes to different fields on different nodes all survive; field TTLs and the hash key's TTL travel with every field; deleting a hash also retracts each field. A clustered field value is limited to `small_key_threshold` minus 17 bytes, and a larger one is refused rather than stored on one node. See [KV RESP Protocol → Hashes in a cluster](/reference/kv-resp/#hashes-in-a-cluster).

**`FLUSHDB` does not replicate.** `FLUSHDB`/`FLUSHALL` empty only the local node's store, deliberately: replicating a flush would let one node erase the whole cluster's data.

**Conditional writes are owner-arbitrated, not last-arrival-wins.** `SETNX` / `SET … NX` and the compare-and-delete `DELIFEQ` (`ephpm_kv_setnx` / `ephpm_kv_del_if_eq` from PHP) cannot be decided on each node's own copy — two nodes would both see "absent" and both acquire a lock. They are routed over the KV data plane to the key's owner (`hash(key) % sorted alive nodes`, whichever tier the value lives in) and decided under that node's shard lock; the owner then replicates its decision like any other write. The guarantee holds while membership is stable: across a membership change two nodes can briefly name different owners, a partition lets each side decide on its own, a new owner only knows about locks that have replicated to it (lock values must fit this gossip tier for that), only these two operations are arbitrated (a plain `SET`/`EXPIRE`/`DEL` on a lock key is still last-arrival-wins and can clobber a grant), and an unreachable owner fails closed with an unknown outcome (`-CLUSTERDOWN` on RESP, `false` from PHP; 2s timeout — a timed-out request may still have been applied on the owner). See [KV from PHP — Locks in a cluster](/guides/kv-from-php/#locks-in-a-cluster).

**What a reader can observe when a node (re)joins.** A node that starts into a running cluster — a rolling restart, a scaled-up replica — receives each peer's gossip state *per peer, in version order*, one ~64 KB datagram per gossip exchange and at most two exchanges per second per peer. Catching up on a large state therefore takes tens of seconds to minutes (a few MB of base64 is a minute; the two-node lab's 160k-key tier took longer than its 60 s tombstone grace and never fully converged under 1,500 writes/s), and everything the node materializes meanwhile is "current as of that peer's cursor": a copy of a key that a peer has already superseded can arrive first, with the newest version sitting further up another peer's backlog. Served as a local hit, that is a **stale read** — the live lab measured 0.3–1.1 % of a restarted node's reads for several minutes after each restart, with ages of tens of seconds to minutes (#631). So a node that has just started does **not** serve its own store: its **catch-up gate** stays *cold* until every alive peer's post-join catch-up mark has arrived (each node refreshes `ephpm:kv_mark` in its gossip state every 2 s; a mark set after the join proves everything below it has been received), and while cold every `GET` / `MGET` / `ephpm_kv_get` asks **every alive peer** over the data plane for its copy *and the stamp it applied it under* and serves the newest — the freshness of the best-informed peer, at one round trip per read. Keys the node wrote itself since it started are served locally (read-your-writes). A key no peer holds is a miss, never a local copy of unknown freshness; if no peer can be reached the read is a miss too, and once the failure detector has declared every peer dead the gate opens (nothing left to catch up on). The gate also opens after 15 minutes regardless, with a `WARN`: that is a stuck catch-up (a state too large to stream within chitchat's tombstone grace, which resets the cursor over and over) and the node then serves locally exactly as it did before the gate existed. A node with no alive peer 5 s after start opens at once. The same applies to the other reads that return a value: `EXISTS`, `STRLEN`, `TTL`/`PTTL`/`EXPIRETIME` of a string key answer from the peers, and so do per-field hash reads (`HGET`, `HMGET`, `HEXISTS`, `HSTRLEN`, `HTTL`…). **Read-modify-writes** (`INCR` and friends, `APPEND`, `GETSET`, `SET … GET|XX|KEEPTTL`, `GETDEL`, `GETEX`, `EXPIRE`, `PERSIST`, `RENAME`, per-field hash writes such as `HINCRBY`) first install the peers' newest copy locally and then run, so an `INCR` on a counter this node has not received yet continues the cluster's count instead of resetting it to `1` everywhere; with no peer reachable they are refused with `-TRYAGAIN` (PHP: `false`) rather than computed from a copy of unknown freshness. Commands over a whole hash (`HGETALL`, `HKEYS`, `HVALS`, `HLEN`, `HSCAN`) are refused until the gate opens. Still local while cold: `EXISTS`/`TYPE`/`TTL` of a hash key, `DEL` of a hash, `SCAN`/`KEYS`. A peer that accepts connections but stops answering is skipped after three timed-out reads (for 15 s at a time) instead of costing every cold read its 2 s budget. See [Read-modify-writes in a cluster](/reference/kv-resp/#read-modify-writes-in-a-cluster). The gate is observable as `ephpm_kv_catch_up_cold` and `ephpm_kv_cold_reads_total` ([metrics](/reference/metrics/#kv-catch-up-gate)) and in the log (`gossip KV catch-up complete … elapsed_ms=`). What the gate does **not** change: a long-running node is still eventually consistent — a write made on one node is visible on another after a gossip round or two (hundreds of milliseconds typically, longer when a node writes distinct small keys faster than gossip moves them), and a peer that is itself behind answers a cold read with what it has.

This is where `kv:sqlite:primary` and other cluster-wide control state lives. Designed for small, frequently-read, eventually-consistent values.

### Tier 2 — TCP data plane (large values)

Values above the threshold live on a set of "owner" nodes. The primary owner is selected as `hash(key)` modulo the sorted alive-node list (no consistent hash ring — see `crates/ephpm-cluster/src/clustered_store.rs`). Requests reach the owners over the TCP data plane (`data_port`, default 7947).

Large-tier values are **replicated** to `[cluster.kv] replication_factor` nodes (default 2): the primary owner plus the next `replication_factor - 1` distinct nodes on the sorted alive-node ring, wrapping around. The factor is clamped to the number of alive nodes.

- **Writes** land on the writing node first (whatever its replica role, so it reads its own write back) and then go to the whole replica set over the data plane, each copy carrying the value's **TTL** and **stamp** — a replica applies a copy only if it is strictly newer than what it holds, through the same last-applied-stamp map the gossip tier uses. Every large write also gossips a tombstone stamped just *before* the value, so a node outside the replica set that still holds an older copy (the previous small value, or an earlier large write it made itself) drops it and fetches through on its next read, while replicas order the marker below the value they received. `replication_mode = "async"` (default) answers the client once the local copy is written and hands the fan-out to a bounded pool of data-plane workers (16 workers, 4,096 queued writes; beyond that a write is shed with a counter and a rate-limited `WARN` — the local copy stands, the replicas get it when the key is written again); `replication_mode = "sync"` makes the client wait until every *reachable* replica has acknowledged (best-effort — a down replica is logged, not fatal; this is not a quorum/consensus protocol).
- **Reads** (`GET`/`MGET` over RESP, `ephpm_kv_get` from PHP) are served locally on the owner and its replicas. On any other node a miss **fetches through**: the owners are asked in ring order within a 2 s budget, which is also what lets a large value survive the loss of its owner — up to `replication_factor - 1` node failures. The fetched value is not kept locally (it arrives without its TTL) beyond the hot-key cache below. The cost, stated plainly: with N nodes and replication factor R, a `GET` of a key that exists nowhere pays one data-plane round trip on `(N−R)/N` of the nodes; misses on owners and replicas stay local. `EXISTS` and `TTL` stay local-only. A **read-modify-write** (`INCR`, `APPEND`, `GETEX`/`EXPIRE`, `SET … XX|GET|KEEPTTL`, `GETDEL`, …) on a node with no local copy asks the replica set first and runs on its copy, keeping its TTL, then replicates the result to the replica set — instead of treating the miss as "absent" and replacing the value cluster-wide; when no replica answers it is refused with `-TRYAGAIN` (PHP `false`). See [Read-modify-writes in a cluster](/reference/kv-resp/#read-modify-writes-in-a-cluster).
- **`EXPIRE` and `DEL`** on a large value reach its replicas: `EXPIRE` re-sends the value with its new TTL over the data plane (which also repairs a replica that missed it), `DEL` rides the gossip tombstone every node applies.

Replication is **write-time only**: there is no active anti-entropy or rebalancing. A node that was down during a write does not hold that key until it is rewritten or fetched-through; when membership changes, existing keys are not retroactively re-replicated. Small (gossip-tier) values are still replicated to *every* node regardless of `replication_factor`.

Data-plane connections are pooled per peer and every step is bounded (2 s to connect, 10 s per exchange, a 2 s per-peer cool-down after a refused connect), so a frozen peer costs a timeout rather than a growing pile of sockets. A node running a build that predates the stamped data-plane write does not accept large values from an upgraded node (the write is logged and counted as failed; reads still fetch through), so upgrade the whole cluster.

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

- [KV snapshots](/architecture/kv-snapshots/) — the encrypted warm-restart snapshot format and lifecycle (`[kv.snapshot]`)

- [KV from PHP](/guides/kv-from-php/) — the SAPI and RESP APIs
- [`ephpm kv` CLI](/reference/cli/kv/) — debug the live store
- [Architecture → Clustering](/architecture/clustering/) — gossip, key ownership, data plane replication
- [`crates/ephpm-kv/`](https://github.com/ephpm/ephpm/tree/main/crates/ephpm-kv) — implementation

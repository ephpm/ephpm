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

Hash entries are kept separate from string entries because Redis types are mutually exclusive — `WRONGTYPE` errors flow naturally from this split.

## Storage

[DashMap](https://docs.rs/dashmap/) — lock-sharded concurrent hash map. Reads are wait-free for the common case; writes scale with the number of shards (default = `4 * num_cpus`). For ePHPm's workloads (cache + counters + sessions) this comfortably outpaces a hand-rolled `Mutex<HashMap>` and avoids the latency tail of a global lock.

Entries carry an optional expiry timestamp. A background reaper sweeps expired entries; reads also opportunistically delete expired entries on access (so even without the reaper, you never see stale data via a Get).

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

- **noeviction** — refuse new writes with an error
- **allkeys-lru** — least recently used across all keys
- **volatile-lru** — least recently used among keys with a TTL only
- **allkeys-random** — uniformly random victim

LRU isn't strict — maintaining a true global LRU over a sharded map would be expensive. Instead, each eviction pass samples 16 candidates from map iteration, compares them by `last_accessed`, and evicts the best victim; passes repeat until usage drops back under the limit. Approximate but cheap.

## RESP2 protocol

The on-wire format is Redis RESP2. We don't fork a Redis-compatible parser — we have a tight implementation in [`crates/ephpm-kv/src/resp/`](https://github.com/ephpm/ephpm/tree/main/crates/ephpm-kv/src/resp). Connections are tokio tasks reading line-buffered RESP frames; one connection = one task, no thread pool.

Supported command groups: strings, hashes (`HSET`, `HGET`, `HDEL`, `HGETALL`, `HKEYS`, `HVALS`, `HLEN`, `HEXISTS`), keys, connection. See [KV from PHP](/guides/kv-from-php/) for the full command list.

## Multi-tenant isolation

In vhost mode (`[server] sites_dir = ...`), the RESP listener is a sharp tool: it gives raw access to *every* site's keys. Recommended posture is to leave `[kv.redis_compat] enabled = false` in multi-tenant deployments and let PHP use the SAPI functions, which are automatically namespaced per host.

When the RESP listener is enabled and AUTH is required, ePHPm derives per-site passwords from `[kv] secret`:

```
password = HMAC-SHA256(secret, hostname)
```

The derived password is injected into PHP's `$_ENV` as `EPHPM_REDIS_PASSWORD` for each request, so each site's PHP code can authenticate to its own scope. If `[kv] secret` is unset, nothing is auto-generated — multi-tenant HMAC AUTH is simply disabled.

## Clustered KV (when `[cluster] enabled = true`)

The KV store goes from in-process to a distributed two-tier system:

### Tier 1 — gossip-backed (small values)

Values up to `[cluster.kv] small_key_threshold` (default 512 bytes) ride the gossip protocol via chitchat. They're encoded into the gossip state with base64 + millisecond expiry + an origin-stamped `write_ms`. Convergence is fast (hundreds of ms typical). Eventually consistent.

Applies are **last-arrival-wins** by origin `write_ms`: each node keeps a per-key `last-applied` timestamp shared between the gossip applier and the origin-side write path, and skips any incoming write whose `write_ms` is not strictly newer. This prevents a slow gossip echo of an older write from clobbering a newer write already materialized locally. The origin itself records its own `write_ms` in the same map, so even the echo of its own gossip broadcast cannot overwrite a follow-up local write.

**`SET`, `DEL`, and `EXPIRE` replicate today.** A `DEL` broadcasts a write-stamped tombstone marker over the same gossip subscription — peers apply it to their local copies (both gossip-materialized and any locally-held data-plane replica) when the tombstone's `write_ms` beats their last-applied write. `EXPIRE` re-emits the existing value with the new expiry stamp, so a session TTL refresh (which *extends* the expiry) propagates just like a shorten. `INCR`, `DECR`, and other read-modify-write ops are still **local-only** — they mutate the owner node's counter without gossiping the new value. The built-in `ratelimit` middleware uses `INCR`, so rate-limit windows are enforced **per node**, not cluster-wide (see [`clustered-kv-v2` roadmap](/roadmap/clustered-kv-v2/) for the owner-routed `INCR` design).

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

- **Eviction storm** — large bulk writes can push the store over `memory_limit`, triggering rapid eviction. Symptoms: spike in `ephpm_kv_evictions_total` (when implemented), p99 read latency rising as shards rebalance. Mitigation: raise `memory_limit`, enable compression, or use TTLs more aggressively.
- **Compression failure on read** — corrupted entry. Read returns an error. Rare; usually indicates an upstream bug (e.g. compression algorithm changed without a flush).
- **Cluster split brain on writes** — async replication means a partitioned node can serve stale reads after a partition heals. The next gossip update invalidates the stale entry. Sync mode avoids this at the cost of write latency.

## Why not just use Redis?

We could. But:

- One process is much easier to operate than two, especially in containers.
- The SAPI path is ~100x faster than the network path. For hot counters and rate limiters, that matters.
- The whole stack (HTTP, PHP, KV, DB proxy) shares one tokio runtime — no inter-process coordination, no extra socket budget, no separate config.

If you outgrow it, the RESP listener means swapping back to standalone Redis is a one-line config change in your PHP app.

## See also

- [KV from PHP](/guides/kv-from-php/) — the SAPI and RESP APIs
- [`ephpm kv` CLI](/reference/cli/kv/) — debug the live store
- [Architecture → Clustering](/architecture/clustering/) — gossip, key ownership, data plane replication
- [`crates/ephpm-kv/`](https://github.com/ephpm/ephpm/tree/main/crates/ephpm-kv) — implementation

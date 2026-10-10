# Clustering & KV Store Architecture

This document covers the in-memory KV store, gossip-based clustering, `hash % nodes` key ownership, large-value replication, and all features that depend on the KV layer (ACME cert coordination, PHP response cache, session storage).

> **Doc status:** this is a broad design document written ahead of implementation. Sections describing the KV store, PHP access paths, sessions, gossip transport security, key ownership, and replication have been reconciled with the shipped code (`crates/ephpm-cluster/`, `crates/ephpm-kv/`). Code listings are illustrative sketches, not the shipped source. Features that do not exist yet (the PHP response cache, the Node API endpoints, RESP lists/sets) are labelled **Planned** inline.
>
> This page covers the **KV** side of clustering. For clustered **SQLite** (Turso CDC replication, primary election, and per-site clustered databases) see [Database → Clustered](/architecture/database/#clustered-turso-cdc--experimental) and [Clustering setup](/guides/clustering-setup/).

---

## Why Build Our Own KV Store

No competitor (FrankenPHP, RoadRunner, Swoole) has built-in multi-node KV clustering. They all require external Redis or Memcached. ePHPm's integrated KV store is a headline differentiator.

We evaluated embedding existing databases (Redis, Dragonfly, KeyDB, Garnet) and rejected them:

| Problem | Detail |
|---------|--------|
| **License** | Dragonfly's BSL 1.1 prohibits use in "in-memory data store" products. Redis has similar restrictions post-7.0. |
| **Not embeddable** | These are standalone servers (100k+ lines), not libraries. No `libdragonfly.a` or `libredis.a` to link. |
| **Runtime conflicts** | Dragonfly uses `io_uring` + Boost.Fiber, Redis uses `ae` event loop. Conflicting with tokio is a performance hazard. |
| **Threading model** | Dragonfly uses shared-nothing (each thread owns a keyspace slice). ePHPm needs any PHP worker to access any key. |
| **Overkill** | Designed for millions of QPS across terabytes. ePHPm needs sessions, app cache, and config — megabytes to low gigabytes. |

## Requirements

| Requirement | What competitors offer | ePHPm goal |
|-------------|------------------------|------------|
| Single-node KV | RoadRunner: in-memory/BoltDB/Redis drivers. Swoole: `Swoole\Table`. | In-process concurrent hashmap, zero-overhead access from PHP via SAPI. |
| Data structures | Redis-style: strings, hashes, lists, sets, sorted sets | Strings + hashes only — a cache, not a broker. No lists, sets, sorted sets or streams; workloads that need them (queues especially) should use a real Redis. |
| TTL / expiry | Standard | Background sweeper + lazy expiry on access. |
| Clustering | **Nobody has this.** All competitors require external Redis/Memcached. | Gossip-based peer discovery, `hash % nodes` key ownership, cross-node routing with replication. |
| PHP access | RoadRunner: Goridge RPC. Swoole: shared memory API. | SAPI function calls — zero serialization, zero network hop for local keys. |
| Persistence | Optional | **Planned — not yet implemented.** Optional AOF/snapshot for crash recovery. This is a cache, not a database. |
| Redis protocol compat | Not required | Optional RESP listener (TCP) so existing Redis clients and tools work for cache-style commands. Not a full Redis: see the gaps under [Redis Protocol](#1-redis-protocol-resp--redis-compatible-for-caching-not-a-redis-replacement). |

---

## Single-Node KV Store

The foundation. Must be implemented and stable before any clustering work begins.

### Core Data Structure

The core is a `DashMap` — a sharded, lock-free concurrent hashmap. Millions of ops/sec with zero contention on reads and fine-grained write locks.

```rust
use dashmap::DashMap;
use std::time::Instant;

struct KvEntry {
    value: KvValue,
    expires_at: Option<Instant>,
}

/// Strings and hashes are implemented. Covers cache, sessions, object cache.
/// Lists, sets, sorted sets added later if needed (see Data Structure Roadmap below).
/// Note: in the actual implementation, strings use `DashMap<String, Entry>` and
/// hashes use a separate `DashMap<String, HashEntry>` for type safety.
enum KvValue {
    String(Vec<u8>),
    Hash(HashMap<Vec<u8>, Vec<u8>>),
}

struct KvStore {
    data: DashMap<Vec<u8>, KvEntry>,
    memory_limit: usize,
    eviction_policy: EvictionPolicy,
}

enum EvictionPolicy {
    NoEviction,    // return error when full
    AllKeysLru,    // evict least recently used
    VolatileLru,   // evict LRU among keys with TTL
    AllKeysRandom,
}
```

### Operations

```rust
impl KvStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let entry = self.data.get(key)?;
        // Lazy expiry: check TTL on access
        if let Some(exp) = entry.expires_at {
            if Instant::now() > exp {
                drop(entry);
                self.data.remove(key);
                return None;
            }
        }
        match &entry.value {
            KvValue::String(v) => Some(v.clone()),
            _ => None, // type mismatch
        }
    }

    fn set(&self, key: Vec<u8>, value: Vec<u8>, ttl: Option<Duration>) {
        let entry = KvEntry {
            value: KvValue::String(value),
            expires_at: ttl.map(|d| Instant::now() + d),
        };
        self.data.insert(key, entry);
    }
}
```

### TTL Expiry

Dual strategy: lazy expiry on access (check TTL on every `get`) + active expiry via background sweeper:

```rust
async fn expiry_sweeper(store: Arc<KvStore>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let now = Instant::now();
        store.data.retain(|_, entry| {
            entry.expires_at.map_or(true, |exp| now < exp)
        });
    }
}
```

This prevents memory leaks from keys that are set-and-forgotten.

### Crate Dependencies

| Crate | Purpose |
|-------|---------|
| `dashmap` | Lock-free concurrent hashmap |

### Configuration

```toml
[kv]
memory_limit = "256MB"
eviction_policy = "allkeys-lru"  # noeviction, allkeys-lru, volatile-lru, allkeys-random
```

---

## PHP Access

PHP reaches the KV store three ways. The SAPI functions and the native session handler run in-process; the RESP listener is a TCP socket for existing Redis clients. All three see the same store, and with clustering on, string writes (`SET`, `DEL`, `EXPIRE`, `INCR`) are replicated by the cluster layer described below. Hash commands (`HSET`/`HDEL`) and `FLUSHDB` stay node-local — see [KV Store → replication](/architecture/kv-store/).

### 1. Redis Protocol (RESP) — Redis-compatible for caching, not a Redis replacement

ePHPm can expose a RESP2 listener that PHP's existing Redis clients (`phpredis`, `predis/predis`) connect to as if it were a Redis server. It is **off by default**:

```toml
[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"   # TCP only
```

**What works.** Plain caching: strings with TTLs, counters, and hashes. That covers most object-cache and page-cache usage. The exact command list (`GET`/`SET` with `EX`/`PX`/`NX`/`XX`/`GET`, `MGET`/`MSET`, `INCR`/`DECR`, `EXPIRE`/`TTL`, `HSET`/`HGET`/`HGETALL` and the other basic hash commands, `KEYS`, `DBSIZE`, `FLUSHDB`, `DELIFEQ`, `AUTH`, `INFO`, …) is in [KV from PHP](/guides/kv-from-php/). Anything else returns `ERR unknown command`.

**What does not exist** — check this list before pointing a framework at the listener:

| Missing | What breaks |
|---------|-------------|
| Unix-socket listener | `[kv.redis_compat] socket` is parsed but **not implemented**; startup warns and the listener stays TCP-only. Connection strings that use `unix://` or a socket path will not connect. |
| Lists (`LPUSH`, `BRPOP`, …) | **Laravel queues**, Symfony Messenger's Redis transport, and any other Redis-backed job queue. |
| Sets and sorted sets | Laravel cache **tags**, leaderboards, rate limiters built on sorted sets. |
| `MULTI`/`EXEC` | Transactions and pipelined-transaction helpers (e.g. Laravel's `RedisStore::putMany`). |
| `EVAL`/`EVALSHA` | Lua scripts, including Laravel's atomic lock release. |
| `SCAN`, `HMGET`, `HMSET`, `HINCRBY`, `UNLINK`, `EXPIREAT` | Clients and libraries that use them (e.g. `SCAN`-based cache clears). |
| Pub/sub | Broadcasting, cache-invalidation channels. |

Two further traps:

- **`INFO` reports `redis_version:7.0.0`** (a fixed string, with `tcp_port:6379` whatever the real port is). Clients that feature-detect on the version will assume a modern Redis and may send commands from the table above.
- **One logical database.** `SELECT` accepts only `0`.

**The KV is a cache, not a broker.** If you need queues, streams, pub/sub, or Lua, run a real Redis for that workload and keep ePHPm's KV for caching, sessions and counters. For Laravel that means `QUEUE_CONNECTION=redis` against a real Redis (or the `database`/`sqs` queue drivers), with the cache pointed at ePHPm.

For framework integration, prefer the in-process packages over RESP. They skip the socket and avoid the command gaps above: `ephpm/cache-laravel`, `ephpm/cache-symfony`, `ephpm/cache-wordpress` and `ephpm/predis-connection` (a drop-in `Predis\Client` connection with no socket). See [PHP packages](/reference/php-packages/#cache-kv-and-session-packages).

In multi-site mode (`[server] sites_dir`), the listener requires `[kv] secret` and each connection authenticates as one site — see [`[kv]`](/reference/config/#kv) and [KV Store → Multi-tenant isolation](/architecture/kv-store/#multi-tenant-isolation).

### 2. Native session handler — opt-in

ePHPm registers a native PHP session save handler named `ephpm` that stores sessions in the KV store. **It is not enabled automatically**: with no configuration, sessions use PHP's default `files` handler, which is node-local (and, in multi-site mode, a per-site directory). Behind a load balancer that means sessions are **not** shared across nodes until you opt in:

```toml
[php]
ini_overrides = [
    ["session.save_handler", "ephpm"],
]
```

What the handler does:

| Callback | Behaviour |
|----------|-----------|
| `open()` | Nothing to connect to; always succeeds. |
| `read($id)` | Takes a per-session lock (`session_lock:<id>`, see below), then reads `session:<id>`. |
| `write($id, $data)` | Stores `session:<id>` with a TTL of `session.gc_maxlifetime`. |
| `close()` | **Releases the session lock.** |
| `destroy($id)` | Deletes `session:<id>`. |
| `gc()` | No-op; the KV's TTL expiry removes old sessions. |

The [`ephpm/session-handler`](/reference/php-packages/#cache-kv-and-session-packages) Composer package is a userland alternative. Both store sessions in the KV, so a session is subject to `[kv] memory_limit` and the eviction policy like any other key.

### 3. Direct SAPI functions

The `ephpm_kv_*` functions call the store directly from PHP, with no socket and no serialization:

```php
ephpm_kv_set("user:123:profile", $json, 3600);   // optional 3rd arg: TTL in seconds
$json = ephpm_kv_get("user:123:profile");
ephpm_kv_del("user:123:profile");

$count  = ephpm_kv_incr("page:views");
$exists = ephpm_kv_exists("cache:key");
```

With clustering on, the call is the same whether the key is local or owned by another node; remote keys cost a data-plane round trip. The full function list is in [KV from PHP](/guides/kv-from-php/).

#### Composer packages

The `ephpm/cache` package wraps the SAPI functions in PSR-16 (SimpleCache) and PSR-6 interfaces, and the framework packages (`ephpm/cache-laravel`, `ephpm/cache-symfony`, `ephpm/cache-wordpress`, `ephpm/predis-connection`) build on it. They are distributed from their GitHub repositories, not Packagist — see [PHP packages](/reference/php-packages/) for the repository setup and each package's README for its API.

### Access Path Comparison

The latency figures are rough design estimates, not benchmarks.

| Path | Latency (local key) | Code changes | Best for |
|------|---------------------|-------------|----------|
| **RESP over TCP** | ~10-50µs | Point the Redis client at the listener | Existing Redis cache code that uses only the supported commands |
| **Native session handler** | in-process | One `ini_overrides` line | PHP sessions |
| **SAPI functions** | ~100-200ns | ePHPm-specific calls | New code, hot paths |
| **Composer cache packages** | in-process | Swap the cache driver | Framework caches without a socket |

### Performance Expectations

Design estimates, not measured benchmarks:

| Operation | SAPI (local) | RESP TCP | SAPI (remote) | External Redis |
|-----------|-------------|----------|---------------|----------------|
| GET (string) | ~100-200ns | ~10-50us | ~0.5-2ms | ~0.5-2ms |
| SET (string) | ~200-400ns | ~10-50us | ~0.5-2ms | ~0.5-2ms |
| Serialization | None (shared memory) | RESP encode/decode | Internal binary | Full RESP |
| Connection | None (in-process FFI) | Localhost TCP | Persistent internal | TCP pool |

---

## Clustering

### Overview

Three components: gossip-based peer discovery, `hash % nodes` key ownership for routing, and replication for fault tolerance.

```
   ┌─────────────────┐   gossip    ┌─────────────────┐   gossip    ┌─────────────────┐
   │ Node A          │ ◄─────────► │ Node B          │ ◄─────────► │ Node C          │
   │ KvStore         │             │ KvStore         │             │ KvStore         │
   │ (local shard)   │             │ (local shard)   │             │ (local shard)   │
   └────────┬────────┘             └────────┬────────┘             └────────┬────────┘
            │                               │                               │
            └────────────── gossip ─────────┴───────────────────────────────┘
            │                               │                               │
            │      all three share one sorted alive-node list (gossip)      │
            ▼                               ▼                               ▼
            ┌───────────────────────────────────────────────────────────────┐
            │            key ownership: hash(key) % alive_nodes             │
            │              (key → owner + replica-set mapping)              │
            └───────────────────────────────────────────────────────────────┘
```

### 1. Gossip-Based Peer Discovery

Nodes find each other via a gossip protocol ([`chitchat`](https://github.com/quickwit-oss/chitchat) — Quickwit's SWIM-based gossip library for Rust, or a custom SWIM implementation).

```
   Node A                    Node B                    Node C
   alive · gen=5             alive · gen=3             alive · gen=7
   load=45%                  load=62%                  load=38%
       ▲                         ▲                         ▲
       │                         │                         │
       └─────── gossip ──────────┴─────── gossip ──────────┘
       │                                                   │
       └────────────────────── gossip ─────────────────────┘
```

Each node broadcasts:
- Its identity (address, port)
- Heartbeat generation (monotonically increasing)
- KV store metadata (memory usage, key count)
- Health status

Gossip handles node join, node leave, and failure detection automatically. No external coordination service (no etcd, no ZooKeeper, no Consul).

Configuration is minimal:
```toml
[cluster]
enabled = true
bind = "0.0.0.0:7946"
join = ["10.0.1.2:7946", "10.0.1.3:7946"]  # seed nodes
```

#### Kubernetes Service Discovery

In Kubernetes, hardcoded IPs don't work — pods are ephemeral. Instead, use a **headless Service** which returns individual pod IPs via DNS (A records) rather than a single cluster IP:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: ephpm-headless
spec:
  clusterIP: None          # headless — returns pod IPs directly
  selector:
    app: ephpm
  ports:
    - name: gossip
      port: 7946
      protocol: UDP
    - name: data
      port: 7947
      protocol: TCP
    # Only needed with clustered SQLite (Turso CDC replication):
    # - name: cluster-channel
    #   port: 7948
    #   protocol: TCP
```

The gossip config uses the service DNS name instead of IPs:

```toml
[cluster]
enabled = true
join = ["ephpm-headless.default.svc.cluster.local"]
```

On startup, the node resolves the DNS name to multiple A records (one per pod), and treats each as a gossip seed. The resolution logic:

1. **Initial join**: Resolve `join` entries — if a value contains no `:port` suffix, append the default gossip port. If resolution returns multiple IPs, use all of them as seeds.
2. **Periodic re-resolution**: Re-resolve DNS every 30s so newly scaled pods are discovered even if all original seeds have been replaced. This handles rolling deployments where every pod eventually gets a new IP.
3. **Self-filtering**: Skip our own pod IP from the seed list to avoid self-connection.

The cluster secret comes from a Kubernetes Secret via env var override:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: ephpm-cluster
type: Opaque
data:
  secret: <base64-encoded-32-byte-key>
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: ephpm
spec:
  serviceName: ephpm-headless
  template:
    spec:
      containers:
        - name: ephpm
          env:
            - name: EPHPM_CLUSTER__SECRET
              valueFrom:
                secretKeyRef:
                  name: ephpm-cluster
                  key: secret
            - name: EPHPM_CLUSTER__ENABLED
              value: "true"
            - name: EPHPM_CLUSTER__JOIN
              value: "ephpm-headless.default.svc.cluster.local"
```

A `StatefulSet` is preferred over a `Deployment` for the KV store — pods get stable network identities (`ephpm-0`, `ephpm-1`, etc.) which makes debugging and log correlation easier, though gossip works with either.

### 2. Inter-Node Security

Cluster communication has two always-on channels plus one opt-in channel, and **all of them** are secured by the same symmetric pre-shared key (`[cluster] secret`) — there is no TLS or mTLS anywhere in the cluster transport. From the shared secret, ePHPm derives domain-separated ChaCha20-Poly1305 keys via HKDF-SHA256 (`crates/ephpm-cluster/src/secure_transport.rs`):

- gossip UDP: HKDF info `"ephpm-gossip-v1"`
- KV data plane TCP: HKDF info `"ephpm-kv-data-v1"`
- cluster channel handshake: HKDF info `"ephpm-cluster-channel-v1"`
- cluster channel session frames: HKDF info `"ephpm-cluster-channel-session-v1"` (salted per-session from the handshake transcript)

Because each plane derives a different key, a ciphertext valid on one plane is meaningless on the others.

#### Gossip Channel (UDP)

The SWIM gossip protocol uses UDP for high-frequency heartbeats and metadata exchange. Every datagram is sealed with the gossip key (nonce + Poly1305 tag). Datagrams that fail authentication are silently dropped, so a node without the matching secret — or one sending plaintext — is invisible to the cluster rather than an error. This prevents unauthorized nodes from joining and protects metadata (node addresses, health, KV stats) in transit.

#### Data Plane (TCP + symmetric PSK)

KV operations between nodes (remote get/set and large-value replication) use TCP. When `[cluster] secret` is set, each request and response is sealed as a single ChaCha20-Poly1305 frame with the data-plane key:

```text
[frame_len: u32 BE][nonce: 12 bytes][ciphertext + 16-byte tag]
```

This is **symmetric pre-shared-key authentication, not mutual TLS**. There are no certificates, no CA, no fingerprint exchange, and no `tls_cert`/`tls_key`/`tls_ca` cluster options. Both sides prove possession of the same secret by being able to seal/open frames; a peer with the wrong secret cannot read values or inject writes (frames fail authentication and the connection is dropped).

**Fail-closed on an empty secret.** Because an unset secret means unauthenticated plaintext gossip and data plane, ePHPm **refuses to start** clustering (`enabled = true`) when `secret` is empty. To run without a secret on a trusted private network, opt in explicitly with `allow_insecure_no_auth = true`; a loud warning is logged. This closes the previous fail-open behavior where an empty secret only produced a startup warning.

#### Cluster Channel (TCP, opt-in)

Opt-in cluster features — today, clustered SQLite's Turso CDC replication and its snapshot bootstrap (enabled whenever `[db.sqlite]` runs with clustering on) — share one additional multiplexed TCP listener, the [cluster channel](/roadmap/cluster-channel/). It binds **only when a feature asks for it** (default port: gossip port + 2, so `7948`; override with `[cluster.channel] listen`) — configs without such a feature open no extra port and are byte-identical to before.

Security is one step stronger than the KV data plane: before any payload flows, both sides complete a **mutual challenge/response handshake** (each proves possession of the secret by sealing the other's challenge), and every post-handshake byte is sealed under a **per-session key** derived from the handshake transcript — so a recorded session cannot be replayed into a new one. Inbound connections are additionally checked against live gossip membership before the handshake is attempted. Like the other planes this is symmetric PSK, not mTLS: there is no per-node identity, and any holder of the secret is a full peer. `[cluster.channel] secret` can override the key for this plane alone; firewall guidance: open TCP 7948 between nodes only when clustered SQLite (Turso CDC replication) is in use.

The shared secret is the only security config — one value to set across all nodes, covering every plane.

#### Security Summary

```
                    ┌─────────────────────────────────┐
                    │         Cluster Secret           │
                    │   (symmetric, pre-shared key)    │
                    └──────────┬──────────────────────┘
                               │  HKDF-SHA256 (domain-separated)
               ┌───────────────┼───────────────┐
               │               │               │
               ▼               ▼               ▼
          ┌─────────┐    ┌─────────┐    ┌─────────┐
          │ Node A  │    │ Node B  │    │ Node C  │
          └────┬────┘    └────┬────┘    └────┬────┘
               │              │              │
  Gossip (UDP) │◄─sealed─────►│◄─sealed─────►│
  ChaCha20     │  (gossip key)│              │
               │              │              │
  Data (TCP)   │◄─sealed─────►│◄─sealed─────►│
  ChaCha20     │  (data key)  │              │
  (PSK, no TLS)│              │              │
```

| Channel | Transport | Encryption | Authentication |
|---------|-----------|------------|----------------|
| Gossip | UDP | ChaCha20-Poly1305 (gossip key from cluster secret) | Symmetric PSK — undecryptable datagrams dropped |
| Data plane | TCP | ChaCha20-Poly1305 (data-plane key from cluster secret) | Symmetric PSK — unauthenticated frames dropped, connection closed |

### 3. Key Ownership (`hash % nodes`)

Large-tier keys are distributed across nodes by a plain modulo of the **sorted alive-node list** — there is no consistent hash ring, no virtual nodes, and no `hashring` crate dependency (see `crates/ephpm-cluster/src/clustered_store.rs`):

```
owner = alive_nodes_sorted[ hash(key) % alive_nodes_sorted.len() ]
```

```
            ┌───────────────────────────┐
            │  sorted alive nodes:       │
            │  [ Node A, Node B, Node C ]│
            │                            │
            │  Key "session:abc"         │
            │  hash % 3 = 0 → Node A     │
            └───────────────────────────┘
```

The trade-off vs a consistent hash ring: when membership changes (a node joins, leaves, or dies), ownership remaps **broadly** — most keys change owner, not the ~1/N a ring would give. For ePHPm's cache-oriented workload (values that can be refetched or recomputed) this is acceptable; a consistent-hashing scheme may replace it later if remapping cost becomes a problem.

```rust
struct ClusterRouter {
    alive_nodes: RwLock<Vec<NodeId>>,   // sorted; from gossip membership
    local_node: NodeId,
}

impl ClusterRouter {
    /// Primary owner of a key.
    fn owner(&self, key: &[u8]) -> NodeId {
        let nodes = self.alive_nodes.read();
        nodes[(hash(key) as usize) % nodes.len()].clone()
    }
}
```

Crate dependencies:

| Crate | Purpose |
|-------|---------|
| `chitchat` | Quickwit's SWIM gossip library |

(Key ownership is a few lines of arithmetic over the gossip membership list — no external hashing crate.)

### 4. Replication

Large-tier writes go to a **replica set**: the primary owner plus the next `replication_factor - 1` distinct nodes on the sorted alive-node ring (wrapping around, clamped to the cluster size). This provides fault tolerance — if the owner dies, a replica can serve reads. Small (gossip-tier) values are separately replicated to *every* node and ignore `replication_factor`.

```
Write large "cart:abc" = <8 KiB>, replication_factor = 2
       │
       ▼
   hash(key) % nodes → Node A (primary owner)
       │
       ├──► write to Node A (primary)
       └──► replicate to Node B (next node on the ring)

Read "cart:abc":  try Node A first → on miss/owner-down, fall back to Node B
```

Replication modes (`[cluster.kv] replication_mode`):
- **async (default)** — the client write returns after the primary/local copy is written; the remaining replicas are updated in the background (fire-and-forget, failures logged and counted). Lower latency, eventual replica convergence.
- **sync** — the write also awaits every *reachable* replica before returning. This is best-effort read-your-writes durability against live peers: a replica that is down is logged but does **not** fail the write. It is **not** a quorum or consensus protocol (no Raft, no rollback).

Read path: reads try the primary owner first, then fall back to the other replicas in ring order until one returns the value. A large value therefore survives up to `replication_factor - 1` node failures.

> **Membership caveat (v1):** replication is write-time only. There is no active anti-entropy or rebalancing — a node that was down during a write does not receive the key until it is rewritten or fetched-through, and existing keys are not re-replicated when membership changes.

For sessions and cache data, async replication is the right default. PHP apps doing `$_SESSION['cart'] = ...` on one request and reading it on the next hit the same owner (ownership is a deterministic function of the session id and the current membership), so within a stable cluster they get read-your-writes for free.

```toml
[cluster.kv]
replication_factor = 2        # copies of each large value (owner + N-1 peers)
replication_mode = "async"    # async (default) or sync
```

### 5. Node Join / Leave / Failure

Because ownership is `hash % alive_nodes` and replication is write-time only (no active rebalancing), membership changes remap ownership but do **not** move existing data:

| Event | What happens |
|-------|-------------|
| **Node joins** | Gossip announces the new member. Ownership is recomputed against the larger node list — most large-tier keys remap to new owner/replica sets. A remapped key is not present on its *new* replicas until it is rewritten or fetched-through; the old replicas may still serve it via read fallback. |
| **Node leaves gracefully** | Departure propagates via gossip; ownership is recomputed. Large values it held survive if another replica in their set is still up (`replication_factor ≥ 2`); with `replication_factor = 1` they are gone. |
| **Node crashes** | Gossip failure detector triggers after missed heartbeats. Surviving nodes recompute ownership and read via replicas — a large value survives up to `replication_factor - 1` simultaneous crashes. Small gossip-tier values always survive (every node has a copy). There is no background re-replication to restore the lost copy count yet. |
| **Network partition** | Nodes on each side continue serving their local keys and any replicas they hold. On heal, conflict resolution via last-write-wins (LWW) timestamps. |

---

## Features Built on the KV Store

The KV layer is a foundation for several higher-level features. Each section below depends on the single-node KV store existing first, and the clustering sections depend on gossip being operational.

### TLS Certificate Management (Clustered)

Single-node ACME is already implemented using filesystem-based `DirCache`. The clustered KV store replaces it with `KvCache`, enabling zero-config HTTPS across multiple nodes. See [http.md](http.md) for single-node ACME details.

#### Problem 1: Cert Issuance Race Condition

Without coordination, multiple nodes detect no cert exists and all request one from Let's Encrypt simultaneously. This wastes ACME quota (50 certs/domain/week) and may trigger rate-limiting bans.

**Solution: Distributed lock via KV store.**

```
Node A: KvStore.lock("acme:lock:example.com", node_id="A", ttl=300s)
  → Lock acquired. Node A proceeds with ACME flow.

Node B: KvStore.lock("acme:lock:example.com", node_id="B", ttl=300s)
  → Lock held by Node A. Node B waits.

Node A completes issuance:
  KvStore.set("certs:example.com:cert", cert_pem)
  KvStore.set("certs:example.com:key", key_pem)
  KvStore.unlock("acme:lock:example.com")
       │
       ├──► replicated to Node B (gossip)
       └──► replicated to Node C (gossip)

Node B: lock released, checks KvStore → cert exists → done.
```

Lock has a TTL (default 5 minutes) to prevent deadlock if a node crashes mid-issuance.

#### Problem 2: ACME Challenge Routing

In a multi-node setup behind a load balancer, Let's Encrypt's challenge request may hit any node — not the one that initiated the ACME order.

**Solution: Challenge token propagation via KV store.**

```
Node A initiates ACME order for example.com
       │
       ▼
   LE returns challenge: token=abc123, response=xyz789
       │
       ▼
   KvStore.set("acme:challenge:abc123", "xyz789", ttl=600s)
       │
       ├──► replicated to Node B (gossip, ~100ms)
       └──► replicated to Node C (gossip, ~100ms)

LE requests: GET http://example.com/.well-known/acme-challenge/abc123
       │
       ▼ (load balancer routes to Node B)
   Node B: KvStore.get("acme:challenge:abc123") → "xyz789"
   Node B: responds 200 OK → challenge passed ✓
```

Works for both HTTP-01 and TLS-ALPN-01 challenges. Gossip replication (~100-200ms) completes well within the ACME protocol's built-in polling delay.

#### Problem 3: Renewal Stampede

All nodes notice the cert expires soon. Without coordination, all attempt renewal simultaneously.

**Solution: Leader election for cert renewal.**

```
KvStore.set("acme:leader", node_id="A", ttl=60s)
  → Node A is the cert renewal leader
  → Node A refreshes TTL every 30s (heartbeat)
  → Node A checks all certs, renews any expiring within 30 days

If Node A dies:
  → TTL expires after 60s
  → Node B or C acquires leadership
  → New leader picks up renewal duties
```

Only the leader initiates renewals. All other nodes receive renewed certs via KV replication.

#### Full Cert Lifecycle

```
1. First HTTPS request arrives for example.com
2. Node checks KvStore for "certs:example.com:cert"
   ├── Found → use it, serve request with TLS
   └── Not found ↓
3. Acquire lock: KvStore.lock("acme:lock:example.com")
   ├── Lock held by another node → wait, then goto 2
   └── Lock acquired ↓
4. Create ACME order (Let's Encrypt)
5. Store challenge token in KV (replicated to all nodes)
6. LE verifies challenge → any node can respond → passes
7. Download issued cert
8. Store cert + key in KV (replicated to all nodes)
9. Release lock
10. All nodes now have the cert locally → zero-latency TLS handshakes
```

No external cert store, lock service, or coordination service needed.

The `rustls-acme` crate has a pluggable `Cache` trait — swap `DirCache` for a `KvCache` implementation. Zero changes to the ACME logic itself.

```
Phase 1 (single-node, implemented):  AcmeConfig → DirCache (filesystem)
Phase 2 (clustered):                 AcmeConfig → KvCache (gossip-replicated KV store)
```

### PHP Response Cache (Clustered)

Intercept PHP-generated `ETag` headers and short-circuit repeat requests across all nodes without executing PHP. This is distinct from static file ETags (which are handled at the server level today).

> **Status:** the **ETag-only** form ships, off by default, as [`[server.php_etag_cache]`](/reference/config/#serverphp_etag_cache): PHP-emitted ETags are cached in the KV and a matching `If-None-Match` gets a `304` without running PHP. Full-response (edge) caching, the purge header and the cache-group logic below are **Planned — not yet implemented**.

#### Flow

```
1. First request: /blog/hello
   → PHP executes, returns response with ETag: "abc123"
   → Server stores in KV: cache:<url_key> → { etag, headers, body }
   → Response sent to client

2. Repeat request: /blog/hello + If-None-Match: "abc123"
   → Server checks KV for cache:<url_key>
   → ETag matches → return 304 Not Modified immediately
   → No PHP execution, no mutex contention

3. Works across all nodes via gossip replication
```

#### Design Decisions

| Decision | Options | Notes |
|----------|---------|-------|
| **Cache key** | URL alone vs URL + vary headers (cookies, auth) | Must not serve cached authenticated pages to anonymous users. WordPress sets different cookies for logged-in users — key should include a cookie-based cache group or skip caching entirely when auth cookies are present. |
| **Invalidation** | TTL, purge header, PHP hook | TTL is simplest. An `X-Ephpm-Cache-Purge` response header from PHP could signal immediate invalidation. For WordPress, a must-use plugin could call a purge endpoint on content updates. |
| **Storage scope** | ETag-only (304s) vs full response (edge cache) | ETag-only saves KV space but still requires PHP on cache miss. Full response storage turns ephpm into an edge cache — much bigger win but needs memory/eviction policy. Start with full response. |
| **Cache bypass** | `Cache-Control: no-cache`, `no-store`, `private` | Respect standard HTTP cache directives from PHP. Never cache responses with `Set-Cookie` or `private`. |

#### Impact

Most WordPress page views are anonymous and return identical content. Skipping PHP entirely for repeat visitors eliminates the single-threaded PHP mutex bottleneck and lets the async HTTP server handle cached responses at full throughput across all nodes.

### Session Storage

PHP sessions can be stored in the KV store instead of the filesystem, through the native `ephpm` save handler (see [Native session handler](#2-native-session-handler--opt-in) above). **This is opt-in.** Without `session.save_handler = ephpm`, sessions use PHP's `files` handler: node-local files that other nodes cannot see, so a load balancer without sticky sessions will log users out. Nothing in ePHPm switches the handler for you, whether or not `[kv]` or `[cluster]` is configured.

#### Enabling it

```toml
[php]
ini_overrides = [
    ["session.save_handler", "ephpm"],      # required: opt in to the KV handler

    # Standard PHP session settings still apply
    ["session.gc_maxlifetime", "1440"],     # session TTL in the KV, seconds
    ["session.cookie_secure", "1"],
    ["session.cookie_httponly", "1"],
    ["session.cookie_samesite", "Lax"],
]
```

ePHPm generates the `php.ini` PHP reads, so a line in a system `php.ini` has no effect; use `ini_overrides` (or `ini_set()` before `session_start()`).

#### Request lifecycle

```
session_start()
  1. open()   — nothing to connect to; always succeeds
  2. read(id) — acquire session_lock:<id>, then GET session:<id> ("" for a new session)
  3. PHP unserializes into $_SESSION; the script runs
session_write_close() / end of request
  4. write(id, data) — SET session:<id> with TTL = session.gc_maxlifetime
  5. close()  — release session_lock:<id>
```

PHP does the (de)serialization (`session.serialize_handler`); the KV stores opaque bytes. `gc()` is a no-op because every session key carries a TTL. The handler is implemented in C in `crates/ephpm-php/ephpm_wrapper.c` on top of the same KV operations the `ephpm_kv_*` functions use.

#### Session locking

Without a lock, two concurrent requests on one session (typically AJAX) can each read the same `$_SESSION` and the second write silently drops the first one's change. PHP runs requests concurrently on ePHPm's threads, so this is a real risk.

The handler takes a pessimistic per-session lock in `read()` and releases it in `close()`:

- The lock key is `session_lock:<id>`, set with `SETNX` and a **30-second TTL**, so a crashed request cannot hold it forever.
- A contending request retries with backoff (10 ms, doubling, capped at 100 ms) for up to **30 seconds** in total. If the lock is still held, it logs an `E_WARNING` and continues **without** the lock — a possible lost update rather than a stuck worker.
- The lock is released with an unconditional delete. If a request holds the lock longer than its 30-second TTL, another request may take it, and the first request's `close()` then deletes the second one's lock.

With clustering on, the lock is a small key and therefore rides the gossip tier, which is eventually consistent. Two requests for the same session on **different** nodes at the same moment can both acquire it, so the lock is only reliable for requests that land on the same node.

#### Clustering behaviour

Session data is ordinary KV data. Small sessions (under `[cluster.kv] small_key_threshold`, default 512 bytes) are replicated to every node over gossip; larger ones are owned by `hash(session_id) % alive_nodes` and replicated to `replication_factor` nodes over the data plane. Any node can therefore read a session written on another node, so sticky sessions are not required.

Caveats, all from the clustered KV's design:

- Replication is asynchronous by default and write-time only. A session written on one node is visible elsewhere only after gossip converges or the data-plane write completes, and there is no background re-replication after a node loss.
- When membership changes, ownership of large sessions remaps and existing copies are not moved; reads fall back to surviving replicas.
- `session_destroy()` (a `DEL`, replicated as a tombstone) and lazy-write TTL refreshes (an `EXPIRE`) do replicate, last-arrival-wins, so like every other session write they are eventually consistent rather than immediate.
- Sessions count against `[kv] memory_limit` and can be evicted under memory pressure like any other key.

### Distributed Locking

Locks for PHP applications, built on the atomic `ephpm_kv_setnx()` primitive
with a TTL, and released with `ephpm_kv_del_if_eq()` so a request only ever
deletes its own lock:

```php
$token = bin2hex(random_bytes(16));

// Set-if-absent with a 30-second TTL in one atomic call — never hold it forever,
// and never leave a window where the key exists without a TTL.
if (ephpm_kv_setnx("deploy:mutex", $token, 30)) {
    try {
        // critical section
    } finally {
        // Delete only if it is still ours: if we overran the TTL and another
        // request took the lock, a plain ephpm_kv_del() would release theirs.
        ephpm_kv_del_if_eq("deploy:mutex", $token);
    }
}
```

These are reliable on a single node. With clustering on, a small lock key is
replicated over the eventually-consistent gossip tier, so two nodes can both
acquire the same lock at the same moment; do not rely on it for cross-node
mutual exclusion. See [KV from PHP](/guides/kv-from-php/) for the function
reference.

ePHPm uses the same KV + TTL pattern internally to elect a single ACME
certificate requester across the cluster, but that coordination lives in Rust
(`crates/ephpm-server/src/acme.rs`) — it is not exposed as a PHP lock API.

### Rate Limiting

Per-IP or per-key rate limiting using KV counters with TTL:

```
Request from 1.2.3.4:
  key = "ratelimit:1.2.3.4"
  count = KvStore.incr(key)
  if count == 1: KvStore.expire(key, window_seconds)
  if count > limit: return 429
```

**Cluster-wide, but approximate.** An increment is atomic on the node that performs it, and since #416 every increment publishes its **resulting total** (with the counter's TTL) to the other nodes, so nodes converge on a shared count. Peers receive totals, not deltas, and apply them last-writer-wins. Concurrent increments on different nodes therefore overwrite each other instead of adding up: if two nodes each raise a counter from 5 to 6 at the same moment, the cluster settles on 6, not 7. The limit can under-count, and in the worst case a client spread across N nodes gets up to roughly N times the allowance. The built-in `ratelimit` middleware has the same property. Exact cluster-wide counting needs owner-routed `INCR`, designed in the [clustered KV v2 roadmap](/roadmap/clustered-kv-v2/) but not implemented.

---

## Node API Endpoints — Planned, not yet implemented

None of these endpoints exist; no `/api/kv/*` route is served. Today, KV and cluster state is visible through the Prometheus metrics on `/metrics` and the [`ephpm kv`](/reference/cli/kv/) CLI. The table records the intended design:

| Endpoint (planned) | Method | Description |
|----------|--------|-------------|
| `/api/kv/stats` | GET | Memory usage, key count, hit/miss rate, evictions |
| `/api/kv/cluster` | GET | Cluster membership, ring state, replication status |

---

## Implementation Order

The KV store and clustering should be built incrementally:

| Phase | Scope | Depends on |
|-------|-------|------------|
| **1. Single-node KV** | `DashMap` store, strings + hashes, TTL, eviction | Nothing — can start now |
| **2. RESP listener (MVP)** | Strings + hashes commands, connection handshake, admin commands | Phase 1 |
| **3. PHP SAPI functions** | `ephpm_kv_get/set/del/hget/hset` via FFI | Phase 1 |
| **4. Session handler** | Custom PHP session save handler using KV (SAPI path) | Phase 3 |
| **5. Composer packages** | `ephpm/cache` (PSR-6/PSR-16) plus `cache-laravel`, `cache-symfony`, `cache-wordpress`, `predis-connection`, `session-handler` — shipped as separate repos | Phase 3 |
| **6. Gossip** | `chitchat` integration, peer discovery, health, symmetric encryption | Nothing — can start in parallel with Phase 1 |
| **6b. Data plane PSK encryption** | ChaCha20-Poly1305 sealing of the TCP data plane with a key derived from `[cluster] secret` (symmetric PSK, not mTLS) | Phase 6 |
| **7. Key ownership** | `hash % nodes` owner selection, local vs remote routing over the (optionally sealed) data plane | Phase 1 + Phase 6b |
| **8. Replication** | Async/sync large-value replication to `replication_factor` nodes, owner-then-replica read fallback (write-time only; no active rebalancing) | Phase 7 |
| **9. ACME on KV** | Swap `DirCache` for `KvCache` in rustls-acme | Phase 8 |
| **10. PHP response cache** | ETag interception, 304 short-circuit | Phase 7 |
| **11. Distributed locks** | TTL-based locks, leader election | Phase 7 |
| **12. Rate limiting** | Counters with TTL windows (replicated as last-writer-wins totals today, so approximate; exact counting needs owner-routed `INCR`) | Phase 7 |

Phases 1-5 (single-node) and Phase 6 (gossip) can be developed in parallel. Everything after Phase 7 depends on key ownership being operational. RESP lists, sets and sorted sets are deliberately **not** on this list: the KV is a cache, not a broker, and queue-style workloads belong on a real Redis.

---

## Configuration Reference

These keys exist today; the authoritative list, with defaults, is the [configuration reference](/reference/config/#kv).

```toml
[kv]
memory_limit = "256MB"                 # max memory for KV data
eviction_policy = "allkeys-lru"        # noeviction, allkeys-lru, volatile-lru, allkeys-random

[kv.redis_compat]
enabled = false                        # RESP protocol listener
listen = "127.0.0.1:6379"             # TCP only; `socket` is parsed but NOT implemented

[cluster]
enabled = false
bind = "0.0.0.0:7946"                 # gossip listen address
join = ["10.0.1.2:7946"]              # seed nodes
secret = "REPLACE_ME"                  # shared secret -- a ChaCha20-Poly1305 PSK that
                                       # seals BOTH gossip UDP and the KV data plane TCP
                                       # (no TLS/mTLS; there are no tls_cert/tls_key/tls_ca
                                       # cluster options). REQUIRED when enabled = true:
                                       # an empty secret is a hard startup error unless
                                       # allow_insecure_no_auth = true (not recommended).
# allow_insecure_no_auth = false       # opt in to empty-secret plaintext clustering

[cluster.kv]
replication_factor = 2                 # copies of each large value (owner + N-1 peers); clamped to cluster size
replication_mode = "async"             # async (default) or sync — see Replication above
```

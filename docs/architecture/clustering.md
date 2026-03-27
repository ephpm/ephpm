# Clustering & KV Store Architecture

This document covers the in-memory KV store, gossip-based clustering, consistent hashing, replication, and all features that depend on the KV layer (ACME cert coordination, PHP response cache, session storage).

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
| Data structures | Redis-style: strings, hashes, lists, sets, sorted sets | MVP: strings + hashes only (covers 95% of PHP usage). Lists, sets, sorted sets added later if needed. |
| TTL / expiry | Standard | Background sweeper + lazy expiry on access. |
| Clustering | **Nobody has this.** All competitors require external Redis/Memcached. | Gossip-based peer discovery, consistent hash ring, cross-node routing. |
| PHP access | RoadRunner: Goridge RPC. Swoole: shared memory API. | SAPI function calls — zero serialization, zero network hop for local keys. |
| Persistence | Optional | Optional AOF/snapshot for crash recovery. This is a cache, not a database. |
| Redis protocol compat | Not required | Optional RESP listener so existing Redis clients/tools work. |

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

/// MVP: strings + hashes only. Covers cache, sessions, object cache.
/// Lists, sets, sorted sets added later if needed (see Data Structure Roadmap below).
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

Three ways for PHP to access the KV store, each serving different use cases. All three work transparently with clustering — local keys are fast-pathed, remote keys are routed automatically.

### 1. Redis Protocol (RESP) — Zero Code Changes

**Priority: implement first.** This gives instant compatibility with every PHP app that already uses Redis. WordPress, Laravel, Symfony, Drupal — they all have Redis session and cache drivers. No code changes, no new dependencies.

ePHPm exposes a RESP (Redis Serialization Protocol) listener that PHP's existing Redis clients connect to as if it were a real Redis server. Two transport options:

**TCP (default)** — drop-in replacement, zero config changes for most apps:

```toml
[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"   # looks like Redis to PHP
```

**Unix socket** — eliminates TCP overhead (no handshake, no Nagle, no loopback routing). ~2-5x faster than TCP for high-frequency small operations like session reads. Requires a one-line config change in the PHP app:

```toml
[kv.redis_compat]
enabled = true
socket = "/run/ephpm/redis.sock"   # unix socket path
# listen = "127.0.0.1:6379"       # TCP and socket can run simultaneously
```

Both can be enabled at the same time — TCP for backward compatibility and tooling (`redis-cli`), Unix socket for performance-sensitive paths.

#### Transport Comparison

| Transport | Latency | Throughput | Config change needed |
|-----------|---------|------------|---------------------|
| **TCP localhost** | ~10-50us | Good | None — point at `127.0.0.1:6379` |
| **Unix socket** | ~2-10us | Better (~2-5x) | Change host to socket path |
| **SAPI (direct FFI)** | ~100-200ns | Best (~100x vs TCP) | Use `ephpm_kv_*()` functions |

Unix socket is the sweet spot for apps that want better performance without rewriting code. Just change the Redis connection string:

```php
// Laravel — config/database.php
'redis' => [
    'default' => [
        'scheme' => 'unix',
        'path' => '/run/ephpm/redis.sock',
        // or for phpredis: 'host' => '/run/ephpm/redis.sock'
    ],
],

// WordPress — wp-config.php
define('WP_REDIS_SCHEME', 'unix');
define('WP_REDIS_PATH', '/run/ephpm/redis.sock');

// Symfony — config/packages/cache.yaml
framework:
    cache:
        default_redis_provider: 'redis:///run/ephpm/redis.sock'
```

#### Supported PHP Clients

Both major PHP Redis clients support TCP and Unix sockets out of the box:

- **`phpredis`** — C extension, best performance for RESP path. Pre-compiled into many PHP distributions. Supports Unix sockets natively via `host` parameter.
- **`predis/predis`** — Pure PHP, most popular Composer package. No extension required. Supports Unix sockets via `scheme: unix` parameter.

#### Framework Integration (Zero Changes)

**Laravel** — point the Redis connection at localhost:

```php
// config/database.php — the only change needed
'redis' => [
    'client' => env('REDIS_CLIENT', 'phpredis'), // or 'predis'
    'default' => [
        'host' => '127.0.0.1',
        'port' => 6379,
        'database' => 0,
    ],
],

// Cache, sessions, queues — all work automatically
Cache::put('key', 'value', 3600);
// config/session.php: 'driver' => 'redis'
```

**WordPress** — with the [Redis Object Cache](https://wordpress.org/plugins/redis-cache/) plugin:

```php
// wp-config.php
define('WP_REDIS_HOST', '127.0.0.1');
define('WP_REDIS_PORT', 6379);
// Plugin handles the rest — object cache, transients, etc.
```

**Symfony** — configure the cache adapter:

```yaml
# config/packages/cache.yaml
framework:
    cache:
        default_redis_provider: 'redis://127.0.0.1:6379'
        pools:
            app.cache:
                adapter: cache.adapter.redis
```

#### RESP Commands to Implement

The RESP protocol is simple text-based. ~500-800 lines of Rust to implement the commands PHP clients actually use.

**What PHP apps actually need:** WordPress, Laravel, and Symfony Redis usage is almost entirely strings and hashes — `GET`/`SET` for cache and sessions, `HGET`/`HSET` for WordPress object cache groups, `INCR`/`DECR` for rate limiting. This covers 95%+ of real-world usage. Lists, sets, and sorted sets are niche — anyone needing those probably has a real Redis anyway.

##### MVP — Strings + Hashes (Phase 1)

These commands cover cache, sessions, object cache, and rate limiting:

| Command | Used by |
|---------|---------|
| `PING`, `ECHO`, `INFO`, `SELECT`, `AUTH` | Connection/handshake (required by all clients) |
| `GET`, `SET`, `SETEX`, `SETNX`, `DEL`, `EXISTS` | All cache/session drivers |
| `TTL`, `PTTL`, `EXPIRE`, `PEXPIRE`, `PERSIST` | TTL management |
| `MGET`, `MSET` | Batch cache reads/writes |
| `INCR`, `DECR`, `INCRBY`, `DECRBY` | Rate limiting, counters |
| `HGET`, `HSET`, `HDEL`, `HGETALL`, `HMSET`, `HMGET`, `HEXISTS`, `HLEN` | WordPress object cache groups, Laravel hash storage |
| `KEYS`, `SCAN`, `FLUSHDB`, `DBSIZE`, `TYPE` | Admin/debug |

This is enough for:
- WordPress with Redis Object Cache plugin
- Laravel cache, sessions, and rate limiting
- Symfony cache adapter
- Any app using Redis as a simple key-value cache

##### Phase 2 — Lists (if needed)

Lists are used primarily by **Laravel queues** (`LPUSH`/`BRPOP`). However, Laravel also supports database and SQS queue drivers, and we may want a native ephpm queue backed by the KV store instead of emulating Redis lists. Add lists only if there's real demand:

| Command | Used by |
|---------|---------|
| `LPUSH`, `RPUSH`, `LPOP`, `RPOP`, `LRANGE`, `LLEN`, `LINDEX` | Laravel queues |
| `BRPOP`, `BLPOP` | Blocking queue pops (requires async listener) |

##### Phase 3 — Sets + Sorted Sets (low priority)

Niche usage — unique tracking (`SADD`/`SISMEMBER`), leaderboards (`ZADD`/`ZRANGE`). Most apps that need these are using Redis as a primary data store, not just a cache, and should keep using a dedicated Redis instance:

| Command | Used by |
|---------|---------|
| `SADD`, `SREM`, `SMEMBERS`, `SISMEMBER`, `SCARD` | Unique tracking, tag sets |
| `ZADD`, `ZRANGE`, `ZRANGEBYSCORE`, `ZRANK`, `ZREM`, `ZSCORE` | Leaderboards, priority queues |

##### Not Implementing

| Category | Why |
|----------|-----|
| `SUBSCRIBE`/`PUBLISH` | Pub/sub is a different architecture. If needed, build a native ephpm event system rather than emulating Redis pub/sub. |
| `EVAL`/`EVALSHA` (Lua scripting) | Complex to implement, rarely used by PHP apps. |
| `MULTI`/`EXEC` (transactions) | Atomic operations (`INCR`, `SETNX`) cover most transaction use cases. |
| Persistence (`BGSAVE`, `RDB`, `AOF`) | ephpm is a cache, not a database. Optional snapshot/AOF may come later as a KV feature, not via RESP commands. |
| Cluster (`CLUSTER *`) | ephpm clustering is transparent — the RESP listener always acts as a single-node interface. Key routing happens behind the scenes. |

Unsupported commands return `-ERR unknown command` so clients handle them gracefully. This matches how Redis itself handles unknown commands — PHP clients and frameworks already have fallback paths.

### 2. Session Handler — Automatic

**Priority: implement second.** Register a custom PHP `session.save_handler` so `session_start()` uses the KV store with zero application changes. This uses the FFI path internally for maximum performance.

```toml
[php]
ini_overrides = [
    ["session.save_handler", "ephpm"],
    ["session.save_path", ""],           # not needed, ephpm manages storage
]
```

From PHP's perspective, sessions just work:

```php
session_start();                    // → ephpm_kv_get("session:<id>")
$_SESSION['cart'] = ['item1'];
// On request end:                  // → ephpm_kv_set("session:<id>", data, ttl: gc_maxlifetime)
```

The session handler implements PHP's `SessionHandlerInterface` at the C level:

| Callback | KV Operation |
|----------|-------------|
| `open()` | No-op (KV store is always available) |
| `close()` | No-op |
| `read($id)` | `ephpm_kv_get("session:$id")` |
| `write($id, $data)` | `ephpm_kv_set("session:$id", $data, ttl: gc_maxlifetime)` |
| `destroy($id)` | `ephpm_kv_del("session:$id")` |
| `gc($max_lifetime)` | No-op (TTL expiry handles this) |

With clustering, sessions are accessible from any node — no sticky sessions required at the load balancer. Consistent hashing on session ID means reads-after-writes within the same session naturally hit the same node (strong consistency for free with async replication).

### 3. Direct SAPI Functions — Maximum Performance

**Priority: implement third.** For applications that want the absolute fastest KV access — 1,000-10,000x faster than RESP for local keys. Requires ephpm-specific PHP code.

C-level functions exposed to PHP via the ePHPm SAPI:

```php
// String operations
ephpm_kv_set("user:123:profile", $json, ttl: 3600);
$json = ephpm_kv_get("user:123:profile");
ephpm_kv_del("user:123:profile");

// Hash operations
ephpm_kv_hset("user:123", "email", "user@example.com");
$email = ephpm_kv_hget("user:123", "email");
$all = ephpm_kv_hgetall("user:123");

// Atomic operations
$count = ephpm_kv_incr("page:views");
$exists = ephpm_kv_exists("cache:key");

// With clustering, this is transparent:
// - If key is local → direct memory access, ~100ns
// - If key is on another node → internal network hop, ~0.5-2ms
// PHP code doesn't know or care which node owns the key
```

#### Composer Package (`ephpm/ephpm`)

Ship a Composer package that wraps the SAPI functions in standard PHP interfaces, with a fallback to Redis for local development without ephpm:

```php
// composer require ephpm/ephpm

use Ephpm\Cache\KvStore;
use Ephpm\Cache\KvCachePool;       // PSR-6 CacheItemPoolInterface
use Ephpm\Cache\KvSimpleCache;     // PSR-16 CacheInterface

// PSR-16 SimpleCache
$cache = new KvSimpleCache();
$cache->set('key', 'value', 3600);
$value = $cache->get('key');

// PSR-6 Cache Pool
$pool = new KvCachePool();
$item = $pool->getItem('key');

// Laravel cache driver registration
// In a ServiceProvider:
Cache::extend('ephpm', function ($app, $config) {
    return new EphpmCacheStore();
});

// config/cache.php
'stores' => [
    'ephpm' => ['driver' => 'ephpm'],
],
```

The Composer package detects whether it's running inside ephpm (SAPI functions available) or standalone (falls back to Redis/Memcached). This means the same application code works in development with a local Redis and in production on ephpm with zero-overhead KV.

### Access Path Comparison

| Path | Latency (local) | Code changes | Best for |
|------|-----------------|-------------|----------|
| **RESP over TCP** | ~10-50us | None — existing Redis config | Drop-in migration, zero effort |
| **RESP over Unix socket** | ~2-10us | Change Redis host to socket path | Existing apps wanting more speed |
| **Session handler** | ~100-200ns | None — INI config only | PHP sessions (automatic) |
| **SAPI functions** | ~100-200ns | ephpm-specific calls | New apps, performance-critical paths |
| **Composer package** | ~100-200ns | PSR-6/PSR-16 interfaces | New apps that want portability + speed |

The RESP paths have overhead from IPC + RESP serialization, but are still **100-1,000x faster than external Redis** since there's no network hop — just in-process communication. Unix sockets cut the RESP overhead roughly in half vs TCP by eliminating the TCP/IP stack.

### Performance Expectations

| Operation | SAPI (local) | RESP Unix socket | RESP TCP | SAPI (remote) | External Redis |
|-----------|-------------|-----------------|----------|---------------|----------------|
| GET (string) | ~100-200ns | ~2-10us | ~10-50us | ~0.5-2ms | ~0.5-2ms |
| SET (string) | ~200-400ns | ~2-10us | ~10-50us | ~0.5-2ms | ~0.5-2ms |
| Serialization | None (shared memory) | RESP encode/decode | RESP encode/decode | Internal binary | Full RESP |
| Connection | None (in-process FFI) | Unix socket | Localhost TCP | Persistent internal | TCP pool |

---

## Clustering

### Overview

Three components: gossip-based peer discovery, consistent hash ring for key routing, and replication for fault tolerance.

```
Node A ◄──gossip──► Node B ◄──gossip──► Node C
  │                   │                   │
  KvStore             KvStore             KvStore
  (local shard)       (local shard)       (local shard)
  │                   │                   │
  └───────── consistent hash ring ────────┘
```

### 1. Gossip-Based Peer Discovery

Nodes find each other via a gossip protocol ([`chitchat`](https://github.com/quickwit-oss/chitchat) — Quickwit's SWIM-based gossip library for Rust, or a custom SWIM implementation).

```
Node A ◄──gossip──► Node B ◄──gossip──► Node C
  │                   │                   │
  alive, gen=5        alive, gen=3        alive, gen=7
  load=45%            load=62%            load=38%
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

Cluster communication has two channels with different security requirements:

#### Gossip Channel (UDP)

The SWIM gossip protocol uses UDP for high-frequency heartbeats and metadata exchange. Encrypted with a symmetric pre-shared key (like Consul's `encrypt` key). This prevents unauthorized nodes from joining the cluster and protects metadata (node addresses, health, KV stats) in transit.

The shared secret is the only required security config — one value to set across all nodes.

#### Data Plane (TCP + mTLS)

KV operations between nodes (remote get/set, replication, key rebalancing) use TCP with mutual TLS. Both sides authenticate — a rogue node can't join the cluster and read session data or inject cache entries.

mTLS uses rustls (already a dependency for HTTPS serving). Two modes:

**Auto-generated certs (default, zero-config):**

```
1. Node starts, generates ephemeral self-signed EC cert (P-256)
2. Publishes cert fingerprint (SHA-256) via gossip
   (gossip is encrypted with cluster secret, so fingerprint is trusted)
3. Other nodes learn the fingerprint, add it to their trust store
4. Data plane connections use mTLS — each side verifies the peer's
   cert fingerprint against what gossip advertised
5. Cert regenerated on restart — fingerprint propagates automatically
```

This is fully zero-config beyond the cluster secret. No CA infrastructure, no cert distribution, no renewal management. The gossip channel bootstraps trust for the data plane.

**Manual certs (enterprise/compliance):**

For environments that require certs from a specific CA (corporate PKI, compliance requirements):

```toml
[cluster]
tls_cert = "/etc/ephpm/node.pem"
tls_key = "/etc/ephpm/node-key.pem"
tls_ca = "/etc/ephpm/ca.pem"        # CA that signed all node certs
```

When manual certs are provided, auto-generation is skipped and standard CA verification is used instead of fingerprint pinning.

#### Security Summary

```
                    ┌─────────────────────────────────┐
                    │         Cluster Secret           │
                    │   (symmetric, pre-shared key)    │
                    └──────────┬──────────────────────┘
                               │
               ┌───────────────┼───────────────┐
               │               │               │
               ▼               ▼               ▼
          ┌─────────┐    ┌─────────┐    ┌─────────┐
          │ Node A  │    │ Node B  │    │ Node C  │
          └────┬────┘    └────┬────┘    └────┬────┘
               │              │              │
  Gossip (UDP) │◄─encrypted──►│◄─encrypted──►│
  symmetric key│              │              │
               │              │              │
  Data (TCP)   │◄───mTLS─────►│◄───mTLS─────►│
  cert pinning │              │              │
  or CA verify │              │              │
               │              │              │
```

| Channel | Transport | Encryption | Authentication |
|---------|-----------|------------|----------------|
| Gossip | UDP | Symmetric (cluster secret) | Pre-shared key |
| Data plane | TCP | TLS 1.3 (rustls) | mTLS — auto-generated certs with fingerprint pinning, or CA-signed certs |

### 3. Consistent Hash Ring

Keys are distributed across nodes using a consistent hash ring. When a node joins or leaves, only ~1/N of keys need to be rebalanced (not all of them).

```
            ┌───────────────────────┐
            │    Hash Ring          │
            │                       │
            │   0x0000 ──► Node A   │
            │   0x5556 ──► Node B   │
            │   0xAAAB ──► Node C   │
            │                       │
            │   Key "session:abc"   │
            │   hash = 0x3A21       │
            │   → owned by Node A   │
            └───────────────────────┘
```

Each node uses virtual nodes (vnodes) for even distribution — e.g., 150 vnodes per physical node. The `hashring` crate handles this.

```rust
use hashring::HashRing;

struct ClusterRouter {
    ring: RwLock<HashRing<NodeId>>,
    local_node: NodeId,
    peers: DashMap<NodeId, PeerConnection>,
}

impl ClusterRouter {
    /// Route a key to the correct node.
    fn owner(&self, key: &[u8]) -> NodeId {
        let ring = self.ring.read();
        ring.get(key).cloned().unwrap()
    }

    /// Get a value — local fast path or network hop.
    async fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let owner = self.owner(key);
        if owner == self.local_node {
            // Fast path: local lookup, no serialization, no network
            self.local_store.get(key)
        } else {
            // Network hop: forward to owner node
            let peer = self.peers.get(&owner)?;
            peer.remote_get(key).await
        }
    }
}
```

Crate dependencies:

| Crate | Purpose |
|-------|---------|
| `chitchat` | Quickwit's SWIM gossip library (or custom implementation) |
| `hashring` | Consistent hashing with virtual nodes |

### 3. Replication

Writes go to the owner node and N replicas (the next N nodes clockwise on the ring). This provides fault tolerance — if a node dies, its replicas can serve reads immediately.

```
Write "session:abc" = "data"
       │
       ▼
   hash(key) → Node A (owner)
       │
       ├──► write locally
       ├──► replicate to Node B (replica 1)
       └──► replicate to Node C (replica 2)
```

Replication modes:
- **Asynchronous (default)** — write returns after local write, replicas updated in background (lower latency, eventual consistency)
- **Synchronous** — write waits for N replicas to confirm (stronger consistency, higher latency)

For sessions and cache data, async replication with read-your-writes consistency is the right default. PHP apps doing `$_SESSION['cart'] = ...` on one request and reading it on the next will hit the same node (session affinity via consistent hashing on session ID), so they get strong consistency for free.

```toml
[cluster.kv]
replication_factor = 2        # copies on 2 additional nodes
replication_mode = "async"    # or "sync"
```

### 4. Node Join / Leave / Failure

| Event | What happens |
|-------|-------------|
| **Node joins** | Gossip announces new member. Hash ring adds vnodes. Affected key ranges transfer from current owners to new node in background. |
| **Node leaves gracefully** | Node announces departure via gossip. Key ranges transfer to next nodes on ring before shutdown. |
| **Node crashes** | Gossip failure detector triggers after missed heartbeats. Replicas promote to owners for affected key ranges. New replicas created on surviving nodes. |
| **Network partition** | Nodes on each side continue serving their local keys. On heal, conflict resolution via last-write-wins (LWW) timestamps. |

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

PHP sessions stored in the KV store instead of the filesystem. With clustering, sessions are accessible from any node — no sticky sessions required at the load balancer.

#### How PHP Sessions Work

PHP's session system is callback-driven. The engine calls a set of handler functions at specific points in the session lifecycle. By default, PHP uses the `files` handler (`/tmp/sess_<id>`). Redis and Memcached replace this with their own handlers. We do the same, implementing the callbacks in C (via `ephpm_wrapper.c`) backed by Rust's KV store.

#### Request Lifecycle

```
PHP: session_start()
       │
       ▼
  1. open(save_path, session_name)
       → No-op. KV store is always available, no connection to establish.
       │
       ▼
  2. read(session_id)
       → C: ephpm_session_read(id)
       → Rust: kv_store.get("session:{id}")
       → Returns serialized $_SESSION data (or "" for new session)
       │
       ▼
  3. PHP deserializes data into $_SESSION superglobal
       │
       ▼
  4. Script runs, modifies $_SESSION freely
       │
       ▼
  5. Request ends (or explicit session_write_close())
       │
       ▼
  6. write(session_id, serialized_data)
       → C: ephpm_session_write(id, data, gc_maxlifetime)
       → Rust: kv_store.set("session:{id}", data, ttl: gc_maxlifetime)
       │
       ▼
  7. close()
       → Release session lock (see Locking below)
```

#### C Handler Implementation

The session handler is registered during SAPI initialization via `php_session_register_module()`. All functions go through `ephpm_wrapper.c` with `zend_try`/`zend_catch` bailout protection.

```c
// ephpm_wrapper.c

#include "ext/session/php_session.h"

// Forward declarations — these call into Rust via FFI
extern int ephpm_kv_session_read(const char *id, size_t id_len,
                                  char **data, size_t *data_len);
extern int ephpm_kv_session_write(const char *id, size_t id_len,
                                   const char *data, size_t data_len,
                                   int gc_maxlifetime);
extern int ephpm_kv_session_destroy(const char *id, size_t id_len);
extern int ephpm_kv_session_lock(const char *id, size_t id_len);
extern int ephpm_kv_session_unlock(const char *id, size_t id_len);

PS_OPEN_FUNC(ephpm) {
    // No-op — KV store is always available in-process
    return SUCCESS;
}

PS_CLOSE_FUNC(ephpm) {
    // Release session lock
    ephpm_kv_session_unlock(/* current session id */);
    return SUCCESS;
}

PS_READ_FUNC(ephpm) {
    // Acquire session lock first (blocking)
    int rc = ephpm_kv_session_lock(key->val, key->len);
    if (rc != SUCCESS) {
        php_error_docref(NULL, E_WARNING,
            "ephpm: session lock timeout for %s, proceeding without lock",
            key->val);
    }

    char *data = NULL;
    size_t data_len = 0;
    rc = ephpm_kv_session_read(key->val, key->len, &data, &data_len);
    if (rc == SUCCESS && data != NULL) {
        *val = zend_string_init(data, data_len, 0);
        free(data);
    } else {
        *val = zend_string_init("", 0, 0);  // new session
    }
    return SUCCESS;
}

PS_WRITE_FUNC(ephpm) {
    int maxlifetime = INI_INT("session.gc_maxlifetime");
    return ephpm_kv_session_write(
        key->val, key->len,
        val->val, val->len,
        maxlifetime
    );
}

PS_DESTROY_FUNC(ephpm) {
    return ephpm_kv_session_destroy(key->val, key->len);
}

PS_GC_FUNC(ephpm) {
    // No-op — TTL-based expiry handles garbage collection automatically.
    // PHP calls this probabilistically (session.gc_probability / session.gc_divisor),
    // but we don't need it since every session key has a TTL.
    *nrdels = 0;
    return SUCCESS;
}

// Register the handler module
ps_module ps_mod_ephpm = {
    PS_MOD_UPDATE_TIMESTAMP(ephpm)
};

void ephpm_register_session_handler(void) {
    php_session_register_module(&ps_mod_ephpm);
}
```

#### Rust Side

```rust
// In ephpm-php/src/lib.rs (or a new session.rs module)

/// Called from C: read session data from KV store.
#[no_mangle]
pub extern "C" fn ephpm_kv_session_read(
    id: *const c_char, id_len: usize,
    data: *mut *mut c_char, data_len: *mut usize,
) -> c_int {
    let id = unsafe { std::slice::from_raw_parts(id as *const u8, id_len) };
    let key = format!("session:{}", std::str::from_utf8(id).unwrap_or(""));

    let store = KV_STORE.get().expect("KV store not initialized");
    match store.get(key.as_bytes()) {
        Some(value) => {
            let ptr = unsafe { libc::malloc(value.len()) as *mut c_char };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    value.as_ptr(), ptr as *mut u8, value.len()
                );
                *data = ptr;
                *data_len = value.len();
            }
            0 // SUCCESS
        }
        None => {
            unsafe {
                *data = std::ptr::null_mut();
                *data_len = 0;
            }
            0 // SUCCESS (empty session is not an error)
        }
    }
}

/// Called from C: write session data to KV store with TTL.
#[no_mangle]
pub extern "C" fn ephpm_kv_session_write(
    id: *const c_char, id_len: usize,
    data: *const c_char, data_len: usize,
    gc_maxlifetime: c_int,
) -> c_int {
    let id = unsafe { std::slice::from_raw_parts(id as *const u8, id_len) };
    let data = unsafe { std::slice::from_raw_parts(data as *const u8, data_len) };
    let key = format!("session:{}", std::str::from_utf8(id).unwrap_or(""));
    let ttl = Duration::from_secs(gc_maxlifetime as u64);

    let store = KV_STORE.get().expect("KV store not initialized");
    store.set(key.into_bytes(), data.to_vec(), Some(ttl));
    0 // SUCCESS
}
```

#### Session Locking

This is the most important design decision. Without locking, concurrent requests sharing a session ID can clobber each other:

```
Request A: session_start() → reads $_SESSION = {cart: [item1]}
Request B: session_start() → reads $_SESSION = {cart: [item1]}
Request A: $_SESSION['cart'][] = 'item2'; session_write_close()
           → writes {cart: [item1, item2]}
Request B: $_SESSION['cart'][] = 'item3'; session_write_close()
           → writes {cart: [item1, item3]}  ← item2 is LOST
```

**Current model (NTS, single PHP request):** Not a problem. The global PHP mutex serializes all requests, so concurrent session access can't happen.

**Future model (external PHP workers, multi-process):** Locking is essential.

**Implementation:** Lock acquired during `read()`, released during `close()`. The lock is a KV key with TTL to prevent deadlocks:

```
KV key: "session_lock:{session_id}"
Value:  "{request_id}"
TTL:    30s (safety net if request crashes without releasing)
```

Lock acquisition strategy:
- **Spin with backoff**: Try to acquire lock, sleep 10ms, retry up to `session.lock_wait_timeout` (default 10s). Same approach as the Redis session handler.
- **If timeout exceeded**: Proceed without lock (same behavior as phpredis `redis.session.lock_retries` exhausted). Log a warning.

```rust
/// Acquire session lock with spin + backoff.
fn session_lock(session_id: &str, timeout: Duration) -> bool {
    let lock_key = format!("session_lock:{session_id}");
    let lock_ttl = Duration::from_secs(30);
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(10);

    loop {
        // SET NX — only succeeds if key doesn't exist
        if store.set_nx(lock_key.as_bytes(), request_id, Some(lock_ttl)) {
            return true;  // lock acquired
        }
        if Instant::now() >= deadline {
            return false;  // timeout
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(200));
    }
}
```

With clustering, the lock lives on whichever node owns the `session_lock:{id}` key via the hash ring. Since the session data key (`session:{id}`) hashes to the same node prefix, lock + data will typically be co-located — no extra network hop for the common case.

#### Serialization

PHP has a built-in session serializer (`session.serialize_handler`). By default it uses `php` format, but `php_serialize` (standard `serialize()`) is also common. We don't touch serialization at all — PHP handles it before calling our `write()` and after our `read()`. The KV store just sees opaque bytes.

#### Configuration

```toml
[php]
ini_overrides = [
    # Enable ephpm session handler (set automatically when [kv] is configured)
    ["session.save_handler", "ephpm"],

    # Standard PHP session settings still apply
    ["session.gc_maxlifetime", "1440"],   # TTL in seconds (default 24 min)
    ["session.cookie_lifetime", "0"],      # browser session cookie
    ["session.cookie_secure", "1"],        # HTTPS only
    ["session.cookie_httponly", "1"],       # no JS access
    ["session.cookie_samesite", "Lax"],    # CSRF protection
]
```

When `[kv]` is configured in ephpm.toml, the session handler is registered automatically. If the user hasn't explicitly set `session.save_handler`, ephpm defaults to the KV handler. If they've set it to `files` or `redis` explicitly, we respect that.

#### Clustering Behavior

With clustering, sessions work transparently across nodes:

```
Client → Load Balancer → Pod 0: session_start()
  │                              → kv_store.get("session:abc123")
  │                              → key hashes to Pod 2 (owner)
  │                              → internal network hop to Pod 2
  │                              → Pod 2 returns session data
  │                              → PHP deserializes, script runs
  │                              → session_write_close()
  │                              → kv_store.set("session:abc123", data)
  │                              → routed to Pod 2 (owner)
  │                              → Pod 2 writes locally + replicates
  │
  ├── Next request → Pod 1: session_start()
  │                          → same flow, key routes to Pod 2
  │                          → session data available immediately
```

No sticky sessions needed at the load balancer. Any node can read/write any session. Consistent hashing ensures the session key always routes to the same owner node, so there's no replication lag concern for read-your-writes within a single session.

If the owner node (Pod 2) crashes, the replica promotes to owner automatically. The next session request routes to the new owner — no data loss, no user impact.

#### Performance vs Other Session Handlers

| Handler | Read latency | Write latency | Clustering | Lock support |
|---------|-------------|---------------|------------|-------------|
| `files` (default) | ~50-200us (disk) | ~50-200us (disk) | No — local filesystem only | flock() |
| `redis` (external) | ~0.5-2ms (TCP) | ~0.5-2ms (TCP) | Yes (Redis Cluster) | Spin lock via SET NX |
| `memcached` (external) | ~0.5-2ms (TCP) | ~0.5-2ms (TCP) | Yes (consistent hashing) | GET + CAS |
| **`ephpm` (local key)** | **~100-200ns** | **~200-400ns** | **Yes (built-in)** | **KV SET NX** |
| **`ephpm` (remote key)** | **~0.5-2ms** | **~0.5-2ms** | **Yes (built-in)** | **KV SET NX** |

For the common case (session key local to the node), ephpm is **1,000-10,000x faster** than Redis or Memcached. For the clustered case (key on another node), it's comparable — but with zero external infrastructure.

### Distributed Locking

General-purpose distributed locks for PHP applications, built on KV with TTL:

```php
$lock = ephpm_kv_lock("deploy:mutex", ttl: 30);
if ($lock) {
    // critical section
    ephpm_kv_unlock("deploy:mutex");
}
```

Used internally for ACME coordination, available to PHP apps for their own needs.

### Rate Limiting

Per-IP or per-key rate limiting using KV counters with TTL:

```
Request from 1.2.3.4:
  key = "ratelimit:1.2.3.4"
  count = KvStore.incr(key)
  if count == 1: KvStore.expire(key, window_seconds)
  if count > limit: return 429
```

With clustering, rate limits are cluster-wide — an attacker can't bypass limits by hitting different nodes.

---

## Node API Endpoints

The Node API exposes KV and cluster state for monitoring and debugging:

| Endpoint | Method | Description |
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
| **5. Composer package** | `ephpm/ephpm` — PSR-6/PSR-16 adapters, Laravel/Symfony drivers, Redis fallback | Phase 3 |
| **6. Gossip** | `chitchat` integration, peer discovery, health, symmetric encryption | Nothing — can start in parallel with Phase 1 |
| **6b. Inter-node mTLS** | Auto-generated certs, fingerprint exchange via gossip, data plane TLS | Phase 6 |
| **7. Hash ring** | Consistent hashing, key routing, local vs remote (over mTLS) | Phase 1 + Phase 6b |
| **8. Replication** | Async/sync replication, failure promotion | Phase 7 |
| **9. ACME on KV** | Swap `DirCache` for `KvCache` in rustls-acme | Phase 8 |
| **10. PHP response cache** | ETag interception, 304 short-circuit | Phase 7 |
| **11. Distributed locks** | TTL-based locks, leader election | Phase 7 |
| **12. Rate limiting** | Cluster-wide counters with TTL windows | Phase 7 |
| **13. RESP lists** | `LPUSH`/`RPUSH`/`LPOP`/`RPOP`/`BRPOP` — only if Laravel queue demand warrants it | Phase 2 |
| **14. RESP sets + sorted sets** | `SADD`/`ZADD` etc. — low priority, most apps needing these keep a dedicated Redis | Phase 2 |

Phases 1-5 (single-node) and Phase 6 (gossip) can be developed in parallel. Everything after Phase 7 depends on the hash ring being operational. RESP data structure phases (13-14) can be added at any time after Phase 2.

---

## Configuration Reference

### Planned

```toml
[kv]
memory_limit = "256MB"                 # max memory for KV data
eviction_policy = "allkeys-lru"        # noeviction, allkeys-lru, volatile-lru, allkeys-random

[kv.redis_compat]
enabled = false                        # RESP protocol listener
listen = "127.0.0.1:6379"             # TCP listener (default, zero-config for existing apps)
socket = "/run/ephpm/redis.sock"       # Unix socket (~2-5x faster than TCP)
# Both can be enabled simultaneously — TCP for tooling, socket for performance

[cluster]
enabled = false
bind = "0.0.0.0:7946"                 # gossip listen address
join = ["10.0.1.2:7946"]              # seed nodes
secret = ""                            # base64-encoded 32-byte key — encrypts gossip, authenticates nodes

# mTLS for data plane (auto-generated certs by default)
# When omitted, nodes generate ephemeral self-signed certs and
# exchange fingerprints via the encrypted gossip channel (zero-config).
# tls_cert = "/etc/ephpm/node.pem"    # manual: PEM cert for this node
# tls_key = "/etc/ephpm/node-key.pem" # manual: PEM private key
# tls_ca = "/etc/ephpm/ca.pem"        # manual: CA that signed all node certs

[cluster.kv]
replication_factor = 2                 # copies on N additional nodes
replication_mode = "async"             # async or sync
```

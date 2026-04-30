+++
title = "Architecture"
weight = 6
+++

## Feature Status

This document describes the full vision for ePHPm. The matrix below tracks what is actually implemented today.

| Feature | Status | Notes |
|---------|--------|-------|
| HTTP/1.1 | **Implemented** | hyper server, async accept loop |
| HTTP/2 | Planned | Dependency present, no code yet |
| TLS / ACME | Planned | No code or dependencies yet |
| PHP embedding (ZTS) | **Implemented** | Full SAPI, concurrent via `spawn_blocking` + per-thread TSRM |
| Static file serving | **Implemented** | MIME detection, path traversal protection |
| Request routing | **Implemented** | `.php` → PHP, pretty permalinks → `index.php`, else static |
| Configuration | **Implemented** | Figment — TOML + `EPHPM_` env var overrides |
| CLI | Partial | `--config` flag only; no subcommands yet |
| Graceful shutdown | Partial | Stops accepting; does not drain in-flight connections |
| Signal handling | Partial | Ctrl+C only; no SIGHUP reload |
| Observability | Partial | `tracing` crate logging; no OTLP export |
| DB proxy | Partial | MySQL transparent proxy (connection pooling, wire protocol, reset strategy); PostgreSQL placeholder; read/write splitting and replication not yet |
| KV store | **Implemented (single-node)** | RESP2 protocol server (~30 Redis commands), TTL/expiry, memory tracking, DashMap-backed store; clustering planned |
| Clustering / gossip | Planned | Multi-node coordination — not started |
| Admin UI / API | Planned | Management interface — not started |
| External PHP mode | Planned | Spawn external PHP workers over pipes — use any PHP binary |
| `ephpm doctor` | Planned | System diagnostics command — not started |
| Extension suites | Planned | Curated extension bundles — not started |

---

## Language Decision: Rust

ePHPm is written in **Rust**. This decision was made after evaluating Go (with CGO), Rust, and C.

### Why Rust over Go

- **Zero-cost PHP FFI** — Go's CGO incurs ~200ns overhead per C function call. FrankenPHP crosses the CGO boundary 11+ times per HTTP request (thread dispatch, SAPI callbacks for headers, output, POST body, cookies, superglobals, etc.), adding ~2.2μs+ of pure overhead per request. Rust's C FFI is a normal function call — zero overhead. This is ePHPm's primary competitive advantage over FrankenPHP.
- **No garbage collector** — Predictable p99 latencies. Critical for the DB proxy where GC pauses add jitter to every proxied query.
- **Lower memory per connection** — Matters at scale for both HTTP connections and DB proxy connections.
- **Marketing story** — "Zero-overhead PHP embedding" directly benchmarks against FrankenPHP's celebrated "30% faster CGO in Go 1.26." They optimized the overhead; we eliminated it.

### Why Rust over C

- **PHP embedding is only ~10% of the project.** C makes that part trivial but the other 90% (HTTP stack, TLS automation, DB proxy, clustering, admin UI, observability) has a terrible ecosystem in C.
- **Memory safety** — A DB proxy parsing untrusted wire protocol in C is a CVE factory. Same for TLS cert handling.
- **Rust's C FFI gives 95% of C's advantage** for the libphp integration while providing a modern ecosystem for everything else.

### Prior Art in Rust

- **[Pasir](https://github.com/el7cosmos/pasir)** — PHP application server in Rust using Hyper + Tokio + ext-php-rs. Active development, not production-ready. Validates the approach.
- **[ripht-php-sapi](https://crates.io/crates/ripht-php-sapi)** — Rust crate for PHP embed SAPI bindings. Building block.
- **[Pingora](https://github.com/cloudflare/pingora)** — Cloudflare's Rust HTTP proxy framework. Potential foundation for the HTTP/proxy layer.

---

## Previous Options Considered (Go-based)

### Option A: Build as a Caddy Module (FrankenPHP's Approach)

Write ePHPm as one or more Caddy modules. Distribute as a pre-built binary (built with xcaddy in CI).

**Verdict:** Poor fit. Caddy owns `main()` and the CLI. ePHPm's scope (DB proxy, clustered KV, admin UI, observability) extends far beyond HTTP serving.

### Option B: Use CertMagic Directly (was Recommended before Rust decision)

Import CertMagic as a library. Build ePHPm's own HTTP server with Go's `net/http`. Embed `libphp` via cgo.

**Verdict:** Was the best Go option. Full control over the binary. But still carries CGO overhead on every PHP call.

### Option C: Embed Caddy as a Library

**Verdict:** Not recommended. Caddy was designed to own the process, not be embedded.

### Option D: Fork FrankenPHP

**Verdict:** Not recommended. Maintaining a fork long-term is expensive, and FrankenPHP is tightly coupled to Caddy.

---

## Recommended Architecture

### Serving Node (`ephpm serve`)

Every ePHPm node runs as a serving node. The Node API is always present — it's the internal interface that Prometheus scrapes, the admin connects to, and OTLP exports through.

```
┌──────────────────────────────────────────────────────┐
│              ePHPm Serving Node                      │
│           ephpm serve (Rust binary)                  │
├─────────────┬───────────────┬────────────────────────┤
│  HTTP Layer │  DB Proxy     │  OTLP Receiver         │
│  (hyper /   │  (MySQL/PG)   │  (gRPC :4317 /         │
│   tokio)    │  + query      │   HTTP :4318)          │
│  + HTTP/2   │    digest     │                        │
│  + QUIC     │  + slow query │                        │
│             │    analysis   │                        │
├─────────────┼───────────────┼────────────────────────┤
│  ACME TLS   │  Clustered KV │  Node API :9090        │
│ (rustls-    │  (gossip +    │  ├─ /health            │
│  acme)      │   hash ring)  │  ├─ /metrics (prom)    │
│             │               │  ├─ /api/workers       │
│             │               │  ├─ /api/db/digests    │
│             │               │  ├─ /api/db/slow       │
│             │               │  ├─ /api/kv/stats      │
│             │               │  ├─ /api/traces        │
│             │               │  ├─ /api/profiling     │
│             │               │  └─ /api/config        │
├─────────────┴───────────────┴────────────────────────┤
│               PHP Embedding Layer                    │
│        (Rust FFI + libphp + custom SAPI)             │
├──────────────────────────────────────────────────────┤
│             Observability Pipeline                   │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────┐ │
│  │ Auto-       │  │ In-Memory    │  │ OTLP Export  │ │
│  │ Instrument  │→ │ Trace/Metric │→ │ (optional)   │ │
│  │ (HTTP, DB,  │  │ Ring Buffer  │  │ → Jaeger     │ │
│  │  KV, PHP)   │  │              │  │ → Datadog    │ │
│  └─────────────┘  └──────────────┘  └─────────────┘ │
├──────────────────────────────────────────────────────┤
│               Debug / Profiling                      │
│  (token-gated Xdebug, cachegrind, request            │
│   capture — surfaced via Node API)                   │
└──────────────────────────────────────────────────────┘
```

### Admin UI (`ephpm admin`)

The admin UI is a **separate mode**, not an embedded feature of the serving node. It runs either standalone (connecting to remote nodes) or embedded in a serving node for dev convenience. Same binary, different subcommand.

```
# Dev / single node — embedded admin, zero config
ephpm serve --admin

# Production — admin runs separately, connects to nodes
ephpm admin --nodes 10.0.1.1:9090,10.0.1.2:9090,10.0.1.3:9090
```

```
┌──────────────────────────────────────────────────────┐
│              ePHPm Admin Instance                    │
│           ephpm admin (same Rust binary)             │
├──────────────────────────────────────────────────────┤
│  Admin Web UI :8080                                  │
│  ├─ Cluster overview (all nodes)                     │
│  ├─ Thread pool status (per-node, aggregate)         │
│  ├─ Query digest dashboard (aggregated)              │
│  ├─ Slow query log + EXPLAIN viewer                  │
│  ├─ Trace viewer (distributed traces across nodes)   │
│  ├─ KV cluster health (ring, membership, memory)     │
│  ├─ Profiling results (cachegrind viewer)             │
│  ├─ Request debug capture viewer                     │
│  └─ Live config viewer (per-node)                    │
├──────────────────────────────────────────────────────┤
│  Node Connector                                      │
│  ├─ Polls/streams Node API from each node            │
│  ├─ Aggregates metrics across nodes                  │
│  ├─ Merges query digests (same digest from N nodes)  │
│  └─ Auth: shared secret / mTLS to Node API           │
└──────────────────────────────────────────────────────┘
       │              │              │
       ▼              ▼              ▼
   Node 1 :9090   Node 2 :9090   Node 3 :9090
```

### Production Cluster Layout

```mermaid
flowchart TB
    subgraph Cluster ["Production Cluster"]
        direction TB
        subgraph Nodes [" "]
            direction LR
            N1["Node 1 — ephpm serve<br/>PHP :443 · DB Proxy · KV<br/>OTLP :4318 · Node API :9090"]
            N2["Node 2 — ephpm serve<br/>PHP :443 · DB Proxy · KV<br/>OTLP :4318 · Node API :9090"]
            N3["Node 3 — ephpm serve<br/>PHP :443 · DB Proxy · KV<br/>OTLP :4318 · Node API :9090"]
        end
        Admin["Admin instance<br/>ephpm admin :8080"]
        Admin --- N1
        Admin --- N2
        Admin --- N3
    end
    Prom[Prometheus] -->|"GET /metrics"| N1
    Prom -->|"GET /metrics"| N2
    Prom -->|"GET /metrics"| N3
    Graf[Grafana] -->|PromQL| Prom
```

### Node API Specification

Every serving node exposes the Node API on a configurable port (default `:9090`). This is a lightweight HTTP/gRPC API — not a web UI. It consumes negligible resources.

| Endpoint | Method | Data |
|---|---|---|
| `/health` | GET | Liveness + readiness status |
| `/metrics` | GET | Prometheus/OpenMetrics scrape endpoint |
| `/api/workers` | GET | Thread pool: busy/idle/total, queue depth, restarts, memory per thread |
| `/api/db/digests` | GET | Query digest table: digest hash, normalized SQL, count, sum/min/max/avg time, rows |
| `/api/db/slow` | GET | Slow query log: recent slow queries with EXPLAIN output |
| `/api/db/pool` | GET | Connection pool stats: active/idle/total connections, wait time, timeouts |
| `/api/kv/stats` | GET | KV store: memory usage, key count, hit/miss rate, evictions |
| `/api/kv/cluster` | GET | Cluster membership: nodes, ring state, replication status |
| `/api/traces` | GET/SSE | Recent traces from ring buffer. SSE for live streaming |
| `/api/profiling` | GET | Cached profiling results (cachegrind, debug captures) |
| `/api/config` | GET | Running configuration (read-only, secrets redacted) |

**Authentication:** Node API requires a shared secret (bearer token) or mTLS. Configurable in `ephpm.toml`:

```toml
[node_api]
listen = "0.0.0.0:9090"
secret = "your-shared-secret"       # bearer token auth
# tls_cert = "/path/to/cert.pem"   # or mTLS
# tls_key = "/path/to/key.pem"
```

**`--admin` flag behavior:** When `ephpm serve --admin` is used, the serving node starts the admin web UI on a separate port (`:8080`) alongside the Node API. The embedded admin connects to its own Node API on `localhost:9090` — same code path as remote admin, just local. In `ephpm.toml`:

```toml
[admin]
enabled = true                        # or use --admin flag
listen = "0.0.0.0:8080"
nodes = ["localhost:9090"]            # auto-configured when embedded
# nodes = ["10.0.1.1:9090", "..."]   # explicit for multi-node from config
username = "admin"
password = "changeme"                 # admin UI auth (separate from node API auth)
```

### Why This Design

| Concern | Solution |
|---|---|
| **Resource isolation** | Admin UI serves static assets, holds websocket connections for live updates, aggregates traces across nodes. None of this runs on serving nodes in production. |
| **Single-node dev** | `ephpm serve --admin` gives the full experience with zero extra setup. |
| **Multi-node production** | Admin runs on a small dedicated box/container. Consumes ~50-100MB RAM, minimal CPU. |
| **Prometheus coexistence** | Node API `/metrics` works regardless of admin. Teams already using Grafana keep their setup and optionally add the admin UI. |
| **Security boundary** | Node API auth (shared secret / mTLS) is separate from admin UI auth (username/password, SSO in enterprise). Internal API never exposed to the internet. |
| **Same binary** | No separate build artifact. `ephpm serve` and `ephpm admin` are subcommands via `clap`. Simpler CI, simpler distribution. |

---

## Key Crates

| Component | Rust Crate |
|---|---|
| Async runtime | `tokio` |
| HTTP/1.1 + HTTP/2 | `hyper` |
| HTTP/3 (QUIC) | `quinn` |
| TLS | `rustls` |
| Automatic ACME TLS | `rustls-acme` |
| PHP embedding | Rust FFI + libphp (reference: `ext-php-rs`, `ripht-php-sapi`) |
| Concurrent hashmap (KV store) | `dashmap` |
| Cluster membership | `chitchat` (Quickwit's gossip lib) or custom SWIM |
| Consistent hashing | `hashring` |
| MySQL protocol | `sqlparser-rs` (query parsing), custom wire protocol |
| PostgreSQL protocol | Custom wire protocol |
| Prometheus metrics | `prometheus` crate |
| OTLP receiver/exporter | `opentelemetry-otlp`, `opentelemetry-sdk` |
| Protobuf (OTLP wire format) | `prost`, `tonic` (gRPC) |
| Static PHP builds | `crazywhalecc/static-php-cli` (for CI) |
| Embedded static assets | `rust-embed` |
| CLI | `clap` |
| Configuration | `toml` / `serde` |

---

## Clustered KV Store

### Status

**Currently Implemented (v0.5 preview):**
- Single-node in-memory KV store using DashMap (lock-free concurrent hashmap)
- RESP2 protocol server accepting Redis-compatible clients
- ~30 Redis commands implemented: GET, SET, DEL, EXPIRE, TTL, INCR, APPEND, KEYS, MGET, MSET, GETSET, etc.
- **SAPI bridge** for direct PHP access (zero serialization, zero network hop): `ephpm_kv_get`, `ephpm_kv_set`, `ephpm_kv_del`, `ephpm_kv_exists`, `ephpm_kv_incr_by`, `ephpm_kv_expire`, `ephpm_kv_pttl`
- TTL / expiry with background sweeper + lazy expiry on access
- Approximate memory tracking (via `mem_used()`)
- INFO command with basic server and memory stats
- Thread-local get buffer for safe C FFI result passing

**Planned / Not Yet Implemented:**
- Additional data structures (hashes, lists, sets, sorted sets) — currently strings only
- Clustering (gossip discovery, consistent hash ring, cross-node replication)
- Persistence (AOF/snapshots)
- Eviction policies (LRU, random)
- Memory limit enforcement

---

### Why Not Embed Dragonfly (or Redis, KeyDB, etc.)

[Dragonfly](https://github.com/dragonflydb/dragonfly) (30k GitHub stars, "modern Redis replacement") was evaluated for embedding. It was rejected for two reasons:

**1. License prohibits it.**
Dragonfly uses BSL 1.1 with this restriction:

> You may use the work (i) only as part of your own product or service, **provided it is not an in-memory data store product or service**; and (ii) provided that you do not use, provide, distribute, or make available the Licensed Work as a Service.

ePHPm's clustered KV store is a headline feature that functions as an in-memory data store. Even though ePHPm is primarily a PHP app server, the KV store directly overlaps with what Dragonfly considers its protected territory. The license risk isn't worth it.

**2. Architecturally incompatible.**

| Problem | Detail |
|---|---|
| Not a library | Dragonfly is a 200k+ line standalone C++ server. There's no `libdragonfly.a` to link against. |
| Conflicting async runtimes | Dragonfly uses `io_uring` + Boost.Fiber. ePHPm uses Tokio. Two async runtimes fighting over cores is a performance hazard. |
| Conflicting threading models | Dragonfly uses shared-nothing (each thread owns a keyspace slice). ePHPm needs any PHP worker to access any key. |
| Overkill | Dragonfly is designed for millions of QPS across terabytes. ePHPm needs sessions, app cache, and config — megabytes to low gigabytes. |

The same arguments apply to embedding Redis, KeyDB, or Garnet. They're all standalone servers designed to own a process, not be embedded.

### What ePHPm's KV Store Actually Needs

| Requirement | What competitors offer | ePHPm goal |
|---|---|---|
| Single-node KV | RoadRunner: in-memory/BoltDB/Redis drivers (single-node). Swoole: `Swoole\Table` (single-process shared memory). | In-process concurrent hashmap, zero-overhead access from PHP via SAPI. |
| Data structures | Redis-style: strings, hashes, lists, sets, sorted sets | `GET`/`SET`/`DEL`, `HGET`/`HSET`, `EXPIRE`, lists. Sorted sets if needed. |
| TTL / expiry | Standard | Background sweeper + lazy expiry on access. |
| Clustering | **Nobody has this.** All competitors require external Redis/Memcached. | Gossip-based peer discovery, consistent hash ring, cross-node routing. |
| PHP access | RoadRunner: Goridge RPC. Swoole: shared memory API. | SAPI function calls — zero serialization, zero network hop for local keys. |
| Persistence | Optional | Optional AOF/snapshot for crash recovery. This is a cache, not a database. |
| Redis protocol | Not required | Optional RESP listener so existing Redis clients/tools work. Nice-to-have, not critical. |

### Architecture: Single-Node Store

The core is a `DashMap` — a sharded, lock-free concurrent hashmap from the Rust ecosystem. It provides millions of ops/sec on a single node with zero contention on reads and fine-grained write locks.

```rust
use dashmap::DashMap;
use std::time::Instant;

/// A single KV entry with optional TTL
struct KvEntry {
    value: KvValue,
    expires_at: Option<Instant>,
}

/// Supported value types (Redis-style)
enum KvValue {
    String(Vec<u8>),
    Hash(HashMap<Vec<u8>, Vec<u8>>),
    List(VecDeque<Vec<u8>>),
    Set(HashSet<Vec<u8>>),
    SortedSet(BTreeMap<OrderedFloat<f64>, Vec<u8>>),
}

/// The single-node KV store
struct KvStore {
    data: DashMap<Vec<u8>, KvEntry>,
    memory_limit: usize,
    eviction_policy: EvictionPolicy,
}

enum EvictionPolicy {
    NoEviction,   // return error when full
    AllKeysLru,   // evict least recently used
    VolatileLru,  // evict LRU among keys with TTL
    AllKeysRandom,
}

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

A background Tokio task runs periodically to sweep expired keys (active expiry), so memory doesn't leak from keys that are set-and-forgotten:

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

### Architecture: Clustering

This is ePHPm's key differentiator — no competitor has built-in multi-node KV clustering. The design uses three components:

#### 1. Gossip-based peer discovery

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

#### 2. Consistent hash ring

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
    /// Route a key to the correct node
    fn owner(&self, key: &[u8]) -> NodeId {
        let ring = self.ring.read();
        ring.get(key).cloned().unwrap()
    }

    /// Get a value — local fast path or network hop
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

#### 3. Replication

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

Replication can be:
- **Synchronous** — write waits for N replicas to confirm (stronger consistency, higher latency)
- **Asynchronous** — write returns after local write, replicas updated in background (lower latency, eventual consistency)

For sessions and cache data, async replication with read-your-writes consistency is the right default. PHP apps doing `$_SESSION['cart'] = ...` on one request and reading it on the next will hit the same node (session affinity via consistent hashing on session ID), so they get strong consistency for free.

```toml
[cluster.kv]
replication_factor = 2        # copies on 2 additional nodes
replication_mode = "async"    # or "sync"
```

#### 4. Node join / leave / failure

| Event | What happens |
|---|---|
| **Node joins** | Gossip announces new member. Hash ring adds vnodes. Affected key ranges transfer from current owners to new node in background. |
| **Node leaves gracefully** | Node announces departure via gossip. Key ranges transfer to next nodes on ring before shutdown. |
| **Node crashes** | Gossip failure detector triggers after missed heartbeats. Replicas promote to owners for affected key ranges. New replicas created on surviving nodes. |
| **Network partition** | Nodes on each side continue serving their local keys. On heal, conflict resolution via last-write-wins (LWW) timestamps. |

### PHP Access: Zero-Overhead via SAPI

PHP accesses the KV store through SAPI function calls — not over TCP, not via a Redis client. This is a direct function call from PHP into Rust through the FFI boundary:

```php
// These are C-level functions exposed to PHP via the ePHPm extension
// — like frankenphp_handle_request() but for KV operations

// Simple string operations
ephpm_kv_set("session:abc", $data, ttl: 3600);
$data = ephpm_kv_get("session:abc");
ephpm_kv_del("session:abc");

// Hash operations
ephpm_kv_hset("user:123", "email", "user@example.com");
$email = ephpm_kv_hget("user:123", "email");
$all = ephpm_kv_hgetall("user:123");

// With clustering, this is transparent:
// - If key is local → direct memory access, ~100ns
// - If key is on another node → internal network hop, ~0.5-2ms
// PHP code doesn't know or care which node owns the key
```

For frameworks that expect a Redis-compatible interface, ePHPm can optionally expose a RESP listener on a local port:

```toml
[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"   # looks like Redis to PHP
```

This lets existing apps using `predis/predis` or `phpredis` point at `localhost:6379` and hit ePHPm's KV store with zero code changes. The RESP protocol is simple — ~500 lines of Rust to implement the common commands.

### Performance Expectations

| Operation | ePHPm (local key) | ePHPm (remote key) | External Redis |
|---|---|---|---|
| GET (string) | ~100-200ns | ~0.5-2ms | ~0.5-2ms |
| SET (string) | ~200-400ns | ~0.5-2ms | ~0.5-2ms |
| Serialization overhead | None (shared memory) | Minimal (internal binary format) | Full RESP encode/decode |
| Connection overhead | None (in-process) | Persistent internal connections | TCP connection pool |

For local keys, ePHPm's KV is **1,000-10,000x faster than Redis** because there's no TCP, no serialization, no context switch. For remote keys (clustered), it's comparable to Redis since the network hop dominates.

### TLS Certificate Management in a Cluster

The clustered KV store backs ePHPm's TLS certificate storage, locking, and ACME challenge coordination. This solves three HA problems that naive auto-TLS implementations get wrong.

#### Problem 1: Cert Issuance Race Condition

Without coordination, multiple nodes all detect that no cert exists for `example.com` and simultaneously request one from Let's Encrypt. This wastes ACME quota (50 certs/domain/week rate limit) and may trigger rate-limiting bans.

**Solution: Distributed lock via KV store.**

Before any node initiates an ACME order, it acquires a lock in the KV store:

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

Node B: lock released, checks KvStore → cert exists → done, no issuance needed.
```

The lock has a TTL (default 5 minutes) to prevent deadlock if a node crashes mid-issuance. If the lock holder dies, another node acquires the lock after TTL expiry and retries.

#### Problem 2: ACME Challenge Routing

Let's Encrypt validates domain ownership by making an HTTP request to `http://example.com/.well-known/acme-challenge/<token>`. In a multi-node setup behind a load balancer, the challenge request may hit any node — not necessarily the one that initiated the ACME order.

**Solution: Challenge token propagation via KV store.**

When Node A creates an ACME order, it stores the challenge token in the KV store. All nodes serve `/.well-known/acme-challenge/*` by reading from the KV store:

```
Node A initiates ACME order for example.com
       │
       ▼
   Let's Encrypt returns challenge: token=abc123, response=xyz789
       │
       ▼
   KvStore.set("acme:challenge:abc123", "xyz789", ttl=600s)
       │
       ├──► replicated to Node B (gossip, ~100ms)
       └──► replicated to Node C (gossip, ~100ms)

Let's Encrypt requests: GET http://example.com/.well-known/acme-challenge/abc123
       │
       ▼ (load balancer routes to Node B)
   Node B: KvStore.get("acme:challenge:abc123") → "xyz789"
   Node B: responds with 200 OK, body: "xyz789"
       │
       ▼
   Let's Encrypt: challenge passed ✓
```

This works for HTTP-01 challenges. For TLS-ALPN-01 challenges, the same approach applies — the challenge certificate is stored in the KV store and any node can present it during the TLS handshake.

**Timing:** Gossip replication typically completes in ~100-200ms. The ACME protocol has a built-in delay between creating the challenge and checking it (the server tells the client to poll for status), so propagation latency is not an issue.

#### Problem 3: Renewal Stampede

All nodes notice the cert for `example.com` expires in 29 days. Without coordination, all nodes attempt renewal simultaneously.

**Solution: Leader election for cert renewal.**

One node is responsible for certificate renewals at any given time. Election uses the KV store:

```
KvStore.set("acme:leader", node_id="A", ttl=60s)
  → Node A is the cert renewal leader
  → Node A refreshes the TTL every 30s (heartbeat)
  → Node A checks all certs, renews any expiring within 30 days

If Node A dies:
  → TTL expires after 60s
  → Node B or C acquires leadership
  → New leader picks up renewal duties
```

Only the leader initiates renewals. All other nodes receive the renewed certs via KV replication. This reduces ACME requests to the minimum necessary (one request per cert, regardless of cluster size).

#### Full Cert Lifecycle

```
1. First HTTPS request arrives for example.com
       │
       ▼
2. Node checks KvStore for "certs:example.com:cert"
   ├── Found → use it, serve request with TLS
   └── Not found ↓
       │
3. Acquire lock: KvStore.lock("acme:lock:example.com")
   ├── Lock held by another node → wait, then goto 2
   └── Lock acquired ↓
       │
4. Create ACME order (Let's Encrypt)
       │
5. Store challenge token: KvStore.set("acme:challenge:<token>", response)
       │ (replicated to all nodes via gossip)
       │
6. Tell Let's Encrypt to verify → challenge request hits any node → passes
       │
7. Download issued cert
       │
8. Store cert: KvStore.set("certs:example.com:cert", cert_pem)
   Store key:  KvStore.set("certs:example.com:key", key_pem)
       │ (replicated to all nodes via gossip)
       │
9. Release lock: KvStore.unlock("acme:lock:example.com")
       │
10. All nodes now have the cert locally (replica) → zero-latency TLS handshakes
```

No external cert store (Redis, S3, etcd) needed. No external lock service (etcd, Consul) needed. Zero-config clustered HTTPS using ePHPm's built-in KV store and gossip protocol.

---

## DB Proxy & Connection Pooling

### Inspiration: ProxySQL

[ProxySQL](https://proxysql.com/) (6.6k GitHub stars, C++, GPL-3.0) is the gold standard for MySQL proxying. It sits between the application and the database, providing:

- **Connection multiplexing** — 100k+ client connections mapped to a few hundred backend connections. Production ratios of 50:1 are common (10,000 clients sustained by 200 backend connections).
- **Query digest** — normalizes queries into fingerprints by stripping literal values. `SELECT * FROM users WHERE id = 5` and `SELECT * FROM users WHERE id = 42` become the same digest. Tracks per-digest stats: execution count, sum/min/max/avg time, first/last seen.
- **Query caching** — caches results at the proxy layer for read-heavy queries.
- **Read/write splitting** — routes `SELECT` to replicas, writes to primary.
- **Query rules** — regex-based routing, rewriting, blocking.
- **Failover detection** — health checks, automatic rerouting on backend failure.
- **Query mirroring** — duplicate live traffic to a secondary for load testing.

ProxySQL can't be embedded (GPL-3.0, standalone C++ server). But ePHPm can replicate its most valuable features — connection pooling, query digest, and slow query analysis — at the wire protocol level.

### Why a DB Proxy in a PHP Server?

The traditional PHP stack opens a new database connection per request (or per worker in persistent mode). This creates several problems:

```
Without proxy (traditional):
  PHP Worker 1 ──► MySQL connection 1 ──┐
  PHP Worker 2 ──► MySQL connection 2 ──├──► MySQL (max_connections = 151 default)
  PHP Worker 3 ──► MySQL connection 3 ──┤
  ...                                   │
  PHP Worker 100 ──► MySQL conn 100 ────┘
  PHP Worker 101 ──► ERROR: Too many connections
```

```
With ePHPm's DB proxy:
  PHP Worker 1 ──┐                    ┌──► MySQL connection 1 ──┐
  PHP Worker 2 ──┤                    ├──► MySQL connection 2 ──├──► MySQL
  PHP Worker 3 ──┼──► ePHPm Proxy ────┼──► MySQL connection 3 ──┤
  ...            │    (multiplexing)  ├──► ...                  │
  PHP Worker 200 ┘                    └──► MySQL connection 20 ─┘
```

Because ePHPm controls both the PHP workers AND the proxy, the connection is never over TCP — it's an in-process function call from the PHP worker to the proxy pool. Zero network overhead.

**No competitor does this.** FrankenPHP and RoadRunner don't have connection pooling. Swoole has `PDOPool` but it's PHP-level (still TCP to the database, no query analysis). ProxySQL is a separate process requiring TCP between the app and the proxy.

### Rust Ecosystem for Wire Protocols

The Rust ecosystem has strong building blocks for this:

#### PostgreSQL

| Crate | Stars | Status | What it provides |
|---|---|---|---|
| [`pgwire`](https://github.com/sunng87/pgwire) | 734 | Active (v0.37.3, Jan 2026) | Full PostgreSQL wire protocol — server and client APIs, SSL, SCRAM-SHA-256 auth, simple + extended query protocol. "Like hyper, but for Postgres." |
| [`postgres-protocol`](https://crates.io/crates/postgres-protocol) | Part of rust-postgres | Active | Low-level protocol message parsing. |
| [`sqlx`](https://github.com/launchbadge/sqlx) | 14k+ | Active | Async Postgres/MySQL driver — useful as the backend connection pool driver. |

`pgwire` is the standout. It's explicitly designed for building Postgres proxies, has 452 downstream dependents, and handles auth, SSL, query protocol, and cancellation. ePHPm would use `pgwire` for the frontend (accepting connections from PHP) and `sqlx` or `tokio-postgres` for the backend (connections to the actual database).

Reference: [PgDog](https://github.com/pgdogdev/pgdog) (4.1k stars, Rust, AGPL-3.0) — a production Postgres connection pooler/load balancer/sharder built in Rust with Tokio. Validates the approach and demonstrates that Rust is well-suited for this. Can't embed PgDog directly (AGPL), but it's excellent reference architecture.

#### MySQL

The MySQL wire protocol ecosystem in Rust is less mature:

| Crate | Status | What it provides |
|---|---|---|
| [`mysql_async`](https://github.com/blackbeam/mysql_async) | Active | Tokio-based MySQL client driver. Useful as the backend pool driver. |
| [`mysql-proxy-rs`](https://github.com/AgilData/mysql-proxy-rs) | **Abandoned** (~2016) | MySQL proxy framework on old tokio-core. Not usable. |

For MySQL, ePHPm would need to implement the server-side wire protocol (accepting connections that look like a MySQL server) from scratch. The MySQL protocol is simpler than Postgres — it's a straightforward packet-based format:

```
MySQL Client Protocol (simplified):
  1. Handshake (server sends greeting, client sends auth)
  2. Command phase:
     - COM_QUERY: text query → ResultSet or OK/ERR
     - COM_STMT_PREPARE: prepared statement
     - COM_STMT_EXECUTE: execute prepared
     - COM_QUIT: disconnect
```

The core proxy only needs to handle `COM_QUERY` and `COM_STMT_*` — ~1,000-2,000 lines of Rust for the protocol layer. This is manageable.

#### SQL Parsing

[`sqlparser-rs`](https://github.com/apache/datafusion-sqlparser-rs) (Apache project, used by DataFusion, Polars, GlueSQL, ParadeDB, and many others) handles SQL parsing with MySQL and PostgreSQL dialect support. This powers:

- Query digest computation (normalize literals → fingerprint hash)
- Read/write classification (`SELECT` → replica, `INSERT`/`UPDATE`/`DELETE` → primary)
- Slow query analysis (identify query structure)

### Architecture

```
PHP Worker calls mysql_connect("127.0.0.1:3306") or PDO("mysql:host=127.0.0.1")
       │
       │  (ePHPm intercepts — this is localhost, so it's the proxy)
       ▼
┌─────────────────────────────────────────────────────────┐
│                    ePHPm DB Proxy                       │
│                                                         │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────┐  │
│  │ Protocol    │  │ Query        │  │ Connection    │  │
│  │ Frontend    │  │ Analyzer     │  │ Pool          │  │
│  │             │  │              │  │               │  │
│  │ Accept      │  │ Parse SQL    │  │ Backend conns │  │
│  │ MySQL/PG    │  │ Compute      │  │ to real DB    │  │
│  │ wire proto  │  │ digest       │  │               │  │
│  │ from PHP    │  │ Classify     │  │ Min/max pool  │  │
│  │             │  │ R/W          │  │ size, idle    │  │
│  │             │  │ Track timing │  │ timeout,      │  │
│  │             │  │ Detect slow  │  │ health check  │  │
│  └──────┬──────┘  └──────┬───────┘  └───────┬───────┘  │
│         │                │                   │          │
│         ▼                ▼                   ▼          │
│  ┌─────────────────────────────────────────────────┐    │
│  │              Metrics / Trace Emitter            │    │
│  │  → Query digest stats (count, sum/min/max time) │    │
│  │  → Slow query log (with EXPLAIN output)         │    │
│  │  → OTel spans per query (for trace correlation) │    │
│  │  → Prometheus metrics (pool utilization, QPS)   │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
       │
       ▼
  Actual MySQL / PostgreSQL server(s)
```

### Connection Pool Implementation

```rust
use sqlx::mysql::MySqlPool;
use sqlx::postgres::PgPool;

/// Pool configuration (from ephpm.toml)
struct PoolConfig {
    /// Backend database URL
    url: String,
    /// Minimum idle connections to maintain
    min_connections: u32,
    /// Maximum total connections
    max_connections: u32,
    /// Idle connection timeout before closing
    idle_timeout: Duration,
    /// Maximum lifetime of a connection
    max_lifetime: Duration,
    /// Health check interval
    health_check_interval: Duration,
}
```

For MySQL, ePHPm uses `sqlx::MySqlPool` (or `mysql_async`) as the backend pool — these handle the real TCP connections to MySQL, keepalive, health checks, and reconnection. The proxy frontend speaks MySQL wire protocol to PHP, and the backend pool speaks MySQL wire protocol to the real database. The proxy maps N frontend connections (from PHP workers) to M backend connections (to the database), where N >> M.

```toml
# ephpm.toml
[db.mysql]
url = "mysql://user:pass@db-primary:3306/myapp"
min_connections = 5
max_connections = 50
idle_timeout = "300s"

[db.mysql.replicas]
urls = [
    "mysql://user:pass@db-replica-1:3306/myapp",
    "mysql://user:pass@db-replica-2:3306/myapp",
]
read_write_split = true   # SELECTs go to replicas

[db.postgres]
url = "postgres://user:pass@pg-primary:5432/myapp"
min_connections = 5
max_connections = 30
```

### Query Digest & Analysis

Inspired by ProxySQL's `stats_mysql_query_digest`, ePHPm computes a fingerprint for every query by normalizing literal values and hashing the result:

```rust
use sqlparser::parser::Parser;
use sqlparser::dialect::MySqlDialect;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

/// Normalize a query by replacing literal values with placeholders
/// "SELECT * FROM users WHERE id = 42 AND name = 'alice'"
/// → "SELECT * FROM users WHERE id = ? AND name = ?"
fn normalize_query(sql: &str) -> String {
    // Use sqlparser to parse, walk the AST, replace Value nodes with "?"
    // This handles strings, numbers, booleans, NULL, etc.
    // ...
}

/// Compute a digest hash from the normalized query
fn query_digest(normalized: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// Per-digest statistics (like ProxySQL's stats_mysql_query_digest)
struct DigestStats {
    digest: u64,
    digest_text: String,         // normalized query text
    schema: String,
    count: AtomicU64,            // total executions
    sum_time_us: AtomicU64,      // total execution time (microseconds)
    min_time_us: AtomicU64,
    max_time_us: AtomicU64,
    sum_rows_affected: AtomicU64,
    sum_rows_sent: AtomicU64,
    first_seen: Instant,
    last_seen: AtomicInstant,
}

/// The digest store — concurrent map of digest → stats
struct DigestStore {
    digests: DashMap<u64, DigestStats>,
}
```

This gives the admin UI ProxySQL-grade query intelligence:

| digest | digest_text | count | avg_time | max_time |
|---|---|---|---|---|
| `0xa3f2...` | `SELECT * FROM users WHERE id = ?` | 45,231 | 2.1ms | 89ms |
| `0xb1c4...` | `INSERT INTO orders (user_id, ...) VALUES (?, ...)` | 12,089 | 5.3ms | 210ms |
| `0xd9e7...` | `SELECT * FROM products WHERE category = ? ORDER BY ? LIMIT ?` | 8,445 | 45.2ms | 1,200ms |

### Slow Query Detection & Auto-EXPLAIN

When a query exceeds a configurable threshold, ePHPm captures it and optionally runs `EXPLAIN` on a replica:

```toml
[db.analysis]
slow_query_threshold = "100ms"
auto_explain = true              # run EXPLAIN on slow queries
auto_explain_target = "replica"  # don't hit primary with EXPLAINs
```

```rust
async fn handle_query_result(
    query: &str,
    digest: u64,
    duration: Duration,
    config: &AnalysisConfig,
    replica_pool: &Pool,
) {
    // Update digest stats
    digest_store.record(digest, duration);

    // Slow query detection
    if duration > config.slow_query_threshold {
        let slow_query = SlowQuery {
            sql: query.to_string(),
            digest,
            duration,
            timestamp: Instant::now(),
            explain: None,
        };

        // Auto-EXPLAIN on a replica (non-blocking, background task)
        if config.auto_explain {
            let explain_sql = format!("EXPLAIN ANALYZE {}", query);
            if let Ok(explain_result) = replica_pool.fetch_one(&explain_sql).await {
                slow_query.explain = Some(explain_result);
            }
        }

        slow_query_store.push(slow_query);
        // Also emits an OTel span event for trace correlation
    }
}
```

The admin UI surfaces this as a slow query dashboard with the EXPLAIN plan inline — no need for external tools like pt-query-digest or Percona Monitoring.

### Read/Write Splitting

`sqlparser-rs` classifies queries by type:

```rust
use sqlparser::ast::Statement;

fn classify_query(stmt: &Statement) -> QueryType {
    match stmt {
        // Obvious reads
        Statement::Query(_) => {
            // Check for FOR UPDATE / FOR SHARE — these are writes (take locks)
            // Check for INTO OUTFILE — this is a write
            // Otherwise: read
            QueryType::Read
        }
        Statement::ShowTables { .. } |
        Statement::ShowColumns { .. } |
        Statement::Explain { .. } => QueryType::Read,

        // Obvious writes
        Statement::Insert { .. } |
        Statement::Update { .. } |
        Statement::Delete { .. } |
        Statement::CreateTable { .. } |
        Statement::AlterTable { .. } |
        Statement::Drop { .. } |
        Statement::Truncate { .. } => QueryType::Write,

        // Transaction control
        Statement::StartTransaction { .. } => QueryType::TransactionBegin,
        Statement::Commit { .. } => QueryType::TransactionEnd,
        Statement::Rollback { .. } => QueryType::TransactionEnd,

        // Unknown — safe default is primary
        Statement::Call { .. } |        // stored procedures may write
        _ => QueryType::Write,          // conservative: unknown goes to primary
    }
}

enum QueryType {
    Read,
    Write,
    TransactionBegin,
    TransactionEnd,
}
```

Read-only queries route to the replica pool. Writes route to the primary. Unknown statement types default to primary (conservative — never risk sending a write to a replica).

```
PHP: SELECT * FROM users WHERE id = 5
  → ePHPm parses → Read → routes to replica pool

PHP: INSERT INTO orders (...) VALUES (...)
  → ePHPm parses → Write → routes to primary

PHP: SELECT * FROM inventory WHERE id = 5 FOR UPDATE
  → ePHPm parses → Write (FOR UPDATE takes locks) → routes to primary

PHP: CALL process_order(42)
  → ePHPm parses → Unknown → defaults to primary (safe)

PHP: BEGIN; SELECT ...; UPDATE ...; COMMIT;
  → ePHPm detects TransactionBegin → pins to primary until TransactionEnd
```

#### Transaction Tracking

Inside an explicit transaction, all queries must go to the primary — even SELECTs. ePHPm tracks this per-connection:

```rust
struct ConnectionState {
    in_transaction: bool,
    pinned_to: Option<PoolTarget>,     // primary or specific replica
    sticky_until: Option<Instant>,     // sticky-after-write expiry
    session_vars: Vec<(String, String)>, // SET variable tracking
}
```

The proxy detects `BEGIN`/`START TRANSACTION` at the wire protocol level and pins the connection to the primary until `COMMIT` or `ROLLBACK`. Implicit transactions (autocommit queries) don't require pinning.

#### Replication Lag Awareness

A write hits the primary, then 50ms later a read goes to a replica that hasn't replicated yet — stale data. Two strategies (configurable):

```toml
[db.read_write_split]
enabled = true
strategy = "sticky-after-write"  # or "lag-aware"
sticky_duration = "2s"           # for sticky-after-write
max_replica_lag = "500ms"        # for lag-aware
```

**Strategy A: Sticky-after-write** (simpler, recommended default)
After a connection performs a write, all subsequent reads from that connection go to the primary for `sticky_duration` seconds. Simple, conservative, slight primary load increase during the sticky window.

**Strategy B: Lag-aware routing** (more complex, better distribution)
ePHPm periodically monitors replica lag via `SHOW SLAVE STATUS` (MySQL) or `pg_stat_replication` (Postgres). Only routes reads to replicas with lag below `max_replica_lag`. Replicas that fall behind are temporarily removed from the read pool.

### Sharding (Roadmap)

Sharding is planned for post-v1. The proxy architecture and SQL parser infrastructure support it, but it's the most complex feature in the DB proxy.

#### What Sharding Means

Unlike read/write splitting (one database, multiple copies), sharding splits **data** across multiple independent database instances. Each shard holds a subset of the data:

```
Shard A: users with id 1-1,000,000
Shard B: users with id 1,000,001-2,000,000
Shard C: users with id 2,000,001-3,000,000
```

The proxy must know which shard holds the data for each query and route accordingly.

#### Shard Key Routing

The application (or ePHPm config) defines a **shard key** — the column used to determine which shard a row lives on:

```toml
[db.sharding]
enabled = true
key_column = "tenant_id"          # shard by tenant
hash_function = "consistent"      # consistent hashing
shards = [
    { name = "shard-a", url = "mysql://shard-a:3306/app", range = "0x0000-0x5555" },
    { name = "shard-b", url = "mysql://shard-b:3306/app", range = "0x5556-0xAAAA" },
    { name = "shard-c", url = "mysql://shard-c:3306/app", range = "0xAAAB-0xFFFF" },
]
```

ePHPm parses the SQL, extracts the shard key from the WHERE clause, hashes it, and routes to the correct shard:

```rust
/// Extract shard key value from a parsed query's WHERE clause
fn extract_shard_key(stmt: &Statement, shard_key: &str) -> Option<ShardKeyValue> {
    // Walk the AST's WHERE clause looking for:
    //   WHERE tenant_id = <value>
    //   WHERE tenant_id IN (<values>)
    // Returns the value(s) for routing
    // ...
}

/// Route a query to the correct shard(s)
fn route_query(stmt: &Statement, config: &ShardConfig) -> RouteDecision {
    match extract_shard_key(stmt, &config.key_column) {
        Some(ShardKeyValue::Single(val)) => {
            // Single shard key — route to one shard
            let shard = config.ring.get(&val);
            RouteDecision::SingleShard(shard)
        }
        Some(ShardKeyValue::Multiple(vals)) => {
            // IN clause — may need multiple shards
            let shards: HashSet<_> = vals.iter()
                .map(|v| config.ring.get(v))
                .collect();
            RouteDecision::MultiShard(shards)
        }
        None => {
            // No shard key in query — must scatter to all shards
            RouteDecision::ScatterGather
        }
    }
}
```

#### The Hard Problems

Sharding introduces several challenges that don't exist in simple read/write splitting:

**1. Scatter-gather queries**

When the shard key isn't in the WHERE clause, ePHPm must send the query to ALL shards and merge results:

```sql
-- Shard key present → single shard, fast
SELECT * FROM orders WHERE tenant_id = 42;

-- No shard key → scatter to all shards, merge, slow
SELECT * FROM orders WHERE status = 'pending' ORDER BY created_at LIMIT 10;
```

The scatter-gather case requires:
- Sending the query to all shards in parallel
- Collecting result sets from each shard
- Merging and re-applying `ORDER BY`, `LIMIT`, `OFFSET` across the merged results
- Handling `COUNT(*)`, `SUM()`, `AVG()` aggregations correctly (sum the counts, sum the sums, recompute averages)

```rust
enum RouteDecision {
    SingleShard(ShardId),
    MultiShard(HashSet<ShardId>),
    ScatterGather,  // all shards — expensive
}

async fn scatter_gather(
    query: &str,
    shards: &[ShardPool],
    merge_strategy: &MergeStrategy,
) -> Result<ResultSet> {
    // Fan out to all shards in parallel
    let futures: Vec<_> = shards.iter()
        .map(|shard| shard.execute(query))
        .collect();
    let results = futures::future::join_all(futures).await;

    // Merge results according to the query's ORDER BY / LIMIT / aggregations
    merge_strategy.merge(results)
}
```

**2. Cross-shard JOINs**

```sql
-- If orders and users are on different shards, this is extremely expensive
SELECT u.name, o.total
FROM users u
JOIN orders o ON u.id = o.user_id
WHERE o.status = 'pending';
```

Options:
- **Prohibit** — return an error if a JOIN spans shards (simplest, what most sharding proxies do)
- **Co-locate** — require tables that JOIN together to shard on the same key (design constraint)
- **Fetch-and-filter** — pull one side locally and filter against the other shard (expensive, last resort)

ePHPm's recommended approach: **co-location by convention**. Tables that need to JOIN should shard on the same key (e.g., both `users` and `orders` shard on `tenant_id`). The admin UI warns when it detects cross-shard JOIN attempts.

**3. Cross-shard transactions**

```sql
BEGIN;
UPDATE accounts SET balance = balance - 100 WHERE tenant_id = 1;  -- shard A
UPDATE accounts SET balance = balance + 100 WHERE tenant_id = 2;  -- shard B
COMMIT;  -- how do you atomically commit across two databases?
```

True distributed transactions require two-phase commit (2PC) — complex and slow. Options:
- **Prohibit** — error if a transaction spans shards (simplest)
- **Best-effort** — commit each shard independently, log failures for manual reconciliation
- **2PC** — full distributed transactions (significant complexity, performance cost)

ePHPm v1 sharding: prohibit cross-shard transactions. This is what ProxySQL and PgDog do. Applications that need cross-shard atomicity use application-level sagas or eventual consistency patterns.

**4. Schema migrations (DDL)**

`ALTER TABLE` must be applied to all shards. ePHPm can fan out DDL statements to all shards in parallel, but failure handling is tricky (what if shard B fails the ALTER while A and C succeed?). This is typically handled outside the proxy via migration tools.

**5. Shard rebalancing**

When adding or removing shards, data must be moved. With consistent hashing, only ~1/N of keys need to move, but the proxy must handle the transition period where some data is on the old shard and some is on the new shard.

#### Implementation Roadmap

| Phase | Scope | Complexity |
|---|---|---|
| **v1** | Connection pooling, multiplexing, query digest, slow query detection, read/write splitting | Moderate |
| **v2** | Single-key shard routing, scatter-gather for keyless queries, shard-aware connection pools | High |
| **v3** | Aggregation merging (COUNT/SUM/AVG across shards), ORDER BY + LIMIT rewriting, cross-shard JOIN detection/warnings | High |
| **v4** | Shard rebalancing, online shard addition/removal, 2PC (optional) | Very High |

The SQL parsing infrastructure (`sqlparser-rs`), wire protocol handling, and connection pool architecture from v1 all carry forward — each phase builds on the previous one.

### PHP Integration: Transparent to the Application

The PHP app connects to `127.0.0.1:3306` (MySQL) or `127.0.0.1:5432` (Postgres) — which is ePHPm's proxy, not the real database. From PHP's perspective, it's talking to a normal database:

```php
// No code changes. Standard PDO.
$pdo = new PDO('mysql:host=127.0.0.1;dbname=myapp', 'user', 'pass');
$stmt = $pdo->prepare('SELECT * FROM users WHERE id = ?');
$stmt->execute([42]);
```

ePHPm intercepts this because it's listening on the MySQL port. The PHP app doesn't know it's talking to a proxy — whether it's doing connection pooling, read/write splitting, or sharding. For even tighter integration, ePHPm could provide SAPI-level functions that bypass TCP entirely:

```php
// Optional: zero-overhead path via SAPI (no TCP, no wire protocol)
$result = ephpm_db_query('SELECT * FROM users WHERE id = ?', [42]);
```

But TCP compatibility is the priority — it means **zero code changes** for existing apps.

#### Auto-Configuration via Environment Injection

Since ePHPm owns both the PHP process and the DB proxy, it can auto-wire the connection — the developer only configures the real database once in `ephpm.toml`, and PHP picks it up automatically:

```toml
# ephpm.toml — the only place you configure your database
[db.mysql]
url = "mysql://appuser:secret@real-db-server:3306/myapp"
max_connections = 50
inject_env = true   # auto-set DB env vars for the PHP app
```

When `inject_env = true`, ePHPm injects environment variables into the PHP runtime before the app boots:

| Variable | Value | Consumed by |
|---|---|---|
| `DB_CONNECTION` | `mysql` | Laravel |
| `DB_HOST` | `127.0.0.1` | Laravel, Symfony, generic |
| `DB_PORT` | `3306` | Laravel, Symfony, generic |
| `DB_DATABASE` | `myapp` | Laravel |
| `DB_USERNAME` | `appuser` | Laravel |
| `DB_PASSWORD` | `secret` | Laravel |
| `DATABASE_URL` | `mysql://appuser:secret@127.0.0.1:3306/myapp` | Symfony, Doctrine |

Laravel reads `DB_HOST` from `.env` with a fallback to actual environment variables. Symfony reads `DATABASE_URL`. Both frameworks pick up ePHPm's injected values automatically — the connection goes through the proxy without any `.env` file changes or application code changes.

The PHP app thinks it's talking to `127.0.0.1:3306`. ePHPm's proxy is listening there, pooling connections, analyzing queries, splitting reads/writes, and forwarding to the real database. Fully transparent.

```
Developer configures:  ephpm.toml → url = "mysql://...@real-db:3306/myapp"
ePHPm injects:         DB_HOST=127.0.0.1, DB_PORT=3306, ...
Laravel reads:         config/database.php → env('DB_HOST') → 127.0.0.1
PDO connects to:       127.0.0.1:3306 (ePHPm proxy)
ePHPm routes to:       real-db:3306 (pooled, analyzed, split)
```

For Postgres, the same pattern applies:

```toml
[db.postgres]
url = "postgres://appuser:secret@real-pg:5432/myapp"
inject_env = true
# Injects: DB_HOST=127.0.0.1, DB_PORT=5432, DATABASE_URL=postgres://...@127.0.0.1:5432/myapp
```

When `inject_env` is disabled, the developer manually points their app at the proxy (e.g., `DB_HOST=127.0.0.1` in `.env`). This is useful when running ePHPm alongside an existing setup or when the developer wants explicit control.

### What ePHPm's DB Proxy Is NOT

- **Not a general-purpose database proxy** — it's purpose-built for the ePHPm PHP server. It doesn't need to handle arbitrary clients, just PHP workers.
- **Not a query rewriter** — ProxySQL can rewrite queries via regex rules. ePHPm observes and analyzes queries, it doesn't modify them (to avoid breaking application semantics).
- **Not a query cache** — ProxySQL caches query results. ePHPm has the KV store for caching. Mixing query caching into the proxy adds complexity and cache invalidation headaches.

### Feature Comparison

| Feature | ProxySQL | PgBouncer | PgDog | ePHPm Goal |
|---|---|---|---|---|
| Connection pooling | Yes | Yes | Yes | v1 |
| Connection multiplexing | Yes (50:1) | Yes | Yes | v1 |
| Query digest / stats | Yes | No | No | v1 |
| Slow query detection | Yes | No | No | v1 + auto-EXPLAIN |
| Read/write splitting | Yes | No | Yes | v1 |
| Replication lag awareness | No | No | Yes | v1 |
| Sharding (single-key) | Yes (regex rules) | No | Yes (wire protocol) | v2 |
| Scatter-gather queries | No | No | Yes | v2 |
| Cross-shard aggregation | No | No | Partial | v3 |
| Shard rebalancing | No | No | No | v4 |
| In-process with app | **No** (separate process) | **No** | **No** | **Yes** (zero TCP to proxy) |
| OTel trace integration | **No** | **No** | **No** | **Yes** (spans per query) |
| Admin UI for queries | **No** (CLI only) | **No** | **No** | **Yes** |
| MySQL support | Yes | No | No | Yes |
| PostgreSQL support | Partial | Yes | Yes | Yes |
| License | GPL-3.0 | ISC | AGPL-3.0 | — |

---

## Latency Analysis: ePHPm vs Competitors

### PHP Embedding Overhead Per Request

| | FrankenPHP (Go + CGO) | RoadRunner (Go + Goridge IPC) | ePHPm (Rust FFI) |
|---|---|---|---|
| **Mechanism** | In-process C calls via CGO | Pipe-based IPC between separate processes | In-process C calls via zero-cost FFI |
| **Per-crossing cost** | ~200ns (CGO boundary) | N/A (different model) | ~0ns (normal C function call) |
| **Crossings per request** | 11+ | 2 round trips (request + response) | 11+ |
| **Serialization** | None (shared memory) | Full request/response serialization into Goridge binary protocol | None (shared memory) |
| **OS context switch** | None (same process) | Yes — switches between Go and PHP processes per round trip | None (same process) |
| **Estimated total overhead** | **~2.2μs** | **~20-80μs** | **~0μs** |

### FrankenPHP CGO Crossings (11+ per request)

Every HTTP request in FrankenPHP crosses the Go↔C boundary for each of these operations:

**Go → C (5-7 crossings):**
1. Thread assignment — dispatching request to a PHP worker
2. Server context update — populating request information
3. Script execution initialization
4. Cleanup / lifecycle management

**C → Go callbacks during PHP execution (4-6+ crossings):**
5. `go_read_post` — reading the HTTP request body
6. `go_read_cookies` — reading request cookies
7. `go_register_variables` — populating `$_SERVER` superglobal
8. `go_write_headers` — sending response headers back to Go
9. `go_ub_write` — called **every time** PHP writes output (`echo`, `print`, etc.) — can fire multiple times per request
10. `go_getenv` — environment variable access

At ~200ns per crossing × 11+ crossings = **~2.2μs minimum per request**. Output-heavy scripts trigger `go_ub_write` many times, increasing this further.

FrankenPHP v1.11.2 specifically celebrated Go 1.26 bringing "30% faster CGO calls" — confirming this overhead is significant enough to optimize. ePHPm eliminates it entirely.

### RoadRunner Goridge IPC Overhead (~20-80μs per request)

RoadRunner uses a completely different model — PHP runs as a separate process and communicates with Go over pipes:

| Component | Estimated Cost |
|---|---|
| Pipe write (Go → PHP request) | ~5-10μs |
| Request serialization (Go side) | ~5-25μs (depends on payload) |
| OS context switch (Go → PHP process) | ~5-10μs |
| Request deserialization (PHP side) | ~5-25μs |
| Pipe write (PHP → Go response) | ~5-10μs |
| Response serialization (PHP side) | ~5-25μs |
| OS context switch (PHP → Go process) | ~5-10μs |
| Response deserialization (Go side) | ~5-25μs |
| **Total per request** | **~20-80μs** |

Additional characteristics:
- 12-byte frame header per Goridge message
- Goridge throughput ceiling: ~300k calls/sec on Ryzen 1700X across 20 threads
- Forces PSR-7 adoption (request/response must be fully serialized) — breaks `$_GET`, `$_POST`, `$_SERVER` superglobals

### Real-World Benchmark Context (Symfony 8, 2026)

From [FrankenPHP vs RoadRunner benchmarks](https://dev.to/mattleads/benchmark-frankenphp-vs-roadrunner-in-symfony-8-2lgp) on 8-core / 16GB RAM:

| Scenario | PHP-FPM | FrankenPHP | RoadRunner |
|---|---|---|---|
| **JSON ping (raw overhead)** | 2,100 req/s / 45ms | 8,500 req/s / 4ms | 9,200 req/s / 3ms |
| **Database write** | 850 req/s / 110ms | 3,200 req/s / 25ms | 3,400 req/s / 22ms |
| **CPU-bound (matrix 150×150)** | 450 req/s / 210ms | 465 req/s / 205ms | 460 req/s / 208ms |
| **Streaming I/O (100K CSV)** | TTFB 30ms | TTFB 5ms | TTFB 12ms |
| **Image processing (p99)** | 1.2s | 950ms | 880ms |
| **Serialization** | 180 req/s | 420 req/s | 405 req/s |
| **Worker restart latency** | N/A | ~40ms | ~15ms |

Key observations:
- RoadRunner slightly beats FrankenPHP on simple JSON despite higher IPC overhead — because FrankenPHP carries Caddy's middleware chain overhead.
- CPU-bound workloads are identical — PHP engine is the bottleneck, not the server layer.
- RoadRunner has lower p99 jitter on CPU-bound tasks (8ms std dev vs 45ms for FrankenPHP) — likely Go GC affecting FrankenPHP more due to in-process PHP memory.
- FrankenPHP wins on TTFB for streaming — in-process embedding avoids pipe buffering.

### ePHPm's Expected Advantage

ePHPm combines the best of both:
- **In-process embedding** (like FrankenPHP) — no IPC serialization, no pipe overhead, no context switches, superglobals work
- **Zero crossing overhead** (better than FrankenPHP) — Rust FFI eliminates the ~2.2μs CGO tax
- **No GC** (better than both) — predictable p99 latencies, no 45ms std dev jitter on CPU-bound tasks
- **No Caddy middleware chain** — direct HTTP handling via hyper, removing the overhead that lets RoadRunner beat FrankenPHP on simple JSON

At scale (100k req/s):

| Server | Overhead CPU burned per second | Per day |
|---|---|---|
| RoadRunner (~50μs IPC) | 5,000ms (5 full core-seconds) | ~7.2 core-hours |
| FrankenPHP (~2.2μs CGO) | 220ms | ~5.3 core-minutes |
| ePHPm (~0μs FFI) | ~0ms | ~0 |

---

## Observability Architecture

### Why OTLP, Not StatsD

StatsD (Etsy's UDP metrics protocol) was originally considered for receiving app-level custom metrics. It's been replaced by **OpenTelemetry Protocol (OTLP)** because:

- StatsD covers metrics only. OTLP covers **metrics + traces + logs** in a single protocol.
- StatsD is legacy. OTLP is the CNCF-graduated industry standard.
- OTLP enables **trace correlation** — stitching app spans with ePHPm's auto-instrumented spans (HTTP, DB, KV). StatsD can't do this.
- The PHP ecosystem has an official OpenTelemetry SDK (`open-telemetry/opentelemetry-php`) with auto-instrumentation for PDO, curl, etc.

### ePHPm as an OTLP Collector

ePHPm acts as a built-in OpenTelemetry Collector endpoint — eliminating the need for a separate `otel-collector` sidecar, `telegraf`, or `datadog-agent`.

**PHP app configuration:**
```bash
# Just point the standard OTLP env var at ePHPm
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
```

The PHP OpenTelemetry SDK sends traces, metrics, and logs to ePHPm over standard OTLP (HTTP on port 4318, gRPC on port 4317).

### Auto-Instrumentation: ePHPm's Unique Advantage

Because ePHPm controls the HTTP layer, DB proxy, and KV store, it can **automatically inject spans** into every trace — without the PHP app doing anything:

```
[HTTP request 320ms]                          ← ePHPm auto-instruments
  └─ [Laravel route dispatch 280ms]           ← PHP app's OTel SDK sends this
       ├─ [DB: SELECT users 12ms]             ← ePHPm DB proxy auto-instruments
       ├─ [DB: UPDATE orders 45ms]            ← ePHPm DB proxy auto-instruments
       ├─ [KV: GET session_abc 0.3ms]         ← ePHPm KV auto-instruments
       └─ [curl: payment API 180ms]           ← PHP OTel auto-instrumentation
```

No other PHP server can produce this view because no other server controls the DB and KV layers. FrankenPHP can't see DB queries. RoadRunner can't see DB queries. Swoole's PDOPool could, but doesn't produce OTel spans.

### Three Output Channels

```
                  Observability Pipeline
                         │
              ┌──────────┼──────────┐
              ▼          ▼          ▼
         Admin UI    Prometheus   OTLP Export
         (built-in)  /metrics     (optional)
                     endpoint
```

**1. Admin UI (built-in, zero-config)**

The primary consumer. Shows traces, metrics, query analysis, and profiling results in ePHPm's embedded web dashboard. Backed by an in-memory ring buffer — stores the last N traces/metrics for recent inspection. Not a long-term analytics platform.

```rust
struct TraceStore {
    traces: VecDeque<Trace>,  // bounded ring buffer (e.g. last 10,000 traces)
    by_id: HashMap<TraceId, usize>,  // index for O(1) lookup
    max_traces: usize,
}
```

Old traces age out automatically. This is a debug/development tool for real-time visibility, not archival storage.

**2. Prometheus `/metrics` endpoint**

Standard OpenMetrics/Prometheus scrape endpoint. Any existing Grafana/Prometheus setup can scrape ePHPm with zero configuration. Exposes:
- HTTP request metrics (rate, latency percentiles, status codes)
- Thread pool utilization (busy, idle, queue depth)
- DB proxy metrics (query rate, latency, pool utilization, slow queries)
- KV store metrics (hit rate, memory usage, cluster membership)
- PHP internals (OPcache hit rate, memory usage per worker)

**3. OTLP export (optional, for existing observability stacks)**

Teams with existing Jaeger, Grafana Tempo, Datadog, or New Relic deployments configure ePHPm to forward telemetry data upstream:

```yaml
# ephpm.toml
[telemetry.export.otlp]
endpoint = "jaeger.internal:4317"    # or tempo, datadog, etc.
protocol = "grpc"                     # or "http"
# headers = { "dd-api-key" = "..." } # for Datadog, etc.
```

ePHPm re-exports the combined data (app spans + auto-instrumented spans) to the external backend. The external tool gets richer traces than it would from the PHP app alone, because ePHPm has injected its DB/KV/HTTP spans.

### What ePHPm Does NOT Do

- **Does not embed Jaeger or Grafana** — they're large, separate services with their own storage backends. Bundling them would bloat the binary and add external dependencies. Teams that need them already run them.
- **Does not replace APM tools** — ePHPm's admin UI is for real-time inspection and debugging, not long-term trending or alerting. For that, export to Datadog/Grafana/New Relic.
- **Does not require the PHP app to use OTel** — auto-instrumentation from the server side works with zero PHP changes. The PHP OTel SDK is optional (adds app-level spans for richer traces).

### The Pitch

"ePHPm auto-instruments the full request lifecycle — HTTP, DB queries, cache operations, and your app's own traces — in a single process. No otel-collector sidecar, no Datadog agent, no configuration. Point your app at `localhost:4318` and everything lights up in the admin UI. Need Jaeger or Grafana? Add one line of config to forward everything upstream."

---

## Strategic Considerations

### PHP Execution Model

FrankenPHP's approach (embedding `libphp`, populating superglobals natively) is the most adoption-friendly model. It allows existing PHP apps to work without code changes, which is critical for adoption. RoadRunner's IPC model (Goridge) forces PSR-7 adoption and breaks superglobals, which is a significant migration barrier.

ePHPm follows FrankenPHP's embedding approach but in Rust, eliminating the CGO overhead while preserving full superglobal compatibility.

### Competitive Positioning

- **vs FrankenPHP**: Zero-overhead PHP embedding (no CGO), plus DB pooling, clustered KV, admin dashboard, query analysis, debug/profiling UI. Built-in OTLP collector with auto-instrumented traces across HTTP/DB/KV layers. FrankenPHP has none of the infrastructure or observability features and pays CGO tax on every request.
- **vs RoadRunner**: Zero-overhead PHP embedding, DB pooling, multi-node KV clustering, admin dashboard, full-stack auto-instrumentation. Plus superglobals work (RoadRunner breaks them, requires PSR-7). RoadRunner has Prometheus and OTel support but can't auto-instrument DB queries or cache operations because it doesn't control those layers.
- **vs Swoole**: Cross-platform (not Linux-only), doesn't require a PECL extension, superglobals work. Swoole has DB pooling but ePHPm matches that and adds clustering, full-stack observability, and OTLP-native telemetry.
- **vs all three**: No GC pauses, lower memory footprint, predictable p99 latencies. Only server that auto-instruments the full request lifecycle (HTTP + DB + KV + app spans) in a single trace. Benchmarkable advantages.

### Benchmark Strategy

Key benchmarks to publish:
1. **PHP embedding overhead** — Rust FFI vs Go CGO per-call latency (the 200ns × 11+ calls story)
2. **p99 latency under load** — no GC pauses vs Go's GC jitter
3. **DB proxy latency** — added latency per proxied query vs direct connection
4. **Memory per connection** — HTTP + DB proxy connections at scale
5. **Requests/sec** — head-to-head with FrankenPHP on identical PHP workloads

---

## Repository Structure

Cargo workspace with virtual manifest and `crates/` directory:

```
ephpm/
├── Cargo.toml                  # Virtual manifest ([workspace] only)
├── Cargo.lock
├── rust-toolchain.toml
├── rustfmt.toml
├── clippy.toml
├── deny.toml
├── ephpm.toml                  # Example config file
├── .github/
│   └── workflows/
│       ├── ci.yml              # Lint, test, deny
│       └── release.yml         # Build matrix (PHP 8.3/8.4 × linux/mac/windows)
├── crates/
│   ├── ephpm/                  # Binary crate (main entry point)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs         # CLI (clap), config loading, server boot
│   ├── ephpm-server/           # HTTP server crate
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── router.rs       # Route .php to PHP, else static files
│   │       └── static_files.rs # Static file serving
│   ├── ephpm-php/              # PHP embedding crate
│   │   ├── Cargo.toml
│   │   ├── build.rs            # bindgen + link libphp.a
│   │   ├── wrapper.h           # C header includes for bindgen
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── sapi.rs         # Custom SAPI implementation
│   │       ├── request.rs      # HTTP request → PHP request mapping
│   │       └── response.rs     # PHP output → HTTP response mapping
│   └── ephpm-config/           # Configuration crate
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs          # Config structs + figment loading
├── benches/
│   └── throughput.rs           # Criterion benchmarks
├── tests/
│   └── integration/
│       └── wordpress.rs        # WordPress smoke test
└── docs/
    ├── analysis/               # Competitive analysis
    └── architecture/           # Architecture docs
```

---

## MVP Specification

### Goal

A single Rust binary that reads a TOML config, boots an HTTP server, embeds PHP via libphp, and serves a WordPress site.

### What the MVP Includes

1. **`ephpm` binary** — single Rust binary with PHP statically linked
2. **TOML config** — `ephpm.toml` with `[server]` and `[php]` sections
3. **HTTP server** — hyper-based, HTTP/1.1 + HTTP/2
4. **PHP execution** — custom SAPI, ZTS mode, concurrent via `spawn_blocking` + TSRM
5. **Static file serving** — CSS/JS/images served directly (not through PHP)
6. **WordPress demo** — documented setup

### What the MVP Does NOT Include

- TLS / ACME
- DB proxy / connection pooling
- KV store
- Clustering
- Observability / admin UI
- Worker mode (persistent PHP processes)
- ZTS / multi-threaded PHP

### MVP Request Flow

```
Client ──HTTP──► hyper (tokio)
                    │
                    ▼
                Router
                    │
            ┌───────┴───────┐
            │ .php request? │
            └───┬───────┬───┘
            no  │       │ yes
                ▼       ▼
          static file   spawn_blocking
          serving           │
                            ▼
                     Mutex<PhpRuntime>
                            │
                     1. Set SAPI request info
                     2. php_request_startup()
                     3. php_execute_script(index.php)
                        ├── ub_write() → buffer body
                        ├── send_header() → capture headers
                        ├── read_post() → provide POST data
                        └── read_cookies() → provide cookies
                     4. php_request_shutdown()
                     5. Return (status, headers, body)
                            │
                            ▼
                Build hyper::Response
                            │
Client ◄──HTTP──────────────┘
```

---

## Tooling & CI

| Tool | Purpose |
|------|---------|
| `rustfmt` | Code formatting |
| `clippy` | Linting |
| `cargo-deny` | License audit, advisory DB, duplicate crate detection |
| `cargo-nextest` | Faster test runner |
| `cargo-llvm-cov` | Code coverage |
| `criterion` | Benchmarking |
| `bindgen` | Generate Rust FFI bindings from PHP C headers |

### CI Matrix

One binary per PHP version per platform (static linking). CI matrix builds for PHP 8.3 and 8.4 across all supported platforms.

**Platform matrix:**

| Platform | Target | Use Case |
|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | Production servers, CI, WSL2 |
| Linux aarch64 | `aarch64-unknown-linux-gnu` | ARM servers (Graviton, Ampere) |
| macOS Apple Silicon | `aarch64-apple-darwin` | Developer machines (M1–M4) |
| macOS Intel | `x86_64-apple-darwin` | Older Macs, CI runners |
| Windows x86_64 | `x86_64-pc-windows-msvc` | Developer machines (native Windows dev) |

Release artifacts: `ephpm-0.1.0-php8.4-linux-x86_64`, `ephpm-0.1.0-php8.4-windows-x86_64.exe`, etc. Linux is the primary production target. macOS and Windows builds enable native local development without Docker/WSL.

---

## PHP Execution Modes

ePHPm supports two PHP execution modes, controlled by `php.mode` in config:

### Embedded mode (default)

PHP is statically linked into the binary via FFI. Zero IPC overhead — Rust calls PHP C functions directly through the SAPI.

| Variant | Concurrency | Status |
|---------|-------------|--------|
| ZTS | `spawn_blocking` + per-thread TSRM — concurrent PHP execution | **Implemented** |
| NTS (Windows) | `Mutex` + `spawn_blocking` — serialized execution | **Implemented** |

**When to use:** Production deployments where performance matters and the bundled PHP extensions are sufficient.

```toml
[php]
mode = "embedded"   # default, no need to specify
```

### External mode

ePHPm spawns PHP worker processes and communicates over stdin/stdout pipes using a binary protocol. The user provides their own PHP binary — any version, any extensions, any custom patches.

```toml
[php]
mode = "external"
binary = "/usr/bin/php"
workers = 4
worker_script = "vendor/ephpm/worker.php"
```

**How it works:**

1. ePHPm spawns N long-lived PHP worker processes (`php worker.php`)
2. Each worker runs a loop: read request from stdin → execute → write response to stdout
3. ePHPm routes incoming HTTP requests to available workers
4. If a worker crashes, ePHPm restarts it automatically

**When to use:**
- Custom PHP builds with proprietary or uncommon extensions
- PHP versions not yet supported by static-php-cli
- Environments where rebuilding the binary isn't practical
- Teams that already manage their own PHP installations

**Tradeoffs vs embedded:**

| | Embedded | External |
|---|---|---|
| Overhead per request | ~0 (native C call) | ~50-200μs (IPC) |
| Concurrency | Concurrent threads (ZTS) | Multiple worker processes |
| Crash isolation | PHP crash = process crash | Worker crash → auto-restart |
| PHP version | Built into binary | User's own binary |
| Custom extensions | Rebuild SDK | Install normally |
| Zero-config drop-in | Yes | Requires `worker.php` package |

**PHP worker side:** External mode requires a small PHP package (`ephpm/worker`) that provides the request loop. This is similar to RoadRunner's worker model — the PHP side reads requests from stdin, runs the application, and writes responses to stdout.

All other ePHPm features (HTTP server, static files, routing, config, and planned features like DB proxy, KV store, admin UI, observability) work identically in both modes.

### Thread Safety: ZTS (embedded mode)

PHP is compiled with ZTS (`--enable-zts`). Each `spawn_blocking` thread auto-registers with TSRM on first use, getting its own isolated PHP context. Multiple PHP requests execute concurrently. The mutex only protects one-time init/shutdown. Windows builds use NTS with serialized execution.

### Building libphp.a

Use `static-php-cli` to build a static `libphp.a`:

```bash
bin/spc download --with-php=8.4 --for-extensions="bcmath,curl,dom,exif,fileinfo,filter,gd,hash,iconv,json,mbstring,mysqli,openssl,pcre,session,simplexml,sodium,xml,xmlreader,zip,zlib"
bin/spc build "bcmath,curl,dom,exif,fileinfo,filter,gd,hash,iconv,json,mbstring,mysqli,openssl,pcre,session,simplexml,sodium,xml,xmlreader,zip,zlib" --build-embed
```

### FFI Approach

Custom `build.rs` using `bindgen` to generate FFI bindings from PHP's C headers. Link against `libphp.a` statically. The SAPI implementation is custom (not using `ripht-php-sapi` or `ext-php-rs` — those are references, not dependencies).

### SAPI Callbacks Required for WordPress

| Callback | Purpose |
|----------|---------|
| `ub_write` | Capture PHP output (echo, print, templates) |
| `send_headers` / `send_header` | Capture HTTP response headers |
| `read_post` | Provide POST body to PHP |
| `read_cookies` | Provide cookie string to PHP |
| `register_server_variables` | Populate `$_SERVER` |
| `startup` / `shutdown` | PHP lifecycle (MINIT/MSHUTDOWN) |
| `activate` / `deactivate` | Per-request lifecycle (RINIT/RSHUTDOWN) |
| `flush` | Flush output buffer |
| `log_message` | PHP error logging → tracing |

### $_SERVER Variables WordPress Needs

```
REQUEST_URI, REQUEST_METHOD, SCRIPT_FILENAME, SCRIPT_NAME,
DOCUMENT_ROOT, SERVER_NAME, SERVER_PORT, SERVER_SOFTWARE,
SERVER_PROTOCOL, HTTPS, HTTP_HOST, HTTP_COOKIE,
CONTENT_TYPE, CONTENT_LENGTH, QUERY_STRING, PATH_INFO,
PHP_SELF, REMOTE_ADDR, REMOTE_PORT
```

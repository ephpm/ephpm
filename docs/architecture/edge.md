# Edge Deployment

ePHPm's embedded SQLite + sqld replication makes global edge deployment possible with a single binary per region. Every node has a full local copy of the database — reads are served at disk speed with no cross-ocean round trips.

## Architecture

```
                         ┌─────────────────────┐
                         │   CDN (Cloudflare)   │
                         └──────────┬──────────┘
                                    │
              ┌─────────────────────┼─────────────────────┐
              │                     │                     │
     ┌────────▼────────┐  ┌────────▼────────┐  ┌────────▼────────┐
     │  Frankfurt       │  │  Tokyo           │  │  São Paulo      │
     │  ephpm + sqld    │  │  ephpm + sqld    │  │  ephpm + sqld   │
     │  (primary)       │  │  (replica)       │  │  (replica)      │
     │  app.db ◄────────┼──┼── WAL sync ──────┼──┼── WAL sync     │
     └─────────────────┘  └─────────────────┘  └─────────────────┘
```

Each region runs one ephpm binary with sqld as a sidecar. sqld replicates via async WAL frame streaming over gRPC. GeoDNS or CDN routing sends users to the nearest node.

## What Works Well

**Reads (95%+ of traffic)**

Every node has a full local copy of the SQLite database. A blog reader in Tokyo hits the Tokyo node — the query runs against local disk in microseconds. No network hop to a central database.

| Operation | Latency (local) | Latency (cross-region to central DB) |
|-----------|----------------|--------------------------------------|
| SELECT query | 0.01-1 ms | 150-300 ms |
| Page render (30 queries) | 1-30 ms | 4,500-9,000 ms |

For read-heavy sites, this is a 100-1000x improvement over a single-region database.

**Eventually consistent data**

Blog posts, comments, user profiles, settings — these replicate to all nodes within seconds. A post published in Frankfurt appears in Tokyo after one replication cycle (network latency + WAL frame transfer, typically 1-5 seconds).

**Static content and cached pages**

Served directly from the local node. No replication concern at all.

## The Write Problem

SQLite is single-writer. sqld has one primary that accepts writes. Replicas forward writes to the primary.

**Impact of cross-region write latency:**

| Scenario | Writes | Latency per write | Total overhead |
|----------|--------|-------------------|----------------|
| Same region as primary | 10 | ~1 ms | ~10 ms |
| Cross-region (100ms RTT) | 10 | ~100 ms | ~1 second |
| Cross-ocean (250ms RTT) | 10 | ~250 ms | ~2.5 seconds |

A WordPress admin saving a post in Tokyo when the primary is in Frankfurt: each of the ~10 sequential INSERT/UPDATE statements takes a 250ms round trip. The save takes ~2.5 seconds instead of ~10ms. Noticeable but not broken.

For anonymous readers (page views, no writes): zero impact.

## Solutions for Write Latency

### 1. Per-Site Regional Primaries (recommended)

With virtual hosts, each site has its own SQLite database. Each site can designate a different region as its primary. Writes go to the nearest primary — no cross-ocean traffic.

```
Frankfurt node:
  alice-blog.com  → primary (Alice is in Germany)
  bobs-recipes.com → replica (Bob is in Japan)

Tokyo node:
  alice-blog.com  → replica
  bobs-recipes.com → primary (Bob is in Japan)
```

Configuration via per-site `site.toml`:

```toml
# Frankfurt: /var/www/sites/alice-blog.com/site.toml
[db.sqlite.replication]
role = "primary"
```

```toml
# Tokyo: /var/www/sites/alice-blog.com/site.toml
[db.sqlite.replication]
role = "replica"
primary_grpc_url = "http://frankfurt:5001"
```

Each site writes locally to its own primary. Reads are always local everywhere. The only cross-region traffic is async WAL replication — background, non-blocking.

This is per-site sharding by region. No single write crosses an ocean.

**Status:** Requires Phase 2 of virtual hosts (per-site SQLite databases and per-site replication config). The architecture supports it — it's implementation work.

### 2. CDN + Aggressive Caching

Put a CDN (Cloudflare, Fastly, CloudFront) in front of all edge nodes. Cache WordPress pages at the CDN layer. Most "writes" from anonymous users are actually just page views that don't modify the database.

Real writes (admin panel, comments, WooCommerce orders) are rare and typically come from known regions. The 250ms overhead is only felt by admins and authenticated users.

```toml
# WordPress: use page caching + object caching
# ephpm: enable KV store as object cache
[kv]
memory_limit = "128MB"
```

With full-page caching, the CDN serves 99%+ of requests. ePHPm edge nodes only handle cache misses and authenticated requests.

### 3. Write Buffering (future, experimental)

Buffer writes locally and batch-forward to the primary. The user sees a fast response; writes replicate asynchronously.

**Tradeoff:** If the local node dies before the batch reaches the primary, those writes are lost. Also, PHP expects synchronous `last_insert_id` responses, which complicates buffering for INSERT statements.

This is not implemented and may not be worth the complexity for the target use case.

### 4. Accept the Latency

For many sites, the write latency is acceptable:

- A blog author saves a post: 2-3 seconds instead of instant. Happens a few times a day.
- A comment is submitted: 250ms extra. Unnoticeable to the commenter.
- WooCommerce checkout: not recommended for cross-region — use a regional primary or external database.

The question is: does the site have writes that are latency-sensitive AND originate from multiple regions? Most WordPress sites don't.

## Use Case Fit

| Use case | Edge with sqld? | Recommendation |
|----------|-----------------|----------------|
| Read-heavy blog network | **Excellent** | Deploy globally, single primary |
| Multi-author CMS (writes from one region) | **Good** | Primary in the author's region |
| Multi-author CMS (writes from many regions) | **OK** | Per-site regional primaries |
| Documentation / marketing sites | **Excellent** | Reads are 99.9% of traffic |
| E-commerce (transactions, inventory) | **Poor** | Use regional MySQL or CockroachDB |
| Real-time collaboration | **Poor** | SQLite single-writer is wrong tool |
| Static sites with rare updates | **Overkill** | Just use a CDN |

## Deployment Example: 3-Region WordPress Network

Three regions serving a network of WordPress blogs. Each blog's primary is in the region where its author lives.

**Infrastructure:**

| Region | Provider | Instance | Monthly |
|--------|----------|----------|---------|
| Frankfurt | Hetzner CAX11 | 2 ARM / 4 GB | $3.69 |
| Tokyo | Vultr | 1 vCPU / 1 GB | $5.00 |
| São Paulo | DigitalOcean | 1 vCPU / 1 GB | $6.00 |
| **Total** | | | **$14.69/mo** |

**Configuration (each node):**

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/marketing"
sites_dir = "/var/www/sites"

[cluster]
enabled = true
bind = "0.0.0.0:7946"
join = ["frankfurt:7946", "tokyo:7946", "saopaulo:7946"]
```

Each site has a `site.toml` in its directory specifying which region is primary. All other regions are replicas that sync automatically.

**What users experience:**

- A reader in Japan requests `bobs-recipes.com` → GeoDNS routes to Tokyo → local SQLite read → ~5ms page render
- Bob (in Tokyo) saves a new recipe → writes go to local primary → instant save → replicates to Frankfurt and São Paulo in ~1-3 seconds
- A reader in Germany requests `bobs-recipes.com` → Frankfurt node → local replica read → sees the new recipe after replication lag (~1-5 seconds)

**Cost per site:** With 30 blogs across 3 regions, that's ~$0.49/site/month for global edge deployment.

## Comparison to Traditional Edge Approaches

| Approach | Read latency | Write latency | Complexity | Cost |
|----------|-------------|---------------|------------|------|
| Single-region MySQL + CDN | CDN hit: fast, miss: 100-300ms | Low (same region) | Low | $$ |
| PlanetScale (Vitess) | Low (regional reads) | Low (regional writes) | Medium | $$$$ |
| CockroachDB | Low (leaseholder reads) | Medium (consensus) | High | $$$$ |
| **ePHPm + sqld edge** | **< 1ms (local disk)** | **Low (regional primary)** | **Low** | **$** |
| Cloudflare D1 | Low (edge reads) | Medium (primary write) | Low | $$ |

ePHPm's edge story is: the simplicity and cost of SQLite, with the read performance of a local database at every edge, and the write consistency of a single-writer model. It's not for every workload — but for read-heavy WordPress/CMS sites, it's hard to beat on cost and simplicity.

## Limitations

- **Single-writer per database** — SQLite fundamental. One primary per site.
- **Async replication** — replicas may lag by seconds. Not suitable for strong consistency requirements.
- **No multi-primary** — sqld doesn't support write-write replication or CRDTs.
- **Failover across regions is slow** — re-bootstrapping a new primary from a remote replica transfers the entire database over the internet.
- **Windows** — sqld not available on Windows. Edge nodes must be Linux or macOS.
- **Per-site databases required** — edge with regional primaries requires Phase 2 virtual host support (per-site SQLite + per-site replication config).

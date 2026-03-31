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

## Traffic Routing: Getting Users to the Nearest Node

The edge nodes are deployed — now users need to reach the closest one. Three approaches, from simplest to most powerful.

### Tier 1: GeoDNS (recommended to start)

Use a DNS provider with location-aware responses. Your domain resolves to different IPs based on where the user is. No special infrastructure needed.

**How it works:**

1. Customer sets: `alice-blog.com CNAME edge.yoursaas.com`
2. Your GeoDNS returns the nearest node's IP for `edge.yoursaas.com`
3. ephpm handles ACME/Let's Encrypt cert issuance on the fly
4. Request served from the nearest node with local SQLite reads

**Providers:**

| Provider | Cost | Notes |
|----------|------|-------|
| AWS Route53 (latency-based) | ~$1.50/mo | Per hosted zone + per-query |
| Cloudflare (free plan) | $0 | Proxy mode with load balancing ($5/mo for geo steering) |
| NS1 | ~$8/mo | Advanced traffic management |

**Total cost for 3-region edge:** ~$16.50/mo (VMs + GeoDNS).

**Tradeoff:** DNS TTL means failover takes 30-300 seconds. Not instant, but fine for most sites.

### Tier 2: Managed Anycast (easy, no BGP knowledge)

A provider gives you an anycast IP that routes to the nearest server automatically at the network layer — no DNS tricks.

| Provider | How it works | Cost |
|----------|-------------|------|
| **Fly.io** | Every app gets anycast IPv4+IPv6 automatically. Deploy ephpm as a container, Fly routes globally. | ~$15-50/mo for 3 regions |
| **Vultr Anycast LB** | Managed anycast IP routing to your Vultr VMs. | $10/mo + VM costs |
| **AWS Global Accelerator** | 2 anycast IPs routing to your ALBs/instances in multiple regions. | ~$18/mo + data transfer |

Fly.io is the simplest — zero networking config, just deploy to regions.

### Tier 3: Own Your Anycast (full control)

Run BGP yourself. The same IP address announced from every edge location. The internet's routing protocol sends users to the nearest node at the network layer — sub-second failover, no DNS involved.

**Requirements:**

| Component | What | Cost |
|-----------|------|------|
| ASN | Autonomous System Number (leased via LIR sponsor) | ~$75/yr |
| IPv4 /24 | 256 IP addresses (minimum BGP will propagate) — lease, don't buy | ~$100-150/mo |
| BGP sessions | Each VM runs BIRD daemon, peers with host's routers | Free on Vultr |
| IRR + RPKI | Route objects + cryptographic origin authorization | Free (part of ASN setup) |

**Setup with Vultr (cheapest practical option):**

| Component | Monthly |
|-----------|---------|
| ASN lease | ~$6 (amortized) |
| /24 IPv4 lease | ~$100-150 |
| 5 Vultr VMs (1 vCPU/1 GB, BGP enabled, 32 locations available) | $30 |
| **Total** | **~$136-186/mo** |

Vultr offers free BGP sessions on all plans. You configure BIRD (BGP routing daemon) on each VM to announce your /24. All 5 VMs advertise the same IP — users hit the nearest one.

**Who should do this:** Only at scale (hundreds of thousands of sites) or if you need instant failover / DDoS absorption. BGP misconfiguration can leak routes and affect other networks — you need to know what you're doing.

**Providers with BGP support:**

| Provider | Locations | VM cost | Notes |
|----------|-----------|---------|-------|
| Vultr | 32 global | $6/mo | Most popular for small anycast |
| BuyVM | 4 (US + LU) | $3.50/mo | Very cheap, limited locations |
| Path.net | US + EU | Custom | DDoS-focused, hands-on support |
| iFog | EU | ~$3/mo | Popular in hobbyist BGP community |

### SaaS: Custom Domain Setup

For a hosting SaaS where customers bring their own domains:

1. Customer adds one DNS record: `alice-blog.com CNAME edge.yoursaas.com` (or A record to your anycast IP)
2. First request arrives at nearest ephpm node
3. ePHPm's built-in ACME issues a Let's Encrypt cert automatically
4. `Host: alice-blog.com` matches the vhost directory
5. All subsequent requests served over HTTPS from local SQLite

No Cloudflare for SaaS needed. No manual cert provisioning. The customer's only action is adding one DNS record.

### Routing Recommendation

| Stage | Approach | Monthly cost | Complexity |
|-------|----------|-------------|------------|
| Starting out | GeoDNS (Route53 / Cloudflare) | ~$1.50 | Minimal |
| Growing (need simplicity) | Fly.io managed anycast | ~$15-50 | Deploy and forget |
| Scale (need control) | Own ASN + Vultr BGP | ~$136-186 | Requires BGP expertise |

Start with GeoDNS. It works today with no infrastructure changes. Move to managed or owned anycast when you outgrow it.

## Comparison to Traditional Edge Approaches

| Approach | Read latency | Write latency | Complexity | Cost |
|----------|-------------|---------------|------------|------|
| Single-region MySQL + CDN | CDN hit: fast, miss: 100-300ms | Low (same region) | Low | $$ |
| PlanetScale (Vitess) | Low (regional reads) | Low (regional writes) | Medium | $$$$ |
| CockroachDB | Low (leaseholder reads) | Medium (consensus) | High | $$$$ |
| **ePHPm + sqld edge** | **< 1ms (local disk)** | **Low (regional primary)** | **Low** | **$** |
| Cloudflare D1 | Low (edge reads) | Medium (primary write) | Low | $$ |

ePHPm's edge story is: the simplicity and cost of SQLite, with the read performance of a local database at every edge, and the write consistency of a single-writer model. It's not for every workload — but for read-heavy WordPress/CMS sites, it's hard to beat on cost and simplicity.

## SaaS Cost Model

Running a multi-tenant WordPress hosting SaaS across 3 regions:

| Component | 100 sites | 1,000 sites |
|-----------|-----------|-------------|
| 3 edge VMs (Hetzner/Vultr/DO) | $15/mo | $45/mo (larger VMs) |
| GeoDNS (Route53) | $1.50/mo | $1.50/mo |
| ACME certs (Let's Encrypt) | Free | Free |
| **Total** | **$16.50/mo** | **$46.50/mo** |
| **Per customer** | **$0.165/mo** | **$0.047/mo** |

At scale with owned anycast:

| Component | 10,000 sites |
|-----------|-------------|
| 5 Vultr VMs (4 vCPU / 8 GB) | $200/mo |
| ASN + /24 lease | $150/mo |
| **Total** | **$350/mo** |
| **Per customer** | **$0.035/mo** |

## Limitations

- **Single-writer per database** — SQLite fundamental. One primary per site.
- **Async replication** — replicas may lag by seconds. Not suitable for strong consistency requirements.
- **No multi-primary** — sqld doesn't support write-write replication or CRDTs.
- **Failover across regions is slow** — re-bootstrapping a new primary from a remote replica transfers the entire database over the internet.
- **Windows** — sqld not available on Windows. Edge nodes must be Linux or macOS.
- **Per-site databases required** — edge with regional primaries requires Phase 2 virtual host support (per-site SQLite + per-site replication config).

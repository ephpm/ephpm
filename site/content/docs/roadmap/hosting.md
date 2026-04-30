# Hosting & Resource Requirements

ePHPm is designed to run on the smallest cloud VMs available. A single binary with embedded SQLite means no MySQL server, no PHP-FPM, no Redis — just one process and a database file.

## Resource Profile

### Memory

| Component | Typical | Peak | Configurable |
|-----------|---------|------|-------------|
| ephpm binary + tokio runtime | 30 MB | 45 MB | No |
| PHP worker (per thread) | 45-70 MB | 130 MB | `php.memory_limit` (default: 128M) |
| SQLite engine + WAL cache | 10 MB | 30 MB | SQLite PRAGMA cache_size |
| KV store | 5 MB | 256 MB | `kv.memory_limit` (default: 256MB) |
| Query stats digests | 2 MB | 50 MB | `db.analysis.digest_store_max_entries` |

**Total by worker count:**

| Workers | Typical | Peak | Good for |
|---------|---------|------|----------|
| 1 | ~130 MB | ~460 MB | Personal blog, < 5 req/s |
| 2 | ~190 MB | ~600 MB | Small business site |
| 4 | ~270 MB | ~900 MB | Medium traffic site |
| 8 | ~430 MB | ~1.5 GB | High traffic single-node |

Worker count defaults to CPU count (capped at 16). Override with `php.workers`.

**To fit in 512 MB RAM:**
```toml
[php]
workers = 1
memory_limit = "64M"

[kv]
memory_limit = "32MB"
```

### CPU

WordPress is CPU-bound during PHP execution. SQLite adds minimal overhead.

| Operation | CPU time per request |
|-----------|---------------------|
| WordPress page render (uncached) | 50-200 ms |
| SQL translation (litewire, ~30 queries) | 1-15 ms |
| SQLite query execution (~30 queries) | 0.3-30 ms |
| Static file serve (CSS/JS/images) | < 0.1 ms |

**Throughput per vCPU:**

| Scenario | req/s per core |
|----------|---------------|
| WordPress uncached (full render) | 5-10 |
| WordPress with page cache plugin | 50-100+ |
| WordPress with KV object cache | 25-50 |
| Static assets only | 2,500+ |

**Minimum viable CPU:** 1 shared vCPU handles a personal blog. 2 vCPUs handles a small business site. 4+ vCPUs for sites with consistent traffic.

### Disk

| Component | Size |
|-----------|------|
| ephpm binary (release, with PHP + sqld) | ~35-50 MB |
| WordPress installation | ~60-80 MB |
| SQLite database (typical blog) | 10-100 MB |
| SQLite WAL file (during writes) | Up to DB size |
| PHP OPcache (in memory, not disk) | 0 |

A 10 GB SSD is more than enough. SQLite benefits from fast random I/O — SSD is recommended, spinning disk works but degrades write throughput.

## Cloud Provider Compatibility

Every major cloud provider has a VM that can run ePHPm with WordPress. The cheapest options:

### Best value (< $5/mo)

| Provider | Instance | vCPUs | RAM | Disk | Price/mo | Fit |
|----------|----------|-------|-----|------|----------|-----|
| **Hetzner** CAX11 | 2 ARM | 4 GB | 40 GB SSD | $3.69 | Excellent — 4 workers, room to spare |
| **Hetzner** CX22 | 2 x86 | 4 GB | 40 GB SSD | $4.35 | Excellent — same as above |
| **OVHcloud** Starter | 1 x86 | 2 GB | 25 GB SSD | $3.50 | Good — 3 workers comfortable |
| **Oracle** Free Tier | 4 ARM | 24 GB | 200 GB | $0.00 | Overkill — could run clustered |
| **Vultr** Smallest | 1 x86 | 512 MB | 10 GB SSD | $2.50 | Tight — 1 worker, minimal KV |
| **AWS** t4g.nano | 2 ARM | 512 MB | EBS | $3.07 | Tight — 2 workers, reduce memory_limit |

### Hyperscaler options ($5-10/mo)

| Provider | Instance | vCPUs | RAM | Price/mo | Fit |
|----------|----------|-------|-----|----------|-----|
| **AWS** t4g.micro | 2 ARM (Graviton) | 1 GB | $6.12 | Good — 2-3 workers |
| **Azure** B2pls v2 | 2 ARM (Ampere) | 1 GB | $6.33 | Good — 2-3 workers |
| **GCP** e2-small | 0.5 x86 (shared) | 2 GB | $12.23 | Good on memory, slow CPU |
| **DigitalOcean** Basic | 1 x86 | 1 GB | $6.00 | Good — 2 workers |
| **Linode** Nanode | 1 x86 | 1 GB | $5.00 | Good — 2 workers |

### Container platforms

| Provider | Smallest | RAM | Price/mo | Notes |
|----------|----------|-----|----------|-------|
| **Fly.io** | 1 shared vCPU | 512 MB | $3.57 | Works, but ephemeral disk — mount a volume for SQLite |
| **Railway** | Shared | Usage-based | ~$10-12 | Container PaaS, consumption pricing |
| **Render** | 0.5 vCPU | 512 MB | $7.00 | Ephemeral disk, add persistent disk for SQLite |

Container platforms work but need persistent storage for the SQLite database file. Fly.io volumes ($0.15/GB/mo) and Render disks ($0.25/GB/mo) solve this.

### Not compatible

| Provider | Why |
|----------|-----|
| **Cloudflare Workers** | Serverless, no VM, no persistent process |
| **Fly.io** 256 MB tier | 256 MB too small for PHP + WordPress |
| **GCP** e2-micro (0.25 vCPU) | Shared 0.25 vCPU is too slow for PHP rendering |

## Recommended Configurations

### Personal blog (< 1,000 visitors/day)

**VM:** Any 1 vCPU / 512 MB-1 GB instance ($2.50-$6/mo)

```toml
[php]
workers = 1
memory_limit = "64M"

[kv]
memory_limit = "32MB"

[db.sqlite]
path = "/var/lib/ephpm/app.db"
```

Handles ~5 req/s dynamic, thousands of static asset requests. Install a WordPress page cache plugin for best results.

### Small business site (< 10,000 visitors/day)

**VM:** 2 vCPU / 2-4 GB ($3.69-$12/mo)

```toml
[php]
workers = 4
memory_limit = "128M"

[kv]
memory_limit = "128MB"

[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.proxy]
hrana_listen = "127.0.0.1:8080"
```

Handles ~20-40 req/s dynamic. Enable Hrana for external tooling access. Use the KV store as a WordPress object cache for 2-3x throughput improvement.

### Production with HA (multi-node)

**VMs:** 3x 2 vCPU / 4 GB ($11-$15/mo total on Hetzner)

```toml
[php]
workers = 4

[db.sqlite]
path = "/var/lib/ephpm/app.db"

[db.sqlite.replication]
role = "auto"

[cluster]
enabled = true
join = ["node1:7946", "node2:7946", "node3:7946"]
```

Automatic primary election, WAL frame replication, failover in seconds. All three nodes serve reads; writes go to the elected primary.

### Already have MySQL? Skip SQLite entirely

**VM:** Any size — ephpm acts as a pooling proxy

```toml
[db.mysql]
url = "mysql://user:pass@db-server:3306/myapp"
max_connections = 20

[db.mysql.replicas]
urls = ["mysql://user:pass@replica:3306/myapp"]

[db.read_write_split]
enabled = true
```

## Comparison: ePHPm vs Traditional Stack

Running WordPress on a $5/mo VM:

| | ePHPm | Nginx + PHP-FPM + MySQL |
|---|-------|------------------------|
| Processes | 1 | 3+ (nginx, php-fpm master + workers, mysqld) |
| Base memory | ~130 MB | ~300-400 MB |
| Setup time | Copy binary + config | Install packages, configure each service |
| Database backup | Copy one `.db` file | mysqldump or xtrabackup |
| Scaling up | Add `workers` config | Tune pm.max_children, mysql connections, nginx workers |
| Monitoring | Built-in `/metrics` | Install node_exporter, mysqld_exporter, php-fpm status |
| TLS | Built-in ACME | Install certbot, configure nginx |

On a 512 MB VM, the traditional stack barely fits. ePHPm leaves room to breathe.

## Backup Strategy for SQLite

Since there's no MySQL server, backups are simpler:

- **Cloud volume snapshots** — AWS EBS snapshots, Hetzner snapshots, DigitalOcean backups. Cheapest, simplest. Most providers offer automated daily snapshots.
- **File copy with WAL checkpoint** — `PRAGMA wal_checkpoint(TRUNCATE)` then copy `app.db`. Safe for consistent backups while the server is running.
- **Litestream** — continuous SQLite replication to S3/GCS/Azure Blob. Sub-second RPO. Free, open-source.
- **Disk-level backups** — rsync, restic, borgbackup on the data directory.

For clustered mode, sqld's "bottomless replication" can continuously stream WAL to S3 for near-zero data loss.

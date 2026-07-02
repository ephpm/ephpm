# Virtual Hosts

ePHPm supports multi-tenant hosting through directory-based virtual hosts. Each domain gets its own document root and its own isolated KV store. No per-site configuration files needed — the directory structure IS the config. (All sites currently share the single global SQLite database — per-site databases are planned for Phase 2, see below.)

## How It Works

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/default"   # fallback site (optional)
sites_dir = "/var/www/sites"         # vhost directory
```

When a request comes in, ePHPm matches the `Host` header against directories in `sites_dir`:

```
Request: Host: alice-blog.com
  → Look for /var/www/sites/alice-blog.com/
  → Found? Serve from that directory (all sites share the global SQLite database)
  → Not found? Fall back to server.document_root (or 404 if not configured)
```

### Directory Convention

```
/var/www/
  default/                        # fallback site (marketing, signup page)
    index.php
    wp-content/
  sites/
    alice-blog.com/               # docroot for alice-blog.com
      index.php
      wp-content/
    bobs-recipes.com/             # docroot for bobs-recipes.com
      index.php
      wp-content/
    cool-photos.net/              # docroot for cool-photos.net
      index.php
      wp-content/
```

Adding a site: create a directory named after the domain, drop WordPress in it.
Removing a site: delete the directory. Requests to that domain hit the fallback.

### Per-Site Overrides

Today, per-site configuration is intentionally minimal. What's discovered per site from `sites_dir` is the document root (the directory itself) plus that site's `index_files` and `fallback`. Everything else — PHP settings, timeouts, security rules, database — comes from the global `ephpm.toml` and is shared by all sites.

A richer per-site override system (a `site.toml` dropped into the site directory with `[php]` and `[db.sqlite]` overrides) is planned for [Phase 2](#phase-2-per-site-databases-and-overrides-future). Until then, if one site needs a larger `memory_limit` or `max_execution_time`, raise the global value in `ephpm.toml`.

### SQLite Database Location

All sites share the single global SQLite database configured via `[db.sqlite] path` in `ephpm.toml`. Per-site databases (an `ephpm.db` inside each site's directory) are planned for Phase 2 — they require litewire `COM_INIT_DB` routing or per-site litewire instances.

### Host Matching

| Request Host | Directory checked | Result |
|-------------|-------------------|--------|
| `alice-blog.com` | `/var/www/sites/alice-blog.com/` | Serve from site directory |
| `www.alice-blog.com` | `/var/www/sites/www.alice-blog.com/` | Serve if exists, else fallback |
| `unknown.com` | `/var/www/sites/unknown.com/` | Not found → fallback to `document_root` |
| No Host header | — | Fallback to `document_root` |

Port numbers and trailing dots are stripped before matching. The match is exact — no wildcard or regex patterns. For `www.` handling, either create a symlink or handle the redirect in your fallback site.

## Architecture

### Single Process, Shared Thread Pool

All sites share one ephpm process and tokio's `spawn_blocking` thread pool. A request to `alice-blog.com` and a request to `bobs-recipes.com` are handled by the same threads — the router sets the correct document root and database before dispatching to PHP.

```
   ┌──────────────────── ePHPm (single process) ────────────────────┐
   │                                                                │
   │   ┌──────────────────────────────┐                             │
   │   │ Router                       │ ──── no match ──────────────┼──► Fallback site
   │   │ (Host → site directory)      │                             │    /var/www/default
   │   └──────────────┬───────────────┘                             │
   │                  │                                             │
   │                  ▼                                             │
   │   ┌──────────────────────────────┐                             │
   │   │ PHP Threads (ZTS)            │                             │
   │   │ (shared spawn_blocking pool) │                             │
   │   └──────────────┬───────────────┘                             │
   │                  │                                             │
   │   ┌──────────────┴───── Shared Backend ─────────────┐          │
   │   │                                                 │          │
   │   │   litewire → rusqlite → global [db.sqlite].path │          │
   │   │   (all sites, one database)                     │          │
   │   │                                                 │          │
   │   └─────────────────────────────────────────────────┘          │
   │                                                                │
   └────────────────────────────────────────────────────────────────┘
```

This is efficient — 20 sites don't need 20x the threads. Any `spawn_blocking` thread can serve any site.

### Shared litewire Instance

One litewire MySQL frontend and one rusqlite backend serve every site. PHP on any site connects to litewire on `127.0.0.1:3306`, and all queries land in the single global SQLite database. Per-site databases would require routing MySQL wire connections per site (the MySQL protocol doesn't carry a Host header), via `COM_INIT_DB` routing or per-site litewire instances — that's Phase 2 work.

## Resource Usage

### Memory (single-node, all sites share workers)

| Sites | Typical memory | Notes |
|-------|---------------|-------|
| 1 | ~270 MB | Baseline (4 workers) |
| 5 | ~300 MB | Small per-site overhead (KV store, file cache) |
| 10 | ~330 MB | Idle sites use near-zero extra memory |
| 20 | ~390 MB | The thread pool doesn't grow with site count |

All sites share one SQLite database and one thread pool, so memory grows only with actively cached data — not with the number of directories in `sites_dir`.

### CPU

Shared across all sites. A 2 vCPU machine handles ~20-40 total req/s across all sites combined, regardless of how many sites exist. Individual site throughput depends on how the traffic is distributed.

### Disk

| Component | Per site |
|-----------|----------|
| WordPress installation | 60-80 MB |
| SQLite database (typical blog) | 10-100 MB |
| Uploads (images, media) | Varies |

20 WordPress sites fit comfortably on a 40 GB SSD.

## Clustered Mode with Virtual Hosts

Virtual hosts work with clustered SQLite. Because every site shares the single global database, each node runs exactly one sqld child process (~40-50 MB) regardless of how many vhosts exist — replication cost does not grow with site count.

The tradeoff is granularity: the whole shared database replicates as a unit. You can't cluster one site and leave the rest single-node.

**Recommendation:** Use single-node SQLite for multi-tenant hosting. Back up with volume snapshots or Litestream. If you need more, consider:

- Enable clustering — the shared database replicates across nodes as one unit
- Move high-traffic sites to an external MySQL via the DB proxy
- Run a separate ephpm instance (with its own database) for sites with different HA needs

## KV Store Isolation

In multi-tenant mode, each virtual host gets its own physically separate KV store. Not key prefixing — a completely separate `DashMap`. PHP applications don't need any code changes, and RESP (Redis protocol) connections are also isolated per-site via AUTH.

### How It Works

When `sites_dir` is configured, ephpm creates a `MultiTenantStore` that manages per-site `Store` instances. Each site's store is created lazily on the first request — same pattern as the vhost directory discovery.

```php
// PHP on alice-blog.com:
ephpm_kv_set("cache:page:home", $html);
// Stored in alice-blog.com's DashMap as "cache:page:home"

// PHP on bobs-recipes.com:
ephpm_kv_get("cache:page:home");
// Looks in bobs-recipes.com's DashMap → not found (physically separate)
```

Keys are stored exactly as PHP sends them — no prefixes, no munging. The isolation is physical, not logical. A site's store is a completely separate data structure.

### Per-Site Memory Limits

Each site store has its own memory limit and eviction policy. One site filling its cache doesn't evict another site's data:

```toml
[kv]
memory_limit = "64MB"   # per-site limit (each site gets up to this much)
```

### Single-Site Mode

When `sites_dir` is not configured, all KV operations go to the global store. No `MultiTenantStore` is created. Zero overhead.

### RESP Protocol (Redis-Compatible) with AUTH

The RESP protocol listener supports per-site isolation via the Redis `AUTH` command. Multi-tenant auth requires a `[kv] secret`:

```toml
[kv]
secret = "generate-a-long-random-string"   # enables multi-tenant HMAC auth

[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"
```

A RESP connection authenticates with **two** arguments — the site's hostname plus a password derived from the secret: `HMAC-SHA256(kv.secret, hostname)`. ePHPm injects that derived password into each site's PHP environment as `EPHPM_REDIS_PASSWORD`, so PHP Redis clients can authenticate without hardcoded credentials:

```
redis-cli -p 6379
AUTH alice-blog.com <derived_password>
SET cache:page:home "<html>..."
GET cache:page:home   → "<html>..."
```

When `[kv] secret` is set, unauthenticated connections are rejected with `NOAUTH` — they do **not** fall back to the default store. The single-argument form `AUTH <password>` is only the legacy plain-password mode (no site isolation); it does not select a site store.

### Architecture

```
PHP (alice-blog.com)                PHP (bobs-recipes.com)
  │                                   │
  ├─ ephpm_kv_set("key", "val")       ├─ ephpm_kv_set("key", "val")
  │                                   │
  ▼                                   ▼
SAPI bridge                         SAPI bridge
  ├─ site store = alice's DashMap     ├─ site store = bob's DashMap
  ├─ store.set("key", "val")          ├─ store.set("key", "val")
  │                                   │
  ▼                                   ▼
MultiTenantStore
  ├─ "alice-blog.com" → DashMap { "key" → "val" }
  ├─ "bobs-recipes.com" → DashMap { "key" → "val" }
  └─ default → DashMap (global, single-site fallback)
```

### RESP connection flow

```
RESP client connects → AUTH alice-blog.com <derived_password>
  → verify password == HMAC-SHA256(kv.secret, "alice-blog.com")
  → MultiTenantStore.auth_site("alice-blog.com")
  → returns alice's Store
  → all subsequent commands operate on alice's DashMap only
(no AUTH while [kv] secret is set → NOAUTH error)
```

## Fallback Site as Marketing Funnel

The fallback `document_root` serves requests for any domain not matched by `sites_dir`. This is useful for hosting businesses:

```
/var/www/default/
  index.php    → "Start your blog today! Sign up at hosting.example.com"
```

When a customer cancels and their site directory is removed, traffic from existing backlinks, bookmarks, and search engine rankings flows to your marketing page instead of a dead 404. Free inbound traffic to your signup funnel.

### Lifecycle

```
1. Customer signs up for alice-blog.com
   → Create /var/www/sites/alice-blog.com/
   → Install WordPress
   → Site is live immediately (no restart needed with future hot-reload)

2. Customer is active
   → Requests to alice-blog.com served from site directory
   → SQLite database grows organically

3. Customer cancels
   → Archive /var/www/sites/alice-blog.com/ (backup the .db file)
   → Delete directory
   → Traffic to alice-blog.com hits fallback marketing page

4. Domain expires / new customer
   → Create directory again for new owner
   → Fresh WordPress install
```

## Configuration Reference

```toml
[server]
listen = "0.0.0.0:8080"

# Fallback document root for unmatched Host headers.
# Omit to return 404 for unknown domains.
document_root = "/var/www/default"

# Virtual host directory. Each subdirectory is named after a domain.
# Omit to disable vhosting (single-site mode).
sites_dir = "/var/www/sites"

# Global PHP settings (shared by all sites)
[php]
workers = 4
memory_limit = "128M"

# Global SQLite config (one database, shared by all sites)
[db.sqlite.proxy]
mysql_listen = "127.0.0.1:3306"
```

Environment variable overrides:

```bash
EPHPM_SERVER__SITES_DIR=/var/www/sites
EPHPM_SERVER__DOCUMENT_ROOT=/var/www/default
```

## Deployment Example: 20-Blog Hosting on Hetzner CAX11

**VM:** Hetzner CAX11 — 2 ARM vCPUs, 4 GB RAM, 40 GB SSD — $3.69/mo

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/marketing"
sites_dir = "/var/www/sites"

[php]
workers = 4
memory_limit = "64M"

[kv]
memory_limit = "64MB"
```

Put a reverse proxy (Caddy recommended — automatic HTTPS per domain) in front for TLS termination, or use ePHPm's built-in ACME with a wildcard cert.

**Capacity:**
- 20 WordPress blogs
- ~390 MB memory used
- ~20-40 req/s total throughput
- $0.18/mo per site
- Zero ops: one binary, one config file, automated backups via Hetzner snapshots

## Implementation Phases

### Phase 1: Directory-Based Routing (implemented)

Host header → site directory mapping with per-site document roots, plus per-site KV store isolation (`MultiTenantStore`). All sites share the global SQLite database and PHP thread pool.

| Step | Change | File |
|------|--------|------|
| 1 | Add `sites_dir: Option<PathBuf>` to `ServerConfig` | `ephpm-config/src/lib.rs` |
| 2 | Add `SiteConfig` struct and site registry `HashMap<String, SiteConfig>` to `Router` | `ephpm-server/src/router.rs` |
| 3 | Scan `sites_dir` at startup, populate registry from directory names | `ephpm-server/src/router.rs` |
| 4 | Add `resolve_site()` — extract Host header, strip port/trailing dot, lowercase, lookup in registry | `ephpm-server/src/router.rs` |
| 5 | Thread per-site `document_root` through `resolve_fallback()`, `probe_path()`, `handle_php()` | `ephpm-server/src/router.rs` |
| 6 | Unit tests: site resolution, fallback, port stripping, case insensitivity | `ephpm-server/src/router.rs` |

When `sites_dir` is not configured, the router behaves identically to today (single-site mode). Zero cost path — the `sites` HashMap is empty and `resolve_site()` returns the global defaults.

### Phase 2: Per-Site Databases and Overrides (future)

| Feature | Description |
|---------|-------------|
| Per-site SQLite | Each site gets its own `ephpm.db` in its directory. Requires litewire COM_INIT_DB routing or per-site litewire instances |
| Per-site `site.toml` | Optional overrides for `index_files`, `fallback`, `php.memory_limit`, `db.sqlite.path`, etc. Merged with global config |
| Per-site metrics | Add `host` label to Prometheus metrics for per-site traffic visibility |

### Phase 3: Operational Features (future)

| Feature | Description |
|---------|-------------|
| Hot reload | Detect new/removed site directories without restart (via `notify` or periodic rescan) |
| Per-site resource limits | Memory and CPU quotas per site to prevent noisy neighbors |
| Site provisioning API | REST API to create/delete sites, manage domains |

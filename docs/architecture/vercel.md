# Vercel for PHP

ePHPm's architecture enables a Vercel-like deployment platform for PHP applications — but with a structural cost advantage that makes the free tier genuinely free.

## The Idea

```
git push → deploy → preview URL → production
```

Every PR gets a full WordPress or Laravel instance with its own database in seconds. No MySQL provisioning, no Docker Compose, no waiting. Merge the PR and the preview disappears. Ship to production with a custom domain and automatic HTTPS.

Nobody does this well for PHP today.

## Why ePHPm Has an Unfair Advantage

### Vercel's Architecture (serverless)

Vercel uses Lambda-style serverless functions. Each deployment is a container image stored in a registry. When a request arrives, the runtime boots, serves the request, and shuts down.

```
Request → Cold start (500ms-2s) → Boot runtime → Execute → Respond → Shutdown
```

This is necessary because keeping a Node.js process running for every free-tier user would be expensive. The tradeoff is cold starts.

### ePHPm's Architecture (vhosts)

ePHPm runs one process with a shared PHP worker pool. Each site is a directory on disk. An idle site uses zero RAM and zero CPU — it's just files. When a request arrives, a pre-warmed PHP worker handles it in milliseconds and returns to the pool.

```
Request → Route by Host header → Shared worker (already hot) → Execute → Respond → Worker returns to pool
```

No boot. No teardown. No cold start. No container. The worker pool is always running regardless of how many sites exist.

### The Cost Comparison

| | Vercel (serverless) | ePHPm (vhost) |
|---|---|---|
| Idle preview | No process, no cost | Directory on disk, no cost |
| First request | Cold start (500ms-2s) | Worker already warm (~5ms) |
| Runtime loaded? | No, must boot per request | Yes, always hot |
| Database per site | External (Postgres, Planetscale) | SQLite file on disk |
| Create a preview | Build container image, push to registry | `mkdir` + `composer install` |
| Delete a preview | Deregister container, GC storage | `rm -rf` |
| Infrastructure per preview | Container image (~200-500 MB) | Directory (~70 MB) |

**The key insight:** An idle preview deployment on ePHPm costs nothing but disk space. There's no process, no container, no Lambda waiting. Just a directory with PHP files and a SQLite database. The marginal cost is ~70 MB of SSD.

## How It Works

### Preview Deployment Flow

```
1. Developer pushes to branch `feature/new-header`
2. Webhook fires → deployment service receives event
3. mkdir /var/www/sites/feature-new-header.preview.yoursaas.com/
4. git clone → composer install → copy into site directory
5. SQLite database seeded from template (or empty)
6. Preview URL is live: https://feature-new-header.preview.yoursaas.com
7. ACME cert issued automatically on first HTTPS request
8. Developer clicks around, tests, shares with reviewer
9. PR merged → rm -rf the site directory
10. Done. No cleanup, no orphaned containers, no dangling volumes.
```

Total time from push to live preview: the time it takes to run `composer install` (~10-30 seconds for WordPress, ~5-15 seconds for Laravel).

### Production Deployment Flow

```
1. Developer merges to main
2. Same flow: clone → install → copy to production site directory
3. Custom domain: customer adds CNAME → ephpm issues cert
4. SQLite database persists across deployments
5. Rollback: swap the site directory to a previous version
```

### What the Customer Sees

**Free tier:**
- `git push` → preview at `branch.app.yoursaas.com`
- Merge → production at `app.yoursaas.com`
- Custom domain with automatic HTTPS
- Built-in SQLite database (no external DB needed)
- Built-in KV store (object caching, sessions)

**Zero configuration.** Detect `composer.json` → Laravel. Detect `wp-config.php` → WordPress. Auto-configure ephpm.

## Single VM Economics

One small VM serves hundreds of preview sites because previews are mostly idle.

### Hetzner CAX11 ($3.69/mo, 2 ARM cores, 4 GB RAM, 40 GB SSD)

| Resource | Capacity | How it's used |
|----------|----------|--------------|
| Disk (40 GB) | ~500 previews at 70 MB each | WordPress install + SQLite DB |
| Memory (4 GB) | 4 PHP workers shared across all sites | Only active requests use RAM |
| CPU (2 cores) | ~20-40 req/s total | Maybe 2-3 devs clicking at any moment |

**What 500 idle previews cost:**

| Resource | Usage | Cost |
|----------|-------|------|
| RAM | 0 MB (directories on disk) | $0 |
| CPU | 0% (no requests) | $0 |
| Disk | ~35 GB | Included in VM |
| **Total** | | **$0 marginal** |

A preview that nobody is looking at is literally just files on disk. Not a container waiting. Not a Lambda ready to cold start. Not a VM idling. A directory.

Need more disk? Hetzner volumes are $0.052/GB/mo. A 100 GB volume ($5.20/mo) gives room for ~1,400 previews.

### Why Serverless Can't Match This

| Operation | Serverless (Vercel) | ePHPm vhost |
|-----------|-------------------|-------------|
| Create preview | Build container image (~60s) | `mkdir` + `composer install` (~15s) |
| First page load | Cold start + boot PHP (~1-3s) | Shared worker, already warm (~50ms) |
| Store database | Provision external DB ($0+/mo) | Create SQLite file ($0) |
| Idle for a week | $0 (but cold start on return) | $0 (instant response on return) |
| Delete preview | Deregister, GC images | `rm -rf` (instant) |
| 500 idle previews | 500 container images in registry | 500 directories on one SSD |

ePHPm doesn't need serverless because the problem serverless solves — "how do I avoid paying for idle compute" — doesn't exist when idle sites use zero compute.

## Pricing Model

Follow Vercel's playbook: free tier as growth engine, charge for production/team use.

| Plan | Price | What you get |
|------|-------|-------------|
| **Free** | $0/mo | 1 production site, unlimited PR previews, custom domain, HTTPS, SQLite database |
| **Pro** | $20/mo | 5 production sites, team access, analytics, priority builds |
| **Team** | $50/mo | 20 sites, multiple members, edge deployment, priority support |
| **Business** | $100/mo | Unlimited sites, SLA, SSO, audit logs |

### Why Free Tier Works Economically

| Metric | Value |
|--------|-------|
| Cost per free user (idle) | ~$0.01/mo (disk only) |
| Cost per free user (light traffic) | ~$0.15/mo (share of VM) |
| VM cost serving 200 free users | ~$3.69/mo (one Hetzner CAX11) |
| Break-even conversion rate | 1 in 25 users upgrades to Pro |
| Industry SaaS conversion rate | 2-5% (1 in 20-50) |

A free user who pushes code and occasionally previews their site costs you a penny a month. You need ~1 in 25 to pay $20/mo to cover the infrastructure. Standard SaaS conversion rates are 2-5%, which is right in that range.

### Where the Money Comes From

Previews are free — they're the hook. Revenue comes from:

1. **Production deployments** — custom domains, guaranteed uptime, the site people actually care about
2. **Team seats** — collaboration features, shared previews, access controls
3. **Bandwidth** — free tier includes X GB, overage billed per GB
4. **Edge deployment** — multi-region with SQLite replication (premium feature)
5. **Add-ons** — email, monitoring, backup retention, dedicated resources

## Competitive Landscape

| Platform | PHP support | Built-in DB | Preview deployments | Free tier | Monthly |
|----------|-----------|-------------|--------------------|-----------| --------|
| **Vercel** | No | Postgres (paid) | Yes (serverless) | Yes | $0-20 |
| **Netlify** | No | No | Yes (serverless) | Yes | $0-19 |
| **Railway** | Yes (mediocre) | Postgres ($) | No | $5 credit | Usage-based |
| **Render** | Yes (mediocre) | Postgres ($) | Yes (paid) | No (removed) | $7+ |
| **Platform.sh** | Yes | MySQL/Postgres | Yes | No | $10+ |
| **Laravel Cloud** | Laravel only | Yes | Yes | No | TBD |
| **Forge** | Yes (BYO server) | No | No | No | $12+ |
| **WP Engine** | WordPress only | MySQL | Staging only | No | $20+ |
| **This (ePHPm)** | All PHP | SQLite (free) | Yes (zero-cost) | Yes | $0-20 |

**The gap:** No platform offers a Vercel-quality developer experience for PHP with a free built-in database and zero-cost preview deployments. Laravel Cloud is the closest competitor but it's Laravel-only and not free.

## What Needs to Be Built

ePHPm provides the runtime. The platform needs:

| Component | Complexity | Description |
|-----------|-----------|-------------|
| Git webhook handler | Low | Receive push events from GitHub/GitLab/Bitbucket |
| Build pipeline | Medium | Clone → detect framework → composer install → deploy to site directory |
| Preview URL routing | Done | Vhosts with wildcard DNS (`*.preview.yoursaas.com`) |
| Custom domains | Done | ACME cert issuance built into ephpm |
| Dashboard / CLI | Medium | Create app, link repo, manage domains, view logs |
| Template seeding | Low | Copy a base WordPress/Laravel install for new sites |
| Database snapshots | Low | Copy SQLite file for rollback or branch-from-production |
| Billing (Stripe) | Medium | Usage metering, plan management |
| Team/org management | Medium | Invites, roles, permissions |
| Log streaming | Low | ephpm tracing output → WebSocket to dashboard |
| Metrics per site | Low | Prometheus already emitting, add per-host labels |
| Auto-scaling | Hard | Multiple preview VMs when one fills up |

### Database Branch Previews

One of the most powerful features: preview deployments can branch the database.

```
1. Production site has app.db with real data
2. Developer opens PR
3. Preview deployment copies app.db → preview gets a snapshot of production data
4. Developer tests against real data, makes schema changes
5. PR merges → production runs migrations
6. Preview's database copy is deleted
```

This is trivial with SQLite — `cp app.db preview-app.db` takes milliseconds. With MySQL you'd need `mysqldump` + `mysql import` (minutes) or filesystem snapshots (complex).

### Framework Detection

```
if composer.json contains "laravel/framework" → Laravel
if wp-config.php exists → WordPress
if composer.json contains "drupal/core" → Drupal
if composer.json contains "symfony/framework-bundle" → Symfony
else → Generic PHP
```

Each framework gets a default ephpm.toml template:
- **WordPress:** `fallback = ["$uri", "$uri/", "/index.php?$query_string"]`
- **Laravel:** `document_root = "public"`, `fallback = ["$uri", "/index.php?$query_string"]`
- **Symfony:** `document_root = "public"`, `fallback = ["$uri", "/index.php?$query_string"]`

## The Pitch

> **Deploy PHP like it's 2026.**
>
> Push your code. Get a preview URL in 15 seconds. Merge to ship. No servers, no Docker, no database provisioning. Built-in SQLite, automatic HTTPS, instant rollbacks. Free forever for personal projects.
>
> WordPress. Laravel. Drupal. Symfony. Any PHP app. One `git push`.

# Preview Deployments

ePHPm offers instant preview deployments for PHP applications via a GitHub bot. Every pull request gets a live preview URL with its own database — deployed in seconds, torn down on merge.

The system has two components: **switchboard** (the webhook handler) and **ephpm** (the runtime). They run side by side on the same VM.

## How It Works

```
Developer pushes PR
       │
       ▼
GitHub sends webhook ──────────────────────────────► switchboard (:9090)
                                                        │
                                                        ├─ Verify signature
                                                        ├─ git clone --depth 1
                                                        ├─ Detect framework
                                                        ├─ composer install
                                                        ├─ mkdir sites/<hostname>/
                                                        ├─ Copy files in
                                                        └─ Post PR comment via GitHub API
                                                              │
                                                              ▼
                                                    ┌─────────────────────┐
                                                    │  PR Comment:        │
                                                    │  ePHPm Preview      │
                                                    │  https://pr-42...   │
                                                    │  WordPress · 14s    │
                                                    └─────────────────────┘

User clicks preview link
       │
       ▼
DNS: *.preview.ephpm.dev ──► VM IP ──► ephpm (:8080)
                                          │
                                          ├─ Host header → lazy vhost lookup
                                          ├─ Directory exists? Serve from it
                                          ├─ ACME cert issued on first HTTPS request
                                          └─ PHP executes against local SQLite
```

No restart. No config reload. Switchboard writes a directory, ephpm discovers it on the next request.

## Architecture

### Components

| Component | Repo | Language | What it does |
|-----------|------|----------|-------------|
| **ephpm** | `ephpm/ephpm` | Rust | PHP runtime + HTTP server with lazy vhost discovery |
| **switchboard** | `ephpm/switchboard` (private) | Rust | GitHub webhook handler, clones repos, deploys to ephpm's `sites_dir` |

### Runtime Flow

```
                    ┌──────────────────────────────────────────┐
                    │                  VM                       │
                    │                                          │
  GitHub ──webhook──┤► switchboard (:9090)                     │
  webhooks          │    │                                     │
                    │    ├─ git clone                          │
                    │    ├─ composer install                    │
                    │    └─ write to /var/www/sites/            │
                    │                    │                      │
                    │                    ▼                      │
  HTTP ─────────────┤► ephpm (:8080)                           │
  requests          │    ├─ Host header → sites_dir lookup     │
                    │    ├─ Lazy discovery (filesystem check)  │
                    │    ├─ PHP execution (shared worker pool) │
                    │    └─ SQLite database (per-site file)    │
                    │                                          │
                    └──────────────────────────────────────────┘
```

Both processes share the filesystem. Switchboard writes to `sites_dir`, ephpm reads from it. No IPC, no API calls between them, no coordination protocol. The filesystem is the interface.

### Switchboard Internals

```
switchboard/
  src/
    main.rs         # axum HTTP server, webhook dispatcher
    config.rs       # CLI args + env vars
    webhook.rs      # GitHub webhook parsing, HMAC-SHA256 signature verification
    deployer.rs     # git clone → detect framework → composer install → deploy
    github.rs       # Post/update PR comments, set deployment status
```

**Webhook handler flow:**

1. Receive `POST /webhook` from GitHub
2. Verify `X-Hub-Signature-256` against shared secret
3. Parse `pull_request` event (opened/synchronize/reopened/closed)
4. Respond 200 immediately (async processing)
5. If deploy: clone repo → detect framework → `composer install` → copy to `sites_dir`
6. If teardown: `rm -rf` the site directory
7. Post/update PR comment with preview URL via GitHub API

**GitHub App authentication:**

1. Switchboard holds the GitHub App's private key (PEM)
2. On each webhook, creates a JWT signed with the private key
3. Exchanges JWT for a short-lived installation access token
4. Uses the token to post comments and set deployment statuses
5. Tokens are scoped to the repos the user authorized — no access beyond that

### Framework Detection

Switchboard auto-detects the PHP framework to configure ephpm correctly:

| Signal | Framework |
|--------|-----------|
| `wp-config.php` or `wp-config-sample.php` exists | WordPress |
| `composer.json` contains `laravel/framework` | Laravel |
| `artisan` file exists | Laravel |
| `composer.json` contains `drupal/core` | Drupal |
| `composer.json` contains `symfony/framework-bundle` | Symfony |
| None of the above | Generic PHP |

### Lazy Vhost Discovery

ephpm's router checks the filesystem when a hostname isn't in its startup cache:

```rust
// In resolve_site():
// 1. Check HashMap (startup-scanned sites) — verify dir still exists
// 2. Check filesystem: sites_dir/<hostname>/ exists?
//    → Yes: serve from it (logged as "discovered new virtual host (lazy)")
//    → No: fall back to default document_root
```

This means:
- **Deploy:** switchboard creates `sites_dir/pr-42.app.preview.ephpm.dev/` → next HTTP request serves it
- **Teardown:** switchboard deletes the directory → next HTTP request falls back to default
- **No restart, no reload, no signal** between switchboard and ephpm

### Preview URL Format

```
pr-{number}.{repo}.preview.ephpm.dev
```

Examples:
- `pr-42.my-blog.preview.ephpm.dev`
- `pr-7.laravel-app.preview.ephpm.dev`

DNS: wildcard `*.preview.ephpm.dev` → VM IP address.

TLS: ephpm's built-in ACME issues a Let's Encrypt cert on the first HTTPS request to each preview URL.

## GitHub Integration

### GitHub App Setup

1. Register at `github.com/organizations/ephpm/settings/apps`
2. Set webhook URL: `https://switchboard.ephpm.dev:9090/webhook`
3. Permissions:
   - `pull_requests: write` — post/edit comments
   - `deployments: write` — create deployment statuses
4. Subscribe to events: `pull_request`
5. Generate private key (PEM file)
6. Make the app public for external users

### Installation

**For the ephpm team (internal):**

Install the app on repos in the `ephpm` org.

**For external users (beta):**

Share the direct install URL:
```
https://github.com/apps/ephpm/installations/new
```

User clicks → authorizes → selects repos → webhooks start flowing. No marketplace approval needed.

**For external users (GA):**

Publish to GitHub Marketplace. Users find it at `github.com/marketplace/ephpm`.

### PR Comment

When a preview deploys, switchboard posts (or updates) a comment:

```markdown
**ePHPm Preview** — deployed

| | |
|---|---|
| URL | https://pr-42.my-blog.preview.ephpm.dev |
| Framework | WordPress |
| Deployed in | 14.3s |

Preview updates automatically on each push to this PR.
```

On teardown (PR closed/merged), the comment is updated:

```markdown
**ePHPm Preview** — removed

Preview deployment has been torn down.
```

### Deployment Status

Switchboard also creates a GitHub deployment with an environment URL. This shows up in the PR's "Environments" section with a green checkmark and a "View deployment" link.

## Repository Config: `.ephpm.yaml`

Developers can place an `.ephpm.yaml` file in their repo root to configure how previews are built and seeded. The file is optional — without it, switchboard auto-detects everything.

```yaml
# .ephpm.yaml

# Run after composer install to seed the database.
# Can be any executable: shell script, PHP script, artisan command.
seed: scripts/seed.sh

# PHP version (default: latest, currently 8.5)
# Determines which ephpm instance handles requests.
php: "8.4"
```

### Database Seeding

Every preview gets its own SQLite database file. Three ways to seed it:

**1. Seed script (recommended)**

```yaml
seed: scripts/seed.sh
```

Switchboard runs this after `composer install`. The script can do anything:

```bash
#!/bin/bash
# scripts/seed.sh — Laravel example
php artisan migrate --seed

# WordPress example
# wp core install --url="$PREVIEW_URL" --title="Preview" --admin_user=admin --admin_password=admin --admin_email=dev@example.com
# wp import fixtures.xml
```

Switchboard sets `PREVIEW_URL` as an environment variable so the seed script knows the preview hostname.

**2. Template database**

```yaml
seed: cp .ephpm/template.db ephpm.db
```

Ship a pre-built SQLite snapshot in the repo. The seed script just copies it. Instant — no migrations, no seeding delay.

**3. Fork from production (future)**

Copy the production site's `ephpm.db` into the preview. Developer tests against real data. Since SQLite is a file, this is a millisecond `cp` operation.

### Future `.ephpm.yaml` Fields

```yaml
# .ephpm.yaml — full spec (most fields are future)

seed: scripts/seed.sh         # database seeding (implemented)
php: "8.4"                     # PHP version (implemented)
# framework: laravel           # override auto-detection
# root: public                 # override document root
# env:                         # environment variables for the preview
#   APP_ENV: staging
#   APP_DEBUG: "true"
```

## Multi-PHP Version Support

Multiple PHP versions run simultaneously on the same VM. Each version is a separate ephpm binary built with `cargo xtask release <version>`. All instances share one `sites_dir` — the same files are served by whichever PHP version the request hits.

```
/var/www/sites/
  pr-42.my-blog.preview.ephpm.dev/    ← same files, served by any PHP version
      index.php
      ephpm.db

ephpm-85 (:443)  → PHP 8.5 (default, latest)
ephpm-84 (:8084) → PHP 8.4
ephpm-83 (:8083) → PHP 8.3
```

### How It Works

1. Developer sets `php: "8.4"` in `.ephpm.yaml`
2. Switchboard deploys files to the shared `sites_dir` (same as always)
3. Switchboard posts the PR comment with the port for PHP 8.4

**Default (no `php` field or `php: "8.5"`):**
```
https://pr-42.my-blog.preview.ephpm.dev
```

**Explicit older version (`php: "8.4"`):**
```
https://pr-42.my-blog.preview.ephpm.dev:8084
```

Port 443 is the latest version — no port in the URL. Older versions get their own port. The version-to-port mapping in switchboard:

| PHP Version | Port | URL |
|-------------|------|-----|
| 8.5 (latest) | 443 | `https://hostname` |
| 8.4 | 8084 | `https://hostname:8084` |
| 8.3 | 8083 | `https://hostname:8083` |

### Shared ACME Certificates via Cluster

All ephpm instances on the same VM join a gossip cluster on localhost. They share a KV store, and ACME certificates are stored in the KV store. One instance handles the Let's Encrypt challenge, all instances serve the same cert.

```toml
# ephpm-85.toml (latest, port 443)
[server]
listen = "0.0.0.0:443"
sites_dir = "/var/www/sites"

[cluster]
enabled = true
bind = "0.0.0.0:7946"
node_id = "php85"
cluster_id = "previews"

# ephpm-84.toml (port 8084)
[server]
listen = "0.0.0.0:8084"
sites_dir = "/var/www/sites"

[cluster]
enabled = true
bind = "0.0.0.0:7947"
join = ["127.0.0.1:7946"]
node_id = "php84"
cluster_id = "previews"
```

This gives you:
- **Shared ACME certs** — one cert issuance, all instances serve HTTPS
- **Shared KV store** — session data, object cache available across PHP versions
- **Shared sites_dir** — one deploy, accessible from any PHP version
- **No reverse proxy needed** — DNS + port routing, no nginx/caddy

The gossip cluster was designed for multi-node deployments across machines, but it works identically on localhost for multi-version setups.

### Resource Usage (Multi-PHP)

Each additional ephpm instance adds its own PHP worker pool:

| Instances | Workers (total) | Memory overhead |
|-----------|----------------|-----------------|
| 1 (PHP 8.5 only) | 4 | ~270 MB baseline |
| 2 (8.5 + 8.4) | 8 | ~470 MB |
| 3 (8.5 + 8.4 + 8.3) | 12 | ~670 MB |

On a 4 GB VM, two PHP versions is comfortable. Three is tight. Most users only need the latest — older versions are for testing compatibility before upgrading.

## Deployment

### Single VM Setup

One VM runs both ephpm and switchboard. This handles hundreds of preview sites.

**Prerequisites:**
- VM with public IP (Hetzner CAX11 recommended: $3.69/mo)
- Wildcard DNS: `*.preview.ephpm.dev → VM IP`
- `git`, `composer` installed on the VM
- GitHub App registered with private key

**Environment:**

```bash
# switchboard
SWITCHBOARD_LISTEN=0.0.0.0:9090
SWITCHBOARD_WEBHOOK_SECRET=<from github app settings>
SWITCHBOARD_APP_ID=<github app id>
SWITCHBOARD_APP_KEY=/etc/switchboard/app-key.pem
SWITCHBOARD_SITES_DIR=/var/www/sites
SWITCHBOARD_PREVIEW_DOMAIN=preview.ephpm.dev

# ephpm
EPHPM_SERVER__LISTEN=0.0.0.0:8080
EPHPM_SERVER__DOCUMENT_ROOT=/var/www/default
EPHPM_SERVER__SITES_DIR=/var/www/sites
```

**Directory layout:**

```
/var/www/
  default/                                    # fallback (marketing page)
    index.html
  sites/                                      # shared between switchboard and ephpm
    pr-42.my-blog.preview.ephpm.dev/          # live preview
      index.php
      wp-content/
      ephpm.db
    pr-7.laravel-app.preview.ephpm.dev/       # another preview
      public/
      artisan
      ephpm.db
```

**Systemd units:**

```ini
# /etc/systemd/system/ephpm-85.service (latest, port 443)
[Service]
ExecStart=/usr/local/bin/ephpm-85 --config /etc/ephpm/ephpm-85.toml
Restart=always

# /etc/systemd/system/ephpm-84.service (optional, port 8084)
[Service]
ExecStart=/usr/local/bin/ephpm-84 --config /etc/ephpm/ephpm-84.toml
Restart=always

# /etc/systemd/system/switchboard.service
[Service]
ExecStart=/usr/local/bin/switchboard
EnvironmentFile=/etc/switchboard/env
Restart=always
```

### Capacity

On one Hetzner CAX11 ($3.69/mo, 2 ARM cores, 4 GB RAM, 40 GB SSD):

| Metric | Capacity |
|--------|----------|
| Concurrent preview sites | ~500 (limited by disk: 70 MB each) |
| Active requests across all sites | ~20-40 req/s (shared 4 PHP workers) |
| Memory for idle previews | ~0 MB each (just files on disk) |
| Memory for active requests | ~50 MB per concurrent request (shared pool) |
| Deploy time (WordPress) | ~15-30s (git clone + composer install) |
| Deploy time (Laravel) | ~10-20s |
| Teardown time | < 1s (rm -rf) |

### Scaling Beyond One VM

When one VM fills up (disk or CPU), add more:

1. Multiple VMs, each running ephpm + switchboard
2. Switchboard routes deploys to the VM with the most free disk
3. GeoDNS or load balancer distributes requests
4. Each VM has its own `sites_dir` — no shared filesystem needed

This is future work. One VM handles the beta and early growth.

## Security

| Concern | Mitigation |
|---------|-----------|
| Webhook spoofing | HMAC-SHA256 signature verification on every webhook |
| Malicious code in PR | Previews run as the ephpm process user — no root. PHP sandbox applies. |
| Cross-site data leakage | Each preview is a separate directory with its own SQLite database |
| Resource exhaustion | Build timeout, max concurrent deploys, disk quota monitoring |
| Token scope | GitHub installation tokens are scoped to authorized repos only |
| Private repos | Switchboard clones via the installation token — respects repo permissions |

### Future Hardening

- **Build sandbox:** Run `composer install` in a container or namespace for isolation
- **Disk quotas:** Per-preview disk limit, auto-teardown if exceeded
- **Stale preview cleanup:** Cron job that removes previews older than N days
- **Rate limiting:** Max deploys per hour per installation

## Testing

### Unit Tests (switchboard)

| Module | Tests | What they cover |
|--------|-------|----------------|
| `webhook.rs` | 5 | Signature verification (valid/invalid/missing), preview host format, deploy/teardown detection |
| `deployer.rs` | 4 | Framework detection (WordPress, Laravel, generic) |
| `github.rs` | 1 | PR comment formatting |

### Unit Tests (ephpm)

| Test | What it covers |
|------|---------------|
| `vhost_lazy_discovery_finds_new_directory` | Directory created after startup is discovered |
| `vhost_lazy_discovery_teardown` | Directory deleted after startup falls back to default |

### E2E Tests (ephpm-e2e)

| Test | What it covers |
|------|---------------|
| `unknown_host_returns_fallback` | Unmatched Host header gets fallback response |
| `lazy_discovered_site_serves_content` | Full lifecycle: create dir → serve → delete dir → fallback |
| `multiple_sites_isolated` | Two sites serve different content independently |

## Cost Model

Previews are effectively free to operate:

| State | Cost |
|-------|------|
| Idle preview (no traffic) | ~70 MB disk, 0 MB RAM, 0% CPU |
| Active preview (developer clicking) | Shared PHP worker (~50 MB, returned to pool after) |
| 500 idle previews on one VM | ~35 GB disk, $3.69/mo total |
| Per preview (marginal cost) | ~$0.007/mo |

The preview infrastructure costs less than a cup of coffee per month regardless of how many previews exist. The only scaling constraint is disk space, which is the cheapest resource in cloud computing.

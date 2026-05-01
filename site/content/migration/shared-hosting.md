# Migrating from Shared Hosting (cPanel / Plesk)

You're on shared hosting — GoDaddy, Bluehost, Hostinger, SiteGround, or similar. You upload files via FTP or cPanel's file manager. You click buttons to set up databases and email. It works, but you're limited: no SSH, no custom PHP versions, slow servers, and your site shares resources with hundreds of others.

ePHPm lets you run your own server for less than most shared hosting plans cost — with better performance, full control, and no noisy neighbors.

## What Changes

| | Shared Hosting | ePHPm on a VPS |
|---|---|---|
| Access | cPanel / FTP | SSH + full root access |
| PHP version | Whatever the host provides | Any version you want |
| Database | Shared MySQL (host manages) | Embedded SQLite (zero setup) or external MySQL |
| File upload | FTP / cPanel file manager | `scp`, `rsync`, or `git push` |
| SSL | Host provides (often paid) | Built-in, free (Let's Encrypt) |
| Performance | Shared with other tenants | Dedicated resources |
| Price | $3-15/mo (often increases after promo) | $3.69/mo (Hetzner) — price doesn't change |
| Email | Usually included | Not included — use a dedicated email service |

## Step-by-Step Migration

### 1. Get a VPS

Sign up for a small VPS. Recommended:

| Provider | Specs | Price |
|----------|-------|-------|
| Hetzner CAX11 | 2 ARM cores, 4 GB RAM, 40 GB SSD | $3.69/mo |
| DigitalOcean | 1 vCPU, 1 GB RAM, 25 GB SSD | $6/mo |
| Vultr | 1 vCPU, 1 GB RAM, 25 GB SSD | $5/mo |
| Linode | 1 vCPU, 1 GB RAM, 25 GB SSD | $5/mo |

Any of these is faster than shared hosting. You get dedicated resources instead of fighting for CPU with hundreds of other sites.

### 2. Install ePHPm on the VPS

SSH into your new server and download ePHPm:

```bash
ssh root@your-server-ip

# Download ePHPm
curl -fSL https://github.com/ephpm/ephpm/releases/latest/download/ephpm-linux-x86_64 -o /usr/local/bin/ephpm
chmod +x /usr/local/bin/ephpm
```

### 3. Copy Your Website Files

From your local machine (or download from cPanel first):

```bash
# Copy your website files to the server
scp -r /path/to/your/site/* root@your-server-ip:/var/www/html/
```

Or if you use git:

```bash
ssh root@your-server-ip
cd /var/www/html
git clone https://github.com/you/your-site.git .
```

### 4. Handle the Database

**Option A: Use embedded SQLite (simplest)**

If your site is WordPress or Laravel, ePHPm's built-in SQLite works without any external database. Your PHP app connects via `pdo_mysql` — ePHPm translates to SQLite transparently.

```toml
[db.sqlite]
path = "/var/www/html/app.db"
```

You'll need to export your data from the shared hosting MySQL and import it. For WordPress:

```bash
# On shared hosting: export via phpMyAdmin or WP-CLI
wp db export backup.sql

# On your VPS: import into SQLite
# (ePHPm's litewire translates MySQL SQL to SQLite automatically)
```

**Option B: Use an external MySQL server**

If you prefer MySQL, use a managed database ($5-15/mo from DigitalOcean, PlanetScale, etc.):

```toml
[db.mysql]
url = "mysql://user:pass@db-host:3306/mysite"
```

**Option C: Run MySQL on the same VPS**

```bash
sudo apt install mariadb-server
mysql < backup.sql
```

```toml
[db.mysql]
url = "mysql://root:password@127.0.0.1:3306/mysite"
```

### 5. Create the Config

```toml
# /etc/ephpm/ephpm.toml

[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"

[php]
memory_limit = "256M"
max_execution_time = 120

[server.tls]
acme_domains = ["yourdomain.com", "www.yourdomain.com"]
acme_email = "you@yourdomain.com"
```

### 6. Point Your Domain

In your domain registrar (GoDaddy, Namecheap, Cloudflare, etc.):

1. Find DNS settings
2. Change the A record to point to your VPS IP address
3. Remove any CNAME records the old host set up
4. Wait for DNS propagation (usually 5-30 minutes)

```
yourdomain.com      A    → your-vps-ip
www.yourdomain.com  A    → your-vps-ip
```

### 7. Start ePHPm

```bash
# Smoke-test in the foreground
ephpm serve --config /etc/ephpm/ephpm.toml &
curl http://localhost:8080
kill %1

# Install as a system service (registers + starts it)
sudo ephpm install
```

### 8. Verify

Visit `https://yourdomain.com` — your site should load. The first request will take a moment while ePHPm issues the TLS certificate via Let's Encrypt.

## Things Shared Hosting Gave You (That You Now Handle)

| Feature | Shared hosting | With ePHPm |
|---------|---------------|------------|
| **Backups** | Host does it (maybe) | Set up Hetzner snapshots ($0.01/GB/mo) or rsync to S3 |
| **Email** | Usually included | Use Fastmail ($5/mo), Google Workspace ($6/mo), or Mailgun (free tier) |
| **phpMyAdmin** | Pre-installed | Not needed with SQLite. For MySQL, use Adminer (one PHP file) |
| **File manager** | cPanel file manager | `scp`, `rsync`, or `sftp` via any FTP client |
| **PHP version** | Host decides | You decide — download the version you want |
| **Server updates** | Host handles | `apt update && apt upgrade` (or enable unattended upgrades) |

## What You Gain

- **Speed** — dedicated resources instead of shared. Typical improvement: 2-5x faster page loads.
- **Control** — any PHP version, any config, any extension. No host restrictions.
- **Price stability** — VPS prices don't increase after a promo period. $3.69/mo is $3.69/mo forever.
- **No noisy neighbors** — your site isn't affected by other people's traffic spikes.
- **Git-based deployment** — push code, site updates. No FTP uploading.
- **Built-in monitoring** — Prometheus metrics at `/metrics` without installing anything.
- **Multiple sites** — host all your sites on one VPS using virtual hosts. No per-site fees.

## What You Lose

- **Managed email** — you need a separate email provider. This is actually better: shared hosting email often lands in spam anyway.
- **One-click installs** — no Softaculous. You install WordPress/Laravel yourself (it's a `git clone` or `unzip`).
- **cPanel GUI** — you manage via SSH and config files. It's simpler than it sounds.
- **Support for "my site is broken"** — you're responsible for your own debugging. But you also have full access to fix things instead of waiting 48 hours for a support ticket response.

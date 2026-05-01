# Migrating from Laravel Forge / Vapor

You're using Forge to manage your servers — it provisions VPS instances, installs Nginx + PHP-FPM + MySQL, handles deployments, and manages SSL certificates. You pay $12/mo for Forge plus the cost of your VPS. Or you're on Vapor ($399/mo+) for serverless Laravel on AWS Lambda.

ePHPm gives you the same deployment experience for free. One binary replaces everything Forge installs on your server.

## What Forge Does vs. What ePHPm Does

| Forge Feature | How Forge Does It | How ePHPm Does It |
|--------------|-------------------|-------------------|
| Server provisioning | Provisions a VPS, installs packages | You get a VPS, copy one binary |
| Nginx config | Generates `server` blocks | `ephpm.toml` (6 lines) |
| PHP-FPM | Installs + configures pools | Built-in (embedded PHP) |
| SSL certificates | Integrates certbot/Let's Encrypt | Built-in ACME (2 lines of config) |
| Database | Installs MySQL/PostgreSQL | Built-in SQLite or DB proxy |
| Redis | Installs Redis server | Built-in KV store |
| Deployment | `git pull` + `composer install` + `php artisan migrate` | Same — deploy script is unchanged |
| Queue workers | Manages Supervisor + `artisan queue:work` | Run alongside ePHPm (not replaced) |
| Cron / scheduler | Manages crontab for `artisan schedule:run` | Same — cron is unchanged |
| Monitoring | Basic server metrics | Built-in Prometheus `/metrics` |

## What It Costs

| | Forge + VPS | ePHPm + VPS |
|---|---|---|
| Server management | $12/mo (Forge) | $0 (ePHPm is free) |
| VPS | $5-12/mo (DigitalOcean/Hetzner) | $3.69-6/mo |
| Database | Included (MySQL on same VPS) | Included (SQLite embedded) |
| SSL | Included (via Forge) | Included (built-in ACME) |
| Redis | Included (installed by Forge) | Included (built-in KV) |
| **Total** | **$17-24/mo** | **$3.69-6/mo** |

Vapor is even more expensive — $399/mo base + per-request AWS Lambda costs. ePHPm gives you a persistent server with sub-millisecond response times instead of Lambda cold starts.

## Step-by-Step Migration

### 1. Your Laravel Project Doesn't Change

Your PHP code, routes, controllers, views, migrations — none of it changes. ePHPm runs the same Laravel application. The difference is what's underneath.

### 2. Create ephpm.toml

Forge generates an Nginx config like this:

```nginx
server {
    listen 80;
    listen 443 ssl http2;
    server_name example.com;
    root /home/forge/example.com/current/public;

    ssl_certificate /etc/nginx/ssl/example.com/...;
    ssl_certificate_key /etc/nginx/ssl/example.com/...;

    index index.html index.php;

    location / {
        try_files $uri $uri/ /index.php?$query_string;
    }

    location ~ \.php$ {
        fastcgi_pass unix:/var/run/php/php8.2-fpm.sock;
        ...
    }
}
```

ePHPm equivalent:

```toml
# /etc/ephpm/ephpm.toml

[server]
listen = "0.0.0.0:8080"
document_root = "/home/forge/example.com/current/public"
fallback = ["$uri", "$uri/", "/index.php?$query_string"]

[server.tls]
acme_domains = ["example.com"]
acme_email = "you@example.com"

[php]
memory_limit = "256M"
max_execution_time = 60
workers = 8

[db.sqlite]
path = "/home/forge/example.com/database/database.sqlite"
```

### 3. Update Your .env

**Before (Forge-managed MySQL + Redis):**

```env
DB_CONNECTION=mysql
DB_HOST=127.0.0.1
DB_PORT=3306
DB_DATABASE=forge
DB_USERNAME=forge
DB_PASSWORD=secret

CACHE_DRIVER=redis
SESSION_DRIVER=redis
REDIS_HOST=127.0.0.1
```

**After (ePHPm embedded SQLite + KV):**

```env
DB_CONNECTION=mysql
DB_HOST=127.0.0.1
DB_PORT=3306
DB_DATABASE=forge
DB_USERNAME=root
DB_PASSWORD=

CACHE_DRIVER=file
SESSION_DRIVER=file
```

`DB_CONNECTION=mysql` stays — ePHPm's litewire handles the translation. If you keep an external MySQL, just update the `DB_HOST` to point at your managed database.

### 4. Deploy Script

Forge's deployment script typically runs:

```bash
cd /home/forge/example.com/current
git pull origin main
composer install --no-dev --optimize-autoloader
php artisan migrate --force
php artisan config:cache
php artisan route:cache
php artisan view:cache
php artisan queue:restart
```

**This script doesn't change.** ePHPm runs the same PHP binary, the same Artisan commands, the same Composer. The deploy script is independent of the HTTP server.

You can trigger it via:
- A simple `git push` + webhook (switchboard handles this)
- Manual SSH + `git pull`
- GitHub Actions / CI pipeline

### 5. Queue Workers

Forge manages queue workers via Supervisor. ePHPm doesn't replace this — queue workers are a separate concern. Keep Supervisor:

```ini
# /etc/supervisor/conf.d/laravel-worker.conf
[program:laravel-worker]
command=php /home/forge/example.com/current/artisan queue:work
autostart=true
autorestart=true
numprocs=2
```

Or use `systemd`:

```ini
[Service]
ExecStart=/usr/bin/php /home/forge/example.com/current/artisan queue:work
Restart=always
```

### 6. Scheduler

Forge manages the cron entry for Laravel's scheduler. Keep it:

```bash
* * * * * cd /home/forge/example.com/current && php artisan schedule:run >> /dev/null 2>&1
```

This doesn't change.

### 7. Switch Over

```bash
# Stop Forge-managed services
sudo systemctl stop nginx php8.2-fpm

# Install ePHPm as a system service (registers + starts it)
sudo ephpm install

# Disconnect the server from Forge (optional)
# Go to Forge dashboard → Server → Delete Server
```

You can keep the server connected to Forge while testing — just stop Nginx/FPM and let ePHPm serve.

## Vapor Migration

If you're on Vapor (serverless AWS):

1. Your code doesn't change
2. Instead of deploying to Lambda, deploy to a VPS with ePHPm
3. No cold starts — ePHPm workers are always warm
4. No per-request billing — flat monthly VPS cost
5. No AWS complexity (API Gateway, Lambda, SQS, RDS, ElastiCache)

**Vapor costs at scale:**

| Requests/month | Vapor (Lambda + RDS) | ePHPm ($6/mo VPS) |
|---------------|---------------------|-------------------|
| 100K | ~$30/mo | $6/mo |
| 1M | ~$150/mo | $6/mo |
| 10M | ~$800/mo | $12/mo (bigger VPS) |

## What You Gain

- **$12/mo saved** on Forge subscription ($144/year)
- **Simpler stack** — one binary instead of Nginx + FPM + MySQL + Redis
- **No vendor lock-in** — Forge owns your server config. ePHPm config is yours.
- **Better performance** — no FastCGI overhead, no cold starts (vs Vapor)
- **Built-in SQLite** — no database server for small-medium apps
- **Built-in KV** — no Redis server for caching and sessions
- **Built-in metrics** — Prometheus endpoint without extra tooling

## What You Lose

- **Forge's GUI** — server management is via SSH + config files, not a web dashboard
- **One-click PHP version switching** — ePHPm ships with one PHP version per binary. Switch by downloading a different build.
- **Forge's database management UI** — use Adminer, TablePlus, or `php artisan tinker`
- **Forge's team management** — share SSH access directly or use GitHub team permissions
- **Automatic security updates** — Forge applies OS patches. You run `apt upgrade` yourself (or enable unattended upgrades).

+++
title = "Your First App"
weight = 2
+++

The minimum viable ePHPm setup is a directory of PHP files plus a few lines of TOML.

## 1. Create a tiny app

```bash
mkdir -p ~/myapp
cat > ~/myapp/index.php <<'EOF'
<?php
echo "Hello from ePHPm. PHP " . PHP_VERSION . " is running embedded.\n";
echo "Today is " . date("Y-m-d H:i:s") . ".\n";
EOF
```

## 2. Create the config

```bash
cat > ~/ephpm.toml <<'EOF'
[server]
listen = "127.0.0.1:8080"
document_root = "/home/you/myapp"   # adjust to absolute path of ~/myapp
EOF
```

That's it. No `php-fpm.conf`, no Nginx `server` block, no Apache vhost.

## 3. Run

```bash
ephpm serve --config ~/ephpm.toml
```

Hit it:

```bash
curl http://127.0.0.1:8080/
```

You should see your "Hello from ePHPm" message. The PHP runtime is embedded inside the same process — no FastCGI, no IPC, no second daemon to manage.

## 4. Add a database

ePHPm bundles SQLite (exposed as MySQL via [litewire](https://github.com/ephpm/litewire)). Append to `ephpm.toml`:

```toml
[db.sqlite]
path = "/home/you/myapp/app.db"
```

Restart the server. PHP can now connect to `127.0.0.1:3306` with `pdo_mysql` — it thinks it's MySQL, but it's actually a single-file SQLite database. See [Database guide](/docs/architecture/database/) for the details.

## 5. WordPress / Laravel

Drop a real app into `document_root` and it works the same way. Tweak the fallback chain so pretty permalinks resolve:

```toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/wordpress"
fallback = ["$uri", "$uri/", "/index.php?$query_string"]   # the default
```

For step-by-step guides see:

- [WordPress](/docs/guides/wordpress/)
- [Laravel](/docs/guides/laravel/)

## What's running?

You started a single process that's serving HTTP, executing PHP, hosting a SQLite database with MySQL wire compatibility, and (if you turn it on) exposing Prometheus metrics, an embedded KV store on `:6379`, and ACME-issued TLS. No other daemons. One binary.

## What's next?

- **Configure** — see [Configuration](configuration/) for the structure of `ephpm.toml`, then [Reference → Configuration](/docs/reference/config/) for every key.
- **Migrate** — coming from Apache or Nginx? [Migration guides](/docs/migration/) show the equivalent ePHPm config.
- **Cluster** — [Clustering setup](/docs/guides/clustering-setup/) walks through gossip + clustered SQLite.

+++
title = "WordPress"
weight = 1
+++


ePHPm is a single binary that embeds PHP, SQLite, a MySQL wire-protocol proxy,
and an in-process KV store. No PHP-FPM, no MySQL, no Redis, no web server.
WordPress runs out of the box against all three embedded subsystems.

This guide walks through three deployment paths, all **live-tested**:

| | `ephpm dev` | Docker | Kubernetes |
|---|---|---|---|
| PHP runtime | embedded | embedded | embedded |
| Database | embedded SQLite | embedded SQLite | embedded SQLite |
| Object cache | embedded KV | embedded KV | embedded KV |
| Use case | local development | single-node prod | clustered prod |

---

## Prerequisites

- ePHPm binary from the [releases page](https://github.com/ephpm/ephpm/releases)
  or `docker pull ephpm/ephpm:8.5`
- Latest WordPress zip from [wordpress.org/latest.zip](https://wordpress.org/latest.zip)
- The [ephpm/cache-wordpress](https://github.com/ephpm/cache-wordpress) object-cache
  drop-in (Composer package or git clone — see below)

---

## Part 1 — `ephpm dev` (local development)

The fastest path. ePHPm's `dev` subcommand auto-picks a free port,
serves the current directory, and requires zero config.

### 1.1 Set up WordPress

```bash
# Download and extract WordPress
curl -O https://wordpress.org/latest.zip
unzip latest.zip        # creates ./wordpress/

# Create the SQLite database directory
mkdir -p wordpress/wp-content/database
```

### 1.2 Install the SQLite drop-in and the object cache

```bash
# SQLite database integration (replaces MySQL with embedded SQLite)
curl -O https://downloads.wordpress.org/plugin/sqlite-database-integration.zip
unzip sqlite-database-integration.zip -d wordpress/wp-content/plugins/
cp wordpress/wp-content/plugins/sqlite-database-integration/db.copy \
   wordpress/wp-content/db.php
```

The object cache is the [`ephpm/cache-wordpress`](https://github.com/ephpm/cache-wordpress)
drop-in. It writes `wp-content/object-cache.php`, which calls ePHPm's
in-process KV store directly via the `ephpm_kv_*` SAPI functions — no Redis
container, no RESP listener, no Predis, no plugin to activate.

**Canonical (Composer):**

```bash
cd wordpress
composer require ephpm/cache-wordpress
# copy the drop-in into wp-content (it auto-discovers the Composer autoload)
cp vendor/ephpm/cache-wordpress/dropin/object-cache.php wp-content/object-cache.php
```

**No Composer:**

```bash
cd wordpress
git clone https://github.com/ephpm/cache-wordpress wp-content/cache-wordpress
printf "<?php\nrequire __DIR__ . '/cache-wordpress/dropin/object-cache.php';\n" \
  > wp-content/object-cache.php
```

### 1.3 Configure WordPress

Copy `wp-config-sample.php` to `wp-config.php` and add before the
`/* That's all */` line:

```php
// DB credentials are placeholders — ePHPm's SQLite handles all queries
define( 'DB_NAME',     'wordpress' );
define( 'DB_USER',     'wp' );
define( 'DB_PASSWORD', '' );
define( 'DB_HOST',     'localhost' );

// Auth keys — generate real values at https://api.wordpress.org/secret-key/1.1/salt/
define( 'AUTH_KEY',         'change-me' );
define( 'SECURE_AUTH_KEY',  'change-me' );
define( 'LOGGED_IN_KEY',    'change-me' );
define( 'NONCE_KEY',        'change-me' );
define( 'AUTH_SALT',        'change-me' );
define( 'SECURE_AUTH_SALT', 'change-me' );
define( 'LOGGED_IN_SALT',   'change-me' );
define( 'NONCE_SALT',       'change-me' );

// Opt into the persistent object cache. The wp-content/object-cache.php
// drop-in (cache-wordpress) is what makes it persistent; it talks straight
// to ePHPm's in-process KV store. No WP_REDIS_* / Predis config needed.
define( 'WP_CACHE', true );
```

### 1.4 Start ePHPm

```bash
# Use absolute path for document root — relative paths cause
# 'Failed to open stream' on subdirectory requests.
ephpm dev --port 8088 --document-root "$(pwd)/wordpress"

#   ePHPm 0.2.0 — dev server
#     serving:  /path/to/wordpress
#     url:      http://127.0.0.1:8088
#     php:      8.5.7
#     press ctrl+c to stop
```

Open `http://127.0.0.1:8088` — WordPress installer appears.
Complete the 5-field form (site title, username, password, email).

### 1.5 Observe KV population

After completing the installer, make a few requests then inspect the
embedded KV store (the drop-in is active automatically — there is no
plugin to enable):

```bash
ephpm kv keys "*"
# 1) wp:default:is_blog_installed
# 2) wp:options:alloptions
# 3) wp:options:notoptions
# 4) wp:site-options:1-notoptions
# 5) wp:transient:doing_cron
# 6) wp:transient:wp_core_block_css_files
# 7) wp:translation_files:38beaa72c3a2c3668f2cf69a6db0fbe0
# 8) wp:site-transient:wp_theme_files_patterns-bf6ab396...

ephpm kv get "wp:default:is_blog_installed"
# 1

ephpm kv get "wp:options:notoptions"
# a:2:{s:6:"WPLANG";b:1;s:14:"theme_switched";b:1;}
```

Everything flows WordPress → cache-wordpress `ObjectCache` → `ephpm_kv_*`
(in-process). No external Redis process, no RESP listener, no Predis.

> `wp_cache_flush()` is a real flush (backed by `ephpm_kv_flush_all()`) and
> needs **ePHPm v0.1.2+**. On older runtimes the cache still works, but flush
> is a no-op and entries age out via TTL instead.

---

## Part 2 — Docker

Single-container deployment. WordPress files are mounted as a volume;
the ePHPm image provides PHP, SQLite, and the KV store.

### 2.1 Directory layout

```
wordpress-docker/
  wordpress/          ← extracted + configured WordPress
  data/
    database/         ← SQLite DB (persisted via volume)
  ephpm.toml
```

### 2.2 `ephpm.toml`

Note: use `[server] document_root`, not `[php] root` — the document root
lives in the server section.

```toml
[server]
listen        = "0.0.0.0:8080"
document_root = "/app/wordpress"

[db.sqlite]
path = "/app/data/database/wordpress.sqlite"

# No [kv.redis_compat] needed — the cache-wordpress drop-in calls the
# in-process KV store directly, so there is no RESP listener to configure.
```

### 2.3 `wp-config.php` additions

```php
// The wp-content/object-cache.php drop-in (cache-wordpress) handles the
// object cache via ephpm_kv_* in-process. Just opt in:
define( 'WP_CACHE', true );
```

Install the drop-in into the mounted `wordpress/` tree the same way as
Part 1.2 (Composer or git clone), so `wp-content/object-cache.php` exists
before the container starts.

### 2.4 Run

```bash
docker run -d \
  --name wordpress \
  -p 8080:8080 \
  -v $(pwd)/wordpress:/app/wordpress \
  -v $(pwd)/data:/app/data \
  -v $(pwd)/ephpm.toml:/app/ephpm.toml \
  ephpm/ephpm:8.5 \
  ephpm serve --config /app/ephpm.toml
```

### 2.5 Verify (live-tested output)

```bash
# PHP version and SAPI name
docker exec wordpress ephpm php -- -r "phpinfo();" | grep -E "PHP Version|Server API"
# PHP Version => 8.5.7
# Server API => ePHPm Embedded Server

# KV keys after serving a few requests
docker exec wordpress ephpm kv keys "*"
# 1)  wp:translation_files:d8e23637f84479ddb9c69ac1010d9605
# 2)  wp:site-transient:wp_theme_files_patterns-947cd8213a68c909c9532a7b4479c043
# 3)  wp:default:is_blog_installed
# 4)  wp:translation_files:b24b2517e590ce31a2d286de890c7b5c
# 5)  wp:posts:3
# 6)  wp:options:notoptions
# 7)  wp:translation_files:d6b2ae33ed84defc9458dd2197de97e7
# 8)  wp:options:nonce_salt
# 9)  wp:translation_files:3dabf541bbb89d77e94dc1a9c297c019
# 10) wp:options:nonce_key
# 11) wp:transient:wp_core_block_css_files
# 12) wp:site-options:1-notoptions
# 13) wp:options:alloptions
```

### 2.6 docker compose (optional)

```yaml
services:
  wordpress:
    image: ephpm/ephpm:8.5
    command: ephpm serve --config /app/ephpm.toml
    ports:
      - "8080:8080"
    volumes:
      - ./wordpress:/app/wordpress
      - wordpress_data:/app/data
      - ./ephpm.toml:/app/ephpm.toml
    restart: unless-stopped

volumes:
  wordpress_data:
```

---

## Part 3 — Kubernetes

ePHPm's single-binary model maps to Kubernetes cleanly: one container,
no sidecars needed for PHP-FPM, MySQL, or Redis.

### 3.1 ConfigMap — ephpm.toml + wp-config.php

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: ephpm-config
data:
  ephpm.toml: |
    [server]
    listen        = "0.0.0.0:8080"
    document_root = "/app/wordpress"

    [db.sqlite]
    path = "/app/data/database/wordpress.sqlite"

    # No [kv.redis_compat]: the cache-wordpress drop-in uses the in-process
    # KV store directly, so no RESP listener is needed.

  wp-config.php: |
    <?php
    define( 'DB_NAME',     'wordpress' );
    define( 'DB_USER',     'wp' );
    define( 'DB_PASSWORD', '' );
    define( 'DB_HOST',     'localhost' );
    define( 'DB_CHARSET',  'utf8' );
    define( 'DB_COLLATE',  '' );

    define( 'AUTH_KEY',         'change-me-in-prod' );
    define( 'SECURE_AUTH_KEY',  'change-me-in-prod' );
    define( 'LOGGED_IN_KEY',    'change-me-in-prod' );
    define( 'NONCE_KEY',        'change-me-in-prod' );
    define( 'AUTH_SALT',        'change-me-in-prod' );
    define( 'SECURE_AUTH_SALT', 'change-me-in-prod' );
    define( 'LOGGED_IN_SALT',   'change-me-in-prod' );
    define( 'NONCE_SALT',       'change-me-in-prod' );

    // Object cache via the cache-wordpress drop-in (ephpm_kv_* in-process).
    define( 'WP_CACHE',  true );
    define( 'WP_DEBUG',  false );

    $table_prefix = 'wp_';
    define( 'ABSPATH', __DIR__ . '/' );
    require_once ABSPATH . 'wp-settings.php';
```

### 3.2 Deployment

Two init containers run before ephpm starts:

1. **`wordpress-download`** (busybox): downloads WordPress + the SQLite
   plugin, clones the cache-wordpress drop-in and wires
   `wp-content/object-cache.php`, copies wp-config.php from the ConfigMap.
2. **`wordpress-install`** (ephpm): starts ephpm temporarily, POSTs the
   WordPress install form to create all 14 DB tables, then exits cleanly.

The main container's readiness probe checks `/license.txt` (a static file)
rather than a PHP endpoint — this avoids triggering WordPress's DB init
before the install init container has run.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: wordpress
spec:
  replicas: 1
  selector:
    matchLabels:
      app: wordpress
  template:
    metadata:
      labels:
        app: wordpress
    spec:
      initContainers:
        - name: wordpress-download
          image: alpine/git    # has git for the clone; unzip added below
          command:
            - sh
            - -c
            - |
              set -e
              apk add --no-cache unzip >/dev/null
              mkdir -p /app/data/database /app/wordpress/wp-content/database

              if [ ! -f /app/wordpress/index.php ]; then
                wget -q -O /tmp/wp.tar.gz https://wordpress.org/latest.tar.gz
                tar -xzf /tmp/wp.tar.gz -C /app/ && rm /tmp/wp.tar.gz
              fi

              if [ ! -f /app/wordpress/wp-content/plugins/sqlite-database-integration/load.php ]; then
                wget -q -O /tmp/s.zip https://downloads.wordpress.org/plugin/sqlite-database-integration.zip
                unzip -q /tmp/s.zip -d /app/wordpress/wp-content/plugins/
                cp /app/wordpress/wp-content/plugins/sqlite-database-integration/db.copy \
                   /app/wordpress/wp-content/db.php
                rm /tmp/s.zip
              fi

              if [ ! -f /app/wordpress/wp-content/cache-wordpress/dropin/object-cache.php ]; then
                git clone --depth 1 https://github.com/ephpm/cache-wordpress \
                  /app/wordpress/wp-content/cache-wordpress
                printf "<?php\nrequire __DIR__ . '/cache-wordpress/dropin/object-cache.php';\n" \
                  > /app/wordpress/wp-content/object-cache.php
              fi

              cp /etc/ephpm/wp-config.php /app/wordpress/wp-config.php
          volumeMounts:
            - name: wordpress-files
              mountPath: /app/wordpress
            - name: wordpress-data
              mountPath: /app/data
            - name: ephpm-config
              mountPath: /etc/ephpm

        - name: wordpress-install
          image: ephpm/ephpm:v0.2.0-php8.5.7
          command:
            - sh
            - -c
            - |
              DB="/app/wordpress/wp-content/database/.ht.sqlite"
              if [ -f "$DB" ]; then echo "Already installed."; exit 0; fi

              ephpm serve --config /etc/ephpm/ephpm.toml &
              EPHPM_PID=$!; sleep 3

              wget -q -O /dev/null --post-data \
                "weblog_title=ePHPm+Demo&user_name=admin&admin_password=ephpm-demo-2026!&admin_password2=ephpm-demo-2026!&admin_email=demo%40ephpm.dev&blog_public=1&Submit=Install+WordPress&language=" \
                "http://127.0.0.1:8080/wp-admin/install.php?step=2" 2>&1 || true

              sleep 2
              kill $EPHPM_PID 2>/dev/null || true
              wait $EPHPM_PID 2>/dev/null || true
          volumeMounts:
            - name: ephpm-config
              mountPath: /etc/ephpm
            - name: wordpress-files
              mountPath: /app/wordpress
            - name: wordpress-data
              mountPath: /app/data

      containers:
        - name: ephpm
          image: ephpm/ephpm:v0.2.0-php8.5.7
          command: [ephpm, serve, --config, /etc/ephpm/ephpm.toml]
          ports:
            - name: http
              containerPort: 8080
          volumeMounts:
            - name: ephpm-config
              mountPath: /etc/ephpm
            - name: wordpress-files
              mountPath: /app/wordpress
            - name: wordpress-data
              mountPath: /app/data
          readinessProbe:
            httpGet:
              path: /license.txt
              port: 8080
            initialDelaySeconds: 5
            periodSeconds: 5
            failureThreshold: 6
          resources:
            requests:
              cpu: 100m
              memory: 256Mi
            limits:
              cpu: 1000m
              memory: 512Mi

      volumes:
        - name: ephpm-config
          configMap:
            name: ephpm-config
        - name: wordpress-files
          emptyDir: {}
        - name: wordpress-data
          emptyDir: {}
---
apiVersion: v1
kind: Service
metadata:
  name: wordpress
spec:
  type: NodePort
  selector:
    app: wordpress
  ports:
    - port: 80
      targetPort: 8080
      nodePort: 30080
```

### 3.3 Deploy and verify (live-tested output)

```bash
kubectl apply -f configmap.yaml -f deployment.yaml

kubectl rollout status deployment/wordpress
# Waiting for deployment "wordpress" rollout to finish: 0 of 1 updated replicas are available...
# deployment "wordpress" successfully rolled out

# 14 WordPress tables created by the install init container
kubectl exec deploy/wordpress -- ephpm php -- -r '
$db=new SQLite3("/app/wordpress/wp-content/database/.ht.sqlite");
$t=$db->query("SELECT name FROM sqlite_master WHERE type='\''table'\'' ORDER BY name");
while ($r=$t->fetchArray(SQLITE3_ASSOC)) echo $r["name"]."\n";
'
# wp_commentmeta, wp_comments, wp_links, wp_options, wp_postmeta,
# wp_posts, wp_term_relationships, wp_term_taxonomy, wp_termmeta,
# wp_terms, wp_usermeta, wp_users  (14 tables)

# KV keys after page loads (20 keys observed)
kubectl exec deploy/wordpress -- ephpm kv keys "*"
# 1)  wp:terms:1
# 2)  wp:post_tag_relationships:1
# 3)  wp:site-options:1-notoptions
# 4)  wp:default:is_blog_installed
# 5)  wp:post-queries:wp_query-6506dec3...
# 6)  wp:site-transient:wp_theme_files_patterns-...
# 7)  wp:terms:last_changed
# 8)  wp:options:notoptions
# 9)  wp:translation_files:3dabf541...
# 10) wp:transient:wp_core_block_css_files
# 11) wp:posts:last_changed
# 12) wp:term-queries:get_terms-...
# 13) wp:options:alloptions
# 14) wp:post_format_relationships:1
# 15) wp:translation_files:d8e23637...
# 16) wp:posts:1
# 17) wp:translation_files:b24b2517...
# 18) wp:translation_files:d6b2ae33...
# 19) wp:category_relationships:1
# 20) wp:post_meta:1

kubectl logs deploy/wordpress | tail -5
# INFO ephpm: starting ePHPm listen=0.0.0.0:8080 document_root=/app/wordpress
# INFO ephpm_php: PHP runtime initialized (libphp linked)
# INFO ephpm_server: opened embedded SQLite database (single-node)
# INFO ephpm_server: SQLite MySQL wire protocol enabled listen=127.0.0.1:3306
# INFO ephpm_server: HTTP listening addr=0.0.0.0:8080
```

> **Note on multi-replica SQLite:** SQLite's WAL mode supports concurrent
> readers but only one writer. For multi-replica deployments, enable
> ePHPm's clustered SQLite mode (`[db.sqlite.replication]` + `[cluster]`),
> which uses sqld for WAL frame replication via gRPC. Clustered mode is
> not supported on Windows (no sqld binary from Turso).

> **Note on persistence:** the manifests above use `emptyDir` volumes for
> simplicity. For production, replace with PersistentVolumeClaims and store
> auth keys in a Secret.

---

## Part 4 — Docker Compose with external MySQL

Parts 1–3 use ePHPm's *embedded* SQLite — one binary, zero external services.
But ePHPm is equally happy as a thin PHP runtime in front of a *real* database.
This stack keeps the object cache embedded (the cache-wordpress drop-in, same
as everywhere else) but moves the database into its own MySQL container, reached
through ePHPm's **connection-pooling MySQL proxy** (`[db.mysql]`). WordPress's
`pdo_mysql` connects to `127.0.0.1:3306` inside the ePHPm container; ePHPm
forwards to `mysql:3306`.

```
WordPress (PHP in ePHPm)
  ├── pdo_mysql     ─► 127.0.0.1:3306 (ePHPm proxy) ─► mysql:3306  (external MySQL)
  └── object cache  ─► cache-wordpress drop-in ─► ephpm_kv_* (in-process)
```

This is the mode you'd use to drop ePHPm into an existing managed-database
environment, or to share one MySQL across many instances — while still getting
a persistent object cache for free from the same single binary, with no Redis.

### 4.1 `ephpm.toml` — proxy, embedded object cache

```toml
[server]
listen        = "0.0.0.0:8080"
document_root = "/app/wordpress"

# Transparent MySQL proxy with pooling. No [db.sqlite], no [kv.redis_compat]
# (the cache-wordpress drop-in uses the in-process KV store directly).
[db.mysql]
url             = "mysql://wordpress:wordpress@mysql:3306/wordpress"
listen          = "127.0.0.1:3306"
max_connections = 20
```

### 4.2 `wp-config.php` — point DB at the proxy

```php
define( 'DB_NAME',     'wordpress' );
define( 'DB_USER',     'wordpress' );
define( 'DB_PASSWORD', 'wordpress' );
define( 'DB_HOST',     '127.0.0.1' );   // ePHPm proxy -> mysql:3306

// Object cache via the cache-wordpress drop-in (ephpm_kv_* in-process).
define( 'WP_CACHE', true );
```

Note: in this mode WordPress uses its **native `mysqli`** against a real MySQL,
so there is **no** SQLite integration plugin and **no** `db.php` drop-in — the
object cache is still the cache-wordpress `wp-content/object-cache.php` drop-in.

### 4.3 `compose.yaml`

```yaml
name: ephpm-wordpress-mysql

services:
  init:        # one-shot: fetch WordPress, wire the cache-wordpress drop-in, drop in wp-config.php
    image: alpine/git
    command:
      - sh
      - -c
      - |
        set -e
        apk add --no-cache unzip >/dev/null
        if [ ! -f /wp/index.php ]; then
          wget -q -O /tmp/wp.tar.gz https://wordpress.org/latest.tar.gz
          tar -xzf /tmp/wp.tar.gz --strip-components=1 -C /wp
        fi
        if [ ! -d /wp/wp-content/cache-wordpress ]; then
          git clone --depth 1 https://github.com/ephpm/cache-wordpress \
            /wp/wp-content/cache-wordpress
          printf "<?php\nrequire __DIR__ . '/cache-wordpress/dropin/object-cache.php';\n" \
            > /wp/wp-content/object-cache.php
        fi
        cp /wp-config.php /wp/wp-config.php
    volumes:
      - ./wordpress:/wp
      - ./wp-config.php:/wp-config.php:ro

  mysql:
    image: mysql:8.4
    environment:
      MYSQL_ROOT_PASSWORD: rootpw
      MYSQL_DATABASE: wordpress
      MYSQL_USER: wordpress
      MYSQL_PASSWORD: wordpress
    volumes: [mysql-data:/var/lib/mysql]
    healthcheck:
      test: ["CMD", "mysqladmin", "ping", "-h", "127.0.0.1", "-uwordpress", "-pwordpress"]
      interval: 5s
      retries: 30

  ephpm:
    image: ephpm/ephpm:8.5
    depends_on:
      init:  { condition: service_completed_successfully }
      mysql: { condition: service_healthy }
    command: ["ephpm", "serve", "--config", "/app/ephpm.toml"]
    ports: ["8080:8080"]
    volumes:
      - ./wordpress:/app/wordpress
      - ./ephpm.toml:/app/ephpm.toml:ro

volumes:
  mysql-data:
```

### 4.4 Run

```bash
docker compose up -d
# open http://localhost:8080 and finish the WordPress installer
```

The `init` service downloads WordPress and wires the cache-wordpress drop-in on
first run; MySQL comes up healthy before ePHPm starts; WordPress installs into
the external MySQL through the proxy, and its object cache lands in ePHPm's
in-process KV store:

```bash
docker compose exec ephpm ephpm kv keys 'wp:*' | head
docker compose exec mysql mysql -uwordpress -pwordpress wordpress -e 'SHOW TABLES;'
```

> **Not run on the authoring machine.** Unlike the embedded demos (Parts 1–3,
> which were live-tested), this external-MySQL stack was authored from ePHPm's
> `[db.mysql]` proxy schema; validate in your own Docker environment before
> relying on it.

---

## What runs inside the single binary

```
HTTP :8080  ──► WordPress PHP 8.5.7 (ePHPm Embedded Server SAPI, ZTS)
                    │
                    ├── pdo_mysql       ──► litewire ──► SQLite (MySQL wire on :3306)
                    │
                    └── object cache    ──► cache-wordpress ──► ephpm_kv_* (in-process)
                                            (object cache, transients, sessions)
```

All three subsystems run inside the single `ephpm` process.
No PHP-FPM. No MySQL. No Redis. No nginx.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Failed opening required '.\wp-admin/install.php'` | Document root as relative path | Use absolute path: `--document-root /abs/path/to/wordpress` |
| `unable to open database file` | SQLite directory missing | `mkdir -p wp-content/database` |
| `failed to bind ... os error 10013` (Windows) | Firewall blocking port | Allow port in Windows Defender Firewall |
| KV keys empty after requests | cache-wordpress drop-in not installed | Ensure `wp-content/object-cache.php` exists (see Part 1.2) |
| `wp_cache_flush()` returns false / no-op | ePHPm runtime older than v0.1.2 | Upgrade to ePHPm v0.1.2+ (the release with `ephpm_kv_flush_all()`) |
| `WP_CACHE` not taking effect | `WP_CACHE` not set before `wp-settings.php` | Add `define('WP_CACHE', true);` above the `require_once` line |
| Readiness probe causes half-initialized DB (k8s) | PHP probe hits before install completes | Probe `/license.txt` (static file); run installer in an init container |

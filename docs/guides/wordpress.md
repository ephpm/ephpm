# Running WordPress on ePHPm

ePHPm is a single binary that embeds PHP, SQLite, a MySQL wire-protocol proxy,
and a Redis-compatible KV store. No PHP-FPM, no MySQL, no Redis, no web server.
WordPress runs out of the box against all three embedded subsystems.

This guide walks through three deployment paths:

| | Dev binary | Docker | Kubernetes |
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
- The [Redis Object Cache](https://wordpress.org/plugins/redis-cache/) plugin

---

## Part 1 — `ephpm dev` (local development)

The fastest path. ePHPm's `dev` subcommand auto-picks a free port,
serves the current directory, and requires zero config.

### 1.1 Set up WordPress

```bash
# Download and extract WordPress
curl -O https://wordpress.org/latest.zip
unzip latest.zip        # creates ./wordpress/

# Create the SQLite database directory (required before first start)
mkdir -p wordpress/wp-content/database
```

### 1.2 Configure WordPress for ePHPm

Copy `wp-config-sample.php` to `wp-config.php`. The DB credentials
are ignored by ePHPm's embedded SQLite — leave them as placeholders:

```bash
cp wordpress/wp-config-sample.php wordpress/wp-config.php
```

Add the following block **before** the `/* That's all */` line:

```php
// ePHPm embedded SQLite — DB_* values are placeholders, not used
define( 'DB_NAME',     'wordpress' );
define( 'DB_USER',     'wp' );
define( 'DB_PASSWORD', '' );
define( 'DB_HOST',     'localhost' );

// ePHPm embedded KV store (Redis-compatible, RESP2 on :6379)
define( 'WP_REDIS_PLUGIN_PATH', __DIR__ . '/wp-content/plugins/redis-cache' );
define( 'WP_REDIS_HOST',        '127.0.0.1' );
define( 'WP_REDIS_PORT',        6379 );
define( 'WP_REDIS_CLIENT',      'predis' );
define( 'WP_REDIS_TIMEOUT',     1 );
define( 'WP_REDIS_READ_TIMEOUT', 1 );
define( 'WP_CACHE', true );
```

### 1.3 Install the SQLite and Redis Object Cache plugins

```bash
# SQLite database integration (replaces MySQL with embedded SQLite)
curl -O https://downloads.wordpress.org/plugin/sqlite-database-integration.zip
unzip sqlite-database-integration.zip -d wordpress/wp-content/plugins/
cp wordpress/wp-content/plugins/sqlite-database-integration/db.copy \
   wordpress/wp-content/db.php

# Redis Object Cache (uses ePHPm's embedded KV via Predis)
curl -O https://downloads.wordpress.org/plugin/redis-cache.zip
unzip redis-cache.zip -d wordpress/wp-content/plugins/

# Copy the object-cache drop-in
cp wordpress/wp-content/plugins/redis-cache/includes/object-cache.php \
   wordpress/wp-content/object-cache.php
```

### 1.4 Start ePHPm

```bash
# Start dev server — auto-picks a free port, serves ./wordpress
ephpm dev --port 8088 --document-root ./wordpress

#   ePHPm 0.1.0 — dev server
#     serving:  ./wordpress
#     url:      http://127.0.0.1:8088
#     php:      8.5.7
#     press ctrl+c to stop
```

Open `http://127.0.0.1:8088` — the WordPress installer appears.
Complete the 5-field form (site title, username, password, email).

### 1.5 Enable the Redis Object Cache plugin

After installation, activate the plugins via the WordPress admin
(`/wp-admin/plugins.php`) or directly via SQL:

```bash
# Activate both plugins via ePHPm's embedded PHP CLI
ephpm php -- -r "
\$db = new SQLite3('wordpress/wp-content/database/.ht.sqlite');
\$row = \$db->querySingle(\"SELECT option_value FROM wp_options WHERE option_name='active_plugins'\", true);
\$plugins = unserialize(\$row['option_value']) ?: [];
\$plugins = array_unique(array_merge(\$plugins, [
    'sqlite-database-integration/load.php',
    'redis-cache/redis-cache.php',
]));
sort(\$plugins);
\$db->exec(\"UPDATE wp_options SET option_value='\" . SQLite3::escapeString(serialize(\$plugins)) . \"' WHERE option_name='active_plugins'\");
echo implode(\"\n\", \$plugins) . \"\n\";
"
```

### 1.6 Observe KV population

With the server running, make a few requests — then inspect
what WordPress wrote into the embedded KV store:

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

ephpm kv ttl "wp:transient:doing_cron"
# no expiry (persistent key)
```

Everything flows through ePHPm's embedded KV via RESP2 on `127.0.0.1:6379`
— no external Redis process.

---

## Part 2 — Docker

Single-container deployment. WordPress files are mounted as a volume;
the ePHPm image provides PHP, SQLite, and the KV store.

### 2.1 Directory layout

```
wordpress-docker/
  wordpress/          ← extracted WordPress files
  data/
    database/         ← SQLite DB lives here (persistent volume)
  ephpm.toml          ← server config
```

### 2.2 `ephpm.toml`

```toml
[server]
listen = "0.0.0.0:8080"

[php]
root = "/app/wordpress"
index = "index.php"

[db.sqlite]
path = "/app/data/database/wordpress.sqlite"

[kv.redis_compat]
enabled = true
listen  = "127.0.0.1:6379"
```

Update `wp-config.php` to reference the container paths:

```php
define( 'WP_REDIS_PLUGIN_PATH', '/app/wordpress/wp-content/plugins/redis-cache' );
define( 'WP_REDIS_HOST',        '127.0.0.1' );
define( 'WP_REDIS_PORT',        6379 );
define( 'WP_REDIS_CLIENT',      'predis' );
define( 'WP_CACHE', true );
```

### 2.3 Run

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

WordPress is available at `http://localhost:8080`.

### 2.4 Verify phpinfo and KV via the container

```bash
# Check the embedded PHP version and SAPI name
docker exec wordpress ephpm php -- -r "phpinfo();" | grep -E "PHP Version|Server API|Thread Safety"
# PHP Version => 8.5.7
# Server API => ePHPm Embedded Server
# Thread Safety => enabled

# List KV keys from inside the container
docker exec wordpress ephpm kv keys "*"
```

### 2.5 docker-compose (optional)

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

```bash
docker compose up -d
docker compose exec wordpress ephpm kv keys "*"
```

---

## Part 3 — Kubernetes

ePHPm's single-binary model maps well to Kubernetes: one container,
no sidecars, no init containers for PHP-FPM or Redis.

### 3.1 ConfigMap — `ephpm.toml`

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: ephpm-config
data:
  ephpm.toml: |
    [server]
    listen = "0.0.0.0:8080"

    [php]
    root = "/app/wordpress"
    index = "index.php"

    [db.sqlite]
    path = "/app/data/database/wordpress.sqlite"

    [kv.redis_compat]
    enabled = true
    listen  = "127.0.0.1:6379"
```

### 3.2 PersistentVolumeClaim

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: wordpress-data
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 5Gi
```

### 3.3 Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: wordpress
spec:
  replicas: 1          # single-node SQLite; for multi-node see cluster docs
  selector:
    matchLabels:
      app: wordpress
  template:
    metadata:
      labels:
        app: wordpress
    spec:
      initContainers:
        # Populate WordPress files on first start
        - name: wordpress-init
          image: busybox
          command:
            - sh
            - -c
            - |
              if [ ! -f /app/wordpress/wp-config.php ]; then
                echo "Extracting WordPress..."
                wget -qO- https://wordpress.org/latest.tar.gz | tar -xz -C /app/
                cp /app/wordpress/wp-config-sample.php /app/wordpress/wp-config.php
                mkdir -p /app/data/database
                echo "Done."
              fi
          volumeMounts:
            - name: wordpress-files
              mountPath: /app/wordpress
            - name: wordpress-data
              mountPath: /app/data

      containers:
        - name: ephpm
          image: ephpm/ephpm:8.5
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
              path: /wp-login.php
              port: 8080
            initialDelaySeconds: 5
            periodSeconds: 10
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
          persistentVolumeClaim:
            claimName: wordpress-data
```

### 3.4 Service and Ingress

```yaml
apiVersion: v1
kind: Service
metadata:
  name: wordpress
spec:
  selector:
    app: wordpress
  ports:
    - port: 80
      targetPort: 8080
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: wordpress
  annotations:
    nginx.ingress.kubernetes.io/proxy-body-size: "64m"
spec:
  rules:
    - host: wordpress.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: wordpress
                port:
                  number: 80
```

### 3.5 Deploy and verify

```bash
kubectl apply -f configmap.yaml -f pvc.yaml -f deployment.yaml -f service.yaml -f ingress.yaml

# Wait for rollout
kubectl rollout status deployment/wordpress

# Check KV keys from inside the pod
kubectl exec deploy/wordpress -- ephpm kv keys "*"

# Tail logs
kubectl logs -f deploy/wordpress
# INFO ephpm: starting ePHPm listen=0.0.0.0:8080 document_root=/app/wordpress
# INFO ephpm_php: PHP runtime initialized (libphp linked)
# INFO ephpm_kv::server: KV store RESP server listening listen=127.0.0.1:6379
# INFO ephpm_server: opened embedded SQLite database (single-node)
# INFO ephpm_server: SQLite MySQL wire protocol enabled listen=127.0.0.1:3306
# INFO ephpm_server: HTTP listening addr=0.0.0.0:8080
```

> **Note on multi-replica SQLite:** SQLite's WAL mode supports
> concurrent readers but only one writer. For multi-replica deployments
> enable ePHPm's clustered SQLite mode (`[db.sqlite.replication]` +
> `[cluster]`) which uses sqld for WAL frame replication via gRPC.
> Windows does not support clustered mode (no sqld binary available).

---

## What runs inside the single binary

```
HTTP :8080  ──► WordPress PHP 8.5.7 (ePHPm Embedded Server SAPI)
                    │
                    ├── pdo_mysql  ──► litewire ──► SQLite :db/wordpress.sqlite
                    │                  (MySQL wire protocol on :3306)
                    │
                    └── Predis     ──► ePHPm KV  ──► RESP2 on :6379
                                       (object cache, transients, sessions)
```

All three subsystems run inside the single `ephpm` process.
No PHP-FPM. No MySQL. No Redis. No nginx.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Failed opening required '.\wp-admin/install.php'` | Document root passed as relative path | Use absolute path: `--document-root /abs/path/to/wordpress` |
| `unable to open database file` | SQLite database directory missing | `mkdir -p wp-content/database` |
| `failed to bind to 0.0.0.0:8080: os error 10013` | Windows firewall / port in use | Use a different port or allow the port in Windows Defender Firewall |
| KV keys empty after requests | `object-cache.php` not installed | Copy drop-in: `cp plugins/redis-cache/includes/object-cache.php wp-content/` |
| `Predis library not found` | `WP_REDIS_PLUGIN_PATH` undefined | Add `define('WP_REDIS_PLUGIN_PATH', __DIR__ . '/wp-content/plugins/redis-cache');` to `wp-config.php` |
| KV shows keys but WordPress isn't using cache | `WP_CACHE` not `true` | Add `define('WP_CACHE', true);` to `wp-config.php` |

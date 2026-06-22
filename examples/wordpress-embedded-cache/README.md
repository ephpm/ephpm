# WordPress on ePHPm — embedded object cache

This demo runs WordPress with its persistent object cache served by ePHPm's
**in-process KV store**, via the [`ephpm/cache-wordpress`](https://github.com/ephpm/cache-wordpress)
drop-in. No Redis container, no RESP listener, no Predis — every cache op is
a direct function call into ePHPm's KV store, and `wp_cache_flush()` is a real
flush backed by `ephpm_kv_flush_all()`.

The only external service is MySQL, reached through ePHPm's connection-pooling
proxy. (For a fully-embedded setup, swap `[db.mysql]` for `[db.sqlite]` in
`ephpm.toml` — then there are no external services at all.)

```
WordPress (PHP in ePHPm)
  ├── pdo_mysql ─► 127.0.0.1:3306 (ePHPm proxy) ─► mysql:3306
  └── object cache ─► ephpm/cache-wordpress drop-in ─► ephpm_kv_* (in-process)
```

## How it compares to the other WordPress demos

| Demo | Database | Object cache |
|---|---|---|
| `../wordpress-compose` (external services) | external MySQL via proxy | external Redis via the redis-cache plugin |
| **this one (embedded cache)** | external MySQL via proxy | **ePHPm KV in-process** (cache-wordpress drop-in) |
| Embedded everything (see the [WordPress guide](https://ephpm.dev/guides/wordpress/)) | embedded SQLite | ePHPm KV in-process |

## Requirements

- Docker (or Podman with the Docker CLI).
- An ePHPm image whose embedded PHP exposes `ephpm_kv_flush_all()`
  (**ePHPm v0.1.2+**). On older images the cache still works, but
  `wp_cache_flush()` is a no-op (cached entries age out via TTL instead).

## Run

```bash
cd examples/wordpress-embedded-cache
docker compose up -d
# open http://localhost:8080 and finish the WordPress installer
```

The `init` service downloads WordPress, clones `ephpm/cache-wordpress` into
`wp-content/`, writes `wp-content/object-cache.php` (a one-line drop-in that
loads the package), and drops in `wp-config.php`. MySQL comes up healthy
before ePHPm starts.

## Verify the cache is live and flush works

```bash
# The object cache class WordPress is using:
docker compose exec ephpm ephpm php -- -r '
  define("WP_USE_THEMES", false);
  require "/app/wordpress/wp-load.php";
  echo get_class($GLOBALS["wp_object_cache"]) . "\n";       // Ephpm\Cache\WordPress\ObjectCache
  wp_cache_set("probe", "hello", "demo");
  echo wp_cache_get("probe", "demo", true) . "\n";          // hello
  var_export(wp_cache_flush());                              // true
  echo "\n";
  var_export(wp_cache_get("probe", "demo", true));          // false (flushed)
  echo "\n";
'
```

> Note: `ephpm php` (CLI) and the HTTP server are separate processes with
> separate in-process stores, so the snippet above proves the drop-in +
> flush wiring within one process. In normal operation every HTTP request
> shares the server's single KV store.

## What's NOT supported

ePHPm's KV store is a string + counter store. The drop-in maps WordPress's
object-cache contract (get/set/add/replace/incr/decr/delete/flush, group and
multisite handling, non-persistent groups) onto it. There are no lists, sets,
hashes, or pub/sub — WordPress core doesn't need them for the object cache.
See the [cache-wordpress README](https://github.com/ephpm/cache-wordpress) for
the full supported surface and `flush_group()` caveats.

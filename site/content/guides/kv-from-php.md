+++
title = "KV from PHP"
weight = 6
+++

ePHPm's built-in KV store is reachable two ways from PHP:

1. **SAPI functions** — `ephpm_kv_*` calls into the embedded store directly. ~100 ns per op, zero serialization.
2. **RESP protocol** — any Redis client (Predis, phpredis) connects to the embedded RESP listener. ~10–100 µs per op.

The store is the same in both cases. Use SAPI for hot paths; use RESP when you need portability or Redis-style commands.

## SAPI functions

No external dependency. Available as native PHP functions whenever PHP is running inside ePHPm.

```php
ephpm_kv_set("greeting", "hello");
$greeting = ephpm_kv_get("greeting");          // "hello"
$missing  = ephpm_kv_get("nope");              // null

ephpm_kv_exists("greeting");                   // 1
ephpm_kv_del("greeting");                      // 1
ephpm_kv_exists("greeting");                   // 0

// Counters
ephpm_kv_incr("page:views", 1);                // 1
ephpm_kv_incr("page:views", 5);                // 6

// TTL (in milliseconds)
ephpm_kv_set("session:abc", "data");
ephpm_kv_expire("session:abc", 60_000);        // 60 seconds
ephpm_kv_pttl("session:abc");                  // ~60000
// returns -1 if no expiry, -2 if missing
```

### When to use SAPI

- High-frequency operations (logging, hit counters, rate limit hot path)
- Simple key/value patterns
- You don't care about portability to standalone Redis

## RESP protocol (Predis / phpredis)

Enable the listener in `ephpm.toml`:

```toml
[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"   # default
# password = "..."           # optional AUTH
```

Then connect like any Redis server:

```php
$redis = new Predis\Client('tcp://127.0.0.1:6379');

$redis->set('greeting', 'hello');
$redis->get('greeting');

$redis->setex('session:abc', 60, json_encode($data));
$count = $redis->incr('page:views');
```

### Supported commands

| Group | Commands |
|-------|----------|
| Strings | `GET`, `SET`, `SETEX`, `MGET`, `MSET`, `SETNX`, `INCR`, `DECR`, `INCRBY`, `DECRBY`, `APPEND`, `STRLEN`, `GETSET` |
| Keys | `DEL`, `EXISTS`, `EXPIRE`, `PEXPIRE`, `PERSIST`, `TTL`, `PTTL`, `TYPE`, `KEYS`, `DBSIZE`, `FLUSHDB`, `FLUSHALL`, `RENAME` |
| Connection | `PING`, `ECHO`, `SELECT`, `QUIT`, `COMMAND`, `INFO`, `AUTH` |

Not implemented: hashes, lists, sets, transactions, `SCAN`, pub/sub. ePHPm targets the cache + counter + session use case — if you need full Redis, run actual Redis.

### Multi-tenant note

The RESP listener can be shared across virtual hosts — each site is isolated by AUTH. When both `[kv] secret` and `[server] sites_dir` are set, ePHPm derives a per-site password as `HMAC-SHA256(secret, hostname)` (lowercase hex, 64 chars) and injects four env vars into every PHP request so the site's code can connect without any per-vhost configuration:

```
EPHPM_REDIS_HOST       # from [kv.redis_compat] listen
EPHPM_REDIS_PORT
EPHPM_REDIS_USERNAME   # the vhost hostname (e.g. alice-blog.com)
EPHPM_REDIS_PASSWORD   # HMAC-SHA256(secret, hostname) hex
```

The RESP server validates the incoming `AUTH <username> <password>` against the same derivation, so requests authenticated as `alice-blog.com` only see alice's `DashMap`. Bob's connection sees a separate one even though both hit the same TCP port.

A PHP app consumes them like any other Redis credentials — Predis, phpredis, or the `ephpm_kv_*` SAPI functions all work without code changes:

```php
$redis = new Predis\Client([
    'scheme'   => 'tcp',
    'host'     => $_SERVER['EPHPM_REDIS_HOST'],
    'port'     => (int) $_SERVER['EPHPM_REDIS_PORT'],
    'username' => $_SERVER['EPHPM_REDIS_USERNAME'],
    'password' => $_SERVER['EPHPM_REDIS_PASSWORD'],
]);
$redis->set('cache:page:home', $html);
```

If `[kv] secret` is unset, no env vars are injected and the RESP listener treats the connection as the global store — fine for single-site mode, never use that combination with `sites_dir` set.

### Automatic value compression

`ephpm_kv_set()` (and the RESP `SET` family) auto-compress values according to the global `[kv]` block — `compression = "gzip" | "brotli" | "zstd"` plus `compression_level` and `compression_min_size`. Values smaller than `compression_min_size` are stored raw. `ephpm_kv_get()` transparently decompresses, so PHP code only ever sees the original bytes regardless of how the value was stored. Mixed compression settings during the lifetime of a store are safe — each entry remembers whether it was compressed when it was written. See [Configuration reference](/reference/config/) for the exact knobs.

## Common patterns

### Cache-aside

```php
$key = "cache:user:{$id}";
$cached = ephpm_kv_get($key);
if ($cached === null) {
    $cached = expensive_lookup($id);
    ephpm_kv_set($key, json_encode($cached));
    ephpm_kv_expire($key, 5 * 60 * 1000);   // 5 minutes
}
return json_decode($cached, true);
```

### Token-bucket rate limit

```php
$key   = "ratelimit:{$ip}";
$count = ephpm_kv_incr($key, 1);
if ($count === 1) {
    ephpm_kv_expire($key, 60_000);          // first request opens a 60s window
}
return $count <= $max_per_minute;
```

### Session storage

```php
ephpm_kv_set("session:{$id}", json_encode($data));
ephpm_kv_expire("session:{$id}", 3600 * 1000);  // 1 hour
```

## Configuration

```toml
[kv]
memory_limit = "256MB"
eviction_policy = "allkeys-lru"   # or noeviction / volatile-lru / allkeys-random
compression = "none"              # or gzip / brotli / zstd
compression_level = 6
compression_min_size = 1024       # bytes — values below this are stored raw

[kv.redis_compat]
enabled = false                   # turn on the RESP listener
listen = "127.0.0.1:6379"
# password = "..."                # AUTH required when set
```

See [Configuration reference](/reference/config/) for every key.

## See also

- [`ephpm kv` CLI](/reference/cli/kv/) — debug the live store
- [KV store architecture](/architecture/kv-store/) — how it works under the hood
- Examples in the repo: [`examples/kv-sapi-basic.php`](https://github.com/ephpm/ephpm/blob/main/examples/kv-sapi-basic.php), [`examples/kv-redis-predis.php`](https://github.com/ephpm/ephpm/blob/main/examples/kv-redis-predis.php)

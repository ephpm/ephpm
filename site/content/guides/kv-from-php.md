+++
title = "KV from PHP"
weight = 6
+++

ePHPm's built-in KV store is reachable two ways from PHP:

1. **SAPI functions** — `ephpm_kv_*` calls into the embedded store directly: an in-process function call with no socket and no serialization. Measured: **~100–220 ns per op for small values** (64 B `get` ≈ 116 ns, `set` ≈ 160 ns, `incr` ≈ 162 ns), rising to ~1–2 µs at 64 KB where the value copy dominates.
2. **RESP protocol** — any Redis client (Predis, phpredis) connects to the embedded RESP listener: one loopback TCP round trip per operation. Measured: **~80–110 µs per round trip** (c=1, raw socket), nearly independent of value size — client libraries add their own overhead on top, and bare-metal loopback can be faster than the bench box.

The store is the same in both cases; only the path differs — by roughly **500×**, because the SAPI path skips the network stack entirely. The numbers are medians from the lab's `kv-micro` suite ([ephpm/lab](https://github.com/ephpm/lab), `kv/`, results in `docs/kv-micro-v070.md`), measured 2026-08-17 against a from-source v0.7.0 build (PHP 8.5 ZTS, WSL2, compression off). Use SAPI for hot paths; use RESP when you need portability or Redis-style commands.

## SAPI functions

No external dependency. Available as native PHP functions whenever PHP is running inside ePHPm.

```php
ephpm_kv_set("greeting", "hello");
$greeting = ephpm_kv_get("greeting");          // "hello"
$missing  = ephpm_kv_get("nope");              // null

ephpm_kv_exists("greeting");                   // true  (bool, not int)
ephpm_kv_del("greeting");                      // 1     (int — keys deleted)
ephpm_kv_exists("greeting");                   // false

// setnx is the atomic check-and-set the PHP lock libraries build on
// (Laravel Cache::lock, Symfony LockFactory)
ephpm_kv_setnx("lock:job", "owner-1");         // true if it did not exist

// Counters — ephpm_kv_incr takes exactly one argument and always adds 1;
// use ephpm_kv_incr_by for arbitrary deltas
ephpm_kv_incr("page:views");                   // 1
ephpm_kv_incr_by("page:views", 5);             // 6

// TTL (in seconds)
ephpm_kv_set("session:abc", "data");
ephpm_kv_expire("session:abc", 60);            // 60 seconds; returns bool
ephpm_kv_ttl("session:abc");                   // ~60      (seconds)
ephpm_kv_pttl("session:abc");                  // ~60000   (milliseconds)
// both return -1 if no expiry, -2 if missing

ephpm_kv_decr("page:views");
ephpm_kv_flush_all();                          // empties the store
```

The full set of SAPI functions: `ephpm_kv_get`, `ephpm_kv_set`,
`ephpm_kv_setnx`, `ephpm_kv_del`, `ephpm_kv_exists`, `ephpm_kv_incr`,
`ephpm_kv_decr`, `ephpm_kv_incr_by`, `ephpm_kv_expire`, `ephpm_kv_ttl`,
`ephpm_kv_pttl`, `ephpm_kv_flush_all`, `ephpm_kv_wait`.

### Blocking waits — `ephpm_kv_wait()`

```php
ephpm_kv_wait(string $key, int $last_version, int $timeout_ms): array|false
```

Blocks the calling PHP thread until `$key` is written again (its watch
version exceeds `$last_version`) or `$timeout_ms` elapses. On change it
returns `['value' => string|null, 'version' => int]` — `value` is `null`
when the key was deleted or expired. On timeout it returns `false`.

Semantics you can rely on:

- **Versions are per-key and monotonic** for the process lifetime. They
  only advance for writes made *after* the first wait on that key, so
  always start the protocol with `$last_version = 0`: that first call
  registers the watch and returns the current value + version
  immediately (a race-free snapshot), without blocking.
- **What bumps the version:** `set`, `setnx` (on insert), `del`, `incr`
  / `decr` / `incr_by`, `append` (via RESP), expiry reaping, and
  `flush_all`. TTL-only changes (`expire`) and hash-field ops (RESP
  `HSET`/`HDEL`) do **not**.
- **Negative arguments clamp to 0**; `$timeout_ms = 0` is a
  non-blocking poll.
- **Cost when unused: zero.** Writes pay a single atomic load until the
  first `ephpm_kv_wait()` in the process; only writes to a *watched* key
  pay a version bump + wakeup. Watch slots are never reclaimed, so wait
  on a small set of well-known channel keys, not on unbounded
  per-request keys.

The intended use is worker-mode SSE fan-out — replacing a
poll-and-`usleep()` loop with zero idle CPU and sub-millisecond wakeup:

```php
// One SSE connection: snapshot once, then block until state changes.
$r = ephpm_kv_wait('board:state', 0, 0);        // register + snapshot
send_event(render($r['value']));
$ver = $r['version'];
while (true) {
    $r = ephpm_kv_wait('board:state', $ver, 15000);
    if ($r === false) { send_keepalive(); continue; }   // timeout tick
    $ver = $r['version'];
    send_event(render($r['value']));
}
```

Blocking is safe in worker mode (the wait parks that connection's
dedicated worker thread — which is exactly what a poll loop did, minus
the burn). In fpm mode the whole request runs under
`[server.timeouts] request` (default 300 s), so keep `$timeout_ms` well
below that.

### When to use SAPI

- High-frequency operations (logging, hit counters, rate limit hot path)
- Simple key/value patterns
- Realtime push (SSE) via `ephpm_kv_wait` — there is no RESP equivalent
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
| Hashes | `HSET`, `HGET`, `HDEL`, `HGETALL`, `HKEYS`, `HVALS`, `HLEN`, `HEXISTS` |
| Connection | `PING`, `ECHO`, `SELECT`, `QUIT`, `COMMAND`, `INFO`, `AUTH` |

Not implemented: lists, sets, transactions, `SCAN`, pub/sub. ePHPm targets the cache + counter + session use case — if you need full Redis, run actual Redis.

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
    ephpm_kv_expire($key, 300);             // 5 minutes (seconds)
}
return json_decode($cached, true);
```

### Token-bucket rate limit

```php
$key   = "ratelimit:{$ip}";
$count = ephpm_kv_incr($key);
if ($count === 1) {
    ephpm_kv_expire($key, 60);              // first request opens a 60s window
}
return $count <= $max_per_minute;
```

### Session storage

```php
ephpm_kv_set("session:{$id}", json_encode($data));
ephpm_kv_expire("session:{$id}", 3600);         // 1 hour (seconds)
```

## Shutdown functions and destructors

**Do not put KV calls in `register_shutdown_function()` callbacks or in
`__destruct()`.** ePHPm keeps one long-lived SAPI request open per worker
thread, so a shutdown function does not run at the end of *your* request —
it runs when the worker thread itself retires, by which point that thread's
per-request KV state is gone.

Calls made from there fail closed rather than crashing the server: a `get`
reports a miss, a `set` returns `false`, and in multi-tenant mode nothing
falls back to another site's keyspace. Do your KV work inside the request.

The same rule applies to [`ephpm_db_*`](/guides/db-from-php/), which throws
a distinct exception in that situation.

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
- [PHP packages](/reference/php-packages/) — PSR-6/PSR-16 caches, WordPress/Laravel/Symfony adapters, and a session handler built on these functions
- [Database from PHP](/guides/db-from-php/) — the sibling `ephpm_db_*` functions
- Examples in the repo: [`examples/kv-sapi-basic.php`](https://github.com/ephpm/ephpm/blob/main/examples/kv-sapi-basic.php), [`examples/kv-redis-predis.php`](https://github.com/ephpm/ephpm/blob/main/examples/kv-redis-predis.php)

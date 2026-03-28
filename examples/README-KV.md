# ePHPm KV Store Examples

The embedded KV store provides two ways to access cached data:

1. **SAPI Functions** — Direct, zero-copy access to the store
2. **Redis Protocol** — Standard Redis clients (Predis, phpredis, etc.)

## SAPI Functions (Built-in)

Use the native `ephpm_kv_*` PHP functions for the fastest, most direct access. No external dependencies needed.

### Example: `kv-sapi-basic.php`

```bash
# Build with KV store enabled
cargo xtask release

# Run the server (with KV store)
./target/release/ephpm

# In another terminal, run the example
php examples/kv-sapi-basic.php
```

**API:**

- `ephpm_kv_set(key, value)` — Store a value
- `ephpm_kv_get(key)` — Retrieve a value (returns `null` if missing)
- `ephpm_kv_del(key)` — Delete a key (returns 1 if found, 0 if not)
- `ephpm_kv_exists(key)` — Check if key exists (returns 1 or 0)
- `ephpm_kv_incr(key, delta)` — Increment a numeric value
- `ephpm_kv_expire(key, ttl_ms)` — Set TTL in milliseconds
- `ephpm_kv_pttl(key)` — Get remaining TTL in milliseconds (-1 = no expiry, -2 = missing)

**Pros:**
- Zero-copy, direct access to embedded store
- No serialization overhead
- No external dependencies
- Lowest latency

**Cons:**
- Limited to basic string operations
- ePHPm-specific (not portable to other PHP servers)

## Redis Protocol (Predis)

Connect to the embedded KV store using any Redis client. The store speaks the Redis RESP2 protocol.

### Example: `kv-redis-predis.php`

```bash
# Install Predis
composer require predis/predis

# Build and run the server
cargo xtask release
./target/release/ephpm

# In another terminal, run the example
php examples/kv-redis-predis.php
```

**Why use this approach?**

- Familiar Redis commands and patterns
- Works with standard PHP Redis libraries (Predis, phpredis, etc.)
- Portable — your code works on Redis, Memcached, or ePHPm
- Rich data structures: strings, hashes, lists, sets

**Supported Commands:**

| Group | Commands |
|-------|----------|
| **Strings** | GET, SET, SETEX, INCR, DECR, APPEND, STRLEN, GETRANGE, SETRANGE |
| **Hashes** | HGET, HSET, HMGET, HMSET, HGETALL, HDEL, HEXISTS, HINCRBY, HLEN |
| **Lists** | LPUSH, RPUSH, LPOP, RPOP, LLEN, LRANGE, LINDEX, LSET, LTRIM |
| **Sets** | SADD, SREM, SMEMBERS, SISMEMBER, SCARD, SINTER, SUNION, SDIFF |
| **Keys** | DEL, EXISTS, EXPIRE, TTL, KEYS, SCAN, TYPE, RENAME |
| **Transactions** | MULTI, EXEC, DISCARD, WATCH |
| **Server** | PING, ECHO, SELECT, FLUSHDB, FLUSHALL, DBSIZE |

## Configuration

The KV store is configured in `ephpm.toml`:

```toml
[kv]
# Enable the KV store (default: true)
enabled = true

# Listen address for Redis protocol
listen = "127.0.0.1:6379"

# Max input buffer per connection (protects against huge payloads)
max_input_buffer = 67108864  # 64 MiB

# Store settings
[kv.store]
# Memory limit (0 = unlimited)
max_memory = 0

# Eviction policy: "noeviction", "allkeys-lru", "volatile-lru", etc.
eviction_policy = "allkeys-lru"

# Approximate memory check interval (milliseconds)
memory_check_interval = 1000

# Expiry scan interval (cleanup expired keys)
expiry_check_interval = 100
```

## When to Use Each

### Use SAPI Functions when:

- You need the absolute lowest latency
- You're working only with string values
- You want zero external dependencies
- You're not concerned about portability

**Example:** Session storage, page view counters, temporary cache

### Use Redis Protocol when:

- You need complex data structures (hashes, lists, sets)
- You want to use existing Redis libraries
- You plan to swap the backend (Redis ↔ ePHPm)
- You're building patterns like rate limiting, queues, leaderboards

**Example:** Shopping carts, job queues, user sessions, rankings

## Performance Notes

- **SAPI**: ~100 nanoseconds per operation (in-process)
- **Redis protocol**: ~10-100 microseconds per operation (network round-trip)

For high-frequency operations (logging, counters), prefer SAPI. For complex data structures and portability, use Redis.

## Common Patterns

### Caching

**SAPI:**
```php
$cached = ephpm_kv_get("cache:user:$id");
if ($cached === null) {
    $cached = expensive_function($id);
    ephpm_kv_set("cache:user:$id", $cached);
    ephpm_kv_expire("cache:user:$id", 5 * 60 * 1000); // 5 minutes
}
```

**Redis:**
```php
$cached = $redis->get("cache:user:$id");
if ($cached === null) {
    $cached = expensive_function($id);
    $redis->setex("cache:user:$id", 300, $cached);
}
```

### Rate Limiting

**SAPI:**
```php
$key = "ratelimit:$user_id";
$count = ephpm_kv_incr($key, 1);
if ($count === 1) {
    ephpm_kv_expire($key, 60 * 1000); // 1 minute window
}
return $count <= $max_requests;
```

**Redis:**
```php
$key = "ratelimit:$user_id";
$count = $redis->incr($key);
if ($count === 1) {
    $redis->expire($key, 60);
}
return $count <= $max_requests;
```

### Session Storage

**SAPI:**
```php
ephpm_kv_set("session:$id", json_encode($data));
ephpm_kv_expire("session:$id", 3600 * 1000); // 1 hour
```

**Redis:**
```php
$redis->setex("session:$id", 3600, json_encode($data));
```

## See Also

- [Architecture docs](../docs/architecture/db-proxy.md)
- [KV store integration tests](../crates/ephpm/tests/kv_sapi_integration.rs)
- [Predis documentation](https://github.com/predis/predis)
- [Redis command reference](https://redis.io/commands)

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
- Simple key-value string storage with expiration and counters

**Supported Commands:**

| Group | Commands |
|-------|----------|
| **Strings** | GET, SET, SETEX, MGET, MSET, SETNX, INCR, DECR, INCRBY, DECRBY, APPEND, STRLEN, GETSET |
| **Keys** | DEL, EXISTS, EXPIRE, PEXPIRE, PERSIST, TTL, PTTL, TYPE, KEYS, DBSIZE, FLUSHDB, FLUSHALL, RENAME |
| **Connection** | PING, ECHO, SELECT, QUIT, COMMAND, INFO |

**Not Yet Implemented:**

Hashes, Lists, Sets, Transactions, SCAN — these would require architectural changes (multi-type store, per-connection state). If you need complex data structures, use a real Redis server. For ePHPm's use case (session caching, counters), strings with TTL cover 99% of patterns.

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

- You want to use existing Redis client libraries (Predis, phpredis)
- You plan to swap the backend (Redis ↔ ePHPm)
- You're building string-based patterns: rate limiting, page counters, session tags
- You prefer familiar Redis command syntax

**Example:** Rate limiting, page view counters, simple session storage, cache tags

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

## CLI Debugging Commands

The `ephpm kv` subcommand provides a debugging interface to inspect and manipulate the live KV store without writing PHP code.

### Basic Usage

```bash
# Test connection
ephpm kv ping

# Get a value
ephpm kv get mykey

# Set a value
ephpm kv set mykey "hello world"

# Set with TTL (in seconds)
ephpm kv set session:abc 'data' --ttl 3600

# Increment a counter
ephpm kv incr page:views
ephpm kv incr counter --by 5

# Check TTL
ephpm kv ttl mykey

# List keys
ephpm kv keys "*"
ephpm kv keys "session:*"

# Delete keys
ephpm kv del mykey
ephpm kv del key1 key2 key3
```

### Useful Patterns

```bash
# Debug rate limiting
ephpm kv get "ratelimit:$user_id"
ephpm kv incr "ratelimit:$user_id"

# Check session data
ephpm kv get "session:$session_id"

# Monitor cache hit/miss
ephpm kv get "cache:page:/$path"

# Find all active sessions
ephpm kv keys "session:*"

# Clear all sessions
ephpm kv del $(ephpm kv keys "session:*" | awk '{print $NF}')
```

### Connecting to Remote Server

```bash
# Connect to a different host/port
ephpm kv --host 10.0.1.5 --port 6379 ping

# Override defaults
ephpm kv --host redis.example.com --port 6380 keys "*"
```

See [docs/architecture/cli.md](../docs/architecture/cli.md) for full CLI documentation.

## See Also

- [Architecture docs](../docs/architecture/db-proxy.md)
- [KV store integration tests](../crates/ephpm/tests/kv_sapi_integration.rs)
- [Predis documentation](https://github.com/predis/predis)
- [Redis command reference](https://redis.io/commands)

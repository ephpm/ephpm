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

**Not under `ephpm php`.** The CLI runs PHP without starting the server, so no KV store is attached: the functions exist but every call returns its failure value (`null` / `false` / `0` / `-2`) and the first call emits an `E_WARNING` saying so. To reach a running server's KV from a script, use its RESP listener (`[kv.redis_compat]`) with any Redis client.

```php
ephpm_kv_set("greeting", "hello");
$greeting = ephpm_kv_get("greeting");          // "hello"
$missing  = ephpm_kv_get("nope");              // null

ephpm_kv_exists("greeting");                   // true  (bool, not int)
ephpm_kv_del("greeting");                      // 1     (int — keys deleted)
ephpm_kv_exists("greeting");                   // false

// setnx is the atomic check-and-set the PHP lock libraries build on
// (Laravel Cache::lock, Symfony LockFactory)
ephpm_kv_setnx("lock:job", "owner-1", 30);     // true if it did not exist (30s TTL)

// del_if_eq is the atomic compare-and-delete that makes the *release* safe:
// it deletes the key only while its value is still your token, so a holder
// that overran its TTL cannot delete the new holder's lock.
ephpm_kv_del_if_eq("lock:job", "owner-1");     // true only if still "owner-1"

// Counters — ephpm_kv_incr takes exactly one argument and always adds 1;
// use ephpm_kv_incr_by for arbitrary deltas
ephpm_kv_incr("page:views");                   // 1
ephpm_kv_incr_by("page:views", 5);             // 6

// TTL (in seconds) — pass it as set()'s third argument so the value and
// its expiry land in one atomic write
ephpm_kv_set("cache:abc", "data", 60);         // expires in 60 seconds
ephpm_kv_expire("cache:abc", 120);             // change the TTL of an existing key; returns bool
ephpm_kv_ttl("cache:abc");                     // ~120     (seconds)
ephpm_kv_pttl("cache:abc");                    // ~120000  (milliseconds)
// both return -1 if no expiry, -2 if missing

ephpm_kv_decr("page:views");
ephpm_kv_flush_all();                          // empties this site's store (see below)
```

### Function reference

| Function | Returns | Notes |
|---|---|---|
| `ephpm_kv_get(string $key)` | `?string` | `null` on a miss. |
| `ephpm_kv_set(string $key, string $value, int $ttl = 0)` | `bool` | `$ttl` in seconds; `0` or negative means no expiry. |
| `ephpm_kv_setnx(string $key, string $value, int $ttl = 0)` | `bool` | `true` only if the key did not exist (atomic; in a cluster decided by the key's owner node, see [Locks in a cluster](#locks-in-a-cluster)). Same `$ttl` rules as `set`. |
| `ephpm_kv_del(string $key)` | `int` | Number of keys deleted (`0` or `1`). |
| `ephpm_kv_del_if_eq(string $key, string $token)` | `bool` | Compare-and-delete — see below. |
| `ephpm_kv_exists(string $key)` | `bool` | |
| `ephpm_kv_incr(string $key)` / `ephpm_kv_decr(string $key)` | `int\|false` | Adds / subtracts 1. A missing key starts at 0. |
| `ephpm_kv_incr_by(string $key, int $delta)` | `int\|false` | `$delta` may be negative. |
| `ephpm_kv_expire(string $key, int $ttl)` | `bool` | `$ttl` in seconds. `false` if the key is missing. On a hash it sets the whole-key expiry. |
| `ephpm_kv_ttl(string $key)` | `int` | Seconds remaining, rounded **up** (1–999 ms reads as `1`). `-1` = no expiry, `-2` = missing. A hash reports its whole-key expiry. |
| `ephpm_kv_pttl(string $key)` | `int` | Milliseconds remaining. `-1` / `-2` as above. |
| `ephpm_kv_persist(string $key)` | `bool` | Removes the expiry (string or hash key). `true` only if there was one. *v0.13+* |
| `ephpm_kv_flush_all()` | `bool` | Empties the store this request is scoped to. |
| `ephpm_kv_wait(string $key, int $last_version, int $timeout_ms)` | `array\|false` | Blocking watch — see below. |
| `ephpm_kv_mget(array $keys)` | `array\|false` | A **list** aligned with `$keys`: each element `string` or `null` (missing). One call for any number of keys. *v0.13+* |
| `ephpm_kv_mset(array $values, int $ttl = 0)` | `bool` | `[key => value]`, one `$ttl` (seconds) for all. Values are converted to string; an int key is written as its decimal string. Not atomic under the memory limit: a refusal stops the write and returns `false` with earlier keys stored. *v0.13+* |
| `ephpm_kv_getdel(string $key)` | `?string` | Returns the value and deletes the key, as one atomic step. *v0.13+* |
| `ephpm_kv_getex(string $key, int $ttl)` | `?string` | Returns the value and sets its TTL: `$ttl > 0` seconds, `0` removes the TTL. `null` (nothing changed) if missing; a negative `$ttl` warns and returns `null`. *v0.13+* |
| `ephpm_kv_set_if_eq(string $key, string $expected, string $value, int $ttl = 0)` | `bool` | Compare-and-set — see [Extending a lock](#extending-a-lock-ephpm-kv-set-if-eq). `$ttl`: `> 0` new TTL, `0` none, `-1` keep the current one. *v0.13+* |
| `ephpm_kv_append(string $key, string $value)` | `int\|false` | New length. Creates the key if missing. *v0.13+* |
| `ephpm_kv_strlen(string $key)` | `int` | Length of the value, `0` if missing. *v0.13+* |
| `ephpm_kv_type(string $key)` | `string` | `"string"`, `"hash"` or `"none"`. *v0.13+* |
| `ephpm_kv_scan(int $cursor, ?string $match = null, int $count = 10)` | `array\|false` | One step of a key scan: `[int $next_cursor, array $keys]` — see [Iterating keys](#iterating-keys-ephpm-kv-scan). *v0.13+* |
| `ephpm_kv_hget(string $key, string $field)` | `?string` | `null` if the hash or field is missing. *v0.13+* |
| `ephpm_kv_hset(string $key, string\|array $field, ?string $value = null)` | `int\|false` | `hset($k, $f, $v)` → `1` created / `0` overwritten. `hset($k, [$f => $v, ...])` → fields created. Clears each written field's TTL. *v0.13+* |
| `ephpm_kv_hsetnx(string $key, string $field, string $value)` | `bool` | `true` only if the field did not exist. *v0.13+* |
| `ephpm_kv_hmget(string $key, array $fields)` | `array\|false` | A **list** aligned with `$fields` (`string` or `null` each). *v0.13+* |
| `ephpm_kv_hgetall(string $key)` | `array\|false` | `[field => value]`; `[]` for a missing hash. A numeric field name becomes an int array key, as in any PHP array. *v0.13+* |
| `ephpm_kv_hkeys(string $key)` / `ephpm_kv_hvals(string $key)` | `array\|false` | Field names / values, as lists. *v0.13+* |
| `ephpm_kv_hdel(string $key, string\|array $fields)` | `int` | Fields removed. Deleting the last field deletes the hash. *v0.13+* |
| `ephpm_kv_hexists(string $key, string $field)` | `bool` | *v0.13+* |
| `ephpm_kv_hlen(string $key)` | `int` | Live fields, `0` if missing. *v0.13+* |
| `ephpm_kv_hincr_by(string $key, string $field, int $delta = 1)` | `int\|false` | `false` when the field is not an integer or the result overflows. Keeps the field's TTL. *v0.13+* |
| `ephpm_kv_hexpire(string $key, string\|array $fields, int $ttl)` | `int\|false` | Gives each field a TTL of `$ttl` seconds; returns how many fields got it (missing fields are skipped). `$ttl <= 0` returns `false`, like `expire`. *v0.13+* |
| `ephpm_kv_httl(string $key, string $field)` / `ephpm_kv_hpttl(...)` | `int` | A field's remaining TTL in seconds (rounded up) / milliseconds; `-1` no TTL, `-2` no such field. *v0.13+* |
| `ephpm_kv_hpersist(string $key, string $field)` | `bool` | Removes a field's TTL; `true` only if it had one. *v0.13+* |

Functions marked *v0.13+* do not exist on older ePHPm. A library that must
run on several versions should check `function_exists('ephpm_kv_mget')` (and
so on) and keep a fallback; the [ePHPm PHP packages](/reference/php-packages/)
do exactly that.

Edge cases worth knowing:

- **`ephpm_kv_expire($key, 0)` (or a negative TTL) returns `false` and leaves
  the key untouched.** This differs from Redis, where a non-positive
  `EXPIRE` deletes the key. Use `ephpm_kv_del()` to remove a key.
- **TTLs are whole seconds.** `set`, `setnx` and `expire` take seconds, so
  there is no sub-second expiry from PHP. `pttl` only *reads* milliseconds.
- **`incr` / `decr` / `incr_by` return `false`** when the stored value is not
  an integer.
- **Keys must be valid UTF-8; values are binary-safe.** Keys are passed with
  their length, so a NUL byte is an ordinary character (`"a\0b"` and `"a\0c"`
  are two keys). A key that is not valid UTF-8, such as raw
  `md5($x, true)` output, is refused: the call raises an `E_WARNING` and
  returns its failure value (`null` from `get`, `false` from writes, `0` from
  `del`, `-2` from `ttl`/`pttl`). Use a text form such as `md5($x)` or
  `bin2hex()` instead. Unlike Redis, the store cannot hold binary keys. Values
  may hold any bytes.
- **An out-of-range TTL is refused.** A `$ttl` so large that the expiry time
  overflows a 64-bit millisecond count (for example `PHP_INT_MAX`) makes
  `set`, `setnx` and `expire` raise an `E_WARNING` and return `false`.
- **`ephpm_kv_flush_all()` is per-site in multi-tenant mode.** With
  `[server] sites_dir` set, it empties only the calling virtual host's
  keyspace, never another tenant's. In single-site mode it empties the whole
  store.
- **No store means silent misses, not errors.** When no KV store is wired to
  PHP, nothing throws: `get`/`getdel`/`getex`/`hget` return `null`, `set` /
  `setnx` / `exists` / `expire` / `persist` / `del_if_eq` / `set_if_eq` /
  `flush_all` / `mset` / `hsetnx` / `hexists` / `hpersist` return `false`,
  `incr*`/`append`/`hset`/`hincr_by`/`hexpire` return `false`, the
  array-returning functions (`mget`, `hmget`, `hgetall`, `hkeys`, `hvals`,
  `scan`) return `false`, `del`/`hdel`/`hlen`/`strlen` return `0`,
  `ttl`/`pttl`/`httl`/`hpttl` return `-2`, `type` returns `"none"`, and `wait`
  returns `false`. The server always wires one, so in practice this is what
  [`ephpm php`](#under-ephpm-php-the-cli) sees.
- **Hash field names follow the key rules** (UTF-8, NUL allowed); a bad field
  name is refused with an `E_WARNING`, like a bad key. Array arguments
  (`mget`, `mset`, `hset`, `hmget`, `hdel`, `hexpire`) are checked in full
  before anything is written, so one bad key writes nothing.

### Under `ephpm php` (the CLI)

The `ephpm_kv_*` functions are *defined* under `ephpm php` (so code that
calls them still compiles and runs), but the CLI does not connect to a KV
store, not even with `--site`. Every call takes the "no store" path above:
reads miss and writes are dropped without an error. A `wp` or `artisan`
command run through `ephpm php` with a KV-backed cache drop-in therefore
reads nothing from, and flushes nothing in, the running server's cache. Use
[`ephpm kv`](/reference/cli/kv/) to inspect or change the live store from
the command line.

### Safe lock release — `ephpm_kv_del_if_eq()`

```php
ephpm_kv_del_if_eq(string $key, string $token): bool
```

Atomically deletes `$key` **only while** its stored value equals `$token`
(a compare-and-delete), returning `true` if this call removed it and
`false` otherwise. The compare and the delete happen under the same
per-key shard lock, so a value written by someone else between your read
and your delete is never destroyed.

This is the delete-side companion to `ephpm_kv_setnx()` and the primitive
for **correct lock release**. Acquire with a unique token and a TTL, then
release with the same token:

```php
$token = bin2hex(random_bytes(16));
if (ephpm_kv_setnx("lock:job", $token, 30)) {   // 30s TTL guards a crash
    try {
        // ... critical section ...
    } finally {
        ephpm_kv_del_if_eq("lock:job", $token);  // releases only if still ours
    }
}
```

Because the release fires only while the value is still *your* token, a
holder that overran the TTL — and whose lock was therefore taken over by
another request — can no longer delete the **new** holder's lock. A plain
`ephpm_kv_del()` release cannot make that distinction and will happily
unlock a lock it no longer owns.

Return values, precisely: `true` when the key was present, matched, and
was deleted; `false` when the token differs, the key is absent, or the key
has expired (an expired entry is treated as absent). Tokens are compared
byte-exactly and may be binary.

### Extending a lock — `ephpm_kv_set_if_eq()`

```php
ephpm_kv_set_if_eq(string $key, string $expected, string $value, int $ttl = 0): bool
```

Compare-and-set (Valkey's `SET key value IFEQ expected`): replaces the value
**only while** it currently equals `$expected`, under the same per-key shard
lock as the compare. It never creates the key. `$ttl` is the new expiry in
seconds; `0` removes the expiry and `-1` keeps the current one.

The three primitives together make a complete lock — acquire, extend while
working, release — none of which can touch a lock another holder has taken
over:

```php
$token = bin2hex(random_bytes(16));
if (ephpm_kv_setnx("lock:job", $token, 30)) {                // acquire, 30 s lease
    try {
        foreach ($batches as $batch) {
            process($batch);
            if (!ephpm_kv_set_if_eq("lock:job", $token, $token, 30)) {
                throw new RuntimeException("lost the lock");  // someone else holds it now
            }
        }
    } finally {
        ephpm_kv_del_if_eq("lock:job", $token);               // release only if still ours
    }
}
```

Extending with `ephpm_kv_expire()` instead would extend whoever holds the lock
*now* — including a new holder after your lease ran out.

### Locks in a cluster

With `[cluster]` enabled, `ephpm_kv_setnx()`, `ephpm_kv_set_if_eq()` and
`ephpm_kv_del_if_eq()` (and their RESP forms `SETNX`, `SET … NX`,
`SET … IFEQ`, `DELIFEQ`) are **routed to the key's owner node** — `hash(key) % sorted alive nodes`, the same owner the
large-value tier uses — and decided there under that node's shard lock. Every
node's attempt on a key is therefore serialized by one lock on one node, and
two nodes cannot both acquire it. (Before this, each node decided on its own
copy and both could win.)

The guarantee, stated precisely:

- **It holds while membership is stable.** Each node computes the owner from
  its own view of cluster membership. Across a membership change — a node
  joining or leaving, or the seconds before gossip declares a dead node dead —
  two nodes can briefly name different owners and both grant the same lock.
  A network partition is worse than brief: each side drops the other from its
  membership and grants locks on its own until the partition heals.
- **A new owner learns about existing locks by replication.** If ownership
  moves, the new owner knows about a lock its predecessor granted only once
  that grant has replicated to it (gossip; typically 1–3 seconds, longer in
  large clusters). A grant that had not replicated when its owner died, or
  when the owner restarted (a restarted node comes back with an empty store),
  is lost, and the lock can be granted again.
- **Lock values must fit the gossip tier.** Only values at or under
  `[cluster.kv] small_key_threshold` (default 512 bytes) replicate to every
  node. A larger lock value lives on `replication_factor` nodes only, and a new
  owner outside that set never learns about the lock. Keep lock tokens short
  (a 32-character hex token is plenty).
- **Only `setnx`, `set_if_eq` and `del_if_eq` are arbitrated.**
  `ephpm_kv_set()`, `ephpm_kv_expire()`, `ephpm_kv_del()` and `incr` on a lock
  key (and their RESP forms) are still decided on the calling node and
  replicated last-writer-wins. They can overwrite an owner-granted lock with a
  stale local value — for example, extending a lease with `ephpm_kv_expire()`
  on a node whose copy still holds a previous holder's token re-publishes that
  old token cluster-wide. Acquire with `setnx`, extend with `set_if_eq`, and
  release with `del_if_eq` only.
- **An unreachable owner fails closed, with an unknown outcome.** If the owner
  does not answer within 2 seconds, the call reports failure:
  `ephpm_kv_setnx()`, `ephpm_kv_set_if_eq()` and `ephpm_kv_del_if_eq()` return
  `false`, and the RESP commands answer `-CLUSTERDOWN …` (never `:0`/`nil`, which would
  claim the key exists or the token did not match). Nothing was written on the
  calling node, but a request that timed out may still have been applied on
  the owner. From PHP, `ephpm_kv_setnx()` returns `false` both when the key
  exists and when the owner is unreachable, so you cannot tell the two
  apart; use a unique token per attempt and always set a TTL, so a grant
  that landed after a timeout either expires or can be released with
  `del_if_eq` and that token. A dead owner stops being the owner once gossip
  declares it dead.
- **The caller's own copy follows by replication.** A routed `setnx()` win is
  visible to `ephpm_kv_get()` on the calling node once the owner's write has
  replicated back, not synchronously.
- **Rolling upgrades:** a node running a version without owner routing cannot
  answer a routed request, so lock keys it owns are refused (fail closed) until
  it is upgraded. It also still decides its own `setnx` calls locally and
  replicates the result last-writer-wins, so there is no mutual exclusion
  against not-yet-upgraded nodes until every node runs this version.

This is single-arbiter routing, not consensus. If you need mutual exclusion
that survives membership changes, fence the protected resource itself (for
example with a monotonically increasing token checked by the database).
An owner running a version without `set_if_eq` routing drops the request,
which reads as unreachable: `set_if_eq` returns `false` until it is upgraded.

### Hashes and per-field TTL

A hash keeps several fields under one key, each with its own value and —
optionally — its own TTL. It is the natural shape for an object's attributes,
a per-user set of flags, or a group of short-lived entries that should also be
deletable together:

```php
ephpm_kv_hset("user:42", ["name" => "Ada", "plan" => "pro"]);   // 2 (fields created)
ephpm_kv_hget("user:42", "name");                               // "Ada"
ephpm_kv_hmget("user:42", ["name", "email"]);                   // ["Ada", null]
ephpm_kv_hincr_by("user:42", "logins");                         // 1

// A field that expires on its own: a one-time code valid for 5 minutes.
ephpm_kv_hset("user:42", "otp", "839201");
ephpm_kv_hexpire("user:42", "otp", 300);                        // 1
ephpm_kv_httl("user:42", "otp");                                // ~300
ephpm_kv_hpersist("user:42", "otp");                            // true (TTL removed)

ephpm_kv_expire("user:42", 86400);                              // the whole hash, 1 day
ephpm_kv_del("user:42");                                        // every field at once
```

- An expired field is gone for every read the moment its TTL passes (`hget`
  returns `null`, `hgetall`/`hkeys`/`hlen` leave it out); its memory is
  reclaimed by the next write to the hash or by the background expiry cycle.
  A hash whose last field expires stops existing.
- `ephpm_kv_hset()` clears the TTL of each field it writes;
  `ephpm_kv_hincr_by()` keeps it. The hash key's own TTL (`ephpm_kv_expire`)
  and the field TTLs are independent.
- **In a cluster, hashes replicate per field** (since v0.13; before that they
  stayed on the node that wrote them). Writes to different fields on different
  nodes never overwrite each other. A field value is limited to
  `[cluster.kv] small_key_threshold` minus 17 bytes (495 bytes by default);
  `ephpm_kv_hset()` returns `false` for a larger one and writes nothing. Keep
  large values in string keys. See
  [KV RESP Protocol → Hashes in a cluster](/reference/kv-resp/#hashes-in-a-cluster).

### Batch reads and writes — `ephpm_kv_mget()` / `ephpm_kv_mset()`

Reading or writing many keys costs one call instead of one per key:

```php
[$a, $b, $missing] = ephpm_kv_mget(["cache:a", "cache:b", "cache:nope"]);  // $missing === null
ephpm_kv_mset(["cache:a" => "1", "cache:b" => "2"], 300);                    // both expire in 300 s
```

`mget` returns a list in the order of `$keys` (not keyed by key, so numeric
keys cannot be confused with positions). `mset` applies one TTL to every key;
use `ephpm_kv_set()` per key when they need different ones.

### Iterating keys — `ephpm_kv_scan()`

```php
$cursor = 0;
do {
    [$cursor, $keys] = ephpm_kv_scan($cursor, "session:*", 500);
    foreach ($keys as $key) {
        // ...
    }
} while ($cursor !== 0);
```

Start with cursor `0` and stop when the returned cursor is `0` again. Every key
that exists for the whole scan is returned **exactly once**, however much the
store is written meanwhile; a key created or deleted during the scan may or
may not be. `$match` is a Redis glob (`*`, `?`, `[abc]`). `$count` is a hint:
a step walks at least one whole internal shard, so a large store returns more
than `$count` keys per step. A name that is both a string and a hash appears
once per type.

In a cluster `ephpm_kv_scan()` lists **this node's** keyspace — every hash and
every string value at or under `small_key_threshold` is on every node, but a
larger string value only on its replica nodes. Do not use it to find every key
to delete cluster-wide (for example a cache group flush); key a group by a
version counter instead.

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
- **What bumps the version:** every write that changes the key's value or
  removes it — `set`, `mset`, `setnx` (on insert), `set_if_eq` (when it
  sets), `del`, `getdel`, `del_if_eq` (only when it actually deletes),
  `incr` / `decr` / `incr_by`, `append`, expiry reaping, `flush_all`, and
  (since v0.13) hash writes: a field set, incremented or deleted, or a field
  expiring away. On a hash key `value` is `null` (a hash has no string value;
  read it with `ephpm_kv_hgetall()`). TTL-only changes (`expire`, `persist`,
  `getex`, `hexpire`, `hpersist`) do **not** bump.
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
the burn). In per-request mode the whole request runs under
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
| Strings | `GET`, `SET` (`EX`/`PX`/`EXAT`/`PXAT`/`KEEPTTL`, `NX`/`XX`/`IFEQ`, `GET`), `SETEX`, `GETDEL`, `GETEX`, `MGET`, `MSET`, `SETNX`, `INCR`, `DECR`, `INCRBY`, `DECRBY`, `APPEND`, `STRLEN`, `GETSET` |
| Keys | `DEL`, `UNLINK`, `DELIFEQ`, `EXISTS`, `EXPIRE`, `PEXPIRE`, `EXPIREAT`, `PEXPIREAT`, `PERSIST`, `TTL`, `PTTL`, `EXPIRETIME`, `PEXPIRETIME`, `TYPE`, `KEYS`, `SCAN`, `DBSIZE`, `FLUSHDB`, `FLUSHALL`, `RENAME` |
| Hashes | `HSET`, `HMSET`, `HSETNX`, `HGET`, `HMGET`, `HDEL`, `HGETALL`, `HKEYS`, `HVALS`, `HLEN`, `HEXISTS`, `HSTRLEN`, `HINCRBY`, `HINCRBYFLOAT`, `HSCAN`, `HEXPIRE`, `HPEXPIRE`, `HEXPIREAT`, `HPEXPIREAT`, `HTTL`, `HPTTL`, `HEXPIRETIME`, `HPEXPIRETIME`, `HPERSIST` |
| Connection | `PING`, `ECHO`, `SELECT`, `QUIT`, `COMMAND`, `INFO`, `AUTH` |

`DELIFEQ key token` is an ePHPm-specific compare-and-delete (not a standard Redis command): it deletes `key` only while its value equals `token`, returning `:1`/`:0`. It is the safe lock-release primitive — reach it over RESP with a raw command (e.g. Predis `$redis->executeRaw(['DELIFEQ', $key, $token])`), or, from in-process PHP, just call `ephpm_kv_del_if_eq()` above. It is a dedicated command rather than a `DEL key IFEQ token` variant because `DEL` is variadic over keys, where a positional marker would be ambiguous with a key named `IFEQ`.

Not implemented: lists, sets, sorted sets, transactions (`MULTI`/`EXEC`), scripting (`EVAL`), pub/sub, and extras such as `INCRBYFLOAT`, `GETRANGE` and `MSETNX`. ePHPm targets the cache + counter + session use case — if you need full Redis, run actual Redis. Note that `INFO` reports `redis_version:7.0.0`, so clients that branch on the server version will assume Redis 7. The [KV RESP Protocol reference](/reference/kv-resp/) lists every command's behavior and where it differs from Redis.

### Multi-tenant note

The RESP listener can be shared across virtual hosts — each site is isolated by AUTH. When **all three** of `[kv.redis_compat] enabled = true`, `[kv] secret`, and `[server] sites_dir` are set, ePHPm derives a per-site password as `HMAC-SHA256(secret, site_key)` (lowercase hex, 64 chars) and injects four variables into every PHP request so the site's code can connect without any per-vhost configuration:

```
EPHPM_REDIS_HOST       # host part of [kv.redis_compat] listen, verbatim (0.0.0.0 stays 0.0.0.0)
EPHPM_REDIS_PORT
EPHPM_REDIS_USERNAME   # the canonical site key (e.g. alice-blog.com; "alice-blog" for
                       # alice-blog.localhost with sites_domain_suffix = ".localhost")
EPHPM_REDIS_PASSWORD   # HMAC-SHA256(secret, site key) hex
```

The site key is the vhost's canonical name: the `Host` header lowercased, with any port and trailing dot removed, and with `[server] sites_domain_suffix` stripped when one is configured. It matches the site's directory name under `sites_dir`. A `Host` that matches no site is not a tenant: it gets no KV keyspace and no `EPHPM_REDIS_*` variables. On a multi-tenant node, `ephpm_kv_*` calls from such a request return their failure value (`null`, `false`, `0` or `-2`) and raise one warning per request: `no per-site KV context for this request`. This matches how `ephpm_db_*` behaves. Without it, every distinct `Host` header would create its own KV store, which is never freed, so unauthenticated clients could grow memory without bound. A PHP session using the `ephpm` save handler in such a request starts empty and cannot be saved. To give the fallback site a keyspace, serve it from its own directory under `sites_dir`. Single-site nodes (no `sites_dir`) are unaffected and always use the one global store.

These are **`$_SERVER` entries, not process environment variables**: read `$_SERVER['EPHPM_REDIS_PASSWORD']`. `getenv()` returns `false` for them, because a process-wide environment would be shared by every tenant's threads.

The RESP server validates the incoming `AUTH <username> <password>` against the same derivation, so requests authenticated as `alice-blog.com` only see alice's `DashMap`. Bob's connection sees a separate one even though both hit the same TCP port. In this mode `[kv.redis_compat] password` is ignored.

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

If `[kv] secret` is unset, no variables are injected. In single-site mode the listener then serves the global store. With `sites_dir` set, that combination is refused at startup, because it would hand every tenant one shared, unauthenticated store.

**Single-site authentication.** With a plain `[kv.redis_compat] password`, configure clients with the password only and **no username**: the listener compares the first `AUTH` argument, so `AUTH <user> <pass>` fails. `[kv] secret` cannot protect a single-site listener, so the RESP listener on with `[kv] secret` set and no `password` is a startup error; set `password` instead. See [KV RESP Protocol → Authentication](/reference/kv-resp/#authentication).

### Automatic value compression

`ephpm_kv_set()` (and the RESP `SET` family) auto-compress values according to the global `[kv]` block — `compression = "gzip" | "brotli" | "zstd"` plus `compression_level` and `compression_min_size`. Values smaller than `compression_min_size` are stored raw. `ephpm_kv_get()` transparently decompresses, so PHP code only ever sees the original bytes regardless of how the value was stored. Mixed compression settings during the lifetime of a store are safe — each entry remembers whether it was compressed when it was written. See [Configuration reference](/reference/config/) for the exact knobs.

## Common patterns

### Cache-aside

```php
$key = "cache:user:{$id}";
$cached = ephpm_kv_get($key);
if ($cached === null) {
    $cached = json_encode(expensive_lookup($id));
    ephpm_kv_set($key, $cached, 300);       // 5 minutes (seconds), set atomically with the value
}
return json_decode($cached, true);
```

### Fixed-window rate limit

```php
$key   = "ratelimit:{$ip}";
$count = ephpm_kv_incr($key);
if ($count === 1) {
    ephpm_kv_expire($key, 60);              // first request opens a 60s window
}
return $count <= $max_per_minute;
```

`incr` has no TTL argument, so the window's expiry is a second call. If the
process dies between the two, the counter has no expiry; `ephpm_kv_ttl($key)
=== -1` detects that.

### Session storage

For PHP's own `$_SESSION`, use the native [`ephpm` session save
handler](/guides/sessions/) rather than hand-rolling it. For ad-hoc session
blobs:

```php
ephpm_kv_set("session:{$id}", json_encode($data), 3600);   // 1 hour (seconds)
```

## Shutdown functions and destructors

**Per-request mode (the default):** both work. `register_shutdown_function()`
callbacks run right after the script, before the response is captured;
global objects' `__destruct()` runs in `php_request_shutdown()` right after
the response is sent, on the same thread and still scoped to the request's
own keyspace. (Before v0.11.3 that teardown waited for the thread's next
request.) Output from a destructor is discarded.

**Worker mode:** a worker keeps one long-lived SAPI request open per thread,
so shutdown functions and end-of-script destructors run when the worker
thread itself retires, by which point that thread's per-request KV state is
gone. Calls made from there fail closed rather than crashing the server: a
`get` reports a miss, a `set` returns `false`, and in multi-tenant mode
nothing falls back to another site's keyspace. Do your KV work inside the
request.

The same split applies to [`ephpm_db_*`](/guides/db-from-php/#shutdown-functions-and-destructors),
which throws a distinct exception in the worker-mode case.

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
- [Sessions](/guides/sessions/) — the native `ephpm` session save handler built on this store
- [PHP API reference](/reference/php-api/) — every native function and injected `$_SERVER` variable in one place
- Examples in the repo: [`examples/kv-sapi-basic.php`](https://github.com/ephpm/ephpm/blob/main/examples/kv-sapi-basic.php), [`examples/kv-redis-predis.php`](https://github.com/ephpm/ephpm/blob/main/examples/kv-redis-predis.php)

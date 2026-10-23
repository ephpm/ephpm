+++
title = "KV RESP Protocol"
weight = 7
+++

The embedded KV store can be reached over the Redis wire protocol (RESP2) when
`[kv.redis_compat] enabled = true`. It speaks enough of Redis for caches,
counters, sessions and simple locks. **It is not Redis**: there are no lists,
sets, transactions, scripting or pub/sub, and a few commands behave differently.
This page lists exactly what the listener accepts.

From PHP running inside ePHPm, the in-process `ephpm_kv_*` functions reach the
same store without a socket. See [KV from PHP](/guides/kv-from-php/).

## Listener

| Setting | Behavior |
|---|---|
| Transport | TCP only, on `[kv.redis_compat] listen` (default `127.0.0.1:6379`). `socket` is parsed but not implemented and logs a startup warning. |
| Protocol | RESP2 arrays, plus inline commands (`PING` typed into `telnet` or `nc`). An inline command is one line ending in `\n` (a `\r` before it is optional), split on whitespace with Redis's quoting rules: `"..."` understands `\n`, `\r`, `\t`, `\b`, `\a`, `\xHH` and `\"`; `'...'` understands only `\'`. A line with unbalanced quotes, or one longer than 64 KiB, is a protocol error. No RESP3: `HELLO` is an unknown command. |
| Databases | One. `SELECT 0` is accepted; any other index is `ERR DB index is out of range`. |
| Connection cap | `max_connections` (default 1000). Excess clients get `-ERR max number of clients reached` and are closed. |
| Input buffer | `max_input_buffer` per connection (default 1 MiB). |
| Idle timeout | `idle_timeout_secs` (default 300). |
| Out of memory | Under `eviction_policy = "noeviction"` at the limit, writes return `-OOM command not allowed when used memory > 'maxmemory'`. |

The full key table is in the [configuration reference](/reference/config/#kvredis_compat).

## Authentication

`AUTH` and `QUIT` are always accepted. When authentication is required, every
other command is answered `-NOAUTH Authentication required` until `AUTH`
succeeds. What `AUTH` checks depends on the configuration:

| Configuration | `AUTH` behavior |
|---|---|
| No `[kv.redis_compat] password`, no `[kv] secret` | No authentication. `AUTH` with any arguments returns `OK`. |
| `[kv.redis_compat] password` set, single-site (no `sites_dir`) | `AUTH <password>`. Only the **first** argument is compared, so the two-argument form `AUTH <user> <password>` compares `<user>` and fails. Configure clients with a password and no username. |
| `[kv] secret` set and `[server] sites_dir` set (multi-tenant) | `AUTH <site-key> <HMAC-SHA256(secret, site-key)>`, both arguments required. The connection is then bound to that site's own store for its lifetime. **`[kv.redis_compat] password` is ignored** in this mode. |
| `[server] sites_dir` set, no `[kv] secret` | Startup error. The listener refuses to serve one shared, unauthenticated store to every tenant. |
| `[kv] secret` set, single-site, no `password` | Startup error (an empty `password` counts as unset). There is no per-site derivation in single-site mode, so the secret cannot authenticate anyone. Set `[kv.redis_compat] password` to protect a single-site listener, or remove `[kv] secret` to run it unauthenticated (keep it bound to loopback). Before this was fixed, such a listener answered `NOAUTH` and then accepted **any** `AUTH`. |
| `[kv] secret` set, single-site, `password` set | `AUTH <password>`, as in the second row. |

Passwords are compared in constant time.

### Credentials injected into PHP (multi-tenant)

When `[kv.redis_compat] enabled = true`, `[kv] secret` is set **and** `[server]
sites_dir` is set, every PHP request gets four `$_SERVER` variables:

| Variable | Value |
|---|---|
| `EPHPM_REDIS_HOST` | The host part of `[kv.redis_compat] listen`, verbatim. A wildcard listen address such as `0.0.0.0` is handed to PHP as `0.0.0.0`. |
| `EPHPM_REDIS_PORT` | The port part of `listen`. |
| `EPHPM_REDIS_USERNAME` | The request's **canonical site key**: the vhost directory name, with `sites_domain_suffix` already stripped (for example `blog`, not `blog.localhost`). A host that matches no vhost gets none of these variables: it is not a tenant and has no KV keyspace. |
| `EPHPM_REDIS_PASSWORD` | `HMAC-SHA256([kv] secret, EPHPM_REDIS_USERNAME)`, lowercase hex. |

With any of the three conditions missing, none of them are set.

## Commands

Arity is checked as a minimum for most commands (extra trailing arguments are
ignored rather than rejected); `GETDEL`, `EXPIRETIME`, `PEXPIRETIME`,
`HSETNX`, `HINCRBY`, `HINCRBYFLOAT` and `HSTRLEN` require exactly their
arguments, as in Redis. Unknown commands return `-ERR unknown command '<NAME>'`.

**Keys, hash fields and `KEYS`/`SCAN` patterns must be valid UTF-8** (a NUL
byte is fine). Unlike Redis, the store cannot hold binary keys, so a command
naming a non-UTF-8 key is refused with `-ERR invalid key: ePHPm KV keys and hash
fields must be valid UTF-8` and changes nothing. That includes a multi-key
`MSET`, `DEL` or `HSET` with one bad key. Values are binary-safe.

| Group | Command | Notes |
|---|---|---|
| Connection | `PING [msg]` | `PONG`, or echoes `msg`. |
| | `ECHO msg` | |
| | `SELECT index` | `0` only. |
| | `AUTH ...` | See [Authentication](#authentication). |
| | `QUIT` | `OK`, then the connection closes. |
| | `COMMAND` | Stub: returns an empty array. Clients that introspect the command table learn nothing. |
| | `INFO` | Fixed sections: `# Server` (`redis_version:7.0.0`, `redis_mode:embedded`, `tcp_port:6379` — **always 6379**, whatever the listen port), `# Memory` (`used_memory`), `# Stats` (`keyspace_hits`, `keyspace_misses`) and `# Keyspace` (`db0:keys=N,expires=M,avg_ttl=0`, omitted when empty). Ignores a section argument. `keyspace_hits`/`misses` count string reads (`GET`, `MGET`, `GETDEL`, `GETEX`, and PHP `ephpm_kv_get`/`mget`) and hash reads, for the store this connection is bound to, since the process started. `keys` and `expires` include expired keys not yet reclaimed, as `DBSIZE` does; `avg_ttl` is always `0`. See the note below. |
| Strings | `GET key` | `nil` for a missing key, and also for a key that only exists as a hash. |
| | `SET key value [NX \| XX \| IFEQ comparison] [GET] [EX s \| PX ms \| EXAT unix-s \| PXAT unix-ms \| KEEPTTL]` | As in Redis: the expiry options are mutually exclusive, and so are `NX`/`XX`/`IFEQ` (`ERR syntax error`; `NX` with `XX` keeps its own error). A time that is not a number is `ERR value is not an integer or out of range`; one that is zero, negative, or so large the expiry time overflows is `ERR invalid expire time in 'set' command`; nothing is written. An `EXAT`/`PXAT` time already in the past performs the write and leaves the key expired (gone). `KEEPTTL` keeps the key's current expiry. **`IFEQ comparison`** (Valkey 8.1) sets only while the current value equals `comparison`, never creates the key, and replies `nil` when it did not set; with `GET` it replies the value the key held before. In a cluster `IFEQ` is decided by the key's owner node like `SETNX`, and an unreachable owner answers `-CLUSTERDOWN …`. |
| | `SETEX key seconds value` | `seconds` must be positive and must not overflow, as for `SET EX`. |
| | `SETNX key value` | `1` inserted, `0` existed, `-OOM` refused. In a cluster it is decided by the key's owner node (see [KV from PHP → Locks in a cluster](/guides/kv-from-php/#locks-in-a-cluster)); an unreachable owner answers `-CLUSTERDOWN …` (nothing written on this node; the owner may still have applied a request that timed out). The same applies to `SET … NX`. |
| | `GETSET key value` | Clears any TTL, as in Redis. |
| | `GETDEL key` | Returns the value and deletes the key; `nil` if missing. The read and the delete are one atomic step on this node. In a cluster the delete then replicates; a large value held only by other nodes is fetched and deleted cluster-wide, which is not atomic across nodes. |
| | `GETEX key [EX s \| PX ms \| EXAT unix-s \| PXAT unix-ms \| PERSIST]` | Returns the value and changes its expiry (no option: a plain `GET`). An `EXAT`/`PXAT` in the past deletes the key, as in Redis. Time errors are `ERR invalid expire time in 'getex' command`. |
| | `MGET key...` / `MSET key value...` | `MSET` is **not atomic** under memory pressure: if a pair is refused with `-OOM`, earlier pairs stay written. |
| | `INCR` / `DECR` / `INCRBY` / `DECRBY` | 64-bit signed. A result that would overflow is an error and leaves the value unchanged; `DECRBY key -9223372036854775808` is `ERR decrement would overflow`, as in Redis. Atomic on the node that runs them. In a cluster each increment replicates its resulting total, last writer wins, so concurrent increments on different nodes can overwrite rather than add: cluster-wide counts are approximate and can under-count. |
| | `APPEND key value` | Creates the key if missing. In a cluster, an `APPEND` on a node that has no copy of a large value (one above `small_key_threshold`, held only by its replica set) answers `ERR APPEND to a large value this node does not hold is not supported in a cluster …` and writes nothing: creating the key from the appended bytes would replace the real value on every node. Run it on a node that holds the key, or rewrite the value with `SET`. |
| | `STRLEN key` | Node-local: in a cluster a large value held only by other nodes reports `0`. |
| Keys | `DEL key...` / `UNLINK key...` | Identical. Removes the string and the hash of that name. Freeing a hash with many fields happens off the calling thread for both (Valkey 8's `lazyfree-lazy-user-del yes` behavior). |
| | `DELIFEQ key token` | **ePHPm-specific.** Deletes `key` only while its value equals `token`; `1` or `0`. The safe lock-release primitive (pair with `SET key token NX PX ms`; extend with `SET key token IFEQ token PX ms`). Owner-decided in a cluster, like `SETNX`; an unreachable owner answers `-CLUSTERDOWN …`. |
| | `EXISTS key...` | Counts strings and hashes. |
| | `EXPIRE key s` / `PEXPIRE key ms` | Positive values only. `EXPIRE key 0` (or negative) is `ERR invalid expire time in 'expire' command`; Redis would delete the key. A value so large the expiry time overflows is the same error, as in Redis. Options (`NX`/`XX`/`GT`/`LT`) are ignored. Works on hashes too (the whole-key expiry). |
| | `EXPIREAT key unix-s` / `PEXPIREAT key unix-ms` | Absolute expiry. A time at or before now deletes the key and returns `1` (as in Redis). Options are ignored. |
| | `TTL` / `PTTL` / `PERSIST` | `-2` missing, `-1` no TTL. A hash reports (or removes) its whole-key expiry; field expiries are read with `HTTL`. |
| | `EXPIRETIME key` / `PEXPIRETIME key` | Absolute expiry as Unix seconds / milliseconds; `-1` no expiry, `-2` missing. |
| | `TYPE key` | `hash`, `string` or `none`. A name that holds both reports `hash`. |
| | `RENAME old new` | **Strings only.** Renaming a hash returns `ERR no such key`. Keeps the TTL. |
| | `KEYS pattern` | Redis glob syntax: `*`, `?`, `[abc]`, `[^abc]`, `[a-z]` and `\` escapes, matched on bytes as Redis does (`!` is not a negation marker, as in Redis). Matching takes time proportional to key length × pattern length at worst, with no exponential backtracking. Walks the whole store, but only holds each internal shard's lock long enough to copy its key names; matching runs with no lock held and off the async workers, so a slow `KEYS` does not block other clients' reads and writes. A name that is both a string and a hash is listed twice. Prefer `SCAN`. |
| | `SCAN cursor [MATCH pattern] [COUNT n] [TYPE string\|hash]` | Incremental iteration. **Every key present for the whole scan is returned** (exactly once — see [SCAN](#scan)); keys added or deleted during it may or may not be. `COUNT` (default 10, must be ≥ 1) is a hint: a call returns at least one whole internal shard, so a large store returns more keys per call. `TYPE` filters by type; any other type name returns nothing. A non-numeric cursor is `ERR invalid cursor`; an out-of-range one ends the scan. |
| | `DBSIZE` | Strings plus hashes (a dual name counts twice). |
| | `FLUSHDB` / `FLUSHALL` | Identical. Empties the store this connection is bound to (in multi-tenant mode, that site's store). Node-local in a cluster. |
| Hashes | `HSET key field value [field value...]` / `HMSET ...` | `HSET` returns the number of new fields, `HMSET` `OK`. Clears the TTL of each field written. Not atomic under memory pressure (as `MSET`). In a cluster a value larger than the [hash-field limit](#hashes-in-a-cluster) is refused and nothing is written. |
| | `HSETNX key field value` | `1` if set, `0` if the field existed. |
| | `HGET`, `HMGET`, `HDEL`, `HGETALL`, `HKEYS`, `HVALS`, `HLEN`, `HEXISTS`, `HSTRLEN` | Expired fields are invisible to all of them. |
| | `HINCRBY key field n` / `HINCRBYFLOAT key field x` | As in Redis, including `ERR hash value is not an integer` / `ERR hash value is not a float`, overflow and NaN/Infinity errors. The field keeps its TTL. `HINCRBYFLOAT` stores the result as plain decimal with no exponent or trailing zeros. In a cluster each increment replicates its resulting value (last writer wins per field), like `INCR`. |
| | `HEXPIRE` / `HPEXPIRE` / `HEXPIREAT` / `HPEXPIREAT key time [NX \| XX \| GT \| LT] FIELDS n field...` | Per-field expiry (Redis 7.4). One reply per field: `-2` no such field (or no such key), `0` the `NX`/`XX`/`GT`/`LT` condition was not met, `1` set, `2` deleted because the time was `0` or in the past. `GT`/`LT` treat a field without a TTL as infinitely far. A negative time is `ERR invalid expire time, must be >= 0`; a time past year 4199 is `ERR invalid expire time in '<cmd>' command`; a `numfields` mismatch and a missing `FIELDS` keyword use Redis' error texts. |
| | `HTTL` / `HPTTL` / `HEXPIRETIME` / `HPEXPIRETIME key FIELDS n field...` | Per field: remaining TTL (s / ms) or absolute expiry (Unix s / ms); `-1` no TTL, `-2` no such field. |
| | `HPERSIST key FIELDS n field...` | Per field: `1` TTL removed, `-1` had none, `-2` no such field. |
| | `HSCAN key cursor [MATCH pattern] [COUNT n] [NOVALUES]` | Returns the whole hash in one page with cursor `0` (`COUNT` is ignored — Redis does the same for small hashes). Clients that loop until cursor `0` work unchanged. |

### Field expiry

A hash field can carry its own TTL (`HEXPIRE` and friends), independent of the
hash key's TTL (`EXPIRE` on the hash). An expired field disappears from every
read the moment its deadline passes; its memory is reclaimed lazily by the next
write to the hash and actively by the background expiry cycle. When the last
field of a hash expires the hash itself stops existing. `HSET` clears a field's
TTL; `HINCRBY`/`HINCRBYFLOAT` keep it. Watch versions (`ephpm_kv_wait`) advance
when a field's value changes or a field is removed, not on TTL-only changes.

### SCAN

The store is split into a fixed set of internal shards, and a key always lives
in the same shard for as long as it exists — however much the shard's table
grows, shrinks or rehashes. The `SCAN` cursor is a shard position, and each
call walks whole shards. That is what makes the guarantee hold under
concurrent writes: a key present from the first call to the last is returned,
exactly once. (A name that is both a string and a hash is returned once per
type.)

In a cluster, `SCAN` and `KEYS` list **this node's** keyspace: every hash and
every string value at or under `[cluster.kv] small_key_threshold` is held by
every node and is always seen; a larger string value is held only by its
replica set and is listed only on those nodes.

### Hashes in a cluster

Hashes replicate across the cluster **per field**: every field is its own
replicated item on the gossip tier, ordered last-writer-wins by its own stamp.
Two nodes writing different fields of one hash at the same time both win; two
nodes writing the same field converge on the later write. Field TTLs and the
hash key's TTL travel with every field, so a node that joins later converges
too. Every node holds every field, so hash reads never leave the node.

Because a field rides a gossip datagram, a clustered hash field value is
limited to `[cluster.kv] small_key_threshold` minus a 17-byte header (495 bytes
at the default 512). A larger value is refused with an error naming the limit
and nothing is written — it is never stored on one node only. Store large
values under string keys, which have a large-value tier.

Deleting a hash (`DEL`/`UNLINK`) removes it on every node and also retracts
each field this node knows, so a node that joins later does not resurrect it.
A field written concurrently on another node that had not reached the deleting
node yet survives the delete, the same per-write window as everything else in
this tier.

Before v0.13, hash writes were node-local in a cluster. During a rolling
upgrade, nodes still on an older version store replicated fields as stray
string keys under an encoded name (unreachable, freed on restart); finish the
upgrade before relying on clustered hashes.

### Strings and hashes do not collide

Strings and hashes are kept in two separate maps, so there is **no
`WRONGTYPE` error**. The same name can hold a string *and* a hash at the same
time: `HSET k f v` on an existing string key succeeds, and `GET` on a hash
returns `nil`. Code that relies on `WRONGTYPE` to detect a type mix-up will not
see one.

### `INFO` claims Redis 7.0.0

`INFO` always reports `redis_version:7.0.0`. Client libraries and frameworks
that pick a code path from the server version will assume Redis 7 and may send
commands this listener does not implement (see below). ePHPm implements
nothing beyond the table above, whatever `INFO` says.

## Not implemented

Any of these returns `-ERR unknown command`:

- **Transactions and scripting:** `MULTI`, `EXEC`, `DISCARD`, `WATCH`, `EVAL`, `EVALSHA`, `SCRIPT`, `FUNCTION`.
- **Other data types:** lists (`LPUSH`, `RPOP`, `BLPOP`, ...), sets (`SADD`, `SMEMBERS`, ...), sorted sets, streams, HyperLogLog, bitmaps, geo.
- **Common extras:** `INCRBYFLOAT`, `SETRANGE`, `GETRANGE`, `MSETNX`, `RANDOMKEY`, `OBJECT`, `DUMP`/`RESTORE`, `HRANDFIELD`, `HGETDEL`/`HGETEX`/`HSETEX` (Redis 8), `SSCAN`/`ZSCAN`.
- **Pub/sub:** `SUBSCRIBE`, `PUBLISH`, ...
- **Server and client admin:** `CLIENT`, `CONFIG`, `HELLO`, `RESET`, `WAIT`.

What that means in practice:

| Typical use | Works? |
|---|---|
| Key/value cache with TTLs (`GET`/`SET EX`/`SETEX`/`DEL`) | Yes |
| Counters and fixed-window rate limits (`INCR` + `EXPIRE`) | Yes (approximate cluster-wide when clustered: totals replicate last-writer-wins and can under-count) |
| Session storage as a string key with a TTL | Yes |
| Simple locks (`SET NX PX`, extend with `SET IFEQ PX`, release with `DELIFEQ`) | Yes, but `DELIFEQ` is not a Redis command, so a client library's built-in lock release (usually a Lua `EVAL`) will not work |
| Hash-per-object storage, including per-field TTLs | Yes (in a cluster: field values up to the [hash-field limit](#hashes-in-a-cluster)) |
| Iterating keys by prefix (`SCAN MATCH`) | Yes (this node's keyspace in a cluster — see [SCAN](#scan)) |
| Anything batched in `MULTI`/`EXEC` | No |
| Queues and job backends (lists, blocking pops, Lua) | No |
| Tag-based cache invalidation (sets) | No |
| Pub/sub, streams | No |

Framework Redis drivers often use these missing commands for features beyond
plain get/set (for example Laravel's queue driver, its Redis cache tags and
lock release, or `putMany`). Test the specific feature before relying on it,
or use the ePHPm-native packages, which are built on the supported surface:
see [PHP Packages](/reference/php-packages/). If you need full Redis, run Redis.

## See also

- [KV from PHP](/guides/kv-from-php/): the in-process `ephpm_kv_*` functions and Predis/phpredis examples
- [`ephpm kv`](/reference/cli/kv/): a CLI client for this listener
- [KV store architecture](/architecture/kv-store/): storage, eviction and clustering

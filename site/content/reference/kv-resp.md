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
| Protocol | RESP2 arrays, plus inline commands (`PING` typed into `telnet`). No RESP3: `HELLO` is an unknown command. |
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
| `[kv] secret` set, single-site, no `password` | **Not access control.** Clients get `NOAUTH` until they send `AUTH`, but **any** `AUTH` succeeds, because there is no per-site derivation in single-site mode and no password to compare. To protect a single-site listener, set `[kv.redis_compat] password` (or keep it bound to loopback). |
| `[kv] secret` set, single-site, `password` set | `AUTH <password>`, as in the second row. |

Passwords are compared in constant time.

### Credentials injected into PHP (multi-tenant)

When `[kv.redis_compat] enabled = true`, `[kv] secret` is set **and** `[server]
sites_dir` is set, every PHP request gets four `$_SERVER` variables:

| Variable | Value |
|---|---|
| `EPHPM_REDIS_HOST` | The host part of `[kv.redis_compat] listen`, verbatim. A wildcard listen address such as `0.0.0.0` is handed to PHP as `0.0.0.0`. |
| `EPHPM_REDIS_PORT` | The port part of `listen`. |
| `EPHPM_REDIS_USERNAME` | The request's **canonical site key**: the vhost directory name, with `sites_domain_suffix` already stripped (for example `blog`, not `blog.localhost`). For a host that matches no vhost it is the normalized `Host` (lowercased, port and trailing dot removed). |
| `EPHPM_REDIS_PASSWORD` | `HMAC-SHA256([kv] secret, EPHPM_REDIS_USERNAME)`, lowercase hex. |

With any of the three conditions missing, none of them are set.

## Commands

Arity is checked as a minimum: extra trailing arguments are ignored rather
than rejected. Unknown commands return `-ERR unknown command '<NAME>'`.

| Group | Command | Notes |
|---|---|---|
| Connection | `PING [msg]` | `PONG`, or echoes `msg`. |
| | `ECHO msg` | |
| | `SELECT index` | `0` only. |
| | `AUTH ...` | See [Authentication](#authentication). |
| | `QUIT` | `OK`, then the connection closes. |
| | `COMMAND` | Stub: returns an empty array. Clients that introspect the command table learn nothing. |
| | `INFO` | Fixed text: `redis_version:7.0.0`, `redis_mode:embedded`, `tcp_port:6379` (**always 6379**, whatever the listen port), and `used_memory`. Ignores a section argument. See the note below. |
| Strings | `GET key` | `nil` for a missing key, and also for a key that only exists as a hash. |
| | `SET key value [EX s \| PX ms] [NX \| XX] [GET]` | `EXAT`/`PXAT`/`KEEPTTL` are `ERR syntax error`. An `EX`/`PX` value that is zero, negative or not a number is **silently ignored** and the key is stored with no TTL (Redis returns an error). `NX` with `XX` is an error. |
| | `SETEX key seconds value` | `seconds` must be positive. |
| | `SETNX key value` | `1` inserted, `0` existed, `-OOM` refused. |
| | `GETSET key value` | Clears any TTL, as in Redis. |
| | `MGET key...` / `MSET key value...` | `MSET` is **not atomic** under memory pressure: if a pair is refused with `-OOM`, earlier pairs stay written. |
| | `INCR` / `DECR` / `INCRBY` / `DECRBY` | 64-bit signed. In a cluster these are **local to the node** (not replicated). |
| | `APPEND`, `STRLEN` | |
| Keys | `DEL key...` | Removes the string and the hash of that name. |
| | `DELIFEQ key token` | **ePHPm-specific.** Deletes `key` only while its value equals `token`; `1` or `0`. The safe lock-release primitive (pair with `SET key token NX PX ms`). |
| | `EXISTS key...` | Counts strings and hashes. |
| | `EXPIRE key s` / `PEXPIRE key ms` | Positive values only. `EXPIRE key 0` (or negative) is `ERR invalid expire time`; Redis would delete the key. Options (`NX`/`XX`/`GT`/`LT`) are ignored. **String keys only:** on a hash it returns `0` and sets nothing. |
| | `TTL` / `PTTL` / `PERSIST` | `-2` missing, `-1` no TTL. String keys only: a hash reports `-2`. |
| | `TYPE key` | `hash`, `string` or `none`. A name that holds both reports `hash`. |
| | `RENAME old new` | **Strings only.** Renaming a hash returns `ERR no such key`. Keeps the TTL. |
| | `KEYS pattern` | Glob with `*` and `?` only; no `[...]` classes or `\` escapes. Walks the whole store. A name that is both a string and a hash is listed twice. |
| | `DBSIZE` | Strings plus hashes (a dual name counts twice). |
| | `FLUSHDB` / `FLUSHALL` | Identical. Empties the store this connection is bound to (in multi-tenant mode, that site's store). |
| Hashes | `HSET key field value [field value...]` | Returns the number of new fields. (No separate `HMSET`.) |
| | `HGET`, `HDEL`, `HGETALL`, `HKEYS`, `HVALS`, `HLEN`, `HEXISTS` | |

Hashes cannot be given a TTL (see `EXPIRE`) and, in a cluster, are **not
replicated**: `HSET`/`HDEL` stay on the node that ran them (a warning is logged
once per process). Use string keys for data that must expire or be visible
cluster-wide.

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
- **Iteration:** `SCAN`, `HSCAN`.
- **Common extras:** `UNLINK`, `GETDEL`, `GETEX`, `EXPIREAT`, `PEXPIREAT`, `HMGET`, `HMSET`, `HINCRBY`, `HSETNX`, `INCRBYFLOAT`, `SETRANGE`, `GETRANGE`, `MSETNX`, `RANDOMKEY`, `OBJECT`, `DUMP`/`RESTORE`.
- **Pub/sub:** `SUBSCRIBE`, `PUBLISH`, ...
- **Server and client admin:** `CLIENT`, `CONFIG`, `HELLO`, `RESET`, `WAIT`.

What that means in practice:

| Typical use | Works? |
|---|---|
| Key/value cache with TTLs (`GET`/`SET EX`/`SETEX`/`DEL`) | Yes |
| Counters and fixed-window rate limits (`INCR` + `EXPIRE`) | Yes (per node when clustered) |
| Session storage as a string key with a TTL | Yes |
| Simple locks (`SET NX PX` + `DELIFEQ` to release) | Yes, but `DELIFEQ` is not a Redis command, so a client library's built-in lock release (usually a Lua `EVAL`) will not work |
| Hash-per-object storage | Partly: `HSET`/`HGET`/`HGETALL` work, `HMGET`/`HINCRBY` do not |
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

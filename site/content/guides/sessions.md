+++
title = "Sessions"
weight = 6
+++

ePHPm ships a native PHP session save handler named `ephpm`. It stores
`$_SESSION` in the same in-process KV store that backs the
[`ephpm_kv_*` functions](/guides/kv-from-php/), so reading a session is a
hash-map lookup with no Redis, memcached, NFS share or socket involved. In
multi-tenant mode each virtual host's sessions live in that site's own
store. With `[cluster]` enabled they replicate through the KV store's
cluster tiers (see [Cluster](#cluster) below for exactly what that gives you).

**It is not enabled by default.** Out of the box PHP uses its stock `files`
handler (in multi-tenant mode, pointed at each vhost's own session
directory). You opt in per server or per script.

## Enabling it

ePHPm builds its own `php.ini` at startup and points PHP at it, so a
system-wide `php.ini` is **not** read and adding `session.save_handler` there
has no effect. Use one of these instead.

In `ephpm.toml`, for every request:

```toml
[php]
ini_overrides = [
    ["session.save_handler", "ephpm"],
    ["session.use_strict_mode", "1"],
    ["session.gc_maxlifetime", "1440"],   # session TTL, seconds
]
```

The same lines in a file named by `[php] ini_file` work too.

Or per script, before `session_start()` (`session.save_handler` is
`PHP_INI_ALL`):

```php
ini_set('session.save_handler', 'ephpm');
session_start();
```

Then PHP behaves as usual:

```php
<?php
session_start();
$_SESSION['user_id'] = 42;
session_write_close();
// ...next request...
session_start();
echo $_SESSION['user_id']; // 42
```

There is no `[session]` config block.

## Known issue (before v0.11.3): release the session explicitly {#known-issue}

This applies to releases up to and including v0.11.2. It is fixed from
v0.11.3 ([#541](https://github.com/ephpm/ephpm/pull/541)): PHP's request
teardown, including the session write and lock release, now runs on the same
thread right after each response is sent.

In per-request mode (the default), releases up to and including v0.11.2
write the session, and release its lock, during PHP's request teardown, and
that teardown runs only when the **same PHP thread** starts its next
request. The response itself goes out normally, but until that thread is
reused the session's data is unwritten and its lock is still held. A
follow-up request with the same session cookie that lands on another
thread then waits for the lock (up to 30 seconds, see [Locking](#locking))
and reads the session as it was **before** the previous request.

On those releases, the workaround is to end the session yourself as soon as
you are done with it:

```php
session_start();
$_SESSION['cart'][] = $item;
session_write_close();   // writes the session and releases the lock now
```

`session_write_close()` writes the session and releases the lock
immediately, so the next request sees the update without waiting. It is
good practice under php-fpm too, since it stops concurrent AJAX requests
from serializing on the lock. Calling it from a `register_shutdown_function()`
callback also works, because shutdown functions run at the end of every
request in per-request mode.

PHP's stock `files` handler has the same exposure on these releases, since
its `flock()` is also released during teardown.

## How it works

`session_start()` calls the handler's `read` callback, which takes the
session lock and then reads `session:<sid>` from the KV store. `write`
stores the serialized blob under the same key with a TTL of
`session.gc_maxlifetime`. `destroy` deletes it. Serialization
(`session.serialize_handler`) is PHP's own; the store only sees opaque
bytes.

The handler implements PHP's full save-handler surface, including:

- `validate_sid`, so `session.use_strict_mode = 1` rejects session ids that
  do not exist in the store;
- `update_timestamp`, so with `session.lazy_write = 1` (PHP's default) an
  unchanged session gets its TTL refreshed with an `EXPIRE` instead of a
  rewrite;
- `create_sid`, which delegates to PHP's own generator, so
  `session.sid_length` and `session.sid_bits_per_character` still apply.

In **multi-tenant mode** (`[server] sites_dir`) every virtual host has its
own physical KV store, and the handler reads through the same per-request
store the `ephpm_kv_*` functions use. `session:abc123` on
`alice.example.com` and `session:abc123` on `bob.example.com` are different
entries in different stores; no prefixing or configuration is involved.

## Locking

The handler takes a pessimistic per-session lock, the same discipline as
PHP's `files` handler, so concurrent read-modify-write cycles on `$_SESSION`
cannot lose updates. Locking is always on; there is no config knob.

- On `read` (inside `session_start()`) the handler acquires
  `session_lock:<sid>` with an atomic `SETNX` and a **30-second TTL**. The
  TTL is a dead-man's switch: a crashed or wedged holder stops blocking the
  session after at most 30 seconds.
- On contention it retries with exponential backoff (10 ms, doubling to a
  100 ms cap) for up to **30 seconds** in total. If the lock is still held,
  it raises an `E_WARNING` ("could not acquire session lock … proceeding
  without lock") and continues **without** the lock: a lost-update window is
  better than a hung request.
- The lock is released in `close`, which PHP calls on
  `session_write_close()` and at request teardown, and in `destroy`
  (`session_destroy()`, `session_regenerate_id(true)`). Only the thread that
  acquired the lock releases it.
- Hold the session only as long as you need it and call
  `session_write_close()` as soon as you have finished changing `$_SESSION`
  (on v0.11.2 and earlier this is required — see the
  [known issue](#known-issue) above).

Known limitation: if a request holds its session for longer than the
30-second lock TTL, the lock expires and a competing request may take it.
When the original holder closes, it releases the lock with an unconditional
`DEL`, which removes the **new** holder's lock early. The KV store now has a
compare-and-delete primitive (`ephpm_kv_del_if_eq`), but the handler does
not use it yet. The window needs a session held for more than 30 seconds
plus overlapping competitors, and the worst case is the lost-update race the
lock otherwise prevents.

In multi-tenant mode the lock key lives in the same per-site store as the
session data, so tenants cannot contend with each other. Windows builds are
ZTS like Linux and macOS, so the same locking applies there.

The lock is **node-local**. In a cluster, concurrent requests for the same
session that land on *different* nodes are not serialized against each
other. Use session-affinity routing if overlapping requests for one session
across nodes are a real pattern for your app.

## TTL and eviction

The session TTL is `session.gc_maxlifetime` (seconds), refreshed on every
write and every `update_timestamp`. With `session.gc_maxlifetime = 0` the
session is stored with no TTL, which is not recommended. PHP's own GC sweep
is a no-op (`gc` reports zero deletions); the KV store's expiry does the
cleanup.

**Sessions can be evicted before their TTL.** The KV store has a
`memory_limit`, and its default `eviction_policy` is `allkeys-lru`, so under
memory pressure a busy cache sharing the store can push sessions out and log
users out. If that is unacceptable, size `[kv] memory_limit` generously, or
switch to `volatile-lru` or `noeviction` (writes fail when full instead of
dropping data).

Sessions are in memory only and do not survive a restart of a single-node
server.

## Framework notes

The handler is used only by code that goes through PHP's native session
functions (`session_start()` and `$_SESSION`).

- **Plain PHP and WordPress plugins** that call `session_start()` use it as
  soon as it is enabled. WordPress core does not use PHP sessions itself.
- **Symfony**: set `framework.session.handler_id: null`, which tells Symfony
  to use the save handler configured in PHP's ini settings.
- **Laravel** and **CodeIgniter 4** run their own session layers on top of
  their own drivers and never hand session storage to PHP's save handler, so
  `session.save_handler = ephpm` has no effect on them. Use one of their
  drivers instead, for example Laravel's `redis` driver pointed at ePHPm's
  [RESP listener](/guides/kv-from-php/#resp-protocol-predis--phpredis), or a
  database driver.

## Under `ephpm php` (the CLI)

The handler is registered under `ephpm php`, but the CLI has no KV store
connected, so sessions read as empty and writes fail. This only matters for
CLI scripts that call `session_start()`.

## Cluster

With `[cluster] enabled = true`, sessions use the KV store's two-tier
cluster path like any other key:

- **Small session blobs** (at or under `[cluster.kv] small_key_threshold`,
  default 512 bytes) ride the gossip tier and replicate to every node, so a
  later request can be served by any node and the session survives node
  loss.
- **Larger blobs** live on the data-plane tier and are replicated to
  `[cluster.kv] replication_factor` nodes (default 2): the owner node plus a
  replica. Any node can read them (owner first, then replica).

Replication is asynchronous and eventually consistent. A session created
on one node can be briefly invisible on another until the write propagates,
which matters mostly for an immediate redirect after creating a brand-new
session. Replication happens at write time only: a large session written
while a replica was down, or whose replica set moved after a topology
change, can have fewer copies until it is rewritten. Keeping `$_SESSION`
small (a user id and a few flags) keeps it on the gossip tier. See
[KV store architecture](/architecture/kv-store/) for the replication
contract.

Restarting every node at once loses all sessions; there is no persistence.

## See also

- [KV from PHP](/guides/kv-from-php/) — the store the handler writes to
- [PHP packages](/reference/php-packages/) — `ephpm/session-handler`, the userland alternative
- [PHP API reference](/reference/php-api/) — every native function, `$_SERVER` variable and ini default

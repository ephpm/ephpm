# Sessions

ePHPm ships a native PHP session save handler called `ephpm`. It stores session
data in the in-process KV store that backs the `ephpm_kv_*` functions, so a
session is one DashMap lookup away from the running script — no Redis,
memcached, NFS share, or sticky load balancer required. In multi-tenant mode
sessions are automatically per-site; in clustered mode they ride the same
gossip replication path the rest of the KV store uses.

## Quick start

Add one line to `php.ini` (or to `ini_set()` at the top of your front
controller — `session.save_handler` is `PHP_INI_ALL`):

```ini
session.save_handler = ephpm
session.use_strict_mode = 1
session.gc_maxlifetime = 1440
```

Then PHP behaves exactly as you'd expect:

```php
<?php
session_start();
$_SESSION['user_id'] = 42;
// ...next request...
session_start();
echo $_SESSION['user_id']; // 42
```

Nothing else to wire up. No `[session]` config block, no extra TOML.

## How it works

`session_start()` calls the handler's `read` callback, which translates to
`Store::get("session:<sid>")` against the per-thread KV store. `write` is the
mirror: serialised session blob → `Store::set("session:<sid>", blob, ttl)`.
`destroy` is `Store::remove`. The session id never leaves the C wrapper —
there is no socket hop, no serialisation overhead, no FastCGI round-trip.

The handler implements PHP's full `ps_module` surface, including
`validate_sid` (so `session.use_strict_mode` actually rejects forged ids) and
`update_timestamp` (so `session.lazy_write` refreshes the TTL via `EXPIRE`
instead of rewriting an unchanged blob).

In **multi-tenant mode** (`sites_dir = ...`) every vhost gets its own physical
`Store` — see `crates/ephpm-kv/src/multi_tenant.rs`. The session handler reads
through the same `effective_store()` path the `ephpm_kv_*` functions use, so
`session:abc123` on `alice.example.com` lives in a different DashMap from
`session:abc123` on `bob.example.com`. No prefixing, no collision risk, no
config.

In **clustered mode** (`[cluster] enabled = true`) sessions use the same
two-tier KV path as everything else — with the same caveats. Session blobs
at or under `cluster.kv.small_key_threshold` (default 512 bytes) ride the
gossip layer and replicate to **every** node, so a session created on node A
is readable from nodes B, C, D within gossip convergence time. Larger session
blobs are placed on a **single** consistent-hash owner node: other nodes can
fetch them remotely (so requests can still route anywhere), but they are not
replicated — if the owner node dies, those sessions die with it. See
[Cluster](#cluster) below.

## Locking

The handler takes a pessimistic per-session lock, the same discipline as
PHP's built-in files handler: `session_start()` blocks while another request
holds the same session open, so concurrent read-modify-write cycles on
`$_SESSION` cannot lose updates. Locking is always on — there is no config
knob.

Mechanics:

- On `read` (inside `session_start()`), the handler acquires
  `session_lock:<sid>` via an atomic `SETNX` with a **30s TTL**. The TTL is
  a dead-man's switch: a crashed or wedged holder stops blocking the session
  after at most 30 seconds.
- On contention it spins with exponential backoff — starting at 10ms,
  doubling up to a 100ms cap — for a total wait of up to **30s**. If the lock
  is still held after that, the handler logs an `E_WARNING` and proceeds
  **without** the lock rather than deadlocking the worker: a degraded
  lost-update window beats a hung request.
- The lock is released (`DEL`) in `close` — which PHP fires on
  `session_write_close()`, at request shutdown, and after fatal
  errors/bailouts — and on `destroy` (`session_destroy()`,
  `session_regenerate_id(true)`). Only the thread that actually acquired the
  lock releases it; a request that proceeded lockless never deletes someone
  else's lock.
- Hold the session only as long as you need it: call `session_write_close()`
  as soon as you're done mutating `$_SESSION`, exactly as you would under
  php-fpm with the files handler, or concurrent AJAX requests from the same
  browser will serialize on the lock.

Known limitation (v1): if a request holds its session open past the 30s lock
TTL, the lock expires and a competing request may acquire it; when the
original holder finally closes, its unconditional `DEL` releases the *new*
holder's lock early (the KV layer has no compare-and-delete yet). The
exposure window requires a >30s session hold plus overlapping competitors,
and the worst case is the same lost-update race that existed before locking.

In multi-tenant mode the lock key lives in the same per-site store as the
session data, so tenants cannot contend with each other. On Windows (NTS,
serialized PHP execution) the lock is uncontended in-process but keeps the
same acquire/release semantics.

The lock is **node-local**: the `SETNX` runs against the in-process store,
not the cluster tier. In clustered deployments, concurrent requests for the
same session that land on *different* nodes are not serialized — use
session-affinity routing (sticky sessions) if cross-node request storms on a
single session are a real pattern for your app. On any single node the
guarantee holds regardless of cluster mode.

## TTL behaviour

The session TTL comes from `session.gc_maxlifetime` (seconds). On every
`session_write_close()` (or implicit shutdown) we call `Store::set` with
`ttl = gc_maxlifetime * 1000` ms. PHP's `session.lazy_write = 1` (the modern
default) routes unchanged writes through `update_timestamp`, which calls
`Store::expire` — same TTL, no value rewrite.

If `session.gc_maxlifetime = 0` the session is stored without a TTL. We do not
recommend this — the KV store has bounded memory and will start evicting under
pressure.

The KV store's lazy + active expiry handles cleanup. `gc_collect` is a no-op
because PHP's GC sweep would be redundant work.

## Framework notes

**WordPress** — works as-is. WP doesn't use `$_SESSION` heavily, but its
plugins do; setting `session.save_handler = ephpm` in `php.ini` is the entire
integration story.

**Laravel** — set `SESSION_DRIVER=php` in `.env`. Laravel's default `file`
driver bypasses `session.save_handler` and writes to `storage/framework/sessions/`
directly; you have to opt back into PHP's native session machinery for the
ephpm handler to see anything.

**Symfony** — set `framework.session.handler_id: null` (the documented
"let PHP handle it" value). Symfony's `NativeFileSessionHandler` ignores
`session.save_handler` for the same reason Laravel's `file` driver does.

**CodeIgniter 4** — use the `php` session driver
(`$session['driver'] = \CodeIgniter\Session\Handlers\NullHandler::class`
is **not** what you want; CI4 ships a thin PHP wrapper that just calls
`session_start()` — set `$session['driver']` to that and you're fine).

## Multi-tenant

No configuration. Each vhost's `effective_store()` resolves to its own
DashMap. The `session:` key prefix is purely cosmetic — a separator from
non-session KV entries — and is per-store, not global. A session id collision
across tenants is harmless because the stores are physically separate
objects.

## Cluster

No configuration beyond enabling `[cluster]` — but be honest with yourself
about what the two-tier KV path gives you:

- **Small session blobs** (≤ `cluster.kv.small_key_threshold`, default 512
  bytes) gossip to every node. These survive node loss — the surviving nodes
  already have the data, and the load balancer is free to route the next
  request anywhere.
- **Larger session blobs** live only on their single consistent-hash owner
  node. Any node can *read* them (remote fetch from the owner), so routing
  is still unrestricted — but there is no replication. Losing the owner node
  loses every large session it owned.

If sessions must survive node failure, keep `$_SESSION` small — a user id
and a handful of flags serialise comfortably under 512 bytes. Shopping-cart
sized payloads belong in the database, with the session holding just the key.

Convergence for the gossip tier is best-effort (~10–30s for full-mesh
convergence under default chitchat timings), so a session created on node A
*can* be invisible to node B for a brief window if the user's request races
ahead of the gossip fan-out. In practice this only matters for sub-second
redirects against brand-new sessions; for any normal browsing flow the gap
is invisible.

## Limitations

- **Memory eviction.** The KV store has a configurable `memory_limit` and an
  eviction policy, and the default policy is `allkeys-lru` — which means
  sessions **can be evicted before their TTL fires** whenever the store is
  under memory pressure. A busy cache workload sharing the store can silently
  log users out. If session loss is unacceptable, give the store a generous
  `memory_limit`, and consider switching the policy to `noeviction` (writes
  fail when full instead of silently dropping data) or `volatile-lru`, sized
  so eviction stays rare.
- **No persistence across `ephpm` restarts** (yet). The KV store is in-memory
  only. In clustered deployments, sessions small enough for the gossip tier
  are re-synced to a restarted node by its peers; large sessions owned by the
  restarted node are gone. Restarting every node simultaneously wipes all
  sessions. AOF/snapshot persistence is on the roadmap.
- **No cross-cluster replication.** Sessions live in the same KV tier as
  the rest of your data; if you have geographically split clusters you'll
  need application-level session token forwarding, the same as any
  Redis-backed deployment.

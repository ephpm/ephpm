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

In **clustered mode** (`[cluster] enabled = true`) the KV store is replicated
across all nodes via the gossip layer, so a session created on node A is
readable from nodes B, C, D within gossip convergence time. Sticky sessions
become unnecessary: any node can serve any request.

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

No configuration beyond enabling `[cluster]`. Session writes are replicated
through the same two-tier gossip path as every other KV mutation. On node
loss the surviving nodes already have the data; the load balancer is free to
route the next request anywhere.

Convergence is best-effort gossip (~10–30s for full-mesh convergence under
default chitchat timings), so a session created on node A *can* be invisible
to node B for a brief window if the user's request races ahead of the gossip
fan-out. In practice this only matters for sub-second redirects against
brand-new sessions; for any normal browsing flow the gap is invisible.

## Limitations

- **Memory eviction.** The KV store has a configurable `memory_limit` and an
  eviction policy. The default `noeviction` will fail writes when full —
  sessions included — which is the right behaviour for correctness. If you
  switch the policy to `allkeys-lru` for caching, sessions can be evicted
  before their TTL fires. Use `volatile-lru` for session-heavy workloads
  (evicts only keys that have a TTL set, which sessions always do).
- **No persistence across `ephpm` restarts** (yet). The KV store is in-memory
  only. Clustered deployments survive single-node restarts because gossip
  re-syncs the store; restarting every node simultaneously wipes all
  sessions. AOF/snapshot persistence is on the roadmap.
- **No cross-cluster replication.** Sessions live in the same KV tier as
  the rest of your data; if you have geographically split clusters you'll
  need application-level session token forwarding, the same as any
  Redis-backed deployment.

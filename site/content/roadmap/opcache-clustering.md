# OPcache Clustering & Per-Vhost Preload

ePHPm runs PHP through the embed SAPI, which means OPcache lives in the
same process as the HTTP server, the KV store, and the gossip cluster.
That alignment unlocks a class of OPcache features that's genuinely
hard to do well in the traditional PHP-FPM + reverse proxy + Redis
stack — and almost free to do in ours.

This page describes the design for two pieces:

1. **Cluster-wide invalidation** — atomic OPcache reset across every
   ePHPm node, triggered by a single KV write. Solves the "we deployed
   new code but half the cluster is still serving the old bytecode"
   problem without `validate_timestamps`, without LB blue/green
   theatrics, and without a separate cache-bust service.
2. **Per-vhost preload** — each virtual host declares its framework
   bootstrap file(s) in `site.toml`; ePHPm compiles them into OPcache
   when the vhost is discovered, so the first request after a cold
   start doesn't pay the autoloader/container-build tax.

Both compose with the existing multi-tenant story: invalidation is
scoped per vhost, preload is per vhost, and neither interferes with
sibling sites on the same node.

---

## The problem in PHP-FPM land

OPcache stores compiled bytecode in shared memory, per process group.
In a typical deployment that's one OPcache per PHP-FPM master, and
many PHP-FPM masters across many machines. After a deploy, every
cached script entry under the old code path still points at the old
bytecode. You have to invalidate.

The standard knobs:

| Approach | Cost | Race window |
|---|---|---|
| `opcache.validate_timestamps = 1` | `stat()` on every include site on every request; multi-millisecond overhead on busy apps; flaky over NFS | None |
| `opcache.revalidate_freq = N` | `stat()` at most every N seconds | Up to N seconds of stale code |
| `opcache_reset()` via cron | Blunt, periodic, blows away every script's bytecode | Up to cron interval |
| Restart PHP-FPM workers (`systemctl reload`) | Drops in-flight requests, slow on warm caches | Brief but real |
| Blue/green deploy + load balancer flip | Requires LB, requires N×2 capacity during deploy | Visible to clients during flip |

None of these are great. Even when they work, they're decoupled from
the actual deploy event — you're either polling (which costs every
request) or sweeping (which is unrelated to whether anything changed).
And on a cluster, every node needs the same treatment, ideally
atomically.

---

## What ePHPm has that PHP-FPM doesn't

Three primitives line up to solve this cleanly:

1. **Embedded KV store with gossip replication.** A write on one node
   propagates to every peer within seconds (chitchat SWIM ~3-5 s in
   typical settings). Already used for SQLite primary election and
   per-site cache stores.
2. **Direct OPcache API from the SAPI process.** `opcache_reset()`,
   `opcache_invalidate($file)`, `opcache_compile_file($file)`,
   `opcache_get_status()` are all in-process function calls from
   inside the embed runtime — no IPC, no FastCGI hop.
3. **Per-vhost docroot resolution.** Each request already routes
   through `Router::resolve_site` and knows which vhost it belongs
   to, so we can scope cache operations to a single vhost without
   touching siblings.

Combined: one KV write fans out to every node, each node invalidates
exactly the scripts under the affected vhost's docroot, and the next
request to that vhost recompiles from disk. No polling. No blue/green.
No stat-storm.

---

## Design

### KV key schema

A single per-vhost version key, monotonically increasing:

```
opcache:version:<vhost>   →   <epoch_ms>
```

When the value changes (any nondecreasing comparison; absolute value
doesn't matter), every node treats it as a deploy event for that
vhost.

For the default `document_root` (no vhost), the key is
`opcache:version:_default`.

#### Why a version key rather than tombstones

The alternative would be per-file tombstones —
`opcache:tombstone:<vhost>:<path>:<epoch_ms>`. That allows surgical
invalidation: only the changed files get their bytecode dropped.
Considered and deferred:

- **Simpler operator model.** Deploys are event-shaped (`ephpm deploy
  --site blog`), not file-shaped. The version key matches that.
- **Smaller KV traffic.** One key per vhost, not one per changed
  file. A WordPress deploy touching 50 files is one write either way.
- **Faster local check.** Watching one key is `O(1)`; matching
  tombstones requires either a prefix scan or a pubsub subscription.
- **Doesn't preclude tombstones.** Phase 3 below adds them as an
  optimization for the file-watcher path, where surgical
  invalidation is genuinely useful.

### Per-request watcher

On every PHP request after vhost resolution:

```rust
// pseudocode in ephpm-server
let site = router.resolve_site(host);
let current_version = kv.get(format!("opcache:version:{}", site.name))
    .and_then(|s| s.parse::<u64>().ok())
    .unwrap_or(0);

if current_version > site.last_invalidated_version.load(Acquire) {
    let _guard = site.invalidation_mutex.lock();
    // re-check under the mutex
    if current_version > site.last_invalidated_version.load(Acquire) {
        php::opcache_invalidate_under(&site.document_root)?;
        site.last_invalidated_version.store(current_version, Release);
    }
}
```

Key properties:

- **Fast path is one atomic load + one KV lookup.** KV is an in-process
  `DashMap` — sub-microsecond. Subsequent requests after invalidation
  return immediately at the first `if`.
- **Double-checked locking** so concurrent requests on the same vhost
  serialize only on the actual reset, not on the per-request check.
- **Per-vhost mutex** so sibling sites don't queue behind each other.
- **Scoped invalidation** via `opcache_invalidate()` for each cached
  script under the vhost's docroot, NOT a global `opcache_reset()`
  that would blow away neighbors.

`php::opcache_invalidate_under(docroot)` is a thin FFI wrapper that:

1. Calls `opcache_get_status(true)`, walks the `scripts` array.
2. For each entry whose `full_path` starts with the vhost's docroot,
   calls `opcache_invalidate($path, force=true)`.
3. Returns the count, for metrics.

### CLI surface

```bash
# Invalidate one vhost cluster-wide
ephpm deploy --site blog

# Tag the deploy with a revision (recorded as 'opcache:revision:<vhost>'
# for observability; doesn't affect invalidation logic)
ephpm deploy --site blog --rev a8f13d2

# Invalidate every vhost
ephpm deploy --all

# Local-only reset (bypass KV, doesn't broadcast — useful in dev mode
# or when running single-node)
ephpm cache reset --site blog
ephpm cache reset --all

# Inspect cache state
ephpm cache status                 # global summary
ephpm cache status --site blog     # per-vhost: hit rate, script count, memory
```

All four are thin commands that either write to KV (`deploy`) or call
the local invalidation directly (`cache reset`).

### Config surface

In `ephpm.toml`:

```toml
[opcache]
# Watch KV for cluster-wide invalidation events. Default: true when
# [cluster] is enabled, false otherwise (single-node — `ephpm cache
# reset` is the right interface).
cluster_invalidation = true

# How often the per-request watcher checks KV for a new version. The
# default checks every request — the lookup is in-process and cheap,
# so the staleness window is essentially zero. Bump this on
# very-high-RPS workloads where the cost adds up.
check_interval = "0s"
```

In `<vhost>/site.toml`:

```toml
[opcache.preload]
# Files compiled into OPcache when this vhost is discovered. Run on
# a tokio worker thread so vhost discovery itself stays non-blocking;
# the first request just sees pre-warmed bytecode. Order matters —
# files are compiled in the order listed.
files = [
    "vendor/autoload.php",
    "bootstrap/app.php",
    "vendor/symfony/runtime/.preload.php",
]
```

### Per-vhost preload

OPcache's built-in `opcache.preload` runs at MINIT (once per process,
hard-coded to a single file path). That doesn't fit multi-tenant — we
have N vhosts, each potentially wanting its own preload set, and they
don't exist yet when MINIT fires.

Instead, ePHPm uses `opcache_compile_file($path)`, which can be called
at any time to push a file into the OPcache. Per-vhost flow:

1. Vhost discovery (existing code in `Router::resolve_site` and
   `scan_sites_dir`) fires.
2. If `<vhost>/site.toml` has `[opcache.preload] files`, queue a
   `spawn_blocking` task that:
   - Runs each preload file through `opcache_compile_file()`.
   - Records timing in `ephpm_opcache_preload_seconds_bucket`.
3. The first request to that vhost finds the bootstrap files already
   compiled — skips the autoloader/container-build hot path (typically
   15-30 ms on framework-heavy apps).

Differences from native `opcache.preload`:

- No persistent "preload" segment — preloaded classes live in the
  normal OPcache and can be invalidated like anything else. We give
  up the "linked-at-preload, can't ever be invalidated" guarantee in
  exchange for per-vhost flexibility.
- Runs on first vhost discovery, not at PHP MINIT. Cold-cache
  scenario: the first request to a vhost MAY race with preload
  completion. Acceptable — that single request pays the normal
  compile cost, every subsequent request gets the warm cache.

### Failure modes

**OPcache disabled** (`opcache.enable=0`, or PHP built without it):
the watcher's `opcache_get_status()` returns false; the invalidation
helper short-circuits to a no-op. Log a startup warning once. Don't
fail.

**Cluster disabled** (`[cluster] enabled = false`): the
`cluster_invalidation` knob defaults to false. `ephpm deploy` falls
back to local-only invalidation (same as `ephpm cache reset`). Single
node, no KV propagation needed.

**KV unavailable during request**: the in-process KV is part of the
same binary — it's never "down" the way a network Redis can be down.
A node-level OOM or panic would take down the whole server anyway.

**Partial cluster failure during deploy**: gossip is eventually
consistent. Nodes that are partitioned or restarting see the new
version key once they're reachable again, invalidate then. Brief
inconsistency window where some nodes serve old bytecode after deploy,
but no requests fail. Document this in the operator guide.

**Race during invalidation**: two requests for the same vhost see the
new version simultaneously. The per-vhost mutex serializes the
invalidation itself — one performs the reset, the other waits on the
lock and sees the updated `last_invalidated_version` after the
re-check. Both then proceed against the freshly-compiled cache.

**Watcher overhead on hot path**: one atomic load + one KV `get`
(sub-microsecond). On a 100k-RPS workload, that's ~100 ms of CPU
across the whole node. Acceptable. The `check_interval` knob lets
operators trade staleness for less work on extreme workloads.

### Metrics

Exposed via the existing Prometheus `/metrics` endpoint:

```
ephpm_opcache_invalidations_total{vhost="blog", trigger="kv|cli|filewatcher"}
ephpm_opcache_compile_seconds_bucket{vhost="blog"}
ephpm_opcache_preload_seconds_bucket{vhost="blog"}
ephpm_opcache_hit_ratio{vhost="blog"}
ephpm_opcache_scripts_cached{vhost="blog"}
ephpm_opcache_memory_used_bytes
ephpm_opcache_memory_free_bytes
```

Most are pulled from `opcache_get_status()` on metrics scrape (cheap;
returns a snapshot). The `invalidations_total` and `compile_seconds`
are incremented inline at the invalidation/compile sites.

---

## Implementation phases

### Phase 1 — Cluster-wide invalidation (the foundation)

| Piece | Where | Effort |
|---|---|---|
| `opcache_invalidate_under(docroot)` FFI helper | `crates/ephpm-php/ephpm_wrapper.c` + `kv_bridge.rs` parallel | ~80 LOC C + 30 LOC Rust |
| Per-vhost watcher: atomic + mutex + KV check | `crates/ephpm-server/src/router.rs` | ~50 LOC |
| `ephpm deploy --site <name>` subcommand | `crates/ephpm/src/main.rs` | ~40 LOC |
| `ephpm cache reset` / `cache status` subcommands | same | ~50 LOC |
| Metrics: invalidations_total, compile_seconds | server crate, plumbed via existing `metrics` macro | ~20 LOC |
| Tests: unit (invalidate one vhost doesn't touch siblings); e2e (two-node cluster, write KV, both nodes invalidate) | `crates/ephpm-server/src/router.rs` tests + `crates/ephpm-e2e/tests/opcache_invalidation.rs` | ~150 LOC |

Phase 1 is the foundational piece — everything else builds on the
KV-driven contract and the FFI helper. Roughly a long weekend's work
end-to-end including tests.

### Phase 2 — Per-vhost preload

| Piece | Where | Effort |
|---|---|---|
| `[opcache.preload]` parsing in site config | `crates/ephpm-config/src/lib.rs` | ~30 LOC |
| Background preload runner (spawn_blocking on vhost discovery) | `crates/ephpm-server/src/router.rs` | ~40 LOC |
| `opcache_compile_file($path)` FFI helper | `crates/ephpm-php/ephpm_wrapper.c` | ~30 LOC |
| Tests: preload entries get cached; preload failures don't break the vhost | server + e2e | ~80 LOC |

Phase 2 is pure additive — doesn't require Phase 1 to be useful in
single-node setups. Could ship independently.

### Phase 3 — File-change watcher (deferred)

Watches `sites_dir` via inotify (Linux), FSEvents (macOS),
ReadDirectoryChangesW (Windows). On `.php` file change:

- Local: `opcache_invalidate($file, force=true)`.
- Cluster: write a tombstone
  `opcache:tombstone:<vhost>:<file>:<epoch_ms>` (TTL'd 24 h) so peers
  invalidate the same file.

Mostly relevant for dev mode (instant code-change pickup without
`ephpm cache reset`) and shared-filesystem prod (NFS, rsync deploys
where the operator updates files in-place without running
`ephpm deploy`).

Deferred because the surface area is platform-specific (three OS-
level watchers) and the value-add over `ephpm deploy` is marginal
once Phase 1 ships. Revisit after Phase 1 is in operator hands.

---

## Alternatives considered

### `opcache.validate_timestamps = 1`

The stock answer. Works, but pays a `stat()` on every include site
on every request. On a Symfony app with several hundred autoloaded
classes that's measurable. On NFS or other network filesystems it's
both slow AND unreliable (timestamps lag). Doesn't compose with
clustered storage.

### `opcache.revalidate_freq = N`

Same `stat()` cost but only once per N seconds per file. Always has
a staleness window of [0, N] s after a deploy. Doesn't propagate
across nodes — each node has its own clock for "every N seconds since
last check," so deploys aren't atomic cluster-wide.

### External cache-bust service (HTTP webhook to each node)

Some shops build a small service that, on deploy, POSTs to every
PHP-FPM node's "reset opcache" endpoint. ePHPm's KV-driven approach
is the same idea minus the service: gossip already handles the fan-
out, KV already handles the durability, we just write the trigger
key.

### Pub-sub on the KV layer

Could subscribe to `opcache:version:*` changes instead of polling on
every request. Cleaner conceptually. Deferred because (a) the KV
doesn't have pubsub today and (b) the per-request poll is fast enough
that the savings would be in the noise. Revisit if KV grows
subscriptions for other reasons.

### Per-file invalidation as the primary primitive

Considered as the v1 schema; rejected because deploys are
event-shaped and operators reason about them at the deploy level. The
per-file path is still available via the file-watcher phase, where
it's actually warranted (surgical invalidation when a single file
changes mid-run).

---

## Open questions

- **OPcache memory pressure during preload.** If a vhost preloads a
  huge framework (Symfony with thousands of autoloaded files), that
  competes with other vhosts for OPcache memory. Document the
  trade-off; expose `opcache.memory_consumption` per-vhost? Or let
  ops just bump the global setting?
- **Preload ordering across vhosts at startup.** Discovery currently
  scans sites_dir sequentially. With preload, that becomes
  serial-compile-then-next-vhost. For 50 vhosts × 30 preload files
  each, startup could grow meaningfully. Parallelize across vhosts
  via the `spawn_blocking` pool (already what we have for PHP
  requests).
- **Revision tagging vs version monotonicity.** The version key is
  `epoch_ms` for simplicity. Operators may want to set it to their
  git SHA hash so `ephpm cache status` shows what's deployed. The
  CLI already records `--rev` separately at
  `opcache:revision:<vhost>` for display; the version key stays
  numeric. Confirmed sound; documenting for clarity.
- **Interaction with JIT.** OPcache JIT (PHP 8+) compiles hot paths
  to machine code on first hit. After invalidation, the JIT cache is
  also dropped (same OPcache process). No special handling needed,
  but worth a confirmation pass once Phase 1 lands.
- **Cluster bootstrap.** A node joining a cluster mid-day inherits
  the current `opcache:version:<vhost>` values via gossip. First
  request after join will trigger an invalidation (last-seen-version
  was 0 locally vs. >0 in the cluster). That's the right behavior
  but worth noting — the alternative would be to seed
  last_seen_version on cluster join, which is more code for the same
  net result.

---

## Why this is uniquely an ePHPm thing

Three things have to all be true for this design to work cleanly:

1. The HTTP server and the PHP runtime share an address space — so
   the invalidation watcher can call into OPcache via direct C
   function calls, not IPC.
2. The HTTP server and the cluster KV share an address space — so
   the per-request KV check is a `DashMap::get`, not a network
   round-trip to Redis.
3. The cluster KV has gossip replication — so a single write fans
   out to every node without an external coordinator.

ePHPm has all three. PHP-FPM + nginx has none of them. FrankenPHP has
#1 but no built-in cluster KV (could bolt on Redis or
something — but then back to network hops). RoadRunner has #1 between
the worker and Go runtime but `opcache_invalidate()` from Go is
non-trivial. Swoole has #1 but is single-server.

The result is "OPcache that cares about your deploys, atomically,
cluster-wide, with no extra infrastructure." Worth building.

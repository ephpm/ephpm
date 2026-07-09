+++
title = "Cluster-Wide OPcache Invalidation"
weight = 13
+++

Since ePHPm 0.4.0, one `ephpm deploy` command atomically invalidates a
vhost's OPcache on **every node** of a cluster — no restarts, no rolling
deploys, no `opcache.validate_timestamps` stat cost on the hot path. The
deploy event is a single write to the gossip-replicated KV store; each
node's per-request watcher (one atomic load + one KV read) drops the
vhost's cached scripts before the next request executes.

## Requirements

- ePHPm **≥ 0.4.0**, `[php] mode = "fpm"` (the default — see
  [gaps](#gaps-and-caveats) for worker mode)
- `[cluster] enabled = true` for multi-node fan-out (single node works
  too — `ephpm deploy` then only affects that node)
- `[kv.redis_compat] enabled = true` — the CLI writes the version key
  over the RESP listener (bind it to loopback)

## Configuration

```toml
[cluster]
enabled = true
bind = "10.0.1.5:7946"          # this node's reachable address
join = ["10.0.1.6:7946"]        # any peer(s)
secret = "…"                     # seals gossip + KV data plane

[kv.redis_compat]
enabled = true
listen = "127.0.0.1:6379"       # loopback: only the local CLI needs it

[opcache]
cluster_invalidation = true      # default: true when [cluster] enabled
```

## Deploying

```bash
# Invalidate one vhost, cluster-wide
ephpm deploy --site blog

# Single-site mode (no sites_dir): the default vhost
ephpm deploy

# Every vhost at once (broadcast key)
ephpm deploy --all

# Record a revision string alongside (observability only)
ephpm deploy --site blog --rev "$(git rev-parse --short HEAD)"

# Same wire effect, distinct name for dev/audit-log clarity
ephpm cache reset --site blog
```

Run it on **any** node; gossip converges to peers in ~1–3 seconds. Each
node invalidates lazily on its next request for that vhost — a node
serving no traffic pays nothing.

## Verifying

`opcache_invalidate()` keeps entries listed in
`opcache_get_status()['scripts']`, so presence-in-list is **not** a
valid check. Use `opcache_is_script_cached()`:

```php
<?php // status.php
echo json_encode([
    'cached' => opcache_is_script_cached($_SERVER['DOCUMENT_ROOT'] . '/index.php'),
]);
```

Warm a script, run `ephpm deploy`, and the next request on every node
reports `cached: false`, then `true` again once re-warmed. The
`ephpm_opcache_invalidations_total{vhost,trigger}` counter increments on
each node that performed an invalidation.

## Kubernetes

A two-node StatefulSet needs three things beyond the config above:
**bind the pod IP** (gossip advertises the bind address), use the
headless-service pod DNS names as seeds, and run the CLI via
`kubectl exec`:

```yaml
env:
  - name: POD_IP
    valueFrom: { fieldRef: { fieldPath: status.podIP } }
# start script: bind = "${POD_IP}:7946"
# join = ["app-0.app-hs.ns.svc.cluster.local:7946", "app-1.app-hs.ns.svc.cluster.local:7946"]
```

```bash
kubectl exec app-0 -- ephpm deploy --site blog
```

A complete, tested manifest pair (demo + php-fpm comparison benchmark)
lives in the [ePHPm-lab repository](https://github.com/tinfoyle/ePHPm-lab)
under `k8s/opcache-cluster.yaml`.

## Why not `opcache.validate_timestamps`?

The stock approach pays a `stat()` per include site per request, always,
on every node — and still can't make a deploy atomic across a cluster.
The comparison table and alternatives are in the
[design document](/roadmap/opcache-clustering/), along with the planned
Phase 2 (per-vhost preload) and Phase 3 (file watcher).

## Gaps and caveats

- **Worker mode is not wired yet** — with `[php] mode = "worker"` the
  watcher is skipped and startup logs a WARN. Planned.
- **`EXPIRE`/`INCR` do not replicate** — only SET/DEL fan out. Rate-limit
  middleware counters are therefore per-node in a cluster (startup warns
  when this combination is active).
- **Remote deletes** propagate as tombstones that peers don't apply to
  local copies until TTL expiry or overwrite; the version-key scheme is
  unaffected (it only ever overwrites).
- `ephpm cache status` is not implemented; use `opcache_get_status()`.

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

## Deploys are events (`ephpm serve`, planned for v0.5.0)

> **Planned for v0.5.0 — not in v0.4.x.** This default flip is a
> user-visible behavior change and ships in the next minor. On v0.4.x,
> serve mode still inherits PHP's `validate_timestamps=1, revalidate_freq=2`.

Starting in **v0.5.0**, `ephpm serve` defaults `opcache.validate_timestamps=0`:
the server **trusts the OPcache** and never `stat()`s cached scripts on the
hot path. Code changes become a deliberate *event* — they go live only when
you run `ephpm deploy` (or `ephpm cache reset`), which invalidates the cache
through the RESP listener. `ephpm dev` keeps `validate_timestamps=1` so the
edit-refresh loop stays instant.

| Mode | `validate_timestamps` default | Code changes go live… |
|------|-------------------------------|-----------------------|
| `ephpm serve` | `0` (off) | on `ephpm deploy` / `ephpm cache reset` |
| `ephpm dev` (or bare `ephpm`) | `1` (on) | on the next request after you save |

Override either way with the `[php] opcache_validate_timestamps` knob
(`true` re-enables stat-on-use under serve, e.g. for a bind-mounted docroot;
`false` freezes the cache under dev). `[php] opcache_revalidate_freq` tunes
how often a re-stat happens when validation is on.

> ⚠️ **Serve mode + validation off needs an invalidation lever.** With
> `validate_timestamps=0`, the *only* way to refresh cached code without a
> restart is `ephpm deploy` / `ephpm cache reset`, and those write over the
> RESP listener. If `[kv.redis_compat] enabled = false`, there is no lever
> at all — startup logs a **WARN** and cached code stays frozen until the
> process restarts. Either enable the RESP listener (loopback is fine) or
> set `[php] opcache_validate_timestamps = true`.

Why off and not just a high `revalidate_freq`? On a 500-file autoload app
(ePHPm-lab bench #5), `validate_timestamps=1 revalidate_freq=2` measured
912 rps, `revalidate_freq=60` measured 999 rps (+9.5%), and off measured
995 rps. Since `freq=60` already recovers essentially all the throughput,
off is chosen for the **deterministic deploys-are-events contract**, not for
raw speed — with a bonus that on container/overlay/network filesystems the
`stat()` savings are larger, and in immutable-container deploys (files can't
change) validation is pure waste.

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

## Why not rely on `opcache.validate_timestamps`?

Timestamp validation pays a `stat()` per include site per request (bounded
by `revalidate_freq`), on every node — and still can't make a deploy atomic
across a cluster. That is why serve mode turns it **off** by default (v0.5.0,
see [above](#deploys-are-events-ephpm-serve-planned-for-v050)) and uses the
KV-driven invalidation event instead. The comparison table and alternatives
are in the [design document](/roadmap/opcache-clustering/), along with the
planned Phase 2 (per-vhost preload) and Phase 3 (file watcher).

## Gaps and caveats

- **Worker mode is not wired yet** — with `[php] mode = "worker"` the
  watcher is skipped and startup logs a WARN. Planned.
- **`INCR` does not replicate** — SET, DEL, and EXPIRE all fan out
  (deletes ride a write-stamped gossip tombstone; EXPIRE re-emits the
  value with the new expiry). Read-modify-write ops like INCR are still
  local-only, so rate-limit middleware counters are per-node in a
  cluster (startup warns when this combination is active). See the
  [`clustered-kv-v2` roadmap](/roadmap/clustered-kv-v2/) for the
  owner-routed `INCR` design.
- `ephpm cache status` is not implemented; use `opcache_get_status()`.

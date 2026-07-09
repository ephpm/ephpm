# The Deploy Story — Warmup, Doctor, and `cache status`

> **Status: DESIGN — not yet implemented.** `ephpm deploy` shipped in
> 0.4.0 as cluster-wide OPcache invalidation. This page designs the
> rest of the deploy lifecycle around it.

## The gap, measured

The 0.4.0 deploy-blip benchmark (ePHPm-lab, two-node kind cluster,
50 req/s) showed the shape of the remaining problem: after
`ephpm deploy`, the first request per script per node pays a visible
recompile spike (max latency roughly 2–3× steady-state on a 31-file
fixture; a real Symfony/Laravel tree is thousands of files). php-fpm's
rolling restart pays a *bigger* cold-cache window — but ours is still
nonzero, and it is fixable, because we know exactly when a deploy
happened and exactly which vhost it touched.

## Pieces

### 1. Post-invalidation warmup (OPcache Clustering Phase 2, activated)

The [per-vhost preload design](/roadmap/opcache-clustering/) already
specifies `[opcache.preload] files` compiled via
`opcache_compile_file()` on vhost discovery. Extend the trigger: the
invalidation watcher, after dropping a vhost's scripts, queues the same
preload set on a background thread. Result: `ephpm deploy` becomes
**invalidate + rewarm** — the blip shrinks from "first request per
script" to near-zero, and the k6 deploy-blip profile can prove it
release over release.

### 2. `ephpm cache status`

Already stubbed in docs as planned. Design: the CLI (a separate
process) queries a tiny status endpoint (`/_ephpm/opcache-status`,
loopback/internal-exempt like the health endpoints) that the server
answers from `opcache_get_status()` — per-vhost script counts, memory,
hit rate, last invalidation version + revision string from
`opcache:revision:<vhost>`. Gives deploys a verification step:
`deploy → cache status --site blog` shows the new revision and a warm
cache.

### 3. `ephpm doctor <framework>`

Requested verbatim by the July lab report ("a command like
`ephpm doctor laravel` could catch missing functions, path issues, and
worker setup problems"). Design: a check-runner with per-framework
profiles —

- **common**: required extensions present (`mb_split` et al.),
  `$argv` registration sanity, docroot/index resolution, OPcache
  active, KV functions when config expects them;
- **laravel**: `document_root` points at the project (not `public/`)
  in worker mode, `vendor/bin/ephpm-octane-worker` resolvable,
  `APP_KEY` set, storage writable;
- **wordpress**: `wp-config.php` reachable, DB connectivity via the
  configured mode, object-cache drop-in consistency.

Output is a pass/warn/fail table with one-line fixes. The July lab
user lost hours to exactly the failures this would catch in seconds;
it is the cheapest trust-building feature on the roadmap.

### 4. Deploy hooks (thin)

`ephpm deploy --exec "php artisan config:cache"` — run a command per
node after invalidation, before rewarm. Deliberately thin: not a CD
system, just the missing glue between "bytecode dropped" and "app
caches rebuilt". Needs the same cluster command-fanout primitive as
warmup (a `deploy:hook` KV event consumed once per node), so it rides
along nearly free.

## Sequencing

Warmup (1) is the natural headliner — it completes the story the
benchmark tells, reuses the Phase-2 design nearly verbatim, and has a
measurable before/after. Doctor (3) is independent and could ship any
release. Status (2) is small and unblocks deploy verification. Hooks
(4) ride on warmup's fanout primitive.

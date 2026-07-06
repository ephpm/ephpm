+++
title = "WordPress Worker Mode"
weight = 9
aliases = ["/roadmap/wordpress-worker-mode/"]
+++

ePHPm 3.0 ships a **WordPress worker-mode adapter**: boot WordPress core and
all plugins once per worker thread, then serve requests in a loop — the
30–80 ms `wp-settings.php` bootstrap happens once per worker, not once per
request. The adapter lives at
[github.com/ephpm/wordpress-worker](https://github.com/ephpm/wordpress-worker)
and provides the worker entrypoint `bin/ephpm-wp-worker`.

WordPress has no service container or reset contract, so this adapter is
opinionated: it resets the per-request state WordPress core recomputes from the
URL/headers/body (`$wp_query`, `$wp`, `$post`, superglobals, current user) and
leaves worker-lifetime state (hook registry, post types, rewrite rules, the
object cache) alone.

ePHPm's PHP packages are distributed via their GitHub repositories (not
Packagist). Install them by adding each repo in the dependency tree as a
Composer `vcs` repository.

## 1. Install the adapter

In a Composer-managed WordPress root, add every ePHPm repo in the tree to
`composer.json`. The adapter depends on `ephpm/worker`, so **both** repos are
listed — Composer does **not** resolve a VCS dependency's own VCS repositories
transitively, so each ePHPm package needs its own `repositories` entry:

```json
// composer.json
{
  "repositories": [
    { "type": "vcs", "url": "https://github.com/ephpm/wordpress-worker" },
    { "type": "vcs", "url": "https://github.com/ephpm/php-worker" }
  ],
  "require": {
    "ephpm/wordpress-worker": "^0.1"
  }
}
```

Both `ephpm/wordpress-worker` and its `ephpm/worker` dependency are tagged
`v0.1.0`, so `^0.1` resolves for each; each still needs its own `repositories`
entry because Composer does not resolve VCS repos transitively. Then:

```bash
composer update
```

This installs the entrypoint at `vendor/bin/ephpm-wp-worker`. (The engine skips
`#!/usr/bin/env php` shebang lines in worker scripts, so Composer bin proxies
work directly as `worker_script`.)

## 2. Configure ePHPm

```toml
# /etc/ephpm/ephpm.toml
[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/wordpress"

[php]
mode = "worker"
worker_script = "vendor/bin/ephpm-wp-worker"
worker_populate_superglobals = true      # REQUIRED for WordPress
```

`worker_populate_superglobals = true` is **required**: unlike Octane/PSR-15
adapters (which build their own request object from the `Envelope`), WordPress
assumes real `$_GET`/`$_POST`/`$_SERVER`/`$_COOKIE` superglobals. ePHPm
repopulates them per request through PHP's normal `treat_data` path.

`worker_script` must resolve to a file under `document_root`, so the adapter
must be installed inside the WordPress root.

## Sizing and recycling

A booted WordPress kernel is heavy (~40 MB per worker) — you may want an
explicit `worker_count` lower than the CPU-derived default:

```toml
[php]
worker_count = 4          # 0 = derive from CPU count, clamped [2, 32]
worker_max_requests = 500 # recycle after N requests (0 = never)
```

Recycling matters more for WordPress than for container-based frameworks:

- **Plugin/theme updates and `wp-config.php` edits do not take effect in
  already-booted workers.** The old code stays loaded until the worker
  recycles (`worker_max_requests`) or you restart ePHPm. Restart after
  updates if you can't wait for the recycle cycle.
- A fatal inside a hook, or a plugin calling `exit()`/`die()` mid-request
  (WordPress does this routinely — redirects, `wp_die()`), never wedges the
  server: ePHPm synthesizes the response from the SAPI headers and captured
  output, then recycles the worker with a clean boot.

## Object cache

Pair worker mode with the
[`ephpm/cache-wordpress`](https://github.com/ephpm/cache-wordpress) drop-in so
`wp_cache_*` calls hit the embedded KV store — the cache persists across
requests and workers, and replicates across nodes in cluster mode. See the
[WordPress guide](/guides/wordpress/) for drop-in installation.

## Limitations

- Worker mode is a whole-server switch; it is **not supported with
  `[server] sites_dir`** (config load hard-errors), so multi-tenant vhosting —
  including WordPress multisite behind vhosts — stays on fpm mode for now.
- Plugins that assume process death for cleanup (e.g. `pcntl_fork`-based
  backup plugins) are not compatible with any worker-mode runtime. If a plugin
  misbehaves under worker mode, run that site in the default fpm mode — it
  remains fully supported and byte-for-byte identical to previous releases.

## See also

- [WordPress guide](/guides/wordpress/) — classic (fpm-mode) deployment paths
- [Config reference — `[php]`](/reference/config/) — authoritative worker knobs
- [Metrics reference](/reference/metrics/) — `ephpm_worker_*` series

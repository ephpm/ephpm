+++
title = "Guides"
type = "docs"
weight = 3
+++

Task-oriented walkthroughs for common deployments.

- **[WordPress](wordpress/)** — drop in a WordPress install with no PHP-FPM.
- **[Laravel](laravel/)** — Laravel with embedded SQLite or MySQL passthrough.
- **[Virtual Hosts](virtual-hosts/)** — multi-tenant directory-based hosting.
- **[TLS / ACME](tls-acme/)** — automatic Let's Encrypt certificates.
- **[Clustering Setup](clustering-setup/)** — gossip-based HA with clustered SQLite.
- **[Database from PHP](db-from-php/)** — the in-process `ephpm_db_*` bridge, and how it relates to the `pdo_mysql` wire path.
- **[KV from PHP](kv-from-php/)** — the `ephpm_kv_*` SAPI functions.
- **[Query Stats with Prometheus](query-stats-prometheus/)** — observability for your database queries.
- **[Worker Mode (Write Your Own Worker)](worker-mode/)** — the engine primitives: boot once, loop on `take_request()`/`send_response()`.
- **[Laravel Octane (Worker Mode)](laravel-octane/)** — boot Laravel once per worker with the native Octane driver.
- **[WordPress Worker Mode](wordpress-worker/)** — boot WordPress once per worker with the `ephpm/wordpress-worker` adapter.
- **[PSR-15 Apps (Worker Mode)](psr15-worker/)** — Slim, Mezzio, or any PSR-15 handler on the generic `ephpm/psr15-worker` adapter.
- **[Native Middleware](native-middleware/)** — compiled `.so` middleware (JWT, CORS, rate limiting, security headers) running in front of PHP, with KV-backed cluster-wide state.
- **[PHP Extensions](php-extensions/)** — loading prebuilt shared extensions beyond the compiled-in set.
- **[Cluster-Wide OPcache Invalidation](opcache-cluster-invalidation/)** — `ephpm deploy` and the deploys-are-events contract.
- **[Diagnosing Crashes](diagnosing-crashes/)** — reading the fatal-signal report when an extension, a middleware, or PHP itself faults.

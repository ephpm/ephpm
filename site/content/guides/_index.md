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
- **[KV from PHP](kv-from-php/)** — the `ephpm_kv_*` SAPI functions.
- **[Query Stats with Prometheus](query-stats-prometheus/)** — observability for your database queries.
- **[Laravel Octane (Worker Mode)](laravel-octane/)** — boot Laravel once per worker with the native Octane driver.
- **[WordPress Worker Mode](wordpress-worker/)** — boot WordPress once per worker with the `ephpm/wordpress-worker` adapter.
- **[PSR-15 Apps (Worker Mode)](psr15-worker/)** — Slim, Mezzio, or any PSR-15 handler on the generic `ephpm/psr15-worker` adapter.

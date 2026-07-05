+++
title = "Roadmap"
type = "docs"
weight = 10
+++

Forward-looking design documents — what we're planning, what we're considering, and what we've decided not to do (yet).

These pages describe targets, not currently-shipped behavior. For what works today, see [Architecture](/docs/architecture/) and [Feature Status](/docs/introduction/feature-status/).

- **[Preview Deployments](preview/)** — instant per-PR preview URLs via a GitHub bot.
- **[OPcache Clustering & Per-Vhost Preload](opcache-clustering/)** — atomic cluster-wide OPcache invalidation via the KV store, plus per-vhost preload via `site.toml`.
- **[Dynamic PHP Extensions](dynamic-extensions/)** — load standard PHP extensions (`.so` / `.dll`) at startup from `site.toml`, the same way `extension=foo.so` works in `php.ini`. Complements the static baseline.
- **[Symfony Runtime Adapter](symfony-runtime-driver/)** — native `ephpm` adapter under `symfony/runtime`, on top of the shipped worker-mode engine.
- **[Kubernetes Operator](kubernetes/)** — first-class K8s deployment.
- **[Edge Deployments](edge/)** — running ePHPm at the edge.
- **[Hosting Models](hosting/)** — how ePHPm could be packaged for cloud providers.
- **[Webserver Feature Parity](webserver-feature-parity/)** — Apache/Nginx feature gap analysis.

Worker mode itself, the Laravel Octane driver, the WordPress worker adapter, and the generic PSR-15 adapter **shipped in 3.0** and moved out of the roadmap — see [Laravel Octane (Worker Mode)](/guides/laravel-octane/), [WordPress Worker Mode](/guides/wordpress-worker/), and [PSR-15 Apps (Worker Mode)](/guides/psr15-worker/). The native middleware loader (v1: chain semantics, C ABI, Rust authoring kit, and the `jwt` / `cors` / `ratelimit` / `security-headers` modules) also **shipped** and moved to [Native Middleware](/guides/native-middleware/).

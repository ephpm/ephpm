+++
title = "Roadmap"
type = "docs"
weight = 10
+++

Forward-looking design documents — what we're planning, what we're considering, and what we've decided not to do (yet).

These pages describe targets, not currently-shipped behavior. For what works today, see [Architecture](/docs/architecture/) and [Feature Status](/docs/introduction/feature-status/).

- **[Preview Deployments](preview/)** — instant per-PR preview URLs via a GitHub bot.
- **[OPcache Clustering & Per-Vhost Preload](opcache-clustering/)** — atomic cluster-wide OPcache invalidation via the KV store, plus per-vhost preload via `site.toml`.
- **[Laravel Octane Driver](laravel-octane-driver/)** — native `ephpm` driver for Laravel Octane worker mode.
- **[Symfony Runtime Adapter](symfony-runtime-driver/)** — native `ephpm` adapter under `symfony/runtime`.
- **[WordPress Worker Mode](wordpress-worker-mode/)** — opinionated WP runtime that boots once per worker thread.
- **[PSR-15 Worker Mode](psr-15-worker-mode/)** — generic adapter for Mezzio, Slim, and any PSR-15 framework.
- **[Kubernetes Operator](kubernetes/)** — first-class K8s deployment.
- **[Edge Deployments](edge/)** — running ePHPm at the edge.
- **[Hosting Models](hosting/)** — how ePHPm could be packaged for cloud providers.
- **[Webserver Feature Parity](webserver-feature-parity/)** — Apache/Nginx feature gap analysis.

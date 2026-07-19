+++
title = "Roadmap"
type = "docs"
weight = 10
+++

Forward-looking design documents — what we're planning, what we're considering, and what we've decided not to do (yet).

These pages describe targets, not currently-shipped behavior. For what works today, see [Architecture](/docs/architecture/) and [Feature Status](/docs/introduction/feature-status/).

- **[Performance Master List](performance/)** — every measured or audited performance item in one living table: shipped (with numbers), backlog, gated, unexplored.
- **[Worker Dispatch Fast Path](worker-dispatch-fastpath/)** — closing the measured gap to in-process runtimes: lazy Envelope, handoff economics, quota-aware defaults.
- **[Turso Engine](turso-engine/)** — one engine for both modes: the Rust SQLite rewrite replacing rusqlite and the sqld sidecar, gated on upstream GA.
- **[Clustered KV v2](clustered-kv-v2/)** — owner-routed counters and cluster-correct rate limiting (delete tombstones and TTL replication shipped as v1.1).
- **[SSE & Realtime](sse-realtime/)** — `ephpm_kv_wait()` and streaming brotli **shipped**; the render-once/fan-out SSE hub that decouples viewer count from `worker_count` is the v0.6.0 target.
- **[Benchmarks as a Release Artifact](benchmarks/)** — in-tree bench recipes, per-release numbers, and a regression gate.
- **[NTS Prefork Mode](nts-prefork/)** — trading features for pure per-request PHP speed, gated on a post-PGO measurement.
- **[The Deploy Story](deploy-warmup/)** — post-invalidation warmup, `ephpm doctor <framework>`, `cache status`, thin deploy hooks.
- **[Preview Deployments](preview/)** — instant per-PR preview URLs via a GitHub bot.
- **[OPcache Clustering & Per-Vhost Preload](opcache-clustering/)** — Phase 1 (cluster-wide invalidation) shipped in 0.4.0; per-vhost preload and worker-mode invalidation remain.
- **[Symfony Runtime Adapter](symfony-runtime-driver/)** — native `ephpm` adapter under `symfony/runtime`, on top of the shipped worker-mode engine.
- **[Kubernetes Operator](kubernetes/)** — first-class K8s deployment.
- **[Edge Deployments](edge/)** — running ePHPm at the edge.
- **[Hosting Models](hosting/)** — how ePHPm could be packaged for cloud providers.
- **[Webserver Feature Parity](webserver-feature-parity/)** — Apache/Nginx feature gap analysis.

Worker mode itself, the Laravel Octane driver, the WordPress worker adapter, and the generic PSR-15 adapter **shipped in 3.0** and moved out of the roadmap — see [Laravel Octane (Worker Mode)](/guides/laravel-octane/), [WordPress Worker Mode](/guides/wordpress-worker/), and [PSR-15 Apps (Worker Mode)](/guides/psr15-worker/). The native middleware loader (v1: chain semantics, C ABI, Rust authoring kit, and the `jwt` / `cors` / `ratelimit` / `security-headers` modules) also **shipped** and moved to [Native Middleware](/guides/native-middleware/). Shared PHP extension loading (`[php] extensions`) **shipped** with the Linux glibc-dynamic release pivot and moved to [PHP Extensions](/guides/php-extensions/); only the per-vhost scoping and build-helper pieces remain future work.

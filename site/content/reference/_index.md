+++
title = "Reference"
type = "docs"
weight = 5
+++

Exhaustive reference for every CLI command, configuration key, and metric.

- **[CLI](cli/)** — `ephpm serve`, `dev`, `php`, `kv`, `deploy`, `cache`, and the service commands (`install`, `start`, `stop`, `restart`, `status`, `logs`, `uninstall`).
- **[Configuration](config/)** — every key in `ephpm.toml`, with type, default, and example.
- **[Environment Variables](environment-variables/)** — `EPHPM_*` overrides.
- **[Metrics](metrics/)** — every Prometheus metric exposed at `/metrics`.
- **[Built-in Middleware](middleware/)** — the twelve compiled-in middleware modules: spellings, phases, failure modes, `config` keys, and the [`ratelimit`](middleware/ratelimit/) reference.
- **[PHP Packages](php-packages/)** — the Composer adapters, drop-ins, and shims for the DB bridge, KV store, and worker mode.

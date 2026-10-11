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
- **[PHP API](php-api/)** — every native function and class ePHPm adds to PHP, the `$_SERVER` variables it injects, and the ini settings it writes (or leaves at PHP's defaults).
- **[PHP Packages](php-packages/)** — the Composer adapters, drop-ins, and shims for the DB bridge, KV store, and worker mode.

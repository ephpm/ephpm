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
- **[HTTP Endpoints & Headers](http-endpoints/)** — the server-owned paths (`/metrics`, `/_ephpm/*`), who can reach them, request-processing order, and the request and response headers ePHPm reads and sets.
- **[KV RESP Protocol](kv-resp/)** — every Redis-protocol command the KV listener accepts, its authentication modes, and what is not implemented.
- **[PHP Packages](php-packages/)** — the Composer adapters, drop-ins, and shims for the DB bridge, KV store, and worker mode.

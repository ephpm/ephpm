+++
title = "Metrics"
weight = 4
+++

Every metric ePHPm exposes at `/metrics`. Enable with:

```toml
[server.metrics]
enabled = true
# path = "/metrics"          # default
```

When `enabled = false`, all metric calls are zero-cost no-ops — there's no overhead from leaving instrumentation in the code paths.

Metrics are emitted via the [`metrics`](https://docs.rs/metrics/) façade and exported through [`metrics_exporter_prometheus`](https://docs.rs/metrics-exporter-prometheus/).

## Build info

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_build_info` | gauge | `version` | Constant `1`. Useful for joining build versions to other queries. |

## HTTP

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_http_requests_total` | counter | `method`, `status`, `handler` | Total HTTP requests handled. `handler` is the route class (e.g. `php`, `static`, `error`). To keep series cardinality bounded, `method` is one of the standard verbs (`GET`, `POST`, ...) or `OTHER` for any non-standard verb, and `status` is the numeric code for the common statuses this server and typical apps emit or `other` for uncommon codes. |
| `ephpm_http_requests_in_flight` | gauge | — | Currently in-flight HTTP requests. |
| `ephpm_http_request_duration_seconds` | histogram | `method`, `handler` | Request handling time, end-to-end (no `status` label). |
| `ephpm_http_request_body_bytes` | histogram | `method` | Request body size. `method` is bounded the same way as above (standard verb or `OTHER`). |
| `ephpm_http_response_body_bytes` | histogram | `handler` | Response body size before compression. Recorded on the PHP path only (`handler="php"`). |
| `ephpm_http_compression_ratio` | histogram | — | Compressed-to-original ratio; covers both Brotli and gzip responses. |
| `ephpm_http_timeouts_total` | counter | `stage` | Requests killed by the request timeout. Only value: `request`. |
| `ephpm_rate_limited_total` | counter | — | Rejections from `[server.limits]`. Incremented only for per-IP rate limiting. |

## HTTP/3 (QUIC)

Emitted only when `[server.http3] enabled = true`; the series are absent otherwise. HTTP/3 requests **also** increment every `ephpm_http_*` metric above — the shared request pipeline records those regardless of transport — so these are transport-level counters on top, not a separate accounting.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_http3_connections_total` | counter | — | QUIC connections that completed their handshake. |
| `ephpm_http3_connections_active` | gauge | — | Currently established QUIC connections. |
| `ephpm_http3_connection_errors_total` | counter | `stage` | Per-connection failures. `stage="handshake"` (bad ALPN, version negotiation, client gave up before the handshake completed) or `stage="session"` (the HTTP/3 session failed after the handshake). |
| `ephpm_http3_requests_total` | counter | — | Requests accepted on HTTP/3 streams. |
| `ephpm_http3_request_duration_seconds` | histogram | — | HTTP/3 request time including writing the response body out over QUIC. Uses the same buckets as `ephpm_http_request_duration_seconds`, so the two overlay directly. |
| `ephpm_http3_stripped_headers_total` | counter | — | Connection-specific response header names removed before sending (RFC 9114 §4.2 forbids `Connection`, `Keep-Alive`, `Proxy-Connection`, `Transfer-Encoding`, `Upgrade` in HTTP/3). Non-zero means an app or a `[server.response] headers` entry is emitting a header that is meaningless over HTTP/3 — worth fixing at the source. |

A rising `ephpm_http3_connection_errors_total{stage="handshake"}` with `ephpm_http3_connections_total` flat usually means UDP is being blocked on the path — clients are trying HTTP/3 and falling back to TCP.

## PHP

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_php_executions_total` | counter | `status` | PHP requests executed. `status` is `ok` or `error`. Timeouts surface as HTTP 504 in the HTTP metrics, not here. |
| `ephpm_php_execution_duration_seconds` | histogram | — | Time spent inside the PHP runtime, per request. |
| `ephpm_php_output_bytes` | histogram | — | Bytes emitted by PHP per request. |

## Native middleware

These appear when at least one `[[middleware]]` mount is configured.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_middleware_invocations_total` | counter | `module`, `action` | Middleware invocations, one per module per matching request. `action` is the verdict: `continue`, `respond`, or `rewrite`. A module `invoke` error (non-zero return, including a caught panic) counts as `respond` — the host fails closed with a 500. |

## Worker mode

These appear when `[php] mode = "worker"`.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_worker_pool_size` | gauge | — | Configured number of persistent worker threads, set at pool startup. |
| `ephpm_worker_busy` | gauge | — | Dispatched requests awaiting a worker response. Includes jobs still sitting in the dispatch queue, so it can exceed `worker_count` when the backlog is deep. |
| `ephpm_worker_idle` | gauge | — | Workers parked in `take_request()` waiting for work (recorded inside the dispatch recv; only moves in PHP-linked builds). |
| `ephpm_worker_dispatch_queue_depth` | gauge | — | Jobs sitting in the dispatch queue, sampled at each dispatch. |
| `ephpm_worker_request_wait_seconds` | histogram | — | Time a request spent waiting to enter the dispatch queue (backpressure when the queue is full). |
| `ephpm_worker_boot_duration_seconds` | histogram | — | Time from worker-thread start to the framework's first `take_request()` (i.e. framework boot time). |
| `ephpm_worker_boot_timeouts_total` | counter | — | Boots still running when `worker_boot_timeout` expired. The thread is not killed; it still becomes ready if the boot completes. |
| `ephpm_worker_boot_failures_total` | counter | — | Worker boots that failed (thread spawn/TSRM init failure, or the script exited before its first `take_request()`). The pool respawns with exponential backoff. |
| `ephpm_worker_recycles_total` | counter | `reason` | Workers recycled. `reason` is `max_requests` (hit `worker_max_requests`), `script_exit` (script called `exit()`/`die()` mid-request), `fatal` (fatal error / bailout), or `hung` (never responded within the request timeout; replaced). |

## OPcache clustering

These appear when a cluster-wide OPcache invalidation actually fires. When
`[opcache] cluster_invalidation = false`, or the watcher never sees an
`opcache:version:*` change, the counter has no series.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_opcache_invalidations_total` | counter | `vhost`, `trigger` | Cluster-wide OPcache invalidations run for a vhost. `trigger` is always `kv` today — both `ephpm deploy` and `ephpm cache reset` arrive via the KV version key. The planned file watcher (roadmap Phase 3) will add a second value. |

## Database (proxy upstream health)

These appear when `[db.mysql]` or `[db.postgres]` is configured. Labels: `db`
(`mysql` / `postgres`) and `upstream` (the resolved `host:port` — never the
URL, which carries credentials).

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_db_proxy_upstream_ever_connected` | gauge | `db`, `upstream` | `1` once the proxy has completed one upstream handshake since boot. Latches — it never returns to `0`. While it is `0`, `/_ephpm/ready` reports 503. |
| `ephpm_db_proxy_upstream_up` | gauge | `db`, `upstream` | `1` if the most recent upstream connect attempt succeeded, `0` if it failed. Flaps with the database. |
| `ephpm_db_proxy_connect_failures_total` | counter | `db`, `upstream` | Upstream connect/handshake failures. |

`ephpm_db_proxy_upstream_up` is the metric to alert on. Readiness deliberately
does **not** flap with it — a shared database going down would otherwise fail
every replica's probe at the same instant and empty the Service. See
[Readiness and the database proxy](/reference/config/#readiness-and-the-database-proxy).

```promql
# A proxy that never came up: the pod is not in rotation and never will be.
ephpm_db_proxy_upstream_ever_connected == 0

# A database outage on a pod that IS still in rotation, serving 500s.
ephpm_db_proxy_upstream_up == 0 and ephpm_db_proxy_upstream_ever_connected == 1
```

## Database (query stats)

These appear when `[db.analysis] query_stats = true` (the default).

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_query_total` | counter | `digest`, `kind`, `status` | Queries executed. `kind` is `query`/`mutation`. `status` is `ok`/`error`. |
| `ephpm_query_duration_seconds` | histogram | `digest`, `kind` | Per-query execution time. |
| `ephpm_query_rows_total` | counter | `digest`, `kind` | Rows returned (queries) or affected (mutations). |
| `ephpm_query_slow_total` | counter | — | Queries exceeding `[db.analysis] slow_query_threshold`. |
| `ephpm_query_active_digests` | gauge | — | Distinct query digests currently tracked. Bounded by `digest_store_max_entries`. |

`digest` is the **normalized SQL** (literals replaced with `?`), truncated to 64 characters for label safety. Cardinality scales with distinct query *shapes*, not executions.

These series are emitted by both the embedded SQLite paths and the MySQL/PostgreSQL proxies, with **no label distinguishing them** — one collector, one metrics surface. That is worth knowing when reading `ephpm_query_duration_seconds`: the embedded paths time in-process execution, while the proxies time a wire round trip to your database server (command written to the backend socket → last response byte read back), which includes network latency. If a deployment runs a proxy and the embedded engine at the same time, the two are mixed under one metric name.

`ephpm_query_rows_total` is likewise narrower on the proxy paths, which report `0` where they cannot count rows rather than estimating: the default MySQL path has no row visibility at all, and the PostgreSQL path counts rows *returned* but not rows *affected* by a mutation. Statement coverage per path — including what the proxies deliberately do not record, such as prepared-statement executes and PostgreSQL extended-protocol traffic — is tabulated under [`[db.analysis]`](/reference/config/#dbanalysis) in the config reference.

## Cardinality notes

The per-metric `digest` label series is **capped** — by default at 1,000 distinct label values per process (`StatsConfig::metric_label_series_max`). Every additional distinct digest observed after the cap is exhausted has its Prometheus emissions folded into a single shared `digest="__other__"` bucket. Internal tracking (`top_queries()`, the digest table, the slow-query log) is **not** affected by this cap and still exposes the real normalized SQL — only the Prometheus label surface is bounded.

The internal digest table itself is bounded separately by `[db.analysis] digest_store_max_entries` (default 100,000). That knob controls how many distinct digests are held in memory for `top_queries()`; the label-series cap above controls Prometheus cardinality.

The cap is configurable: `[db.analysis] metric_label_series_max` (default `1000`, `0` = unlimited). If your Prometheus is unhappy regardless, set `query_stats = false` to disable the metrics entirely.

The `path`-style labels you might expect on HTTP metrics (`/users/123`) are deliberately *not* present — Prometheus' best-practice is to keep label cardinality bounded, and request paths in PHP apps explode it. Use the slow-query log + tracing for path-level debugging.

## Histogram buckets

Buckets are custom per metric — configured with `Matcher::Full` rules in [`crates/ephpm-server/src/metrics.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-server/src/metrics.rs), not the `metrics_exporter_prometheus` builder defaults:

- Duration histograms (`ephpm_http_request_duration_seconds`, `ephpm_http3_request_duration_seconds`, `ephpm_php_execution_duration_seconds`, `ephpm_worker_request_wait_seconds`): 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 seconds
- `ephpm_worker_boot_duration_seconds`: 0.01, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 20, 30 seconds (framework boot can take seconds)
- Body-size histograms (`ephpm_http_request_body_bytes`, `ephpm_http_response_body_bytes`, `ephpm_php_output_bytes`): 100 B, 1 KB, 10 KB, 50 KB, 100 KB, 500 KB, 1 MB, 5 MB, 10 MB
- `ephpm_http_compression_ratio`: 0.05 through 0.9

## See also

- [Query Stats with Prometheus](/guides/query-stats-prometheus/) — practical PromQL queries
- [Architecture → Query Stats](/architecture/query-stats/) — how the digest normalizer works
- [Architecture → Metrics](/architecture/metrics/) — design rationale

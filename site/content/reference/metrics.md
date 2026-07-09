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
| `ephpm_http_requests_total` | counter | `method`, `status`, `handler` | Total HTTP requests handled. `handler` is the route class (e.g. `php`, `static`, `error`). |
| `ephpm_http_requests_in_flight` | gauge | — | Currently in-flight HTTP requests. |
| `ephpm_http_request_duration_seconds` | histogram | `method`, `handler` | Request handling time, end-to-end (no `status` label). |
| `ephpm_http_request_body_bytes` | histogram | `method` | Request body size. |
| `ephpm_http_response_body_bytes` | histogram | `handler` | Response body size before compression. Recorded on the PHP path only (`handler="php"`). |
| `ephpm_http_compression_ratio` | histogram | — | Compressed-to-original ratio; covers both Brotli and gzip responses. |
| `ephpm_http_timeouts_total` | counter | `stage` | Requests killed by the request timeout. Only value: `request`. |
| `ephpm_rate_limited_total` | counter | — | Rejections from `[server.limits]`. Incremented only for per-IP rate limiting. |

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

## Cardinality notes

The per-metric `digest` label series is **capped** — by default at 1,000 distinct label values per process (`StatsConfig::metric_label_series_max`). Every additional distinct digest observed after the cap is exhausted has its Prometheus emissions folded into a single shared `digest="__other__"` bucket. Internal tracking (`top_queries()`, the digest table, the slow-query log) is **not** affected by this cap and still exposes the real normalized SQL — only the Prometheus label surface is bounded.

The internal digest table itself is bounded separately by `[db.analysis] digest_store_max_entries` (default 100,000). That knob controls how many distinct digests are held in memory for `top_queries()`; the label-series cap above controls Prometheus cardinality.

If you need finer-grained control (raise or lower the label cap), that value is currently a build-time default and not yet a config knob — planned. In the meantime, if your Prometheus is unhappy, set `query_stats = false` to disable the metrics entirely.

The `path`-style labels you might expect on HTTP metrics (`/users/123`) are deliberately *not* present — Prometheus' best-practice is to keep label cardinality bounded, and request paths in PHP apps explode it. Use the slow-query log + tracing for path-level debugging.

## Histogram buckets

Buckets are custom per metric — configured with `Matcher::Full` rules in [`crates/ephpm-server/src/metrics.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-server/src/metrics.rs), not the `metrics_exporter_prometheus` builder defaults:

- Duration histograms (`ephpm_http_request_duration_seconds`, `ephpm_php_execution_duration_seconds`, `ephpm_worker_request_wait_seconds`): 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 seconds
- `ephpm_worker_boot_duration_seconds`: 0.01, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 20, 30 seconds (framework boot can take seconds)
- Body-size histograms (`ephpm_http_request_body_bytes`, `ephpm_http_response_body_bytes`, `ephpm_php_output_bytes`): 100 B, 1 KB, 10 KB, 50 KB, 100 KB, 500 KB, 1 MB, 5 MB, 10 MB
- `ephpm_http_compression_ratio`: 0.05 through 0.9

## See also

- [Query Stats with Prometheus](/guides/query-stats-prometheus/) — practical PromQL queries
- [Architecture → Query Stats](/architecture/query-stats/) — how the digest normalizer works
- [Architecture → Metrics](/architecture/metrics/) — design rationale

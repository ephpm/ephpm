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

`digest`-labeled series are bounded by `[db.analysis] digest_store_max_entries` (default 100,000). If your Prometheus is unhappy with the cardinality, lower that limit or set `query_stats = false`.

The `path`-style labels you might expect on HTTP metrics (`/users/123`) are deliberately *not* present — Prometheus' best-practice is to keep label cardinality bounded, and request paths in PHP apps explode it. Use the slow-query log + tracing for path-level debugging.

## Histogram buckets

Buckets are custom per metric — configured with `Matcher::Full` rules in [`crates/ephpm-server/src/metrics.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-server/src/metrics.rs), not the `metrics_exporter_prometheus` builder defaults:

- Duration histograms (`ephpm_http_request_duration_seconds`, `ephpm_php_execution_duration_seconds`): 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 seconds
- Body-size histograms (`ephpm_http_request_body_bytes`, `ephpm_http_response_body_bytes`, `ephpm_php_output_bytes`): 100 B, 1 KB, 10 KB, 50 KB, 100 KB, 500 KB, 1 MB, 5 MB, 10 MB
- `ephpm_http_compression_ratio`: 0.05 through 0.9

## See also

- [Query Stats with Prometheus](/guides/query-stats-prometheus/) — practical PromQL queries
- [Architecture → Query Stats](/architecture/query-stats/) — how the digest normalizer works
- [Architecture → Metrics](/architecture/metrics/) — design rationale

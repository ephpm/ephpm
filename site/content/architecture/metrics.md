# Observability: Prometheus Metrics

ePHPm exports Prometheus metrics when `[server.metrics] enabled = true`.
The metrics endpoint defaults to `/metrics` (configurable via `path`).

---

## Configuration

```toml
[server.metrics]
enabled = true
path = "/metrics"     # default
```

Or via environment variables:

```bash
EPHPM_SERVER__METRICS__ENABLED=true
EPHPM_SERVER__METRICS__PATH="/metrics"
```

---

## Exported Metrics

### Build Info

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_build_info` | gauge | `version` | Always `1.0`. Carries the binary version as a label. |

### HTTP Request Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_http_requests_total` | counter | `method`, `status`, `handler` | Total HTTP requests processed. `handler` is `"php"`, `"static"`, `"health"`, or `"error"`. |
| `ephpm_http_request_duration_seconds` | histogram | `method`, `handler` | End-to-end request duration (includes PHP execution for PHP requests). |
| `ephpm_http_requests_in_flight` | gauge | — | Number of requests currently being processed. |
| `ephpm_http_timeouts_total` | counter | `stage` | Requests that hit the timeout. `stage` is `"request"`. |
| `ephpm_http_request_body_bytes` | histogram | `method` | Request body size in bytes. |
| `ephpm_http_response_body_bytes` | histogram | `handler` | Response body size in bytes (before compression). |
| `ephpm_http_compression_ratio` | histogram | — | Compression ratio (compressed / original). Values near 0 = excellent compression. |
| `ephpm_rate_limited_total` | counter | — | Requests rejected by rate limiting. |

### PHP Execution Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_php_execution_duration_seconds` | histogram | — | Time spent executing PHP code (excludes body read, response write). |
| `ephpm_php_executions_total` | counter | `status` | PHP executions by result. `status` is `"ok"` or `"error"`. |
| `ephpm_php_output_bytes` | histogram | — | Raw PHP output size in bytes (before compression). |

### Query Stats Metrics (from `ephpm-query-stats`)

Enabled when `[db.analysis] query_stats = true` (default). These metrics track
SQL queries flowing through the DB proxy or litewire.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_query_duration_seconds` | histogram | `digest`, `kind` | Per-query execution time. `kind` is `"read"` or `"write"`. `digest` is the normalized SQL. |
| `ephpm_query_total` | counter | `digest`, `kind`, `status` | Query count by digest. `status` is `"ok"` or `"error"`. |
| `ephpm_query_rows_total` | counter | `digest`, `kind` | Total rows returned/affected per digest. |
| `ephpm_query_slow_total` | counter | — | Number of queries exceeding the slow query threshold. |
| `ephpm_query_active_digests` | gauge | — | Number of unique query digests currently tracked. |

---

## Histogram Buckets

Custom bucket configurations are tuned for PHP workloads:

| Metric | Buckets |
|--------|---------|
| `ephpm_http_request_duration_seconds` | 1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s |
| `ephpm_php_execution_duration_seconds` | 1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s |
| `ephpm_php_mutex_wait_seconds` | 0.1ms, 0.5ms, 1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms |
| Body size histograms | 100B, 1KB, 10KB, 50KB, 100KB, 500KB, 1MB, 5MB, 10MB |
| `ephpm_http_compression_ratio` | 0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9 |

---

## Scraping

The metrics endpoint returns `text/plain; version=0.0.4` (standard Prometheus
text format). Example scrape output:

```
# HELP ephpm_build_info Build information
# TYPE ephpm_build_info gauge
ephpm_build_info{version="0.1.0"} 1
# HELP ephpm_http_requests_total Total HTTP requests
# TYPE ephpm_http_requests_total counter
ephpm_http_requests_total{method="GET",status="200",handler="php"} 42
ephpm_http_requests_total{method="GET",status="200",handler="static"} 150
# HELP ephpm_http_request_duration_seconds Request duration
# TYPE ephpm_http_request_duration_seconds histogram
ephpm_http_request_duration_seconds_bucket{method="GET",handler="php",le="0.01"} 5
...
```

---

## Disabling Metrics

Set `enabled = false` (the default). When disabled, all `metrics` facade calls
are zero-cost no-ops — there is no overhead from unused metric instrumentation.

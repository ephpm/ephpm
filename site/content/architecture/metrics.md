# Observability: Prometheus Metrics

ePHPm exports Prometheus metrics when `[server.metrics] enabled = true`.
The metrics endpoint defaults to `/metrics` (configurable via `path`).

This page covers the design and the core families only. The
[metrics reference](/reference/metrics/) is the complete, authoritative list:
it also covers HTTP/3, WebSockets, the FPM and worker pools, middleware,
OPcache, the JIT, the DB proxy, the reverse proxy and CDC replication.

The endpoint answers for every `Host`, ahead of `trusted_hosts` and
middleware. It is open by default on a single-site node and needs a token or
mTLS on a multi-site node; see
[HTTP Endpoints → Exposure](/reference/http-endpoints/#exposure-read-this-first).

---

## Configuration

```toml
[server.metrics]
enabled = true
path = "/metrics"     # default
```

Access control (IP ACL, bearer token, separate mTLS listener, and the token
requirement on multi-site nodes) is described under
[access to internal endpoints](/reference/config/#access-to-internal-endpoints);
Prometheus scrape configs are in
[Scraping with Prometheus](/reference/metrics/#scraping-with-prometheus).

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
| `ephpm_http_requests_total` | counter | `method`, `status`, `handler` | Total HTTP requests processed. `handler` is one of `php`, `static`, `middleware`, `error`, `health`, `metrics`, `diagnostics`, `acme`, `proxy`, `websocket`, `auth`, `site_override_unusable` ([value table](/reference/metrics/#handler-label-values)). |
| `ephpm_http_request_duration_seconds` | histogram | `method`, `handler` | End-to-end request duration (includes PHP execution for PHP requests). |
| `ephpm_http_requests_in_flight` | gauge | — | Number of requests currently being processed. |
| `ephpm_http_timeouts_total` | counter | `stage` | Requests that hit the request deadline. `stage` is `"fpm_pool"` or `"worker"` (waiting on PHP; a thread that had started the request is also marked hung and replaced) or `"request"` (waiting on anything else). See [Metrics reference](/reference/metrics/#http). |
| `ephpm_http_body_overflow_total` | counter | — | `413`s raised while reading an over-limit request body. |
| `ephpm_http_request_body_bytes` | histogram | `method` | Request body size in bytes. `method` is a standard verb or `OTHER`, never the client's raw verb. |
| `ephpm_http_response_body_bytes` | histogram | `handler` | Response body size in bytes (before compression). |
| `ephpm_http_compression_ratio` | histogram | — | Compression ratio (compressed / original). Values near 0 = excellent compression. |
| `ephpm_rate_limited_total` | counter | — | Requests rejected by per-IP rate limiting. |
| `ephpm_site_rate_limited_total` | counter | — | PHP requests rejected by the per-site rate limit (`[server.limits] per_site_rate`). |
| `ephpm_site_override_unusable_total` | counter | — | Requests refused (503) because that vhost's per-site override file exists but cannot be honoured. Deliberately fails closed rather than falling back to serving the site container, which would be wider than the override asked for. |

### PHP Execution Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_php_execution_duration_seconds` | histogram | — | Time from dispatch to the PHP pool until the response comes back. **Includes the dispatch-queue wait**, so it rises under saturation even when PHP itself is fast. Excludes the body read and the response write. |
| `ephpm_php_executions_total` | counter | `status` | PHP executions by result. `status` is `"ok"` or `"error"`. |
| `ephpm_php_output_bytes` | histogram | — | Raw PHP output size in bytes (before compression). |

### Query Stats Metrics (from `ephpm-query-stats`)

Enabled when `[db.analysis] query_stats = true` (default). These metrics track
SQL queries flowing through the DB proxy or litewire, sharing one collector and
one set of label dimensions — there is no label identifying which path a sample
came from. Note that proxy durations are wire round trips (they include network
latency to the database server) while litewire durations are in-process, and
that the proxy records a narrower set of statements. See the
[`[db.analysis]` coverage table](/reference/config/#dbanalysis).

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_query_duration_seconds` | histogram | `digest`, `kind` | Per-query execution time. `kind` is `"query"` or `"mutation"`. `digest` is the normalized SQL. |
| `ephpm_query_total` | counter | `digest`, `kind`, `status` | Query count by digest. `status` is `"ok"` or `"error"`. |
| `ephpm_query_rows_total` | counter | `digest`, `kind` | Total rows returned/affected per digest. |
| `ephpm_query_slow_total` | counter | — | Number of queries exceeding the slow query threshold. |
| `ephpm_query_active_digests` | gauge | — | Number of unique query digests currently tracked. |

### CDC Replication Metrics (experimental)

Emitted only when the experimental Turso CDC replication path is running
(`[db.sqlite]` + clustering). The full table — every series, its labels, and the
PromQL to alert on — is in the
[metrics reference](/reference/metrics/#cdc-native-turso-replication).

The design decision worth knowing here is what "replication lag" means. Two
positions are published: `ephpm_cdc_primary_head_change_id` (the primary's write
head in its `turso_cdc` log) and `ephpm_cdc_applied_change_id` (the replica's
applied watermark). `ephpm_cdc_replication_lag_changes` is the primary-local
difference between the head and the slowest attached subscriber's shipped
cursor.

All three are counted in **`change_id`s — change-log rows, not seconds**. That is
what the data supports: a seconds-valued lag needs a commit timestamp on the
wire, and while `turso_cdc` records one (`change_time`), litewire's `CdcRow` does
not carry it. Publishing a time-based lag would therefore require a litewire
change plus a CDC wire-format change; inventing one from row counts would be
worse than not having it, so the row lag is what ships and the unit is stated
everywhere it appears.

Two smaller consequences of that model:

- The shipped cursor is the **minimum** across attached subscribers, so a single
  caught-up replica cannot mask a lagging one, and it is **retained** after the
  last subscriber detaches, so a primary that keeps taking writes after its
  replica dies shows a growing lag rather than a frozen one.
- The lag/head/shipped gauges are not pre-seeded at zero the way the counters
  are. A node that has never replicated anything publishing `lag = 0` would read
  as "fully caught up", which is the opposite of the truth.

---

## Histogram Buckets

Custom bucket configurations are tuned for PHP workloads:

| Metric | Buckets |
|--------|---------|
| `ephpm_http_request_duration_seconds`, `ephpm_http3_request_duration_seconds`, `ephpm_php_execution_duration_seconds`, `ephpm_worker_request_wait_seconds` | 1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s |
| `ephpm_worker_boot_duration_seconds` | 10ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s, 20s, 30s |
| Body size histograms | 100B, 1KB, 10KB, 50KB, 100KB, 500KB, 1MB, 5MB, 10MB |
| `ephpm_http_compression_ratio` | 0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9 |
| `ephpm_cdc_apply_duration_seconds` | 100µs, 500µs, 1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 500ms, 1s, 5s |
| `ephpm_cdc_snapshot_duration_seconds` | 10ms, 50ms, 100ms, 500ms, 1s, 2.5s, 5s, 10s, 30s, 60s, 120s, 300s |

---

## Scraping

The metrics endpoint returns `text/plain; version=0.0.4` (standard Prometheus
text format) for `GET` only. No metric registers a description, so there are
`# TYPE` lines but no `# HELP` lines. Example scrape output (abridged):

```
# TYPE ephpm_build_info gauge
ephpm_build_info{version="0.1.0"} 1
# TYPE ephpm_http_requests_total counter
ephpm_http_requests_total{method="GET",status="200",handler="php"} 42
ephpm_http_requests_total{method="GET",status="200",handler="static"} 150
# TYPE ephpm_http_request_duration_seconds histogram
ephpm_http_request_duration_seconds_bucket{method="GET",handler="php",le="0.01"} 5
...
```

---

## Disabling Metrics

Set `enabled = false` (the default). When disabled, no recorder is installed and
every `metrics` facade call is a no-op. The cost is near-zero rather than zero:
a few call sites build their label values (query digest, middleware module,
vhost, upstream) before the call.

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

When `enabled = false`, no Prometheus recorder is installed, so every `metrics` façade call dispatches to a no-op. A handful of call sites still construct their label values first (the query digest, middleware module name, vhost, and upstream labels are cloned before dispatch), so the cost is *near*-zero rather than exactly zero.

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
| `ephpm_http_timeouts_total` | counter | `stage` | Requests killed by the request timeout. Two values: `request` (the fpm-mode request deadline) and `worker` (worker mode — the worker never responded within the request timeout; the request gets a 504 and the worker is marked hung, which also increments `ephpm_worker_recycles_total{reason="hung"}`). |
| `ephpm_rate_limited_total` | counter | — | Rejections from `[server.limits]`. Incremented only for per-IP rate limiting. |
| `ephpm_site_rate_limited_total` | counter | — | 429s from the per-site PHP rate limit (`[server.limits] per_site_rate`; on by default under `[server] preview`). |

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

## WebSockets

Emitted only when `[server.websocket] enabled = true`; the series are absent otherwise. See the [WebSockets guide](/guides/websockets/).

The upgrade request itself also increments the `ephpm_http_*` metrics above with `handler="websocket"` — everything after the `101` is accounted for here instead, because an upgraded socket is no longer an HTTP request.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_ws_connections_total` | counter | — | Connections admitted to the registry. Counted at admission, which is *before* the `connect` handler runs — so this exceeds the number of established sockets by however many `connect` handlers refused. |
| `ephpm_ws_connections_active` | gauge | — | Currently registered connections, across all vhosts. |
| `ephpm_ws_connections_rejected_total` | counter | `reason` | Upgrades refused by a capacity cap. `reason="server_full"` (`max_connections`) or `reason="site_full"` (`max_connections_per_site`). |
| `ephpm_ws_handshake_rejected_total` | counter | `reason` | Upgrades refused before any PHP ran. `reason="handshake"` (missing `Sec-WebSocket-Key`, or a `Sec-WebSocket-Version` other than 13), `reason="transport"` (no upgrade handle — not an HTTP/1.1 connection), `reason="capacity"` (the `503` companion to `ephpm_ws_connections_rejected_total`). |
| `ephpm_ws_connect_rejected_total` | counter | — | Upgrades refused by the application's `connect` handler returning a non-2xx. This is the auth-rejection counter; a sudden rise usually means expired tokens, not an attack. |
| `ephpm_ws_events_total` | counter | `event` | PHP executions dispatched. `event` is `connect`, `message`, or `disconnect`. |
| `ephpm_ws_event_timeouts_total` | counter | — | `message` handlers that exceeded `[server.timeouts] request`. Each one also closes its connection with `1011`. Non-zero means a handler is blocking; the socket cannot be abandoned the way an HTTP request can. |
| `ephpm_ws_frames_received_total` | counter | — | Inbound data frames (text and binary). Ping/pong are not counted. |
| `ephpm_ws_frames_queued_total` | counter | — | Frames accepted into a connection's outbound queue by `ephpm_ws_send()` / `ephpm_ws_broadcast()`. A broadcast to N subscribers counts N. |
| `ephpm_ws_frames_sent_total` | counter | — | Frames actually written to a socket. The gap between this and `ephpm_ws_frames_queued_total` is what is sitting in outbound queues (plus anything dropped by a connection that died before its queue drained). |
| `ephpm_ws_frames_rejected_total` | counter | `reason` | Frames dropped before the wire. `reason="too_large"` (payload over `max_message_size`; the connection is **not** shed — an oversized payload is the caller's bug, not back-pressure) or `reason="invalid_utf8"` (a text frame whose payload is not valid UTF-8, which RFC 6455 §5.6 forbids). |
| `ephpm_ws_send_queue_overflow_total` | counter | — | Sends refused because a connection's outbound queue was full. Each one also closes that connection with `1013`. **Worth alerting on**: it means clients are not draining and connections are being shed. |
| `ephpm_ws_cross_site_denied_total` | counter | — | Attempts to reach a connection or channel belonging to a different virtual host. **Should be flat at zero.** Anything else is a bug or an attempt — the operation is refused either way, and reported to PHP as "not found" so it is not an existence oracle. |

## PHP

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_php_executions_total` | counter | `status` | PHP requests executed. `status` is `ok` or `error`. Timeouts surface as HTTP 504 in the HTTP metrics, not here. |
| `ephpm_php_execution_duration_seconds` | histogram | — | Time spent inside the PHP runtime, per request. |
| `ephpm_php_output_bytes` | histogram | — | Bytes emitted by PHP per request. |
| `ephpm_php_shed_total` | counter | `engine` | PHP requests rejected with `503` + `Retry-After` because no execution slot came free within `[php] shed_after_ms`. Only recorded under [`[php] overload_policy = "shed"`](/reference/config/#php); always `0` on the default `"wait"` policy. `engine` is `pool` (the dispatch backlog was full) or `spawn_blocking` (no `[php] workers` permit). A 503 from a *draining* pool is not counted here — that is shutdown, not overload. Rising values mean the server is saturated and saying so, which is the healthy failure mode; the alternative is client timeouts with no server-side signal at all. |

## FPM pool engine

These appear when `[php] fpm_engine = "pool"` in fpm mode.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_fpm_pool_size` | gauge | — | Configured number of dedicated FPM pool threads, set at pool startup. Also the concurrency cap for this engine. |
| `ephpm_fpm_pool_queue_depth` | gauge | — | Requests enqueued on the dispatch queue but not yet pulled by a thread. |
| `ephpm_fpm_pool_boot_failures_total` | counter | — | Pool threads that failed to start (thread spawn or TSRM registration failure). The pool respawns with exponential backoff. |
| `ephpm_fpm_pool_panics_total` | counter | — | Rust panics escaping a PHP job. The request still gets a 500 and the thread is retired. A PHP bailout is *not* a panic and is not counted here. |
| `ephpm_fpm_pool_contained_crashes_total` | counter | — | PHP C-stack overflows contained instead of aborting the process. Requires [`[php] crash_containment`](/reference/config/#php); always `0` without it. Any non-zero value means a request crashed PHP and a thread was poisoned. |
| `ephpm_fpm_pool_recycles_total` | counter | `reason` | Pool threads retired and replaced. `reason` is `hung` (never responded within the request timeout; abandoned and replaced), `panic` (a Rust panic escaped the job), or `poisoned` (a crash was contained on it — abandoned **without** PHP teardown, since its Zend context is corrupt). |

## Native middleware

These appear when at least one `[[middleware]]` mount is configured.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_middleware_invocations_total` | counter | `module`, `action` | Middleware invocations, one per module per matching request. `action` is the verdict: `continue`, `respond`, or `rewrite`. A module `invoke` error (non-zero return, including a caught panic) counts as `respond` — the host fails closed with a 500. |

**PHP mounts** (`library = "php:<path>"`, experimental) share this counter,
with `module` set to the full `library` string. Their `action` is `continue`,
`respond` (the mount called `exit()`), or `error` (the mount fataled) — never
`rewrite`, because PHP expresses a rewrite by assigning to `$_SERVER` and
detecting that would mean diffing the superglobal on the hot path. Mounts that
never ran because an earlier one short-circuited are not counted.

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
| `ephpm_worker_stream_stalls_total` | counter | — | Streamed responses (`send_response_stream`) abandoned because the client stopped reading for longer than [`[server.timeouts] idle`](/reference/config/#servertimeouts-all-in-seconds) (default 60 s). Note that `idle` does double duty: it is both the connection idle timeout and the worker-mode streaming send timeout, so changing it moves this threshold too. |
| `ephpm_worker_stream_aborts_total` | counter | — | Streamed responses whose worker died in a bailout after the headers were already sent. The body is deliberately ended with an error (no terminating chunk) so the client sees a failed transfer rather than a truncated 200. Any non-zero value is a PHP crash. |

## OPcache clustering

These appear when a cluster-wide OPcache invalidation actually fires. When
`[opcache] cluster_invalidation = false`, or the watcher never sees an
`opcache:version:*` change, the counter has no series.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_opcache_invalidations_total` | counter | `vhost`, `trigger` | Cluster-wide OPcache invalidations run for a vhost. `trigger` is always `kv` today — both `ephpm deploy` and `ephpm cache reset` arrive via the KV version key. The planned file watcher (roadmap Phase 3) will add a second value. |

## OPcache JIT

The JIT buffer gauges are **sampled on the PHP request path** (the fpm dispatch closure and the worker-mode `take_request` loop), at most once per 10 s process-wide — `opcache_get_status()` needs a TSRM-registered PHP thread, so there is no background sampler. Consequences: the series **appear only after the first PHP request** in fpm mode (in worker mode, as soon as a worker boots and parks) on a PHP-linked build with OPcache active (stub builds and `opcache.enable=0` never record them), and with zero traffic they hold their last value (with zero traffic the JIT state cannot change). They are recorded whether the JIT is on or off, so "JIT off" reads as an honest `buffer_size = 0` rather than a missing series.

**Multi-tenant caveat:** the multi-tenant hardening preset (default with `sites_dir`) removes `opcache_get_status` from the function table unless [`[opcache] cluster_invalidation`](/reference/config/#opcache) keeps the OPcache API open — with the API removed, the sampler has nothing to call and these gauges never record. If you force the JIT on in multi-tenant mode (the case where `buffer_free` matters most), enable cluster invalidation to keep the gauge alive, or accept flying blind — the startup WARN spells this out.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_opcache_jit_buffer_size_bytes` | gauge | — | `opcache.jit_buffer_size` in bytes; `0` when no JIT buffer is configured. |
| `ephpm_opcache_jit_buffer_free_bytes` | gauge | — | Free bytes remaining in the JIT code buffer. Trending to 0 means the JIT is about to **silently** stop compiling new code — its only failure mode has no error or log. Watch this especially with the JIT forced on in multi-tenant mode: per-vhost `opcache_invalidate` never returns buffer to the free pool, so every deploy consumes some for good (see the [config reference](/reference/config/#opcache-jit)). |

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

## CDC-native Turso replication

These appear only when `[db.sqlite]` runs with clustering enabled (the
Turso CDC replication path). A single-node deployment has no `ephpm_cdc_*`
series at all. The whole path is **experimental** — see
[Roadmap → Turso engine](/roadmap/turso-engine/).

Every node registers the **counters** and `ephpm_cdc_subscribers` at startup,
zeroed, so an idle node is distinguishable from a build without the
instrumentation. The four `*_change_id` gauges, the replication-lag gauge, and
the two histograms are deliberately **not** pre-seeded — they are absent from
`/metrics` until the first subscriber connects or the first batch is applied,
so a query against them returns no data on an idle cluster. Which series
actually move depends on the elected role.

### Primary (shipping) side

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_cdc_subscribers` | gauge | — | CDC subscriber streams currently attached to this node. Counted from the moment a stream sends its `Subscribe` frame until the stream ends for any reason. **This is the metric to alert on**: `0` on a primary means nothing is being replicated. |
| `ephpm_cdc_batches_shipped_total` | counter | — | Committed transaction batches written into subscriber streams, summed across subscribers. Counted after the frame is on the wire, so a batch that failed to write is not counted. |
| `ephpm_cdc_rows_shipped_total` | counter | — | `turso_cdc` rows contained in those batches. |
| `ephpm_cdc_shipped_change_id` | gauge | — | The **lowest** `change_id` shipped across all attached subscribers — i.e. the slowest replica's position, so one caught-up replica cannot mask another that is far behind. Retained after the last subscriber detaches. Absent until the first subscriber ever attaches. |
| `ephpm_cdc_primary_head_change_id` | gauge | — | `MAX(turso_cdc.change_id)` on this node: the write head of the change log. Sampled once a second while primary, and also advanced for free whenever a tailer runs dry. Monotonic. |
| `ephpm_cdc_replication_lag_changes` | gauge | — | `primary_head_change_id - shipped_change_id`. **The headline replication-lag metric — see the unit warning below.** Clamped at `0`. Absent until the first subscriber ever attaches. |
| `ephpm_cdc_tail_poll_errors_total` | counter | — | `turso_cdc` poll failures. The stream is dropped and the replica resumes from the same watermark, so this is a retry signal, not a data-loss signal. |
| `ephpm_cdc_streams_refused_total` | counter | `stream` | Inbound streams refused because this node is not the elected primary — a peer is dialing a stale primary address. `stream` is `cdc` or `snapshot`. |
| `ephpm_cdc_snapshots_served_total` | counter | `status` | Snapshot bootstraps served to cold replicas. `status` is `ok` or `error`. |
| `ephpm_cdc_snapshot_bytes_served_total` | counter | — | Logical-dump bytes written to dialing replicas. Successful transfers only. |
| `ephpm_cdc_snapshot_duration_seconds` | histogram | `role` | Snapshot transfer time. `role` is `serve` (primary produced and streamed the dump) or `fetch` (replica received and applied it). |

### Replica (apply) side

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_cdc_batches_applied_total` | counter | — | CDC batches applied to the local database. A batch that failed to apply is **not** counted here; it appears in `ephpm_cdc_apply_errors_total`. |
| `ephpm_cdc_rows_applied_total` | counter | — | `turso_cdc` rows applied. |
| `ephpm_cdc_applied_change_id` | gauge | — | This replica's applied watermark, mirroring litewire's `__litewire_cdc_watermark` table. Published on subscribe, after a snapshot bootstrap, and after every applied batch — deliberately **not** advanced past a batch that failed to apply. Watermarks are scoped to the source primary's CDC log identity, so this gauge legitimately **drops** (usually to 0) when the replica re-subscribes to a newly promoted primary after a failover. |
| `ephpm_cdc_apply_errors_total` | counter | — | `apply_batch` failures. The stream is failed and the watermark does not move; the replica reconnects and retries from the identical cursor. Any non-zero rate here means replication is stalled, not degraded. |
| `ephpm_cdc_apply_duration_seconds` | histogram | — | Per-batch apply time on the replica. One batch is one replicated transaction. |
| `ephpm_cdc_replica_connects_total` | counter | `outcome` | Subscribe attempts, one increment per attempt by terminal outcome: `closed` (primary closed the stream cleanly), `dial_error` (could not reach the primary), `stream_error` (connected, then the stream failed), `watermark_error` (could not resolve the local watermark for the log the primary announced). Reconnect rate is the sum across all four. |
| `ephpm_cdc_bootstrap_total` | counter | `outcome` | Cold-start snapshot bootstrap decisions. `outcome` is `ok`, `skipped` (local database already populated) or `failed` (retry budget exhausted — startup aborts deliberately rather than serving incomplete data). |
| `ephpm_cdc_snapshot_bytes_received_total` | counter | — | Logical-dump bytes received and successfully applied during bootstrap. |

### What the lag metric means (and does not)

`ephpm_cdc_replication_lag_changes` is a **row count, not a duration**. Both of
its inputs are `change_id` values, and turso allocates one `change_id` per
captured row change. A lag of `500` means five hundred row changes behind — which
is sub-millisecond on an idle cluster and several minutes behind a bulk import.
**Do not graph it with a seconds unit and do not alert on it as if it were one.**

It is measured at the *ship* boundary — the moment a batch is written into the
subscriber's stream — not at the *apply* boundary on the replica. It therefore
excludes network flight time and the replica's apply cost. For true end-to-end
lag, subtract across nodes:

```promql
# End-to-end replication lag, in change-log rows.
max(ephpm_cdc_primary_head_change_id) - min(ephpm_cdc_applied_change_id)

# Replication is not happening at all — the unambiguous alert.
max(ephpm_cdc_subscribers) == 0

# Replication is stalled on a failing apply (the watermark is not moving).
rate(ephpm_cdc_apply_errors_total[5m]) > 0
```

There is deliberately **no seconds-valued lag metric**. Producing one requires a
commit timestamp travelling with each change so the consumer can subtract it
from its own clock. The `turso_cdc` table does store one (`change_time`), but
litewire's `CdcRow` does not expose it, so surfacing a time-based lag needs a
litewire change to carry `change_time` plus a matching CDC wire-format change —
not an ePHPm-side calculation. Until both land, any "seconds behind" figure
would be invented, so none is published.

### Cardinality

Every series above is either unlabelled or carries a label with a fixed, small
set of values (`stream`: 2, `status`: 2, `role`: 2, `outcome`: 4 or 3). There are
deliberately **no per-peer or per-address labels** — a label per replica would
scale series count with cluster size and churn a new series on every pod
replacement.

## Cardinality notes

The per-metric `digest` label series is **capped** — by default at 1,000 distinct label values per process (`StatsConfig::metric_label_series_max`). Every additional distinct digest observed after the cap is exhausted has its Prometheus emissions folded into a single shared `digest="__other__"` bucket. Internal tracking (`top_queries()`, the digest table, the slow-query log) is **not** affected by this cap and still exposes the real normalized SQL — only the Prometheus label surface is bounded.

The internal digest table itself is bounded separately by `[db.analysis] digest_store_max_entries` (default 100,000). That knob controls how many distinct digests are held in memory for `top_queries()`; the label-series cap above controls Prometheus cardinality.

The cap is configurable: `[db.analysis] metric_label_series_max` (default `1000`). **There is no unlimited setting** — a digest is admitted only while the admitted count is *below* the cap, so `0` admits nothing and folds every digest into `digest="__other__"`. Raise the number if you want a higher cap. If your Prometheus is unhappy regardless, set `query_stats = false` to disable the metrics entirely.

The `path`-style labels you might expect on HTTP metrics (`/users/123`) are deliberately *not* present — Prometheus' best-practice is to keep label cardinality bounded, and request paths in PHP apps explode it. Use the slow-query log + tracing for path-level debugging.

## Histogram buckets

Buckets are custom per metric — configured with `Matcher::Full` rules in [`crates/ephpm-server/src/metrics.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-server/src/metrics.rs), not the `metrics_exporter_prometheus` builder defaults:

- Duration histograms (`ephpm_http_request_duration_seconds`, `ephpm_http3_request_duration_seconds`, `ephpm_php_execution_duration_seconds`, `ephpm_worker_request_wait_seconds`): 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 seconds
- `ephpm_worker_boot_duration_seconds`: 0.01, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 20, 30 seconds (framework boot can take seconds)
- `ephpm_cdc_apply_duration_seconds`: 0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1, 5 seconds (one replicated transaction; the healthy range is sub-millisecond, so the buckets start two decades lower than the HTTP ones)
- `ephpm_cdc_snapshot_duration_seconds`: 0.01, 0.05, 0.1, 0.5, 1, 2.5, 5, 10, 30, 60, 120, 300 seconds (a logical dump of a large database is minutes, and it blocks a cold replica's startup)
- Body-size histograms (`ephpm_http_request_body_bytes`, `ephpm_http_response_body_bytes`, `ephpm_php_output_bytes`): 100 B, 1 KB, 10 KB, 50 KB, 100 KB, 500 KB, 1 MB, 5 MB, 10 MB
- `ephpm_http_compression_ratio`: 0.05 through 0.9

## See also

- [Query Stats with Prometheus](/guides/query-stats-prometheus/) — practical PromQL queries
- [Architecture → Query Stats](/architecture/query-stats/) — how the digest normalizer works
- [Architecture → Metrics](/architecture/metrics/) — design rationale

+++
title = "Metrics"
weight = 4
+++

Every metric ePHPm exposes at `/metrics`. Enable with:

```toml
[server.metrics]
enabled = true
# path = "/metrics"          # default
# allow = ["10.0.0.0/8"]     # optional IP ACL (default: allow all); deny = [...] wins
# token = "..."              # optional bearer token; REQUIRED on multi-site nodes
# listen = "127.0.0.1:9100"  # optional separate listener (tls_cert/tls_key/client_ca for mTLS)
```

The output includes every tenant's SQL digests and upstream addresses. Restrict network exposure with `allow`/`deny` or a separate `listen`. On a multi-site node `/metrics` answers `403` until a `token` (or an mTLS listener) is configured. See [access to internal endpoints](/reference/config/#access-to-internal-endpoints).

## Scraping with Prometheus

Pick one of the two setups below. Both satisfy the multi-site credential
requirement, and both work on single-site nodes too.

### Option A: bearer token on the main listener

`ephpm.toml`:

```toml
[server.metrics]
enabled = true
token = "<long random string>"   # e.g. `openssl rand -hex 32`
# allow = ["10.0.0.0/8"]         # optional: also restrict by client address
```

Put the token in the config file, not in `EPHPM_SERVER__METRICS__TOKEN`. On a
multi-site node, tenant PHP shares the server's environment (`getenv()`), so a
token in the environment is readable by every tenant, and startup warns about
it.

`prometheus.yml`:

```yaml
scrape_configs:
  - job_name: ephpm
    metrics_path: /metrics
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/secrets/ephpm-metrics-token  # file holds only the token
    static_configs:
      - targets: ["ephpm-1.internal:8080", "ephpm-2.internal:8080"]
```

`credentials_file` keeps the token out of `prometheus.yml`. An inline
`credentials: <token>` also works. Prometheus re-reads the file on every
scrape, so the token can be rotated without a reload. A scrape with no token or
the wrong token gets `401`.

### Option B: mTLS on a separate metrics listener

This moves `/metrics` (and `/_ephpm/requests`) off the public port. Only
clients holding a certificate signed by your CA can connect.

`ephpm.toml`:

```toml
[server.metrics]
enabled = true
listen = "10.0.0.5:9100"                     # private address; probes stay on the main listener
tls_cert = "/etc/ephpm/metrics/server.pem"   # chain: server cert, then CA
tls_key = "/etc/ephpm/metrics/server.key"
client_ca = "/etc/ephpm/metrics/ca.pem"      # clients must present a cert signed by this CA
```

`prometheus.yml`:

```yaml
scrape_configs:
  - job_name: ephpm
    scheme: https
    metrics_path: /metrics
    tls_config:
      ca_file: /etc/prometheus/ephpm/ca.pem          # verifies ePHPm's server cert
      cert_file: /etc/prometheus/ephpm/prometheus.pem
      key_file: /etc/prometheus/ephpm/prometheus.key
      # server_name: metrics.internal               # only if the target address is not in the cert's SAN
    static_configs:
      - targets: ["10.0.0.5:9100"]
```

A client with no certificate, or with one from another CA, fails the TLS
handshake and never reaches HTTP. You can combine this with `token`. Prometheus
then needs both `tls_config` and `authorization`.

**Creating the CA and certificates.** Any private CA works, for example
[smallstep](https://smallstep.com/docs/step-cli/), [cfssl](https://github.com/cloudflare/cfssl),
or cert-manager in Kubernetes. With plain OpenSSL 3:

```bash
# 1. A private CA (keep ca.key offline / secret).
openssl req -x509 -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout ca.key -out ca.pem -days 3650 -subj "/CN=ephpm metrics CA"

# 2. The metrics listener's server certificate. The SAN must match the address
#    Prometheus scrapes (here an IP and a DNS name).
openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout server.key -out server.csr -subj "/CN=metrics.internal"
printf "subjectAltName=DNS:metrics.internal,IP:10.0.0.5\nextendedKeyUsage=serverAuth\n" > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out server-leaf.pem -days 825 -extfile server.ext
cat server-leaf.pem ca.pem > server.pem

# 3. Prometheus's client certificate.
openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout prometheus.key -out prometheus.csr -subj "/CN=prometheus"
printf "extendedKeyUsage=clientAuth\n" > client.ext
openssl x509 -req -in prometheus.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out prometheus.pem -days 825 -extfile client.ext
```

ePHPm needs `server.pem`, `server.key` and `ca.pem`. Prometheus needs `ca.pem`,
`prometheus.pem` and `prometheus.key`. Check the setup by hand with
`curl --cacert ca.pem --cert prometheus.pem --key prometheus.key https://10.0.0.5:9100/metrics`.

For Kubernetes, see the `ServiceMonitor` example in
[Kubernetes](/roadmap/kubernetes/#prometheus-metrics).

When `enabled = false`, no Prometheus recorder is installed, so every `metrics` façade call dispatches to a no-op. A handful of call sites still construct their label values first (the query digest, middleware module name, vhost, and upstream labels are cloned before dispatch), so the cost is *near*-zero rather than exactly zero.

Metrics are emitted via the [`metrics`](https://docs.rs/metrics/) façade and exported through [`metrics_exporter_prometheus`](https://docs.rs/metrics-exporter-prometheus/). The scrape is `text/plain; version=0.0.4`. No metric registers a description, so the output has `# TYPE` lines but no `# HELP` lines.

> **The endpoint is not tenant-scoped.** It is served for `GET` for every
> `Host`, ahead of `trusted_hosts`, `blocked_paths` and all middleware, and it
> carries every tenant's normalized SQL (`digest`), vhost names and DB upstream
> addresses. On a single-site node it is open by default; restrict it with
> `[server.metrics] allow`/`deny`, a `token`, or a separate `listen` address.
> On a multi-site node it answers `403` until a `token` or an mTLS `listen` is
> configured. See [access to internal endpoints](/reference/config/#access-to-internal-endpoints)
> and [HTTP Endpoints → Exposure](/reference/http-endpoints/#exposure-read-this-first).

## Build info

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_build_info` | gauge | `version` | Constant `1`. Useful for joining build versions to other queries. |

## HTTP

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_http_requests_total` | counter | `method`, `status`, `handler` | Total HTTP requests handled. `handler` is the route class; see the value table below. To keep series cardinality bounded, `method` is one of the standard verbs (`GET`, `POST`, ...) or `OTHER` for any non-standard verb, and `status` is the numeric code for the common statuses this server and typical apps emit or `other` for uncommon codes. Requests refused by the per-IP rate limit (`429`) are counted only in `ephpm_rate_limited_total`, not here. |
| `ephpm_http_requests_in_flight` | gauge | — | Currently in-flight HTTP requests. Decremented however a request ends: a response, a timeout, or the client disconnecting mid-request. |
| `ephpm_http_request_duration_seconds` | histogram | `method`, `handler` | Request handling time, end-to-end (no `status` label). |
| `ephpm_http_body_overflow_total` | counter | — | `413`s raised while **reading** a request body that turned out larger than `[server.request] max_body_size`: chunked or under-declared bodies, the middleware pre-buffer (`middleware_body_limit`), and worker-mode streamed bodies. A body whose declared `Content-Length` already exceeds the limit is refused before reading and is **not** counted. A body read error during the middleware pre-buffer is also answered `413` and counted here. |
| `ephpm_http_request_body_bytes` | histogram | `method` | Request body size. `method` is bounded the same way as above (standard verb or `OTHER`). |
| `ephpm_http_response_body_bytes` | histogram | `handler` | Response body size before compression. Recorded on the PHP path only (`handler="php"`). |
| `ephpm_http_compression_ratio` | histogram | — | Compressed-to-original ratio; covers both Brotli and gzip responses. |
| `ephpm_http_timeouts_total` | counter | `stage` | Requests answered `504` because `[server.timeouts] request` expired. Each request has one deadline, measured from when it arrived. `stage` says what it was waiting on: `fpm_pool` (per-request mode, waiting on PHP), `worker` (worker mode, waiting on PHP), or `request` (anything else: routing, middleware, or a dispatch queue so full the request never got in). The `request` stage fires 50 ms after the deadline, so a request waiting on PHP is always counted under its PHP stage. When an `fpm_pool` or `worker` timeout's job had already **started** on a thread, that thread is also marked hung (`ephpm_fpm_pool_recycles_total` / `ephpm_worker_recycles_total` with `reason="hung"`). A job still in the queue at the deadline is overload, not a hung thread, and nothing is replaced. |
| `ephpm_rate_limited_total` | counter | — | Rejections from `[server.limits]`. Incremented only for per-IP rate limiting. |
| `ephpm_site_rate_limited_total` | counter | — | 429s from the per-site PHP rate limit (`[server.limits] per_site_rate`; on by default under `[server] preview`). |
| `ephpm_site_override_unusable_total` | counter | — | 503s from a per-site override file that exists but cannot be honoured (`[server] site_overrides_dir`). Non-zero means a vhost is serving nothing rather than falling back to its whole container — see [Virtual Hosts → Failure modes](/guides/virtual-hosts/#failure-modes-a-broken-override-takes-the-site-out-of-service). |

### `handler` label values

| Value | Requests |
|---|---|
| `php` | Everything dispatched to the PHP path, whatever the status: executions, a `304` from the PHP ETag cache, the per-site `429`, a shed or draining `503`, a `413` body overflow, and an `fpm_pool`/`worker`-stage `504`. In worker mode this includes every non-static request. |
| `static` | Static files served from disk. |
| `middleware` | A static-file request answered by a request-phase middleware `RESPOND` (auth denial, redirect). |
| `error` | Server-generated error responses: `400` path decoding, `403`/`404` from the security gates, `404`/`405` in `/_ephpm/`, fallback `=NNN` statuses, a `403` from `allowed_php_paths`, and a `stage="request"` timeout `504`. |
| `health` | `/_ephpm/health`, `/_ephpm/ready` **and** `/_ephpm/primary`. |
| `metrics` | Scrapes of the metrics path. |
| `diagnostics` | `/_ephpm/requests`, including its `404` when the request timeline is off. |
| `acme` | `/.well-known/acme-challenge/*`. |
| `proxy` | `[[server.proxy]]` forwards. |
| `websocket` | WebSocket upgrade requests (the `101` or the refusal). |
| `auth` | `/_ephpm/auth/*`. |
| `site_override_unusable` | `503` from a per-site override file that cannot be honoured. |

Filtering out `handler="health"` still leaves Prometheus's own scrapes
(`metrics`) and timeline polls (`diagnostics`) in your request rate.

### Timeouts and the `fpm_pool` / `worker` stages

Each request has one deadline, `[server.timeouts] request` after it arrived.
The wait for PHP's reply uses that same deadline, and the outer whole-request
guard fires 50 ms later, so a request that is waiting on PHP when the deadline
passes is always answered (`504`) and counted under `stage="fpm_pool"` or
`stage="worker"`. `stage="request"` is left for requests stuck somewhere else:
routing, middleware, or a dispatch queue so full the job never got in.

A PHP-stage timeout replaces a thread only if the job had **started** on it:
the thread is marked hung, a replacement is spawned (unless as many hung
threads as the pool size are already outstanding, so a stream of slow
requests cannot grow the pool past twice its size; past that cap an ERROR is
logged and nothing is spawned), and
`ephpm_fpm_pool_recycles_total{reason="hung"}` /
`ephpm_worker_recycles_total{reason="hung"}` increments. A job still queued at
the deadline is overload and replaces nothing. The hung thread keeps running
until PHP finishes the request (or `max_execution_time` stops it, where
enforced); when it answers, one surplus thread retires so the pool returns to
its configured size. A request whose client disconnects while it waits on PHP
is still watched until its deadline and counted hung the same way. On the
worker-mode Xdebug debug lane, a timed-out request is only logged: its thread
was never part of the serving pool.

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

## Reverse proxy

These appear once a `[[server.proxy]]` rule matches a request. See the
[Reverse Proxy guide](/guides/reverse-proxy/). Proxied requests also count in
`ephpm_http_*` with `handler="proxy"`.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_proxy_requests_total` | counter | — | Requests matched by a proxy rule and forwarded (plain HTTP and WebSocket upgrades). |
| `ephpm_proxy_errors_total` | counter | `kind` | Upstream failures answered `502`. `kind` is `read_timeout` (no response head within the rule's `read_timeout`), `connect` (the upstream HTTP request failed: refused, unreachable, connect timeout, reset), `ws_connect` (TCP connect to a WebSocket backend failed or timed out), `ws_handshake` (HTTP/1 handshake with the WebSocket backend failed) or `ws_request` (sending the upgrade request failed). |
| `ephpm_proxy_ws_tunnels_total` | counter | — | WebSocket upgrades the backend accepted (`101` relayed to the client). Counted when the `101` is returned, before both sides have finished upgrading, so a client that drops at that moment is still counted. |

## PHP

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_php_executions_total` | counter | `status` | PHP requests executed. `status` is `ok` or `error`. Timeouts surface as HTTP 504 in the HTTP metrics, not here. |
| `ephpm_php_execution_duration_seconds` | histogram | — | Time from dispatch to the PHP pool until the response comes back, per successfully dispatched request. **Includes the dispatch-queue wait** in both modes, so under saturation it grows with queueing, not with slow PHP. Use `ephpm_fpm_pool_queue_depth` / `ephpm_worker_dispatch_queue_depth` to tell queueing apart from slow PHP. Not recorded for timeouts or shed requests. |
| `ephpm_php_output_bytes` | histogram | — | Bytes emitted by PHP per request. |
| `ephpm_php_shed_total` | counter | `engine` | PHP requests rejected with `503` + `Retry-After` because no execution slot came free within `[php] shed_after_ms`. Only recorded under [`[php] overload = "shed"`](/reference/config/#php); always `0` on the default `"wait"` policy. `engine` is always `pool` (the dispatch backlog was full; the label is kept for continuity). A 503 from a *draining* pool is not counted here — that is shutdown, not overload. Rising values mean the server is saturated and saying so, which is the healthy failure mode; the alternative is client timeouts with no server-side signal at all. |

## FPM pool engine

These appear in per-request mode (`[php] mode = "per_request"`, the default execution model); they are absent only in worker mode.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_fpm_pool_size` | gauge | — | Configured number of dedicated FPM pool threads, set at pool startup. Also the concurrency cap for this engine. |
| `ephpm_fpm_pool_queue_depth` | gauge | — | Requests waiting in the dispatch overflow queue because every pool thread was busy, sampled at each dispatch. A request that finds an idle thread is handed to it directly and never counts. |
| `ephpm_fpm_pool_boot_failures_total` | counter | — | Pool threads that failed to start (thread spawn or TSRM registration failure). The pool respawns with exponential backoff. |
| `ephpm_fpm_pool_panics_total` | counter | — | Rust panics escaping a PHP job. The request still gets a 500 and the thread is retired. A PHP bailout is *not* a panic and is not counted here. |
| `ephpm_fpm_pool_contained_crashes_total` | counter | — | Per-request mode: PHP crashes contained to one thread instead of ending the process. Two causes, told apart by the ERROR log line: a C-stack overflow (only with [`[php] crash_containment`](/reference/config/#php)), and PHP's [hard memory limit](/reference/config/#phps-hard-memory-limit) exit (Linux, PHP 8.3.36/8.4.27/8.5.12+, always on). Any non-zero value means a request was answered `500` and its thread was retired and replaced. |
| `ephpm_fpm_pool_recycles_total` | counter | `reason` | Pool threads retired and replaced. `reason` is `hung` (was running a request that hit its deadline: abandoned and replaced, because a PHP thread cannot be killed safely; if it answers later, one surplus thread retires so the pool returns to its configured size), `panic` (a Rust panic escaped the job), or `poisoned` (a crash was contained on it — abandoned **without** PHP teardown, since its Zend context is corrupt). |

## Native middleware

These appear when at least one `[[middleware]]` mount is configured.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_middleware_invocations_total` | counter | `module`, `action` | Request-phase middleware invocations, one per module per matching request. `action` is the verdict: `continue`, `respond`, or `rewrite`. A module `invoke` error (non-zero return, including a caught panic) counts as `respond` — the host fails closed with a 500. `php:` mounts (the PHP lane) record into the same counter with `continue`, `respond` (the mount called `exit()`) or `error` (the mount raised a fatal; the request fails closed), never `rewrite`; mounts after the one that ended the chain are not counted. |
| `ephpm_middleware_response_invocations_total` | counter | `module` | Response-phase invocations (`invoke_response`), one per response-capable module per matching buffered response. Streamed responses bypass the response phase and are not counted. |

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
| `ephpm_worker_busy` | gauge | — | Dispatched requests awaiting a worker response. Includes jobs still sitting in the dispatch queue, so it can exceed `concurrency` when the backlog is deep. Released when the wait ends for any reason, including a timeout or a client disconnect. |
| `ephpm_worker_idle` | gauge | — | Workers waiting for work in `take_request()` (polling their own dispatch mailbox for up to 200 µs by default — see `EPHPM_DISPATCH_SPIN_US` in [environment variables](/reference/environment-variables/#other-non-config-environment-variables) — then parked; recorded inside the dispatch recv; only moves in PHP-linked builds). |
| `ephpm_worker_dispatch_queue_depth` | gauge | — | Jobs waiting in the dispatch overflow queue because every worker was busy, sampled at each dispatch. A job that finds an idle worker is handed to it directly and never counts. |
| `ephpm_worker_request_wait_seconds` | histogram | — | Time a request spent waiting to **enter** the dispatch queue (backpressure when the queue is full). Time spent inside the queue waiting for a free worker is not included. |
| `ephpm_worker_boot_duration_seconds` | histogram | — | Time from worker-thread start to the framework's first `take_request()` (i.e. framework boot time). |
| `ephpm_worker_boot_timeouts_total` | counter | — | Boots still running when `[php.worker] boot_timeout` expired. The thread is not killed; it still becomes ready if the boot completes. |
| `ephpm_worker_boot_failures_total` | counter | — | Worker boots that failed (thread spawn/TSRM init failure, or the script exited before its first `take_request()`). The pool respawns with exponential backoff. |
| `ephpm_worker_recycles_total` | counter | `reason` | Workers recycled. `reason` is `max_requests` (hit `[php.worker] max_requests`), `script_exit` (script called `exit()`/`die()` mid-request), `fatal` (fatal error / bailout), `contained` (PHP's [hard memory limit](/reference/config/#phps-hard-memory-limit) ended the request — contained on Linux; the worker's PHP memory is freed and it is replaced), or `hung` (was running a request that hit its deadline: replaced; if it answers later, one surplus worker retires at its next `take_request()` so the pool returns to its configured size). |
| `ephpm_worker_stream_stalls_total` | counter | — | Streamed responses (`send_response_stream`) abandoned because the client stopped reading for longer than [`[server.timeouts] idle`](/reference/config/#servertimeouts-all-in-seconds) (default 60 s). Note that `idle` does double duty: it is both the connection idle timeout and the worker-mode streaming send timeout, so changing it moves this threshold too. |
| `ephpm_worker_stream_aborts_total` | counter | — | Streamed responses whose worker died in a bailout after the headers were already sent. The body is deliberately ended with an error (no terminating chunk) so the client sees a failed transfer rather than a truncated 200. Any non-zero value is a PHP crash. |
| `ephpm_xdebug_debug_requests_total` | counter | `outcome` | Worker-mode [Xdebug debug lane](/guides/xdebug/) requests (only present when the lane is armed — Xdebug loaded with step debugging on). Per request: `dispatched` (a debug thread was spawned), or a refusal instead — `rejected` (answered 503: `16 × debug_workers` requests were already waiting for a debug slot, or `2 × debug_workers` debug threads are alive) or `spawn_failed` (OS thread spawn failed; 503). `abandoned` counts dispatched requests whose HTTP side gave up first (request timeout or client disconnect) — the thread keeps running until the debugger releases it. Per debug thread, how it ended: `ok`, `no_response` (script finished without answering → 500), `script_exit`, `script_fatal`, `bailout`, `contained` (PHP's hard memory limit, contained on Linux), `run_failed`, `startup_failed` (request startup failed → 500). |
| `ephpm_xdebug_debug_workers` | gauge | — | Debug-lane threads currently alive: serving, finishing their debug session, or abandoned and still paused in the debugger. Registered when the lane is armed. |

Debug-lane requests go through the same dispatch path as pool requests, so they are also counted in `ephpm_worker_busy`, `ephpm_worker_request_wait_seconds` (which then includes the wait for a free debug slot) and `ephpm_php_execution_duration_seconds`. Their boots are **not** recorded in `ephpm_worker_boot_duration_seconds` (a debug boot is not a pool boot), and a debug request hitting the request timeout never increments `ephpm_worker_recycles_total{reason="hung"}` — no pool worker is involved. Its `ephpm_http_timeouts_total` label depends on when the deadline hits: `stage="worker"` once its debug thread was running, `stage="request"` while it was still waiting for a debug slot (the overall request deadline fires first there — the same applies to a pool request that waited in the dispatch queue).

## OPcache clustering

These appear when a cluster-wide OPcache invalidation actually fires. When
`[opcache] cluster_invalidation = false`, or the watcher never sees an
`opcache:version:*` change, the counter has no series.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_opcache_invalidations_total` | counter | `vhost`, `trigger` | Cluster-wide OPcache invalidations run for a vhost. `trigger` is always `kv` today — both `ephpm deploy` and `ephpm cache reset` arrive via the KV version key. The planned file watcher (roadmap Phase 3) will add a second value. |

## OPcache JIT

The JIT buffer gauges are **sampled on the PHP request path** (the per-request dispatch closure and the worker-mode `take_request` loop), at most once per 10 s process-wide — `opcache_get_status()` needs a TSRM-registered PHP thread, so there is no background sampler. Consequences: the series **appear only after the first PHP request** in per-request mode (in worker mode, as soon as a worker boots and parks) on a PHP-linked build with OPcache active (stub builds and `opcache.enable=0` never record them), and with zero traffic they hold their last value (with zero traffic the JIT state cannot change). They are recorded whether the JIT is on or off, so "JIT off" reads as an honest `buffer_size = 0` rather than a missing series.

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

## Embedded database (WAL checkpoint gate)

These appear whenever `[db.sqlite]` is configured, in every embedded-Turso mode. A sampler republishes them every 2 seconds for each **open** database. The WAL checkpoint gate is ePHPm's always-on, non-configurable workaround for Turso 0.8.3 never rewinding its `-wal` file while readers are active; see [Engines → WAL growth under load, and the checkpoint gate](/architecture/database/engines/#wal-growth-under-load-and-the-checkpoint-gate) for what it does. The gate is temporary (it will be replaced by a periodic `TRUNCATE` checkpoint once Turso's checkpoint can wait for readers), so only the two series an operator needs are exported.

Both series carry one label, `site`: the vhost's canonical site key in the per-site modes, or `default` in the single-database modes (single-node and whole-database clustered — the same name the clustered path uses for its `cdc/default` stream). Nothing is pre-seeded — a series appears when its database is first open, and stops updating (but is not removed) when a per-site database is evicted.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_sqlite_wal_bytes` | gauge | `site` | Current size of the database's `-wal` file. **The headline number**: it should saw-tooth below 64 MiB, the size at which the gate truncates the log. A file that stays well above that means the gate cannot win. |
| `ephpm_sqlite_wal_gate_timeouts_total` | counter | `site` | Truncation attempts abandoned because in-flight statements and open transactions did not drain within the attempt's wait (250 ms, escalating to at most 2 s once the log is past 256 MiB). Mirrors litewire's counter (`Counter::absolute`). Timeouts on their own are normal — one slow read makes an attempt give up and the next one waits longer. Timeouts while `ephpm_sqlite_wal_bytes` stays high mean something, usually a transaction held open for seconds, is pinning the log. |

```promql
# The alert: the gate is losing. Attempts keep timing out AND the log stays
# past twice the 64 MiB truncation threshold. Find the long-running
# transaction or statement on that site.
rate(ephpm_sqlite_wal_gate_timeouts_total[5m]) > 0
  and on (site) ephpm_sqlite_wal_bytes > 2 * 67108864
```

**Cardinality.** One series per open database. In the per-site modes the serving registry's open set is capped by `[db.sqlite] max_open_dbs`, and the per-site clustered replication registry holds one entry per vhost directory on disk — the same bound the `vhost` label on `ephpm_opcache_invalidations_total` has. An owned site in per-site clustered mode is open in both registries, but the gate is per *file*, so it is reported once. There are no per-peer or per-connection labels.

## CDC-native Turso replication

These appear only when `[db.sqlite]` runs with clustering enabled (the
Turso CDC replication path). A single-node deployment has no `ephpm_cdc_*`
series at all. The whole path is **experimental** — see
[Roadmap → Turso engine](/roadmap/turso-engine/).

**Positions are per site; event counts are per node.** Every *gauge* below —
the four position gauges and `ephpm_cdc_subscribers` — carries a `site` label,
because a `change_id` is a position in one database's change log and each site
has its own log. In whole-database clustered mode there is one database and the
label is `site="default"` (the same value `ephpm_sqlite_wal_bytes{site}` uses,
so the two join). In per-site clustered mode it is the vhost's canonical site
key. Every *counter and histogram* is node-wide with no `site` label: it counts
events, and a sum across sites is exactly the right number for "is anything
replicating" and "is anything failing".

Every node registers the counters at startup, zeroed, so an idle node is
distinguishable from a build without the instrumentation. In whole-database
mode `ephpm_cdc_subscribers{site="default"}` is also present from boot. The
position gauges are deliberately **not** pre-seeded — they appear once there is
a position to report (see each row) — so a query against them returns no data
on an idle cluster. Which series a node publishes for a site depends on its role
**for that site**.

### Shipping side (the primary / the site's owner)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_cdc_subscribers` | gauge | `site` | CDC subscriber streams currently attached for `site`. Counted from the moment a stream sends its `Subscribe` frame until the stream ends for any reason. Published by the node shipping the site: in per-site mode it appears (at `0`) as soon as this node becomes the site's owner. **This is the metric to alert on**: `0` on the shipping node means that site is not being replicated. |
| `ephpm_cdc_batches_shipped_total` | counter | — | Committed transaction batches written into subscriber streams, summed across subscribers and sites. Counted after the frame is on the wire, so a batch that failed to write is not counted. |
| `ephpm_cdc_rows_shipped_total` | counter | — | `turso_cdc` rows contained in those batches. |
| `ephpm_cdc_shipped_change_id` | gauge | `site` | The **lowest** `change_id` shipped across the site's attached subscribers — i.e. the slowest replica's position, so one caught-up replica cannot mask another that is far behind. Retained after the last subscriber detaches. Absent until the first subscriber of the site attaches. |
| `ephpm_cdc_primary_head_change_id` | gauge | `site` | `MAX(turso_cdc.change_id)` of the site's change log on the node shipping it: the write head. Sampled once a second while this node is the primary (whole-database mode) or the site's owner (per-site mode — one indexed `MAX()` per owned site per second), and also advanced for free whenever a tailer runs dry. Monotonic while the node keeps shipping the site. |
| `ephpm_cdc_replication_lag_changes` | gauge | `site` | `primary_head_change_id{site} - shipped_change_id{site}`. **The headline replication-lag metric — see the unit warning below.** Clamped at `0`. Published only by the node shipping the site, and absent until the site's first subscriber attaches. |
| `ephpm_cdc_tail_poll_errors_total` | counter | — | `turso_cdc` poll failures. The stream is dropped and the replica resumes from the same watermark, so this is a retry signal, not a data-loss signal. |
| `ephpm_cdc_streams_refused_total` | counter | `stream` | Inbound streams refused because this node is not the elected primary (whole-database) or not the site's owner (per-site) — a peer is dialing a stale address. `stream` is `cdc` or `snapshot`. |
| `ephpm_cdc_snapshots_served_total` | counter | `status` | Snapshot bootstraps served to cold replicas. `status` is `ok` or `error`. |
| `ephpm_cdc_snapshot_bytes_served_total` | counter | — | Logical-dump bytes written to dialing replicas. Successful transfers only. |
| `ephpm_cdc_snapshot_duration_seconds` | histogram | `role` | Snapshot transfer time. `role` is `serve` (primary produced and streamed the dump) or `fetch` (replica received and applied it). |

### Applying side (a replica of the site)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `ephpm_cdc_batches_applied_total` | counter | — | CDC batches applied to the local database, across sites. A batch that failed to apply is **not** counted here; it appears in `ephpm_cdc_apply_errors_total`. |
| `ephpm_cdc_rows_applied_total` | counter | — | `turso_cdc` rows applied. |
| `ephpm_cdc_applied_change_id` | gauge | `site` | This replica's applied watermark for `site`, mirroring litewire's `__litewire_cdc_watermark` table in that site's database. Published on subscribe, after a snapshot bootstrap, and after every applied batch — deliberately **not** advanced past a batch that failed to apply. Watermarks are scoped to the source's CDC log identity, so this gauge legitimately **drops** (usually to 0) when the replica re-subscribes to a newly promoted primary or a new owner. Not published by the site's owner. |
| `ephpm_cdc_apply_errors_total` | counter | — | `apply_batch` failures. The stream is failed and the watermark does not move; the replica reconnects and retries from the identical cursor. Any non-zero rate here means replication is stalled, not degraded. |
| `ephpm_cdc_apply_duration_seconds` | histogram | — | Per-batch apply time on the replica. One batch is one replicated transaction. |
| `ephpm_cdc_replica_connects_total` | counter | `outcome` | Subscribe attempts, one increment per attempt by terminal outcome: `closed` (primary closed the stream cleanly), `dial_error` (could not reach the primary), `stream_error` (connected, then the stream failed), `watermark_error` (could not resolve the local watermark for the log the primary announced). Reconnect rate is the sum across all four. Whole-database mode only — the per-site driver does not record it, so it stays at `0` there. |
| `ephpm_cdc_bootstrap_total` | counter | `outcome` | Cold-start snapshot bootstrap decisions. `outcome` is `ok`, `skipped` (local database already populated) or `failed` (retry budget exhausted — startup aborts deliberately rather than serving incomplete data). Whole-database mode only; stays at `0` in per-site mode. |
| `ephpm_cdc_snapshot_bytes_received_total` | counter | — | Logical-dump bytes received and successfully applied during a bootstrap (both modes, including a per-site ownership handoff). |

### Per-site series lifecycle

In per-site clustered mode a node's role is decided per site, and the per-site
series follow it:

- When this node becomes a site's **owner**, its `applied_change_id{site}` is
  removed and `subscribers{site}` appears (at `0`); the head sampler starts.
- When ownership **moves away**, the site's `subscribers`,
  `primary_head_change_id`, `shipped_change_id` and `replication_lag_changes`
  series are removed; the node then publishes `applied_change_id{site}` as a
  replica.
- When the site's replication driver **exits** (the site left this node's
  replication set), every series for that site is removed.

"Removed" means the series is absent from the next scrape — not set to `0`,
which for a position gauge would be a false value. A replica never publishes a
lag (it does not know the owner's head), and the owner publishes one only for
replicas that have subscribed.

### What the lag metric means (and does not)

`ephpm_cdc_replication_lag_changes` is a **row count, not a duration**. Both of
its inputs are `change_id` values from the same site's change log, and turso
allocates one `change_id` per captured row change. A lag of `500` means five
hundred row changes behind — which is sub-millisecond on an idle cluster and
several minutes behind a bulk import. **Do not graph it with a seconds unit and
do not alert on it as if it were one.**

It is measured at the *ship* boundary — the moment a batch is written into the
subscriber's stream — not at the *apply* boundary on the replica. It therefore
excludes network flight time and the replica's apply cost. For true end-to-end
lag, subtract across nodes **per site** (only the shipping node publishes a
site's head, so `max by (site)` picks it):

```promql
# End-to-end replication lag per site, in change-log rows.
max by (site) (ephpm_cdc_primary_head_change_id)
  - min by (site) (ephpm_cdc_applied_change_id)

# Ship-side lag per site, as the owner sees it.
max by (site) (ephpm_cdc_replication_lag_changes)

# A site whose shipping node has no replica attached — not being replicated.
max by (site) (ephpm_cdc_subscribers) == 0

# Replication is stalled on a failing apply (the watermark is not moving).
rate(ephpm_cdc_apply_errors_total[5m]) > 0
```

Do not aggregate the position gauges *across* sites (`max(...)` without
`by (site)`): each site's `change_id`s count a different log, so a cross-site
difference is meaningless. Before the `site` label existed that is exactly what
the gauges did internally, and a stress run reported thousands of changes of
lag while every replica was byte-identical.

There is deliberately **no seconds-valued lag metric**. Producing one requires a
commit timestamp travelling with each change so the consumer can subtract it
from its own clock. The `turso_cdc` table does store one (`change_time`), but
litewire's `CdcRow` does not expose it, so surfacing a time-based lag needs a
litewire change to carry `change_time` plus a matching CDC wire-format change —
not an ePHPm-side calculation. Until both land, any "seconds behind" figure
would be invented, so none is published.

### Cardinality

The `site` label is bounded by the number of sites this node replicates — one
`default` series in whole-database mode, and in per-site mode one set per site
in this node's replication set (the same bound `ephpm_sqlite_wal_bytes{site}`
has: one entry per vhost directory on disk). A node publishes at most four
series for a site it owns and one for a site it replicates, and removes them
when that stops. Counters and histograms have no `site` label; their other
labels have a fixed, small set of values (`stream`: 2, `status`: 2, `role`: 2,
`outcome`: 4 or 3). There are deliberately **no per-peer or per-address
labels** — a label per replica would scale series count with cluster size and
churn a new series on every pod replacement.

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
- [HTTP Endpoints & Headers](/reference/http-endpoints/) — who can reach the metrics path, and how to restrict it

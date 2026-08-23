+++
title = "Configuration"
weight = 2
+++

Every key in `ephpm.toml`, with type, default, and a short description. The source of truth is [`crates/ephpm-config/src/lib.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — if a field has been added there but not here, that's a doc bug.

All sections and keys are optional. Missing sections use defaults; `Config::default_config()` produces a fully working configuration.

## `[server]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `listen` | string | `"0.0.0.0:8080"` | Address to listen on. |
| `document_root` | path | `"."` | Document root for static files and PHP scripts. |
| `sites_dir` | path | (none) | Virtual host directory. Each subdirectory is named after a domain. Omit for single-site mode. |
| `sites_domain_suffix` | string | (none) | Suffix stripped from the `Host` header before resolving vhosts against `sites_dir` (e.g. `".localhost"` maps `blog.localhost` → `<sites_dir>/blog`). Used by `ephpm dev --sites`. **Must begin with a dot** — startup fails closed otherwise (a dotless suffix would let the apex host strip to the empty vhost key and resolve the whole `sites_dir` as one virtual host). |
| `site_overrides_dir` | path | (none) | **Multi-tenant only.** Directory of operator-supplied per-site overrides, one `<site-key>.toml` per virtual host, declaring that site's web root (`document_root = "public"`). **Must live outside `sites_dir`** — startup fails closed otherwise. `open_basedir` is unaffected and stays the site container. Unset disables the mechanism. See [Virtual Hosts → Per-site document root](/guides/virtual-hosts/#per-site-document-root-frameworks-with-a-public-directory). |
| `run_as_user` | string | (none) | **Unix only.** Numeric uid or username to drop the whole process to after binding privileged ports and opening root-owned files, before serving. See [Virtual Hosts → Dropping root](/guides/virtual-hosts/#dropping-root-run_as_user). A **single non-root uid for the whole process, not per-tenant.** Ignored (with a warning) when not started as root or on Windows. |
| `run_as_group` | string | (none) | **Unix only.** Numeric gid or group name to drop to alongside `run_as_user`. Defaults to the user's primary group (named user) or the same numeric id as the uid. Only consulted when `run_as_user` is set. |
| `index_files` | array of strings | `["index.php", "index.html"]` | Index file names to try when a directory is requested. |
| `fallback` | array of strings | `["$uri", "$uri/", "/index.php?$query_string"]` | URL fallback chain. Variables: `$uri`, `$query_string`. Last entry is the fallback (prefix `=` for status code, e.g. `=404`). |
| `preview` | bool | `false` | Preview-host preset for **not-production** PR-preview instances. Adds `X-Ephpm-Preview: 1` to every response, and every `[server.limits]` knob you did not set explicitly resolves to a preview default instead of "off": `max_connections = 256`, `per_ip_max_connections = 32`, `per_ip_rate = 10.0`, `per_ip_burst = 50`, `per_site_rate = 5.0`, `per_site_burst = 20`. The preset also supplies one `[php]` knob: [`overload_policy = "shed"`](#php), so a saturated preview instance answers `503` + `Retry-After` instead of absorbing requests until the client times out (note this is inert on the default engine unless `[php] workers` is set — startup WARNs if so). Explicit values always win — including explicit `0`, which disables that limit even under preview, and an explicit `overload_policy = "wait"`. Startup logs exactly which values the preset supplied. Env override: `EPHPM_SERVER__PREVIEW=true`. |

### `[server]` — WebSocket entrypoint

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `websocket_files` | array of strings | `["websocket.php"]` | Entrypoint script names tried, in order, when a WebSocket upgrade request arrives — the `index_files` of the WebSocket path. Resolved against the **vhost's** document root, so each virtual host has its own handler or none. The first name that exists wins and receives every event for connections upgraded on that vhost (`connect`, `message`, `disconnect`, distinguished by `$_SERVER['WS_EVENT']`). If **no** name exists in that document root, the upgrade request is answered `404` — it never falls through to static files, `index.php`, or the `[server] fallback` chain. Only consulted when `[server.websocket] enabled = true`. Env override: `EPHPM_SERVER__WEBSOCKET_FILES`. |

### `[server.request]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_body_size` | u64 (bytes) | `10_485_760` (10 MiB) | Max request body. `0` = unlimited. Exceeding sends 413. |
| `max_header_size` | usize (bytes) | `8192` | Max total request header size. |
| `trusted_hosts` | array of strings | `[]` | Allowed `Host` header values. Empty = allow all. Mismatched hosts get 421. `/_ephpm/health`, `/_ephpm/ready`, `/_ephpm/requests` (when the request timeline is enabled), and the metrics path are exempt (probes/scrapes address pods by IP). |

### `[server.timeouts]` (all in seconds)

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `header_read` | u64 | `30` | Time to receive complete request headers after connect. |
| `idle` | u64 | `60` | Idle connection timeout. |
| `request` | u64 | `300` | Total request timeout including PHP execution. `0` disables the per-request deadline (the router skips arming a tokio timer per request); a stuck request then relies on the idle/header-read timeouts instead of a hard cutoff. |
| `shutdown` | u64 | `30` | Grace period for in-flight connections during shutdown. |

### `[server.response]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `compression` | bool | `true` | Enable compression for text responses — brotli when the client accepts it, gzip fallback. |
| `compression_level` | u32 | `1` | Compression level (1=fastest, 9=best). |
| `compression_min_size` | usize (bytes) | `1024` | Minimum response size before compression applies. |
| `compression_streaming` | string | `"off"` | Streamed worker-response (`send_response_stream`) compression: `"off"` (identity, byte-for-byte the previous behavior), `"sse"` (brotli with a per-event flush and a stream-lifetime window for `text/event-stream` responses), `"all"` (every streamed response). Needs `compression = true` and a client `Accept-Encoding: br`; unknown values warn at startup and act as `"off"`. Buffered responses are unaffected. |
| `headers` | array of `[string, string]` | `[]` | Custom headers added to every response. |

### `[server.static]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cache_control` | string | `""` | Cache-Control header value for static files. Empty = no header. |
| `hidden_files` | string | `"deny"` | How to handle dot-files: `"deny"` (403), `"ignore"` (404), `"allow"`. |
| `etag` | bool | `true` | Emit `ETag` headers and serve `304 Not Modified` on conditional requests. |

### `[server.php_etag_cache]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Cache PHP-emitted ETags in the KV store; serve 304s without re-running PHP. |
| `ttl_secs` | i64 | `300` | TTL for cached entries. `<=0` means cache indefinitely. |
| `key_prefix` | string | `"etag:"` | KV key prefix for cached entries. |

### `[server.security]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `trusted_proxies` | array of strings | `[]` | CIDR ranges trusted for `X-Forwarded-For`/`X-Forwarded-Proto`. |
| `blocked_paths` | array of strings | `[]` | Glob patterns blocked with 403. |
| `allowed_php_paths` | array of strings | `[]` | When non-empty, only matching PHP paths execute. Others get 403. |
| `open_basedir` | bool | `true` if a `[server.security]` section is present **or** `server.sites_dir` is set, else `false` | Restrict PHP filesystem access to the **site container** (the vhost directory under `sites_dir`) plus that site's own private temp/session state root (never the shared system temp — see [Virtual Hosts → Filesystem Isolation](/guides/virtual-hosts/#filesystem-isolation-temp-sessions)). Note *container*, not document root: with `site_overrides_dir` a site's web root may be a subdirectory, and the sandbox deliberately stays the whole container so PHP can `require` from above the web root. **Multi-tenant only — see below.** |
| `disable_shell_exec` | bool | `true` if a `[server.security]` section is present **or** `server.sites_dir` is set, else `false` | Disable `exec`, `shell_exec`, `system`, `passthru`, `proc_open`, `popen`, `pcntl_exec`. **Multi-tenant only — see below.** |
| `multi_tenant_hardening` | bool | `true` if a `[server.security]` section is present **or** `server.sites_dir` is set, else `false` | Apply the [multi-tenant confidentiality/integrity denylist preset](/guides/virtual-hosts/#multi-tenant-hardening-preset) on top of `disable_shell_exec`: `pcntl_*`, `posix_kill`/`posix_set*id`, `pfsockopen`/`fsockopen`, SysV `shm_*`/`sem_*`/`msg_*`, `opcache_reset`/`opcache_compile_file`, `dl`, `mail`, plus `mysqli.allow_persistent=0` and (when `[opcache] cluster_invalidation` is off) `opcache.restrict_api`. Composed as a **union** with any operator `disable_functions`. **Cost: persistent DB/socket connections stop working.** **Multi-tenant only — see below.** |

> **These knobs are enforced only in multi-tenant mode.** They take effect only
> when `[server] sites_dir` is set. In single-site mode the values resolve
> normally but nothing acts on them: `open_basedir` is gated on
> `sites_dir.is_some()` before the per-request ini hook applies it, and
> `disable_shell_exec` is gated the same way before it is written into the
> generated `php.ini`. Setting either to `true` on a single-site deployment
> gives you **no sandboxing and no warning**. To harden a single-site install,
> use `[php] ini_overrides` to set `open_basedir` / `disable_functions`
> directly.

**Note:** an explicitly set value always wins. When unset, these three resolve to `true` if either the `[server.security]` section is present (matching earlier releases) or `server.sites_dir` is set — so multi-tenant deployments get filesystem isolation, shell-exec hardening, and the full hardening denylist by default, even without a `[server.security]` section. To opt out in multi-tenant mode you must set them to `false` explicitly (ephpm logs a warning at startup when you do). `multi_tenant_hardening = false` keeps persistent DB/socket connections at the cost of the cross-tenant channels the preset closes.

**These flags do nothing in single-site mode** (`server.sites_dir` unset). They are implemented only on the vhost request path: `open_basedir` is set per-request from the resolved site's directory, and the `disable_functions` line (baseline + hardening) is only written into the generated php.ini when `sites_dir` is set. Because a single-site `document_root` is the *web root* rather than a site container directory, confining PHP to it would break any application that keeps its code above the web root — so the multi-tenant mechanism is not reused here. ephpm logs a warning at startup for each flag that resolves to `true` while inert, rather than silently doing nothing.

To sandbox a single-site deployment, set PHP's own directives through `[php] ini_overrides` — those lines go into the generated php.ini and are applied at MINIT:

```toml
[php]
ini_overrides = [
  ["open_basedir", "/app:/tmp"],
  ["disable_functions", "exec,passthru,shell_exec,system,proc_open,popen,pcntl_exec"],
]
```

### `[server.logging]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `access` | string | `""` | Path to access log file. Empty = disabled. |
| `level` | string | `"info"` | Log level: `trace`, `debug`, `info`, `warn`, `error`. Overridden by `RUST_LOG`. |

### `[server.metrics]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable the Prometheus `/metrics` endpoint. |
| `path` | string | `"/metrics"` | URL path for the metrics endpoint. |

### `[server.diagnostics]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `request_log` | bool | (mode default) | Per-request timeline: the last 256 completed requests (method, path, status, total/queue-wait/PHP durations in ms, response bytes, timestamp) served as JSON at `GET /_ephpm/requests`, newest first. Unset resolves per mode: **on** under `ephpm dev` / bare `ephpm`, **off** under `ephpm serve`. `queue_wait_ms` is `null` outside worker mode (there is no dispatch queue on the fpm path); in worker mode `php_ms` includes the queue wait, matching `ephpm_php_execution_duration_seconds`. Requests to `/_ephpm/*` and the metrics path are not recorded. When off, `/_ephpm/requests` is not registered and the path falls through to normal routing. Env override: `EPHPM_SERVER__DIAGNOSTICS__REQUEST_LOG`. |
| `otlp_endpoint` | string | (none) | OTLP trace-export endpoint, e.g. `"http://127.0.0.1:4318"` (OTLP **http/protobuf**; `/v1/traces` is appended when missing). Exports one `http.request` span per request (attrs: method, path, status) with `worker.queue_wait` and `php.execute` children, and honors an incoming W3C `traceparent` header. Requires a binary built with the `otlp` cargo feature — without it, a set value only logs a startup warning. The standard `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT` env vars take precedence over this knob, and `OTEL_SERVICE_NAME` overrides the default service name `ephpm`. Unset (and no env vars): no exporter is built and no background export thread runs. Both `http://` and `https://` endpoints are supported — see the note below. |

**OTLP over HTTPS.** An `https://` endpoint works with no extra configuration.
TLS is rustls (the same crypto provider the HTTPS listener uses — never
OpenSSL or a second TLS stack), and the exporter trusts the **union of the
operating system's trust store and the bundled Mozilla root set**. The OS
store is what lets ePHPm reach an internal collector fronted by a corporate or
private CA; the bundled set is a fallback so a publicly trusted endpoint still
works from a `scratch`/distroless image that ships no CA bundle. To trust a
private CA that is not installed system-wide, point the standard
`SSL_CERT_FILE` (a PEM bundle) or `SSL_CERT_DIR` environment variable at it —
there is no ePHPm-specific trust-store setting. Note that setting either
variable *replaces* the OS trust store rather than adding to it (the bundled
Mozilla set is still trusted), so such a bundle must contain every CA the
exporter needs. `http://` endpoints are
unaffected and remain the zero-config default for a collector on localhost.
Export timeout follows the standard `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` /
`OTEL_EXPORTER_OTLP_TIMEOUT` variables (milliseconds, default `10000`).

### `[server.limits]`

Defaults below are the resolved values when the key is absent. Under
`[server] preview = true`, absent keys resolve to the preview preset instead
(shown per key); an explicitly set value always wins over either default,
including explicit `0` = that limit off.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_connections` | usize | `0` (preview preset: `256`) | Total concurrent connections. `0` = unlimited. Connections beyond the limit get a raw 503 at accept time and are closed, not served. |
| `per_ip_max_connections` | usize | `0` (preview preset: `32`) | Per-IP concurrent connections. `0` = unlimited. |
| `per_ip_rate` | f64 | `0.0` (preview preset: `10.0`) | Per-IP requests/second (token bucket). Over-limit requests get 429. `0` = unlimited. |
| `per_ip_burst` | u32 | `50` (preview preset: `50`) | Burst allowance for per-IP rate limiting. |
| `per_site_rate` | f64 | `0.0` (preview preset: `5.0`) | Per-virtual-host **PHP executions**/second (token bucket), keyed by the canonical site key from vhost resolution — so one tenant addressed as `blog.localhost` and `blog` drains one bucket. Over-limit requests get 429 with `Retry-After`, before PHP runs. Static files and PHP-ETag-cache 304s are not counted. Only acts when `[server] sites_dir` is set (requests that match no site have no site key and are not per-site-capped). `0` = unlimited. |
| `per_site_burst` | u32 | `20` (preview preset: `20`) | Burst allowance for the per-site rate limit: PHP executions a site may make instantly before `per_site_rate` applies. |

### `[server.file_cache]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | In-memory cache for static file metadata + small-file content. |
| `max_entries` | usize | `10_000` | Max cached entries. Oldest evicted on overflow. |
| `valid_secs` | u64 | `30` | Re-stat interval. |
| `inactive_secs` | u64 | `60` | Evict entries not accessed within this many seconds. |
| `inline_threshold` | usize (bytes) | `1_048_576` (1 MiB) | Cache file content below this size; metadata-only above. |
| `precompress` | bool | `true` | Pre-compute gzip-compressed variants for small compressible files. |

### `[server.tls]`

Two mutually exclusive modes — manual (`cert`+`key`) or ACME (`domains`). If both are set, manual wins.

> **Fixed in v0.6.2.** Manual `cert`+`key` mode panicked at startup on every
> release from v0.1.0 through v0.6.1 — the process exited before binding a
> listener ([#243](https://github.com/ephpm/ephpm/pull/243)). ACME mode was
> unaffected. If you are on v0.6.2 or newer, both modes work; upgrade rather
> than working around it. See [TLS / ACME](/guides/tls-acme/).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cert` | path | (none) | PEM-encoded certificate chain (manual mode). |
| `key` | path | (none) | PEM-encoded private key (manual mode). |
| `domains` | array of strings | `[]` | Domains for ACME / Let's Encrypt (auto mode). |
| `email` | string | (none) | Contact email for ACME registration. |
| `cache_dir` | path | `"certs"` | Directory for ACME cert + account key cache. **Set this in production.** |
| `staging` | bool | `false` | Use Let's Encrypt staging (untrusted certs, generous rate limits). |
| `listen` | string | (none) | Separate HTTPS listener. When set, `[server] listen` serves HTTP and this serves HTTPS. |
| `redirect_http` | bool | `false` | When `listen` is set, the HTTP listener redirects everything to HTTPS (301). |

### `[server.http3]`

HTTP/3 runs over **UDP**, *in addition to* the TCP listeners — HTTP/1.1 and HTTP/2 keep working exactly as before. Enabling it binds one extra UDP socket.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable the HTTP/3 (QUIC) listener. Requires a static `[server.tls]` `cert`+`key`; startup **fails** if TLS is absent or in ACME mode rather than silently serving TCP only. |
| `listen` | string | (none) | UDP address for QUIC. Unset derives it from the HTTPS listener — `[server.tls] listen` when set, otherwise `[server] listen` — i.e. the same port as HTTPS, on UDP. |
| `alt_svc_max_age` | u64 (sec) | `86400` (24 h) | `ma=` value of the `Alt-Svc` header advertised on HTTPS responses. `0` suppresses the header entirely. |

**Clients only reach HTTP/3 after seeing `Alt-Svc`.** A browser speaks TCP first and switches to HTTP/3 on a later request, once it has seen `Alt-Svc: h3=":443"; ma=86400` on an HTTPS response. ePHPm emits that header on every TLS-terminated response (not on plain `http://`, which cannot upgrade). Setting `alt_svc_max_age = 0` means no browser will ever discover HTTP/3 — only clients told about it out of band (`curl --http3-only`, or an `HTTPS`/`SVCB` DNS record) will connect.

**Limitation — ACME is not supported on HTTP/3 yet.** QUIC bakes its certificate into the endpoint at bind time, whereas `rustls-acme` rotates certificates during the process lifetime. `enabled = true` together with ACME TLS is a startup error, deliberately: ePHPm refuses to come up quietly without the HTTP/3 listener you asked for. Use a static `cert`/`key` for now; ACME support is planned.

**Firewalls:** QUIC is UDP. Opening TCP/443 is not enough — UDP on the same port must also be reachable, or clients will try HTTP/3, fail, and silently fall back to TCP.

### `[server.websocket]`

**Experimental.** Native WebSocket termination: Rust owns the sockets, PHP runs per event via the vhost's `[server] websocket_files` entrypoint. See the [WebSockets guide](/guides/websockets/).

With `enabled = false` (the default) an upgrade request routes exactly like any other `GET`, so turning this section on is the only thing that changes upgrade routing.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable native WebSocket support. Opt-in: it changes how upgrade requests route and admits long-lived connections that outlive their HTTP request. Env override: `EPHPM_SERVER__WEBSOCKET__ENABLED`. **Startup error** when combined with `[php] mode = "worker"` (WebSocket events dispatch through the fpm per-request path), or when `[server] websocket_files` is empty. |
| `max_connections` | usize | `10000` | Total concurrent WebSocket connections across all vhosts. `0` = unlimited. Upgrades beyond the cap get `503`. A **separate** budget from `[server.limits] max_connections`: an upgraded socket is handed to its own task and stops occupying an HTTP connection slot, so the HTTP cap cannot bound it. |
| `max_connections_per_site` | usize | `1000` | Per-vhost cap, enforced in addition to `max_connections` so one tenant cannot consume a shared node's whole budget. `0` = unlimited. Rejected upgrades get `503`. |
| `max_message_size` | usize (bytes) | `1048576` (1 MiB) | Largest inbound message (after reassembling continuation frames), and largest payload PHP may push with `ephpm_ws_send()` / `ephpm_ws_broadcast()`. Deliberately independent of `[server.request] max_body_size` — a WebSocket event's payload is bounded by this knob, not the HTTP body limit. |
| `max_frame_size` | usize (bytes) | `1048576` (1 MiB) | Largest single inbound frame. `max_message_size` bounds the reassembled total. |
| `send_queue` | usize (frames) | `64` | Depth of each connection's outbound queue. When full, the frame is **not** buffered: the send reports failure and the socket is closed with WebSocket status `1013`. A slow reader costs one connection, never the server's memory. `0` is normalized to `1` with a warning. |
| `ping_interval_secs` | u64 | `30` | Seconds between server-initiated pings. `0` disables keepalive — which also means idle connections will be dropped, since pings are what refresh `idle_timeout_secs`. |
| `idle_timeout_secs` | u64 | `120` | Seconds a connection may receive **nothing** (including a pong) before it is closed with `1001`. `0` disables the check. Keep comfortably larger than `ping_interval_secs`; a warning is logged if it is not. |

Two ceilings are fixed rather than configurable: 64 channel subscriptions per connection and 256-byte channel names. They bound what one misbehaving script can pin.

```toml
[server.tls]
cert = "/etc/ephpm/fullchain.pem"
key  = "/etc/ephpm/privkey.pem"

[server.http3]
enabled = true          # binds UDP on the HTTPS address
alt_svc_max_age = 86400
```

## `[php]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_execution_time` | u32 (sec) | `30` | PHP `max_execution_time` per request (`0` = unlimited). Natively enforced on Linux ZTS builds (per-thread execution timers): exceeding it raises the catchable "Maximum execution time exceeded" fatal (HTTP 500, shutdown functions run, output flushed), is wall-clock (`sleep()` counts), and is overridable at runtime with `set_time_limit()`. `[server.timeouts] request` is the outer hard 504 backstop — keep this below it. Not natively enforced on macOS/Windows (no per-thread timers); there the request timeout is the only ceiling. |
| `memory_limit` | string | `"128M"` | PHP `memory_limit`. Serves as the dev-mode value and the ultimate fallback; in serve mode it is superseded by the auto-derived per-request limit (see `php_memory_limit` and [Resource-aware autotuning](#resource-aware-autotuning)). |
| `opcache_validate_timestamps` | bool | (mode default) | Override `opcache.validate_timestamps`. Unset resolves per mode: **off** (`0`) under `ephpm serve` (trust the cache — refresh code with `ephpm deploy` / `ephpm cache reset`), **on** (`1`) under `ephpm dev` (instant edit-refresh). Set `true`/`false` to force a value in either mode. See the [deploy guide](/guides/opcache-cluster-invalidation/) for the deploys-are-events contract. |
| `opcache_revalidate_freq` | u32 (sec) | (none → PHP default `2`) | Override `opcache.revalidate_freq`. Only meaningful when timestamp validation is on: how often (at most) the engine re-`stat()`s a cached script. Raising it (e.g. `60`) cuts `stat()` traffic on overlay/network filesystems at the cost of slower edit pickup. Ignored when validation is off. |
| `opcache_memory_consumption` | u32 (MB) | (auto-derived) | Override `opcache.memory_consumption`. Unset → auto-derived in serve mode (~18% of the detected memory budget, clamped `[64, 512]` MB on Linux/macOS and `[64, 256]` MB on Windows); dev keeps PHP's 128 MB. An explicit value is honoured on every platform. See [Resource-aware autotuning](#resource-aware-autotuning). |
| `opcache_interned_strings_buffer` | u32 (MB) | (auto-derived) | Override `opcache.interned_strings_buffer`. Unset → auto-derived (~1 MB per 16 MB of opcache SHM, clamped `[8, 64]` MB) in serve mode; PHP default in dev. |
| `opcache_jit_buffer_size` | u32 (MB) | (auto-derived) | Override `opcache.jit_buffer_size`. Unset → auto-sized (~1/64 of memory, clamped `[32, 64]` MB) in serve mode. Whether the JIT *uses* the buffer is governed by `opcache_jit` (below). When the JIT is on and no size was derived (dev mode), the same derivation is forced so an enabled JIT is never silently bufferless. An explicit `0` is respected — and warned about — because it makes the JIT inert. |
| `opcache_jit` | string | (shaped — see [OPcache JIT](#opcache-jit)) | `opcache.jit` mode: `"tracing"`, `"function"`, or `"disable"` (anything else is a startup error; raw CRTO digits are deliberately not accepted — use `ini_overrides` if you truly need them). Set explicitly, it wins in **every** mode. **Unset, the default is shaped:** `tracing` in single-site `ephpm serve`; `disable` in multi-tenant serve (`sites_dir` set — per-vhost invalidation never reclaims JIT buffer), in worker mode (not positively verified against the persistent-worker lifecycle), and in dev. Startup always logs the effective state and why. Env: `EPHPM_PHP__OPCACHE_JIT=disable`. |
| `opcache_max_accelerated_files` | u32 | `20000` (serve) | Override `opcache.max_accelerated_files`. A generous **fixed** default in serve mode (PHP default in dev). Deliberately *not* derived from memory — the right value is shaped by how many `.php` files the app has, not the machine size. |
| `php_memory_limit` | string | (auto-derived) | Override the per-request `memory_limit`, taking precedence over `memory_limit` **and** the derivation. Unset → serve mode derives `(memory_budget − opcache_shm − ~64 MB overhead) / worker_count`, floored at `128 MB`; with no detectable memory budget it keeps PHP's `128M`. Dev keeps `memory_limit`. |
| `realpath_cache_size` | string | `16M` (serve) | Override `realpath_cache_size`. Serve uses `16M` (vs PHP's `256K`) to cut `realpath()`/`stat()` traffic on deep autoload trees; dev keeps the PHP default so new files resolve instantly. |
| `realpath_cache_ttl` | u32 (sec) | `600` (serve) | Override `realpath_cache_ttl`. Serve uses `600` (vs PHP's `120`); dev keeps the PHP default. |
| `zend_assertions` | i8 | `-1` (serve) / `1` (dev) | Override `zend.assertions`. Serve uses `-1` (assertions compiled out — zero runtime cost, production-recommended); dev uses `1` (assertions active). Set `-1`/`0`/`1` to pin. |
| `ini_file` | path | (none) | Custom `php.ini` loaded before `ini_overrides`. |
| `ini_overrides` | array of `[string, string]` | `[]` | INI directives applied after `ini_file`. In worker mode, `log_errors=On` is seeded as a default before `ini_file`/`ini_overrides` (either can override it) so worker-script fatals reach the engine log — `display_errors` output is captured into a buffer that is discarded when no request is in flight. |
| `extensions` | array of string | `[]` | Shared PHP extensions loaded at startup as `extension=` lines in the generated php.ini, emitted **before** `ini_file`/`ini_overrides`. Bare names (`"redis"`) use PHP's `extension_dir` search; paths load verbatim. Must match the embedded PHP's ABI: same PHP minor, ZTS (every platform, Windows included), glibc on Linux — PHP reports a mismatch at startup. Note distro/[Sury](https://deb.sury.org/) extension packages are NTS-only (no ZTS variants as of July 2026) — on Linux, compile the extension for ZTS (phpize/gcc against matching ZTS headers). Empty entries fail validation. See the [PHP Extensions guide](/guides/php-extensions/). |
| `workers` | usize | `0` (unlimited) | Max concurrent PHP executions (php-fpm `pm.max_children` semantics); excess requests queue. `0` = unlimited. **Ignored in worker mode** (startup logs a WARN if set). |
| `mode` | string | `"fpm"` | Request-execution model. `"fpm"` = per-request startup/shutdown (default, unchanged). `"worker"` = persistent worker mode: boot the framework once per worker, loop over requests (Octane/RoadRunner model). |
| `fpm_engine` | string | `"spawn_blocking"` | **Experimental (fpm mode only).** How a per-request PHP execution is scheduled onto a thread. `"spawn_blocking"` (**default**, unchanged) runs it on tokio's shared blocking pool. `"pool"` runs it on ePHPm's own dedicated OS-thread pool sized to `worker_count` — the pool size is the concurrency cap, so `workers` is bypassed (full queue → 504, closed pool → 503, wedged thread → 504 + replacement). An unknown value is a startup error. Ignored in worker mode (startup logs a WARN if `pool` is set there). Env: `EPHPM_PHP__FPM_ENGINE=pool`. Benchmark before enabling. |
| `crash_containment` | bool | `false` | **Experimental. Requires `fpm_engine = "pool"`.** Contain a PHP C-stack overflow **from a deep object-graph free** instead of letting its `SIGSEGV` abort the whole process. (Runaway *recursion* does not need this: PHP's own C-stack guard catches it at the VM's call checkpoints and raises the catchable `Error: Maximum call stack size ... reached`, which is a plain `500` on every platform and in every mode. A destructor cascade passes no such checkpoint, which is what this knob is for.) The offending request is answered `500` and the pool thread that ran it is retired and replaced. **Only stack-overflow faults are contained** — heap corruption and wild writes produce the same signal but may already have damaged shared memory, so they still die with the usual fatal-signal diagnostic. Costs: each contained crash abandons the poisoned thread's PHP context (a bounded leak), and once any crash has been contained the process skips PHP module shutdown at exit. Set without the pool engine (or in worker mode) it changes nothing and startup logs a WARN. Env: `EPHPM_PHP__CRASH_CONTAINMENT=true`. |
| `overload_policy` | string | unset (`"wait"`; `"shed"` under `[server] preview`) | **fpm mode only.** What a PHP request does when no execution slot is free. `"wait"` queues and waits — historical behaviour, where the outer `[server.timeouts] request` deadline is the only bound. `"shed"` answers `503 Service Unavailable` + `Retry-After: 1` once the request has waited `shed_after_ms`, turning overload into fast, countable errors instead of client timeouts. **Unset is not the same as `"wait"`**: it resolves to `"shed"` under `[server] preview = true` (set `"wait"` explicitly to opt out). What it bounds depends on the engine — with `fpm_engine = "pool"` it is the dispatch backlog (`worker_backlog`); with the default `fpm_engine = "spawn_blocking"` it is the `workers` semaphore, and **with `workers = 0` (the default) nothing is shed**, because tokio's blocking queue is unbounded and its entries cannot be withdrawn. Startup logs which policy is in force, where it came from, and WARNs on the inert combination. Ignored in worker mode (startup WARNs). Shed responses are counted by `ephpm_php_shed_total`. An unknown value is a startup error. Env: `EPHPM_PHP__OVERLOAD_POLICY=shed`. |
| `shed_after_ms` | u64 (ms) | `0` | How long a request may wait for a PHP execution slot before `overload_policy = "shed"` rejects it. `0` = do not wait at all — take a free slot if there is one, otherwise shed immediately (the admission queue, `worker_backlog` slots or `workers` slots, is already the buffer). Raise it to absorb bursts before shedding. Inert when the policy is `"wait"`. Env: `EPHPM_PHP__SHED_AFTER_MS=250`. |
| `worker_script` | path | (none) | Worker-mode entrypoint, relative to `document_root`. **Required** when `mode = "worker"`; config load hard-errors if absent or not a file under `document_root`. |
| `worker_count` | usize | `0` (derive) | Number of persistent worker threads. `0` derives from the cgroup CPU quota when running under one (Linux), otherwise from host parallelism clamped `[2, 32]`. Applies on every platform — Windows is ZTS and runs multiple workers concurrently (the historical forced-to-1 Windows clamp was removed, #326). Startup logs the derivation source. Worker mode only. |
| `worker_max_requests` | u64 | `10000` | Recycle a worker after N requests — pure leak guard for the framework kernel. For a leak-free loop, recycling is pure overhead (framework re-boot cost); prefer `0` when you trust your kernel. Each recycle is logged at debug (worker id, requests served, uptime). Worker mode only. |
| `worker_backlog` | usize | `0` (= `worker_count`) | Dispatch-queue depth. A full queue applies backpressure; a starved queue becomes a 504 via the request timeout. Worker mode only. |
| `worker_boot_timeout` | u64 (sec) | `30` | Seconds a worker gets to boot and reach its first `take_request()`. A boot still running when this expires is logged as an error and counted in `ephpm_worker_boot_timeouts_total`; the thread is not killed and still becomes ready if the boot completes. (A boot that *fails* — the script exits before its first `take_request()` — is counted as a boot failure and respawned with backoff, independent of this timeout.) Worker mode only. |
| `worker_populate_superglobals` | bool | `false` | Populate native `$_GET`/`$_POST`/`$_SERVER`/... per request. Off for Octane/PSR-15 (they build their own request); on for the WordPress adapter. Worker mode only. |
| `worker_stream_threshold` | u64 (bytes) | `1048576` (1 MiB) | Request-body size at/above which the body **streams** into the worker in fixed-size chunks instead of buffering whole (Phase 3). Requests with a `Content-Length` at/above this — or with no `Content-Length` (chunked) — flow through `Envelope::bodyStream()` / PHP's POST reader with flat worker memory (a multi-GB upload never materializes in RAM). Smaller bodies stay buffered. Worker mode only. |

> **Worker mode is not supported with `[server] sites_dir`** (multi-tenant vhosting) in Phase 1 — config load hard-errors. Worker mode boots one framework per worker; per-host worker pools are a later phase.

### Resource-aware autotuning

On boot, `ephpm serve` detects the container's CPU and memory limits (cgroup-aware) and derives a tuned set of PHP/OPcache ini defaults sized to the box it is actually running on. This is the *deploys-are-events, right-size-the-runtime* story: you ship the same image to a 320 MiB / 0.25-CPU pod and a 4 GiB / 4-CPU node, and each one sizes OPcache, the per-request memory limit, interned-string and JIT buffers, and the realpath cache to fit — without a per-environment config file.

**Detection (Linux):**

1. **CPU quota** — cgroup v2 `cpu.max`, else v1 `cpu.cfs_quota_us`/`cpu.cfs_period_us`. `None` when unlimited. (Already drives `worker_count`.)
2. **Memory budget** — cgroup v2 `/sys/fs/cgroup/memory.max`, else v1 `memory.limit_in_bytes`; `"max"`/the unlimited sentinel means no limit, in which case ePHPm falls back to total system memory (`/proc/meminfo` `MemTotal`). No new crate — it reads the same cgroupfs/`/proc` files as the CPU path.

**Detection (Windows):**

1. **CPU quota** — no cgroup equivalent is read; `worker_count` derives from host parallelism.
2. **Memory budget** — the calling process's **job-object** memory limit (`QueryInformationJobObject` with `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`; the smaller of `JOB_OBJECT_LIMIT_PROCESS_MEMORY` / `JOB_OBJECT_LIMIT_JOB_MEMORY` when set), else **total physical RAM** (`GlobalMemoryStatusEx` → `ullTotalPhys`). A job limit is only used when it is strictly below physical RAM — one that isn't restricts nothing, so the physical figure is reported instead.

macOS has no memory probe: it reports `mem=unknown (unknown)` and keeps PHP defaults for memory-shaped knobs.

The source label in the startup line says which probe won — `cgroup v2`, `cgroup v1`, `job-object`, `system-total`, or `unknown`. A budget is never guessed: if every probe fails the label is `unknown` and the derivation falls back to the floors in the table below.

**Derivation (serve mode):**

| Directive | Formula | Clamp |
|-----------|---------|-------|
| `opcache.memory_consumption` | ~18% of memory budget | `[64, 512]` MB (Linux/macOS)<br>`[64, 256]` MB (Windows — see below) |
| `opcache.interned_strings_buffer` | ~1 MB per 16 MB of opcache SHM | `[8, 64]` MB |
| `opcache.jit_buffer_size` | ~1/64 of memory budget (used when the JIT is on — see [OPcache JIT](#opcache-jit)) | `[32, 64]` MB |
| `opcache.max_accelerated_files` | fixed `20000` (app-shaped, not memory-shaped) | — |
| `memory_limit` (per request) | `(budget − opcache_shm − ~64 MB overhead) / worker_count` | floor `128` MB |
| `realpath_cache_size` | `16M` | — |
| `realpath_cache_ttl` | `600` | — |
| `zend.assertions` | `-1` (compiled out) | — |

Dev mode (`ephpm dev` / bare `ephpm`) derives none of these: it keeps PHP-friendly defaults (timestamp validation on, assertions on, loose realpath) so the edit-refresh loop stays tight.

**Resolution precedence (per directive):** explicit `[php]` value → auto-derived → PHP stock default. Pin any single knob (e.g. `opcache_memory_consumption = 256`) and the rest keep auto-tuning. `ini_overrides` still layers last as the ultimate escape hatch.

**Windows: OPcache shared memory.** Two things differ on Windows, both because PHP's Windows OPcache backend is a named, pagefile-backed section object rather than an anonymous mapping:

- **The derived `opcache.memory_consumption` is capped at 256 MB**, not 512 MB. Windows charges the whole segment against the system commit limit the moment it is created — before a single script is cached — whereas the Unix mapping commits pages lazily as the cache fills. 256 MB still holds far more compiled script than WordPress-plus-plugins or a large Laravel app produces. An **explicit** `opcache_memory_consumption` is honoured as written on every platform; on Windows a value above 256 MB additionally logs a startup `WARN`, because if the reservation fails PHP aborts the process from module startup rather than starting without OPcache.
- **ePHPm sets `opcache.cache_id` to a per-process value** (`ephpm-<pid>`). PHP names its Windows SHM section from the user name, SAPI name, build id, `opcache.cache_id`, and the requested size, in a namespace shared across the session. Two ePHPm processes that compute the same name make the second one *reattach* to the first one's segment, which PHP permits only if the VM opcode handlers sit at the same address in both images — cached opcodes store absolute handler pointers. Different binaries get different ASLR image bases, so the second process would die at startup with `Opcode handlers are unusable due to ASLR`. A per-process `cache_id` gives each process its own segment, which is how ePHPm already behaves on Linux and macOS (PHP does not support cross-process reattachment there at all). Setting your own `opcache.cache_id` through `ini_overrides` overrides this.

**Transparency:** serve startup logs one INFO line summarizing what was detected and derived, marking any explicitly-pinned value with a `*`. Example for a 320 MiB / 0.25-CPU pod:

```
autotune (serve): cpu_quota=0.25 mem=320MiB (cgroup v2) -> workers=1[cgroup_quota] opcache.memory_consumption=64MB memory_limit=192M interned=8MB jit_buffer=32MB (jit=disable) max_files=20000 realpath=16M/ttl=600 validate_timestamps=0 assertions=-1
```

and for a 4 GiB / 4-CPU node:

```
autotune (serve): cpu_quota=4.00 mem=4096MiB (cgroup v2) -> workers=4[cgroup_quota] opcache.memory_consumption=512MB memory_limit=880M interned=32MB jit_buffer=64MB (jit=disable) max_files=20000 realpath=16M/ttl=600 validate_timestamps=0 assertions=-1
```

(The `jit=` segment shows the resolved `opcache.jit` mode. With the knob unset it reads `jit=disable` in every serve mode — see [OPcache JIT](#opcache-jit) — and an explicitly pinned mode carries the `*` marker like every other tunable. Dev mode with the knob unset shows `jit=off (php default)` — no line is emitted at all.)

### OPcache JIT

`ephpm serve` **disables the JIT by default in every mode**, for a different reason in each. In single-site serve the reason is an unfixed upstream defect in PHP's tracing JIT that kills the process (see below); this was Windows-only until the same crash was reproduced on Linux, and the default is now off on **every platform**. `[php] opcache_jit` overrides the default in any mode. A dedicated startup INFO line always states the effective JIT state and the reason (and risky explicit combinations WARN) — the state is never silent.

| Mode | `opcache_jit` unset | Why |
|------|--------------------|-----|
| Single-site serve (no `sites_dir`, `mode = "fpm"`) | **`disable`** | Upstream PHP defect, not an ePHPm one. PHP's tracing JIT resolves an opcode handler through `ZEND_FUNC_INFO(exit_info->op_array)` when it compiles a **side trace**; that `op_array` can be a heap copy (a method of a linked class the inheritance cache could not persist into SHM) which is freed at request shutdown while the parent trace's exit info lives on in SHM. Compiling the side trace in a *later* request reads a dangling pointer: `0xC0000005` on Windows, `SIGSEGV` on Unix — no PHP error, process gone, every in-flight request lost. A stock Laravel app dies after **three requests** on Windows; on Linux the same app dies at **request 2** as soon as any class links against a parent that is not in OPcache SHM, in the identical faulting frames. It reproduces on **stock `php -S`** with no ePHPm involved, on Linux and Windows alike, on both PHP 8.4.23 and 8.5.7. Tracked upstream at [php-src PR 21710](https://github.com/php/php-src/pull/21710) (open; regression from php-src PR 21368, first released in 8.4.24 / 8.5.5). PHP 8.3 predates the regression and is unaffected. |
| Multi-tenant serve (`sites_dir` set) | **`disable`** | Per-vhost deploys invalidate OPcache with `opcache_invalidate`, and invalidation **never reclaims JIT buffer** (measured: `buffer_free` is untouched). Each deploy would permanently consume buffer until the JIT silently stops compiling — no error, no log. Only a full `opcache_reset` reclaims it, and the multi-tenant hardening preset disables `opcache_reset`; a restart is the practical reset. |
| Worker mode (`mode = "worker"`) | **`disable`** | Long-lived workers are *theoretically* ideal for the JIT (compile once, stay hot — see the worker-mode design doc), but the combination has not been positively verified against the persistent-worker request lifecycle, so it stays off until it is. Opt in explicitly if your workload benefits. |
| Dev (`ephpm dev` / bare `ephpm`) | off (nothing emitted) | Dev keeps the generated ini minimal; PHP's own defaults keep the JIT off. |

Caveats, honestly stated:

- **JIT helps CPU-bound *PHP* code, not builtins or I/O.** A workload dominated by C builtins (e.g. `hash()`) measured **−17% RPS** with the JIT on — tracing overhead with nothing compilable to win back (see [benchmarking findings](/benchmarking/findings/)). Real filesystem-bound apps measure roughly flat. The `disable` knob is the benched answer for such workloads.
- **A JIT miscompile has no crash containment.** The JIT emits native code; a miscompiled trace that faults is a process-level `SIGSEGV`/access violation, and (unlike the contained PHP stack-overflow case on Linux) there is **no** containment for it — on Windows none at all. This is exactly why the escape hatch is a single trivially set knob: `opcache_jit = "disable"` (or `EPHPM_PHP__OPCACHE_JIT=disable`) and nothing else changes.
- **Forcing the tracing JIT back on** works and is the operator's call, but buys the crash described above; startup WARNs. Whether *your* app trips it depends on whether it links a class whose parent is not in OPcache SHM — an untouched Laravel skeleton on Linux served 20 000 requests clean, but an `eval()`-defined parent (mock and proxy generators do this) or an OPcache-blacklisted parent file both killed it at request 2, and a full or restarting OPcache has the same effect. If you want *some* JIT, use `opcache_jit = "function"` — the function JIT compiles whole hot functions and never builds traces, so it cannot reach the defective side-trace path (verified clean where `"tracing"` dies at 2–3). Its measured benefit on real web apps is close to zero, though; `disable` is the honest default.
- **Forcing the JIT on in multi-tenant mode** (`opcache_jit = "tracing"` with `sites_dir`) works and is the operator's call, but inherits the buffer-leak-on-deploy cost above; startup WARNs, and the `ephpm_opcache_jit_buffer_free_bytes` gauge ([metrics reference](/reference/metrics/#opcache-jit)) is the thing to watch — when it flatlines near 0, newly deployed code runs interpreted. Note the gauge samples `opcache_get_status`, which the multi-tenant hardening preset removes unless `[opcache] cluster_invalidation` keeps the OPcache API open — enable it if you want the gauge in this configuration.

## `[db]`

### `[db.mysql]` / `[db.postgres]` / `[db.tds]`

All three share the same backend config schema. Adding a `[db.mysql]` or `[db.postgres]` section enables that proxy. The TDS proxy is **not yet implemented** — a `[db.tds]` section is accepted, but startup logs a warning and skips it.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | (required) | Connection URL: `mysql://user:pass@host:port/db`, `postgres://...`. |
| `listen` | string | `"127.0.0.1:3306"` (mysql), `"127.0.0.1:5432"` (postgres) | TCP address PHP connects to. |
| `socket` | path | (none) | **Planned — not yet implemented.** Parsed but unused on **both** proxies: the MySQL and PostgreSQL paths each log a "Unix socket listeners are not yet supported" warning at startup and fall back to the TCP `listen` address. |
| `min_connections` | u32 | `2` | Warm pool size (idle connections kept open). |
| `max_connections` | u32 | `20` | Max total backend connections. |
| `idle_timeout` | duration string | `"300s"` | Close idle backend connections after this. |
| `max_lifetime` | duration string | `"1800s"` | Recycle connections older than this. |
| `pool_timeout` | duration string | `"5s"` | Time to wait for a connection before failing. |
| `health_check_interval` | duration string | `"30s"` | Frequency of backend health checks. |
| `inject_env` | bool | `true` | Inject `DB_CONNECTION`, `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`, `DB_PASSWORD`, `DATABASE_URL` into PHP. |
| `reset_strategy` | string | `"smart"` | `"smart"` (reset after non-SELECT), `"always"`, `"never"`. Both proxies frame every session under every strategy, so [query stats](#dbanalysis) coverage does not depend on this knob on either engine. |
| `replicas.urls` | array of strings | `[]` | Read replica URLs. Reads distributed across; writes go to primary. |

> **The proxy listener is unauthenticated — keep it on loopback.**
> The proxy does not validate client credentials. The MySQL proxy reads the client handshake response and discards it; the PostgreSQL proxy answers any startup message with `AuthenticationOk`. The real credentials in `url` are used only for the proxy's own pooled connections to the backend, and are never required of the client.
>
> Binding `listen` to a non-loopback address therefore gives full read/write access to your database to any host that can reach the port. ePHPm logs a startup warning when `listen` is a non-loopback IP literal (`0.0.0.0:3306`, `10.0.0.5:3306`, …), but does **not** refuse to start — binding `0.0.0.0` inside a container that is firewalled by a network policy is a legitimate deployment. Addresses given as hostnames (`localhost:3306`, `db.internal:3306`) are not classified, because that would require a DNS lookup at startup; no warning is logged for them either way.

#### Startup: the upstream does not have to be reachable

The proxy binds `listen` at startup and reaches `url` from a background task,
retrying forever with exponential backoff (250 ms doubling to a 30 s ceiling).
A database that is slower to start than ePHPm — or that restarts later — is
picked up when it appears; no ePHPm restart is needed.

Clients that connect during the first **5 seconds** of that window queue in the
kernel accept backlog rather than getting `ECONNREFUSED`, so PHP's first
request waits a moment and then succeeds. Past 5 seconds the proxy accepts and
immediately closes each client, so callers fail fast: a client whose TCP
connect succeeded would otherwise block reading a server greeting that never
comes, and mysqlnd's read timeout is 24 hours by default — that would pin a PHP
worker until the HTTP request deadline. PHP reports the close promptly
(`Lost connection to MySQL server at 'reading initial communication packet'`).

Two things *are* fatal at startup, because both are configuration errors: a
`url` that cannot be parsed, and a `listen` address that cannot be bound.

#### Readiness and the database proxy

`/_ephpm/ready` reports **503 until every configured proxy has reached its
upstream once**. A process whose proxy has never connected cannot serve a
single query, so it must stay out of load-balancer rotation, and a rollout
containing such a pod should stall rather than replace healthy pods.

After that first success, readiness never flaps on upstream state again. This
is deliberate: gating readiness on live database reachability would fail every
replica's probe at the same instant during a shared-database outage, empty the
Service, and turn a degraded database into a total outage — including for the
static assets and non-DB routes those pods could still serve. Liveness
(`/_ephpm/health`) stays green throughout; restarting the process does not
bring a remote database back.

A post-startup outage is reported instead of routed around:
`ephpm_db_proxy_upstream_up` drops to `0`,
`ephpm_db_proxy_connect_failures_total` climbs, and the proxy logs at ERROR
(throttled to one line per minute). See
[Metrics → Database (proxy upstream health)](/reference/metrics/#database-proxy-upstream-health).

### `[db.sqlite]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `path` | string | `"ephpm.db"` | SQLite database file path. Used in **single-site** mode. In multi-site mode (`[server] sites_dir` set) it is ignored in favour of `dir` (see below). |
| `dir` | string | (none) | **Per-site database directory (multi-site mode).** When `[server] sites_dir` is set, each virtual host gets its **own** database file at `<dir>/<site-key>.db`, opened lazily on that site's first query. This is the tenant-isolation boundary: Turso has no per-schema ACL, so one database file per site is what keeps one tenant's SQL from reading or writing another's data (closes the cross-tenant hole of issue #274). The filename component is the **canonical site key** — the traversal-safe `[a-z0-9._-]` key that also selected the document root, i.e. `Host` normalized (port and trailing dot stripped, lowercased) with `[server] sites_domain_suffix` removed — never a raw `Host` header. One tenant therefore has exactly one database no matter which of its names a client used (issue #290). A well-formed but **unknown** host resolves to `[server] document_root` and gets *no* database context at all — `ephpm_db_*` reports `no per-site database context for this request` rather than creating a file named after the header (issue #291). **Required in multi-site single-node mode: startup fails closed if `sites_dir` is set, `[db.sqlite]` is present, and `dir` is unset** — ePHPm refuses to share one database across tenants. Ignored (with a warning) in single-site mode. Not available in clustered mode (see below). |
| `max_open_dbs` | integer | `256` | Maximum per-site databases held open at once (multi-site mode). Turso holds a file open per database (~3 fds each: `db` + `-wal` + `-shm`); when the cache is full the least-recently-used **idle** site (no in-flight request or live bridge session) is closed to make room, and a later request re-opens it. A site with a live session is never evicted, so this is a **soft** bound — size it with headroom under the process `RLIMIT_NOFILE` (`max_open_dbs × ~3 + server sockets`). |
| `engine` | string | `"turso"` | The embedded SQLite-compatible engine. **`"turso"` is the only accepted value (and the default).** ePHPm embeds the [Turso Database](https://github.com/tursodatabase/turso) engine — a Rust rewrite of SQLite (**Beta upstream**; `VACUUM` and multi-process access unsupported) — through litewire. As of v0.7.0 the rusqlite (genuine SQLite C engine) backend and the sqld sidecar were removed: legacy `engine = "sqlite"` or `engine = "rusqlite"` is now a **hard startup error** with a migration message — it fails closed, never silently falling back. See [Database engines](/architecture/database/engines/) and the [Turso engine roadmap](/roadmap/turso-engine/). |

> **Per-site isolation reaches both paths (multi-site mode).** PHP can use the native `ephpm_db_query()` / `ephpm_db_execute()` functions (see [Using the database from PHP](/guides/db-from-php/)) **or** stock `pdo_mysql`. The MySQL handshake carries no virtual-host name, so the tenant is identified by a per-site credential instead: `DB_USER` is the site key and `DB_PASSWORD` is derived per site, both injected into that site's `$_SERVER` per request, and the connection's database is fixed by the credential it authenticates with — see [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/). Only the MySQL frontend can do this; `hrana_listen` / `postgres_listen` / `tds_listen` are **not started** in multi-site mode. Per-site isolation is **single-node only**; combining multi-site with clustered replication logs a warning and shares the clustered database across tenants.

#### `[db.sqlite.proxy]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mysql_listen` | string | `"127.0.0.1:3306"` | MySQL wire protocol address (PHP connects here with `pdo_mysql`). In multi-site mode this is the single listener every tenant uses, and it **requires per-site credentials** — see the note below. |
| `hrana_listen` | string | (none) | Hrana HTTP API listener. |
| `postgres_listen` | string | (none) | PostgreSQL wire protocol listener. |
| `tds_listen` | string | (none) | TDS (SQL Server) wire protocol listener. |
| `max_connections` | integer | `0` (unlimited) | Cap on concurrent wire connections across the MySQL/PostgreSQL/TDS frontends combined. Beyond the cap, connections are refused at accept time (MySQL clients get error 1040 "Too many connections"), never queued. Each wire session holds one OS thread, so this also bounds those threads. Hrana (stateless HTTP) is not counted. Same semantics as `[db.mysql] max_connections`. |

> **Single-site: these listeners are unauthenticated.** litewire's MySQL, Hrana, PostgreSQL, and TDS frontends accept any client — the PostgreSQL frontend is explicitly wired to a no-op startup handler, and the others never ask for credentials. The design assumes only PHP inside this process reaches them. As with `[db.mysql]`, each of these four keys is checked at startup and a non-loopback IP literal logs a warning naming the risk; startup is not blocked. Bind loopback unless the port is firewalled from untrusted networks.
>
> **Multi-site (`[server] sites_dir` + `[db.sqlite] dir`): `mysql_listen` IS authenticated, and it is the only listener served.** Each virtual host gets its own MySQL account — `DB_USER` is the site key, `DB_PASSWORD` is derived per site from a secret generated in memory at startup — and the connection's database is fixed by the credential it authenticates with. A tenant claiming another site's username without its password gets `ERROR 1698 (28000)`. `hrana_listen`, `postgres_listen`, and `tds_listen` are **not started** in this mode (they cannot bind a database per connection); configuring them logs a warning. Credentials rotate on restart, so read them from `$_SERVER` rather than hard-coding them — see [Multi-tenant `pdo_mysql`](/guides/multi-tenant-pdo-mysql/) for the threat model and the failure modes.

> **Removed in v0.7.0:** the `[db.sqlite.sqld]` block (`http_listen`, `grpc_listen`, `write_permits`) is gone along with the sqld sidecar. Clustered SQLite now replicates in-process via the Turso CDC path (see `[db.sqlite.replication]` below) — there is no sqld child process, no gRPC listener, and no write-admission semaphore. A config that still sets these keys logs a deprecation warning at startup; they have no effect.

#### `[db.sqlite.replication]` (clustered mode only)

Clustered SQLite is **experimental** in v0.7.0: it uses the in-process Turso CDC replication path (`turso_cdc`) over the [cluster channel](#clusterchannel), tested on Linux/macOS. The Turso engine is Beta upstream — treat clustered mode accordingly.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `role` | string | `"auto"` | `"auto"` (gossip-elected), `"primary"`, `"replica"`. |
| `primary_grpc_url` | string | `""` | The primary's **cluster channel address** (e.g. `10.0.0.1:7948` — the channel defaults to the gossip port + 2). Set automatically in `auto` mode; required for `replica`. Replicas tail the primary's `turso_cdc` stream over this address. (The key name is retained for config compatibility; despite the name there is no gRPC — the sqld/gRPC transport was removed in v0.7.0.) |
| `max_snapshot_bytes` | u64 (bytes) | `1073741824` (1 GiB) | Largest snapshot-bootstrap payload a cold replica will accept from the primary. Used by the Turso CDC replication path. Both the length the primary advertises and the running total of received chunks are checked against it, so a peer cannot exhaust the replica's memory by claiming an absurd size or streaming without an end marker. Bootstrap fails with a message naming this knob when a legitimate dump is larger. |

> **Removed in v0.7.0:** the `cdc_experimental` opt-in flag is gone. Clustered mode always uses CDC replication now — there is no per-node flag to set. A config that still sets `cdc_experimental` logs a deprecation warning; it has no effect.

### `[db.read_write_split]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable R/W splitting. Requires backend with `replicas`. |
| `strategy` | string | `"sticky-after-write"` | After a write, reads stick to primary for `sticky_duration`. `"lag-aware"` is parsed but **not yet implemented**. |
| `sticky_duration` | duration string | `"2s"` | How long reads stay on primary after a write. |
| `max_replica_lag` | duration string | `"500ms"` | **Not yet implemented** — parsed but unused. |

### `[db.analysis]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `query_stats` | bool | `true` | Track per-digest timing/throughput metrics. Applies to the embedded SQLite paths **and** the MySQL/PostgreSQL proxies — see the coverage table below. |
| `slow_query_threshold` | duration string | `"1s"` | Queries exceeding this are logged at WARN. |
| `auto_explain` | bool | `false` | **Not yet implemented** — parsed but unused. |
| `auto_explain_target` | string | `"stderr"` | **Not yet implemented**. |
| `digest_store_max_entries` | usize | `100_000` | Max in-memory query digests; oldest evicted on overflow. |
| `metric_label_series_max` | usize | `1000` | Max distinct `digest` label values emitted to Prometheus; overflow folds into `digest="__other__"` (and logs a warning once). **There is no unlimited setting** — admission is `len() < metric_label_series_max`, so `0` admits nothing and folds *every* digest into `digest="__other__"`, effectively turning per-digest labels off. Use a large number if you want a high cap. Internal tracking (`top_queries()`) is unaffected either way and is bounded by `digest_store_max_entries`. |

#### What query stats cover

One collector serves every database path, so proxied and embedded queries land on the same metric names with no extra label to distinguish them. What each path can *see* differs, because the proxy only observes statements at the points where it already parses the wire protocol:

| Path | Statements recorded | Duration measured | Row counts |
|------|--------------------|-------------------|------------|
| Embedded SQLite (`[db.sqlite]`, single-node or clustered) | All queries and mutations | In-process execution time | Rows returned / rows affected |
| MySQL proxy, single-backend path (any reset strategy) | `COM_QUERY` only | Wire round trip: command written to the backend → last response byte read back | Not available |
| MySQL proxy, R/W splitting enabled with replicas | `COM_QUERY` and `COM_STMT_EXECUTE` | Same wire round trip | Rows returned, or affected rows from the `OK` packet — summed across every result set of a multi-result command (`CALL`, multi-statement), which is recorded as one statement |
| PostgreSQL proxy, any reset strategy, with or without replicas | Simple `Query` messages | Wire round trip: message written → `ReadyForQuery` | Rows returned (`DataRow` count); mutations record `0` |

No `reset_strategy` value turns recording off on either engine — every proxy path frames the wire protocol.

Notable gaps, all deliberate:

- **Proxy durations include the network.** The embedded-SQLite numbers are in-process execution time; the proxy numbers are a round trip to your database server. Comparing them directly is comparing two different things.
- **`COM_STMT_PREPARE` is never recorded.** Preparing is a metadata round trip, not an execution — recording it under the statement's digest would publish parse latency as query latency.
- **`COM_STMT_EXECUTE` is invisible on the default MySQL path.** Attributing an execute to its SQL requires having parsed the *prepare response*, which only the R/W-split routing loop does. Applications that turn off PDO's emulated prepares (`PDO::ATTR_EMULATE_PREPARES = false`) will therefore see little or no proxy query-stat traffic unless R/W splitting is on.
- **PostgreSQL extended-protocol executions are invisible.** Messages stay framed, but attributing an `Execute` to the SQL a `Parse` carried means tracking named statements and portals across the session; recording the `Parse` alone would publish planning time as query time, exactly as for `COM_STMT_PREPARE`. Same for `COPY`. A simple `Query` issued later on such a session *is* recorded.
- **MySQL durations on the single-backend path are an upper bound.** That path does not frame the backend→client direction, so completion is inferred from the arrival of the next client command. PostgreSQL has an explicit `ReadyForQuery` marker on every path and is exact everywhere.
- **Rows the proxy cannot count are reported as `0`, never estimated.**

## `[kv]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `memory_limit` | string | `"256MB"` | Max memory for **stored key/value payloads**. Per-connection RESP protocol buffers are NOT counted here — bound those with `[kv.redis_compat]` `max_connections` / `max_input_buffer`. |
| `eviction_policy` | string | `"allkeys-lru"` | `noeviction`, `allkeys-lru`, `volatile-lru`, `allkeys-random`. Case-sensitive; any other value is rejected at startup with an error listing the four valid options. |
| `compression` | string | `"none"` | `none`, `gzip`, `brotli`, `zstd`. |
| `compression_level` | u32 | `6` | 1=fastest, 9=best. |
| `compression_min_size` | usize (bytes) | `1024` | Values below this are stored uncompressed. |
| `secret` | string | (none) | Master secret for per-site RESP AUTH (`HMAC-SHA256(secret, hostname)`). Not auto-generated. **Required to run the RESP listener in multi-tenant mode:** with `sites_dir` set and `[kv.redis_compat] enabled = true`, an unset (or empty/whitespace-only) secret is a **hard startup error** — without it every tenant would share one unauthenticated global store. Single-site deployments (no `sites_dir`) don't need it. |

### `[kv.redis_compat]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable the RESP listener. Off by default. In multi-tenant (`sites_dir`) mode, enabling it **requires** `[kv] secret` so per-site `AUTH <hostname> <derived-password>` scopes each connection to its own site store; without the secret startup fails closed (see `[kv] secret`). Prefer keeping it off in multi-tenant mode and using the per-vhost `ephpm_kv_*` PHP functions. |
| `listen` | string | `"127.0.0.1:6379"` | RESP listener address (TCP only). |
| `socket` | string | (none) | **Not yet implemented** — parsed but unused; startup logs a warning if set. |
| `password` | string | (none) | RESP `AUTH` password. |
| `max_connections` | usize | `1000` | Max concurrent RESP connections; excess clients get `ERR max number of clients reached` (like Redis `maxclients`). `0` = unlimited. |
| `max_input_buffer` | usize (bytes) | `1048576` (1 MiB) | Per-connection input buffer cap (like Redis `client-query-buffer-limit`). Not counted against `[kv] memory_limit`. |
| `idle_timeout_secs` | u64 | `300` | Close RESP connections idle this long, freeing their buffers. `0` = never. |

## `[cluster]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable gossip clustering. |
| `bind` | string | `"0.0.0.0:7946"` | Gossip UDP listener. |
| `join` | array of strings | `[]` | Seed addresses for initial cluster join. |
| `secret` | string | `""` | Shared secret for cluster transport security. When set, gossip UDP and the KV TCP data plane are encrypted and authenticated (ChaCha20-Poly1305, keys derived via HKDF-SHA256); nodes without it cannot join, read, or inject. **Required when `enabled = true`:** an empty secret is a hard startup error unless `allow_insecure_no_auth = true`. |
| `allow_insecure_no_auth` | bool | `false` | Opt in to running clustering with an empty `secret` (unauthenticated plaintext gossip + KV data plane). Off by default so clustering fails closed. Set `true` only on a fully trusted private network with ports 7946/7947 firewalled from untrusted hosts; a loud warning is still logged. Not recommended. |
| `node_id` | string | (auto) | Unique node identifier. Auto-generated if empty. |
| `cluster_id` | string | `"ephpm"` | Nodes with different `cluster_id`s ignore each other. |

### `[cluster.channel]`

**Experimental-adjacent.** The cluster channel is a single,
authenticated, `yamux`-multiplexed TCP listener that opt-in cluster
features share (Turso CDC replication and its snapshot bootstrap
today). It is **only bound when at least one feature asks for it**: a
config that ships no channel feature is byte-identical to a config
without this section — no socket, no task, no startup log noise above
`debug!`. Adding `[cluster.channel]` to a config is not itself an
opt-in; a feature elsewhere (today just clustered SQLite — i.e.
`[db.sqlite]` with clustering enabled, which replicates via Turso CDC)
has to ask. See the
[cluster channel roadmap](/roadmap/cluster-channel/) for the design.

**Security posture.** Connections complete a mutual challenge/response
handshake in which both peers prove possession of the shared secret and
both contribute fresh randomness, so a recorded handshake cannot be
replayed. Both ends then derive a per-connection key from the secret
salted with the handshake transcript, and every subsequent byte —
yamux framing included — is sealed with ChaCha20-Poly1305. Inbound
connections must additionally come from an IP that gossip currently
knows as a cluster member. There is **no TLS and no certificate-based
peer identity**: authentication is "holds the shared cluster secret",
and the membership check is per-host and trusts the TCP source address.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `listen` | string, optional | *(derived: gossip bind IP with port `bind_port + 2` — `+ 2` because the KV data plane already claims gossip + 1 (7947), so defaults land on 7948)* | TCP listen address for the channel. Ignored when no channel feature is enabled. |
| `secret` | string, optional | *(fall back to `[cluster] secret`)* | Shared secret for the channel handshake and per-connection frame keys (distinct HKDF domains from gossip/KV and from each other). When neither this nor `[cluster] secret` is set, the channel refuses to bind — channel features require authentication (fail-closed). |

### `[cluster.kv]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `small_key_threshold` | usize (bytes) | `512` | Boundary between gossip tier and TCP data plane. |
| `replication_factor` | usize | `2` | Replicas for large-tier values. |
| `replication_mode` | string | `"async"` | `"async"` or `"sync"`. |
| `hot_key_cache` | bool | `true` | Promote frequently-fetched remote values to a local cache. |
| `hot_key_threshold` | u32 | `5` | Remote fetches in `hot_key_window_secs` before promotion. |
| `hot_key_window_secs` | u64 | `10` | Window for counting fetches. |
| `hot_key_local_ttl_secs` | u64 | `30` | Max age of cached hot-key values. |
| `hot_key_max_memory` | string | `"64MB"` | Memory budget for hot-key cache. |
| `data_port` | u16 | `7947` | TCP listener for the KV data plane. |

## `[[middleware]]`

Native middleware mounts — repeatable array-of-tables. Each mount resolves
against the **builtin registry first**: the four in-tree modules (`jwt`,
`cors`, `ratelimit`, `security-headers`) are compiled into every binary and
run in-process — no shared library on disk, no `dlopen`. Any other name
loads a shared library (`.so`/`.dylib`/`.dll`) at startup. Loading is
fail-fast: a builtin rejecting its config, an unresolvable library, a
missing ABI symbol, or a failing module `init` aborts server startup. The
chain is evaluated on every PHP-bound request, before the request body is
read. Mounts apply globally, not per vhost — a module can discriminate by
vhost via the request's server name. See the
[Native Middleware guide](/guides/native-middleware/).

**Built-ins work in every binary.** Shared-library mounts (custom
out-of-tree modules) work out of the box with the stock release binaries
on all platforms — the Linux release is glibc-dynamic
(`<arch>-unknown-linux-gnu`), so `dlopen` is available. Only a self-built
fully static musl binary lacks `dlopen` (`Dynamic loading not supported`
at startup) — see the guide's
[dynamic-lane section](/guides/native-middleware/#the-dynamic-lane).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `library` | string | **required** | Builtin name (`jwt`, `cors`, `ratelimit`/`rate-limit`, `security-headers`, or their `ephpm-middleware-*` long forms; `-`/`_` interchangeable), a bare module name resolved through the middleware search path (`<name>.<os>-<arch>.<ext>`, `lib<name>.<ext>`, `<name>.<ext>` in the working directory, `$EPHPM_MIDDLEWARE_DIR`, then `/usr/local/lib/ephpm/middleware`), or an explicit path (any value containing a path separator or file extension). Must not be empty. |
| `match` | string | (none) | Glob the request path must match for the mount to run. `*` matches any character sequence, including `/`. Unset = every PHP-bound request. |
| `order` | u32 | **required** | Chain position; lower runs first. Equal orders keep declaration order. |
| `config` | inline table | (none) | Arbitrary module configuration, serialised to JSON and passed to the module's `init`. |

## `[opcache]`

Governs the cluster-wide OPcache invalidation watcher (Phase 1 of the
[OPcache clustering roadmap](/roadmap/opcache-clustering/)). When enabled,
every PHP request checks `opcache:version:<vhost>` in the in-process KV
store and, when the value has advanced since this node last saw it, runs
`opcache_invalidate()` for every cached script under the vhost's docroot
before executing the request. The lookup is one atomic load plus one
`DashMap::get` — sub-microsecond in the fast path.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `cluster_invalidation` | bool | (auto) | Watch the KV store for invalidation events. Unset defaults to `true` when `[cluster] enabled = true`, `false` otherwise. Applies to fpm mode only (`[php] mode = "fpm"`); worker mode logs a WARN at startup and skips the watcher. |

The companion CLI is `ephpm deploy` / `ephpm cache reset` — both write
the version key via the RESP listener, so `[kv.redis_compat] enabled = true`
is required for the CLI to reach the running server. See the roadmap
page for the wire semantics.

## See also

- [Environment variables](environment-variables/) — how to override any of these via `EPHPM_*`
- [`crates/ephpm-config/src/lib.rs`](https://github.com/ephpm/ephpm/blob/main/crates/ephpm-config/src/lib.rs) — definitive source

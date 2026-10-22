+++
title = "HTTP Endpoints & Headers"
weight = 6
+++

Everything ePHPm answers on its own (rather than handing to your application),
the order in which a request is checked, which request headers ePHPm reads, and
which response headers it adds or changes.

## Exposure: read this first

> **The metrics path and every `/_ephpm/*` endpoint are served for every
> `Host`, ahead of the usual request gates.**
>
> - They are matched **before** `trusted_hosts`, the malformed-Host gate,
>   `hidden_files`, `blocked_paths`, the built-in reverse proxy, the per-site
>   preview gate and the whole `[[middleware]]` chain. None of those can block
>   or protect them. (The per-IP rate limit and the accept-time connection
>   limits do still apply.)
> - They answer on the plain listener, the TLS listener and HTTP/3, and for any
>   `Host` header, including a vhost name.
> - The two data-bearing ones, the metrics path and `/_ephpm/requests`, have
>   their own access control: an IP `allow`/`deny` ACL, a bearer `token`, and
>   (metrics, plus the timeline) a separate `[server.metrics] listen` address
>   with optional mTLS. The ACL is **open by default**, so on a single-site node
>   with no access config they are readable by anyone who reaches the listener.
>   On a **multi-site** node (`[server] sites_dir`) they answer `403` until a
>   token or an mTLS listener is configured. See
>   [access to internal endpoints](/reference/config/#access-to-internal-endpoints).
> - The probes (`/_ephpm/health`, `/ready`, `/primary`) are always open.
>
> What a reader learns:
>
> | Endpoint | Exposes |
> |---|---|
> | metrics path (when `[server.metrics] enabled = true`) | Every tenant's normalized SQL (`ephpm_query_*{digest}`: table, column and query shape, no literal values), vhost names (`ephpm_opcache_invalidations_total{vhost}`), DB proxy upstream `host:port` (`ephpm_db_proxy_*{upstream}`), middleware module names, request and error rates, the binary version (`ephpm_build_info`). |
> | `/_ephpm/requests` (when the request timeline is on) | The last 256 request **paths** (no query string, no headers) with method, status and timings, from **every** vhost in one process-wide buffer with no host field. On by default under `ephpm dev` and bare `ephpm`; off under `ephpm serve`. |
> | `/_ephpm/health`, `/ready`, `/primary` | Liveness, readiness, and whether this node is the writable clustered-SQLite primary. While a DB proxy has never connected, the `/ready` 503 body names its listen address and upstream `host:port`, but only to clients the `[server.diagnostics]` IP ACL admits (everyone by default); others see only the proxy kind. |
>
> **Hardening:**
>
> 1. **Require a credential.** Set `[server.metrics] token` and
>    `[server.diagnostics] token`, or serve both endpoints on an mTLS
>    `[server.metrics] listen` with `client_ca`. On a multi-site node this is
>    mandatory, because tenant PHP can reach every address the scraper can.
> 2. **Move them off the public port.** `[server.metrics] listen = "10.0.0.5:9100"`
>    (or a loopback address) serves the metrics path and `/_ephpm/requests`
>    only there.
> 3. **Restrict by address.** `allow`/`deny` on `[server.metrics]` and
>    `[server.diagnostics]`. Behind a reverse proxy, list it in
>    `[server.security] trusted_proxies`; with an ACL set, forwarded requests
>    from an untrusted peer are refused.
> 4. **Turn off what you do not use.** Leave `[server.metrics] enabled = false`
>    (the default) if nothing scrapes it, and do not set
>    `[server.diagnostics] request_log = true` under `ephpm serve` on a shared
>    or public node.
> 5. **Do not rely on an obscure `path`.** `[server.metrics] path` changes where
>    metrics are served, not who can read them.
>
> Probes and scrapes are exempt from `trusted_hosts` on purpose: Kubernetes and
> Prometheus address pods by IP, so a Host-gated probe would never pass.
>
> See also [Multi-tenant hardening](/guides/multi-tenant-hardening/#server-endpoints-are-not-tenant-scoped).

## Server-owned paths

| Path | Method | Answer | `handler` metric label |
|---|---|---|---|
| `[server.metrics] path` (default `/metrics`) | `GET` only | Prometheus text format, `text/plain; version=0.0.4`. Only when `[server.metrics] enabled = true`, and only on the main listener when `[server.metrics] listen` is unset. Any other method, metrics disabled, or a separate `listen` set, falls through to normal routing, so your application answers it. There is no `405` here, unlike `/_ephpm/*`. `401`/`403` per the [access policy](/reference/config/#access-to-internal-endpoints). | `metrics` |
| `/_ephpm/health` | `GET` | `200 {"status":"ok"}`. Liveness. | `health` |
| `/_ephpm/ready` | `GET` | `200 {"status":"ready"}`, or `503 {"status":"not_ready","reason":"..."}` until PHP is initialized, at least one worker has booted (worker mode), and every configured DB proxy has connected to its upstream once. | `health` |
| `/_ephpm/primary` | `GET` | `200 {"primary":true}` when this node accepts writes, `503 {"primary":false}` on a single-database clustered-SQLite replica. | `health` |
| `/_ephpm/requests` | `GET` | The request timeline as JSON, newest first, when `[server.diagnostics] request_log` is on. Otherwise `404` naming the knob. `401`/`403` per the `[server.diagnostics]` [access policy](/reference/config/#access-to-internal-endpoints); `404` naming `[server.metrics] listen` when that moves it to the metrics listener. | `diagnostics` |
| `/_ephpm/kv/snapshot` | `GET`, `POST` | Warm-restart [KV snapshot](/reference/config/#kvsnapshot) control. `GET`: status JSON (file, last save, last load). `POST`: write a snapshot now — `200` with `{"ok":true,"bytes":…,"keys":…}`, `409` while one is already being written, `500` when the write failed (the previous file is kept). `404` naming `[kv.snapshot] mode` when snapshots are off, for every method. `401`/`403` per the `[server.diagnostics]` [access policy](/reference/config/#access-to-internal-endpoints), checked **before** the method. | `diagnostics` |
| `/_ephpm/kv/snapshot/load` | `POST` | Load the configured snapshot into the live store now; `200` with the load report (`outcome` is `ok`, `partial` or `cold`). Same gate and `404` as above. | `diagnostics` |
| `/_ephpm`, any other `/_ephpm/...` | `GET` | `404`, listing the endpoints above. | `error` |
| any `/_ephpm/...` above | not an allowed method | `405` with `Allow` naming the allowed method(s) (`GET` for the probes and the timeline; `GET, POST` / `POST` for the two snapshot paths). `HEAD` is refused too. | `error` |
| `/_ephpm/auth/...` | any | **Not** answered by the server directly. Dispatched to the global `[[middleware]]` request-phase chain (the `github-auth` issuer's login and callback), *after* `trusted_hosts`, the malformed-Host gate, the hidden-file, manifest and blocked-path gates, and vhost resolution. `404` when no mounted module claims the path. The per-site preview gate does not run here. | `auth` |
| `/.well-known/acme-challenge/<token>` | any | Always intercepted (after `trusted_hosts` and the malformed-Host gate). Serves the KV key `acme:challenge:<token>` if present, else `404`. Nothing in ePHPm writes those keys today: ACME uses TLS-ALPN-01 or DNS-01, and HTTP-01 is not implemented. A side effect is that your application can never serve this path, so a certbot `--webroot` flow that writes files into the document root will not work against ePHPm. | `acme` |

The three probes all record `handler="health"`. Metrics scrapes and timeline
polls record `handler="metrics"` and `handler="diagnostics"`, so a dashboard
that filters out `handler="health"` still counts them.

Requests to `/_ephpm/*` and the metrics path are not written to the
`/_ephpm/requests` timeline. They **are** written to the access log
(`[server.logging] access`) when it is enabled.

## Request processing order

Each request is checked in this order. The first step that answers ends it.

1. **Accept-time limits.** `[server.limits] max_connections` and
   `per_ip_max_connections` are checked against the **raw TCP peer address**
   before any HTTP is parsed. The trusted-proxy headers are not available yet,
   so behind a load balancer every connection counts against the balancer's IP.
2. **`redirect_http`.** With `[server.tls] redirect_http = true`, the plain
   listener answers **every** request with `301` to
   `https://<Host header><path and query>`, before anything below runs. That
   includes `/_ephpm/health`, `/_ephpm/ready`, the metrics path and
   `/.well-known/acme-challenge/*`. Point HTTP-port probes and scrapes at the
   TLS listener instead. The `Location` is built from the client's `Host` header
   as sent (port included). It is not checked against `trusted_hosts`.
3. **`Host` fallback.** An HTTP/2 or HTTP/3 request with no `Host` header gets
   one synthesized from its `:authority`.
4. **Per-IP request rate** (`[server.limits] per_ip_rate`), keyed by the
   effective client IP after trusted-proxy resolution. Over-limit is `429`,
   counted in `ephpm_rate_limited_total` and **not** in
   `ephpm_http_requests_total`.
5. **Request timeout** (`[server.timeouts] request`) is armed around
   everything below. On expiry the answer is `504`.
6. **Path decoding.** Malformed escapes, encoded or literal `/`/`\` tricks,
   `..` segments and invalid UTF-8 are `400`.
7. **Metrics path**, then **`/_ephpm/*`**, as in the table above. The metrics
   path and `/_ephpm/requests` apply their own credential, IP ACL and token
   checks here (`403`/`401`).
8. **`trusted_hosts`** (`[server.request]`). `421` when the `Host` matches no
   entry. Compared case-insensitively, with and without the port, but **not**
   with a trailing dot stripped: `example.com.` gets a `421` even when
   `example.com` is listed, although vhost resolution would have accepted it.
9. **Malformed `Host`** (multi-site only). A host that cannot be a vhost
   directory name is `404`.
10. **ACME HTTP-01 path.**
11. **Hidden files** (`[server.static] hidden_files`), **deploy manifests**
    (`deploy_manifests`), **`blocked_paths`** (`[server.security]`).
12. **Built-in reverse proxy** (`[[server.proxy]]`).
13. **Vhost resolution.** A site whose override file cannot be honoured is
    `503` with `Retry-After: 2`.
14. **`/_ephpm/auth/*`** to the middleware chain.
15. **WebSocket upgrade** (when `[server.websocket]` is enabled).
16. **Fallback resolution** to a PHP script, a static file, or a status code.
    The PHP path then applies `allowed_php_paths`, the PHP ETag cache, the
    per-site rate limit (`429` + `Retry-After`), request-phase middleware, the
    body-size limit and dispatch. The static path runs request-phase middleware
    and serves the file.
17. **Response.** `[server.response] headers`, then response-phase middleware,
    then `Alt-Svc` and `X-Ephpm-Preview`.

## Request headers ePHPm reads

| Header | Used for |
|---|---|
| `Host` (or HTTP/2 and HTTP/3 `:authority` when `Host` is absent) | `trusted_hosts`, vhost resolution (port and trailing dot stripped, lowercased, `sites_domain_suffix` removed), `$_SERVER['SERVER_NAME']`, the `redirect_http` `Location`. |
| `X-Forwarded-For` | Client IP (`REMOTE_ADDR`, per-IP rate limit, access log), **only** when the TCP peer is inside `[server.security] trusted_proxies`. Every `X-Forwarded-For` line is read, in order, as one list, walked right to left; the first address that is not a trusted proxy wins. If every address is trusted, the leftmost is used. An entry that is not a bare IP address (for example `ip:port`) stops the walk, and the TCP peer is used instead. IPv4-mapped peers match IPv4 `trusted_proxies` entries. |
| `X-Forwarded-Proto` | `$_SERVER['HTTPS']`, only from a trusted proxy. When present, it **replaces** the connection's own TLS status: any value other than `https` turns `HTTPS` off, even on a native TLS connection. When absent, the connection's TLS status stands. |
| `Accept-Encoding` | Brotli is chosen when the client accepts `br`, else gzip, else identity. |
| `If-None-Match` | Static-file `304`s, and the PHP ETag cache. |
| `Upgrade` / `Connection` / `Sec-WebSocket-*` | WebSocket upgrades. |
| `Authorization` | Decoded into `AUTH_TYPE`, `PHP_AUTH_USER`, `PHP_AUTH_PW` and `PHP_AUTH_DIGEST`. Not verified. See [HTTP Authentication Variables](/architecture/http/#http-authentication-variables). |
| `traceparent` | Parent of the OTLP request span (binaries built with the `otlp` feature). |

**Not honoured:** `X-Forwarded-Host`, `X-Forwarded-Port`, and RFC 7239
`Forwarded`. Vhost resolution and `SERVER_NAME` always use `Host`. `If-Modified-Since`
and `Range` are ignored too (see static files below).

### Running behind a load balancer or reverse proxy

When nginx, HAProxy, a cloud load balancer or Cloudflare sits in front of
ePHPm:

- **Set `[server.security] trusted_proxies`** to the proxy's address ranges.
  Without it, every request appears to come from the proxy. `REMOTE_ADDR` is
  wrong, and `per_ip_rate` throttles all your traffic as a single client.
- **`per_ip_max_connections` cannot see through the proxy.** It is enforced at
  accept time against the TCP peer, so with a proxy in front it caps the
  **total** connections from that proxy. `[server] preview = true` sets it to
  `32`, which then caps the whole site at 32 concurrent connections. Set
  `per_ip_max_connections = 0` explicitly (explicit values beat the preview
  preset) and enforce per-client connection limits at the proxy.
- **Make sure the proxy sends `X-Forwarded-Proto`** if it terminates TLS and
  you serve PHP that checks `HTTPS`. A proxy that forwards `http` to a TLS
  listener turns `HTTPS` off.
- **Pass the original `Host` through.** ePHPm does not read
  `X-Forwarded-Host`.
- **Block the metrics path and `/_ephpm/requests` at the proxy** for public
  traffic, or protect them with a token (see
  [Exposure](#exposure-read-this-first)). If you set an `allow`/`deny` ACL on
  them, the proxy must be in `trusted_proxies`: a forwarded request from an
  untrusted peer is refused.

The built-in `[[server.proxy]]` is the opposite direction (ePHPm proxying to a
backend); see [Reverse Proxy](/guides/reverse-proxy/).

## Response headers ePHPm adds or changes

| Header | When |
|---|---|
| `Alt-Svc: h3=":<port>"; ma=<seconds>` | Every response on a TLS connection (including HTTP/3 itself), except the per-IP rate-limit `429`, once the HTTP/3 socket has bound (`[server.http3] enabled = true`). `ma` is `[server.http3] alt_svc_max_age` (default `86400`); `0` suppresses the header. Never on plain HTTP. |
| `X-Ephpm-Preview: 1` | Every response the router produces when `[server] preview = true`, including errors, `429`s, `504`s and the internal endpoints. |
| `Retry-After` | `1` on a shed `503` (`[php] overload = "shed"`); `2` on a site-override `503`; the bucket refill time on a per-site `429`. |
| `Allow: GET` | `405` from `/_ephpm/*`. |
| `Content-Encoding: br` or `gzip`, plus `Vary: Accept-Encoding` | Compressed PHP and static responses (see [Compression](/architecture/http/#compression)). |
| `ETag`, `Cache-Control` | Static files; see below. |
| `[server.response] headers` | See below. Not applied everywhere. |

ePHPm sends no `Server` header. A custom or disabled `Server` header
(`server.response.server_header`) is planned, not implemented.

**`X-Powered-By`.** ePHPm does not override PHP's `expose_php`, whose built-in
default is `On`, so PHP responses normally carry `X-Powered-By: PHP/<version>`.
To drop it, set `[php] ini_overrides = [["expose_php", "Off"]]`, or remove the
header with the `header_transform` middleware.

### `[server.response] headers`

These are **set**, not appended. A configured header **replaces** a header of
the same name that your application or a static file already set. For example, a
global `Cache-Control` or `Content-Security-Policy` here overrides the value
your PHP code chose.

They are applied to PHP responses (including the per-site `429`, shed `503`
and `413`s on the PHP path), static files, the fallback-status responses,
reverse-proxy responses (not a `101`), WebSocket upgrade errors (not a `101`),
and the site-override `503`. They are **not** applied to:

- the metrics path and every `/_ephpm/*` response;
- `/.well-known/acme-challenge/*` responses;
- the early gate responses: `400` path decoding, `421` `trusted_hosts`, `404`
  malformed Host, `403` hidden file, deploy manifest and `blocked_paths`;
- the per-IP `429` and the request-timeout `504`;
- `304`s served from the PHP ETag cache;
- `/_ephpm/auth/*` responses.

If you rely on this knob for HSTS or CSP, those responses go out without it.
Response-phase middleware (such as `security_headers`) does not run on them
either. Set security headers at your edge proxy if they must be on every
response, errors included.

### Static files

- **Compression.** Files up to 1 MiB that are read from disk are Brotli- or
  gzip-compressed when the type is compressible and the size is at least
  `[server.response] compression_min_size`. Files served from the open-file
  cache (`[server.file_cache]`, off by default) are offered **gzip only**,
  never Brotli — and with the cache on, that includes the first read of a file
  up to 1 MiB, which is served from the entry it just created. Files
  larger than 1 MiB are streamed from disk uncompressed.
- **`ETag`.** Weak in every case. Files up to 1 MiB read from disk get a
  content hash (`W/"<16 hex>"`). Files larger than 1 MiB, and anything served
  from the file cache, get `W/"<mtime-secs hex>-<size hex>"`.
  `If-None-Match` is evaluated on `GET`/`HEAD` with RFC 9110 weak comparison
  (`W/"x"` matches `"x"`). `[server.static] etag = false` turns off the
  `ETag` header **and** the `304` on every static path (small, streamed and
  file-cache). It does not affect the PHP ETag cache, which has its own
  switch (`[server.php_etag_cache] enabled`).
- **No range requests and no `Last-Modified`.** There is no `Accept-Ranges`,
  no `206 Partial Content` and no `If-Modified-Since` handling, so interrupted
  downloads cannot resume and video seeking re-downloads from the start. Serve
  large media from a CDN or object store if that matters.
- **`Cache-Control`** comes from `[server.static] cache_control` (empty = not
  sent).

## See also

- [Configuration → `[server.metrics]`](/reference/config/#servermetrics) and [the reserved namespace](/reference/config/#the-_ephpm-namespace-is-reserved)
- [Metrics](/reference/metrics/) for every series the metrics path exposes
- [HTTP Server Architecture](/architecture/http/)

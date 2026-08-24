+++
title = "Reverse proxy"
weight = 20
+++

ePHPm has a **built-in HTTP reverse proxy**. An ordered list of rules matches a request on **host** and **path** and forwards it to a single upstream, streaming both directions. It is configured entirely in `[[server.proxy]]` — no nginx, no Caddy, one fewer moving part.

It is deliberately a **single-hop forwarder, not an edge load balancer**: one upstream per rule, no pool, no health checks, no retries. Everything on this page is implemented and covered by tests; the "not in v1" list at the end says plainly what it does *not* do.

## Why it exists

**1. Multi-PHP-version hosting.** ePHPm runs one PHP version per process. Put an edge instance in front that routes **by host** to version-pinned backends, and you get multi-version hosting with clean URLs — no ports in the URL, no SNI hacks, TLS terminated once at the edge:

```toml
# The edge instance. It runs no app itself; it just routes.
[server]
listen = "0.0.0.0:443"

[server.tls]
cert = "/etc/ephpm/fullchain.pem"
key  = "/etc/ephpm/privkey.pem"

[[server.proxy]]
host = "pr-a.preview.example.com"
upstream = "http://127.0.0.1:9084"   # an ephpm built against PHP 8.4

[[server.proxy]]
host = "pr-b.preview.example.com"
upstream = "http://127.0.0.1:9085"   # an ephpm built against PHP 8.5
```

Each backend is an ordinary ePHPm process listening on loopback; the edge terminates TLS and forwards plaintext to it.

**2. Strangler migration.** Route **by path** to a legacy backend and move an app onto ePHPm one route at a time — the [strangler-fig pattern](https://martinfowler.com/bliki/StranglerFigApplication.html):

```toml
[[server.proxy]]
path = "/api"                        # still served by the legacy app...
upstream = "http://127.0.0.1:8000"
# ...everything else falls through to local ePHPm serving (static + PHP).
```

Because a matched rule short-circuits local serving and an unmatched request falls through, you can migrate route by route: add local handlers for the paths you've ported, leave the `/api` rule pointing at the legacy backend until it's empty.

## How matching works

Rules are tried **top-to-bottom; the first match wins.** Put specific rules before general ones.

* **`host`** — `"app.example.com"` (exact, case-insensitive), `"*.example.com"` (wildcard over exactly one leftmost label), `".example.com"` (suffix: the apex and any subdomain), or `"*"`/omitted (any host).
* **`path`** — a segment-aware prefix by default: `/api` matches `/api`, `/api/`, and `/api/v1` but **not** `/apiary`. Set `path_exact = true` to require an exact match. The default `"/"` matches everything.

A matched rule replaces **all** local serving for that request — static files, PHP execution, and native WebSocket termination.

## Where the proxy sits relative to security

The proxy is checked **after** the global security gates, so those gates always win:

* `[server.request] trusted_hosts` (Host allow-list → `421`) runs first.
* `[server.security]` hidden-file blocking and `blocked_paths` (→ `403`) run first.

A proxy rule can therefore never be used to reach a path an operator has blocked. If you need the opposite precedence for a specific deployment, that is a documented v2 knob — not a v1 default.

## Streaming, timeouts, and failures

* **Streaming both ways.** Request bodies (uploads) stream to the upstream unbuffered; response bodies (SSE, large downloads) stream back unbuffered.
* **`connect_timeout_secs`** (default 5) bounds the TCP connect. A rule that matches an **unreachable** upstream returns `502 Bad Gateway` within this bound — never a hang.
* **`read_timeout_secs`** (default 60) bounds only the time to receive the response **head** (time-to-first-byte). It does **not** bound the streamed body, so SSE and long downloads are safe. The outer `[server.timeouts] request` ceiling still applies to the whole request.

## WebSockets

A WebSocket upgrade on a matched rule is **tunnelled** to the upstream — a raw bidirectional byte copy after the `101`. This is distinct from ePHPm's native WebSocket *termination* (`[server.websocket]`), which parses frames and runs PHP per event. Use the proxy tunnel when the upstream owns the WebSocket app; use native termination when *this* instance is the WebSocket app.

## Forwarded headers

The upstream sees a correct client view:

| Header | Value |
|--------|-------|
| `Host` | **preserved** — the client's original `Host`, unchanged |
| `X-Forwarded-For` | the resolved client IP (the inbound chain has been collapsed per `[server.security] trusted_proxies`) |
| `X-Forwarded-Proto` | `https` or `http`, the client-facing scheme |
| `X-Forwarded-Host` | the client's original `Host` |

Hop-by-hop headers (`Connection`, `Transfer-Encoding`, …) are stripped in both directions, per RFC 7230.

## Not in v1

Out of scope, and enforced as such — these either error at startup or simply do not exist:

* **Load balancing / multiple upstreams per rule**, health checks, retries, circuit breaking, dynamic upstreams, canary/percentage routing.
* **Response-body or redirect rewriting** (`sub_filter`-style).
* **`https://` upstreams.** A `https://` upstream is a **startup error**. Terminate TLS at the edge instance and proxy plaintext to a loopback backend.
* **Request-path rewriting.** The original request path is forwarded **unchanged**; there is no prefix strip/prepend. A path/query/fragment in the `upstream` URL is a **startup error**. Strangler backends that serve under the same prefix (`/api` → a backend that also serves `/api`) work without rewriting.

These are candidates for a future version.

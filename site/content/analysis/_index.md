+++
title = "Analysis"
type = "docs"
weight = 9
+++

This directory contains research and analysis of the competitive landscape and key technologies relevant to building ePHPm — an all-in-one PHP application server.

No single existing product covers the full ePHPm feature set. The closest competitors are FrankenPHP, RoadRunner, and Swoole/OpenSwoole.

---

## Documents

- **[FrankenPHP](frankenphp/)** — Caddy integration, CGO/SAPI embedding, worker suspend/resume mechanism
- **[RoadRunner](roadrunner/)** — Goridge IPC, plugin architecture, PSR-7 requirements
- **[Swoole](swoole/)** — coroutine runtime, connection pooling, invasiveness
- **[Caddy](caddy/)** — why it was considered and rejected, lessons learned, Rust equivalents
- **[CertMagic](certmagic/)** — TLS & ACME — CertMagic (Go) vs Rust stack (rustls, rustls-acme, instant-acme)
- **[Laravel Octane](laravel-octane/)** — adapter layer, not a server, backend comparison
- **[Popularity](popularity/)** — GitHub stars, Docker pulls, Packagist downloads, business adoption, estimated MAU

---

## Feature Gap Matrix

| Feature | FrankenPHP | RoadRunner | Swoole | ePHPm Goal |
|---|---|---|---|---|
| HTTP serving (no nginx) | Yes | Yes | Yes | Yes |
| PHP execution (no php-fpm) | Yes | Yes | Yes | Yes |
| Auto TLS (Let's Encrypt) | **Yes** (Caddy) | Partial | No | Yes |
| Prometheus metrics | Yes | **Yes** (best) | Yes | Yes |
| DB connection pooling | **No** | **No** | **Yes** | Yes |
| In-memory KV / sessions | **No** | Yes (single node) | Yes (single process) | Yes + clustering |
| Multi-node clustering | **No** | **No** | **No** | Yes |
| Debug / profiling UI | **No** | **No** | **No** | Yes |
| Query digest / analysis | **No** | **No** | **No** | Yes |
| Slow query + EXPLAIN | **No** | **No** | **No** | Yes |
| Request debug mode | **No** | **No** | **No** | Yes |
| Built-in OTLP collector | **No** | **No** | **No** | Yes |
| Full-stack auto-instrumentation | **No** | **No** | **No** | Yes |
| Single binary, all platforms | Yes | Yes | No (Linux) | Yes |

---

## PHP-Side Invasiveness Comparison

| | Superglobals | Sessions | PSR-7 Required | Packages Needed | Existing App Compat |
|---|---|---|---|---|---|
| FrankenPHP classic | Work | Work | No | None | Drop-in |
| FrankenPHP worker | Work | With care | No | None | High |
| RoadRunner | Broken | Broken | **Yes** | 2-3 packages | Low-Medium |
| Swoole | Broken | Broken | No (own API) | PECL ext | Low |

---

## Key Market Gaps (ePHPm Opportunities)

1. **Database connection pooling in Go-based servers** — Neither FrankenPHP nor RoadRunner has this. RoadRunner has a years-old open feature request that was never built. Only Swoole has it, but requires a C extension and is Linux-only.

2. **Multi-node clustered KV store** — All three rely on external Redis or Kubernetes. None has built-in gossip protocol, peer discovery, or distributed cache.

3. **On-demand production profiling** — No server supports token-gated profiling/cachegrind via request headers with results surfaced in a web UI.

4. **SQL-layer intelligence** — No server intercepts and analyzes SQL traffic (query digests, slow query identification, auto-EXPLAIN). This requires control of the DB proxy layer.

5. **Request-level debug capture** — No server captures per-request data (queries, cache hits, session data, timing) at the infrastructure level. Framework-level tools (Laravel Debugbar, Symfony Profiler) exist but only work within their respective frameworks.

# ePHPm Analysis Overview

This directory contains research and analysis of the competitive landscape and key technologies relevant to building ePHPm — an all-in-one PHP application server.

No single existing product covers the full ePHPm feature set. The closest competitors are FrankenPHP, RoadRunner, and Swoole/OpenSwoole.

---

## Documents

| File | Description |
|---|---|
| [frankenphp.md](frankenphp.md) | FrankenPHP deep dive — Caddy integration, CGO/SAPI embedding, worker suspend/resume mechanism |
| [roadrunner.md](roadrunner.md) | RoadRunner deep dive — Goridge IPC, plugin architecture, PSR-7 requirements |
| [swoole.md](swoole.md) | Swoole/OpenSwoole deep dive — coroutine runtime, connection pooling, invasiveness |
| [caddy.md](caddy.md) | Caddy server — why it was considered and rejected, lessons learned, Rust equivalents |
| [certmagic.md](certmagic.md) | TLS & ACME — CertMagic (Go) vs Rust stack (rustls, rustls-acme, instant-acme) |
| [laravel-octane.md](laravel-octane.md) | Laravel Octane — adapter layer, not a server, backend comparison |
| [ephpm-architecture.md](ephpm-architecture.md) | ePHPm architecture decisions — build options, recommended stack, key libraries |
| [popularity.md](popularity.md) | GitHub stars, Docker pulls, Packagist downloads, business adoption, estimated MAU |
| [monetization.md](monetization.md) | Competitor revenue models, monetization strategy options for ePHPm |

---

## Feature Gap Matrix

| Feature | FrankenPHP | RoadRunner | Swoole | ePHPm Goal |
|---|---|---|---|---|
| HTTP serving (no nginx) | Yes | Yes | Yes | Yes |
| PHP execution (no php-fpm) | Yes | Yes | Yes | Yes |
| Auto TLS (Let's Encrypt) | **Yes** (Caddy) | Partial | No | Yes |
| Prometheus metrics | Yes | **Yes** (best) | Yes | Yes + Admin UI |
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

3. **Integrated observability dashboard** — No server ships a built-in profiling dashboard, query inspector, or admin panel. All punt to external tools (Grafana, Blackfire, Xdebug).

4. **On-demand production profiling** — No server supports token-gated profiling/cachegrind via request headers with results surfaced in a web UI.

5. **SQL-layer intelligence** — No server intercepts and analyzes SQL traffic (query digests, slow query identification, auto-EXPLAIN). This requires control of the DB proxy layer.

6. **Request-level debug capture** — No server captures per-request data (queries, cache hits, session data, timing) at the infrastructure level. Framework-level tools (Laravel Debugbar, Symfony Profiler) exist but only work within their respective frameworks.

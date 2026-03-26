# AGENTS.md — ePHPm

## What Is ePHPm?

ePHPm is an **all-in-one PHP application server** written in **Rust**. It embeds the PHP interpreter directly via zero-cost C FFI into a single binary and wraps it with infrastructure features that no existing PHP server provides: database connection pooling, a clustered in-memory KV store, an observability dashboard, SQL query analysis, and on-demand production profiling.

The name is "ePHPm" — think of it as "enhanced PHP manager."

## Why It Exists

The PHP ecosystem has three main app servers — FrankenPHP, RoadRunner, and Swoole. All three solve PHP execution, but **none** provides:

- Database connection pooling (Go-based servers have zero support; only Swoole has it, but it's Linux-only and requires a C extension)
- Multi-node clustered KV/cache (all rely on external Redis)
- Built-in admin dashboard or profiling UI
- SQL-layer intelligence (query digests, slow query detection, auto-EXPLAIN)
- Request-level debug capture at the infrastructure level
- On-demand production profiling gated by request headers

ePHPm fills all of these gaps in a single binary.

## Architecture Decision

The project is written in **Rust**. It does NOT use Caddy, FrankenPHP, or any Go-based framework. It references FrankenPHP's SAPI code and Pasir's ext-php-rs integration as study guides for the PHP embedding layer.

### Why Rust?

- **Zero-cost C FFI** — Go's CGO incurs ~200ns overhead per call. FrankenPHP crosses the CGO boundary 11+ times per request (~2.2μs+ overhead). Rust FFI to libphp is zero overhead. This is ePHPm's primary competitive advantage.
- **No GC** — Predictable p99 latencies. Critical for the DB proxy.
- **Lower memory per connection** — matters at scale for both HTTP and DB proxy.
- **Memory safety** — unlike C, safe for parsing untrusted wire protocols (DB proxy, HTTP).

### Why Not Go?

CGO overhead on every PHP call, GC pauses affecting tail latency. ePHPm's scope extends far beyond HTTP serving (DB proxy, clustered KV, StatsD server, admin UI), so it needs full control of the binary and runtime.

### Why Not C?

PHP embedding is trivial in C but only ~10% of the project. The other 90% (HTTP stack, TLS, DB proxy, clustering, admin UI) has a terrible ecosystem in C and no memory safety.

## High-Level Architecture

### Serving Node (`ephpm serve`)

```
┌──────────────────────────────────────────────────────┐
│              ePHPm Serving Node                      │
├─────────────┬───────────────┬────────────────────────┤
│  HTTP Layer │  DB Proxy     │  OTLP Receiver         │
│  (hyper /   │  (MySQL/PG)   │  (gRPC :4317 /         │
│   tokio)    │  + query      │   HTTP :4318)          │
│  + HTTP/2   │    digest     │                        │
│  + QUIC     │  + slow query │                        │
├─────────────┼───────────────┼────────────────────────┤
│  ACME TLS   │  Clustered KV │  Node API :9090        │
│ (rustls-    │  (gossip +    │  (metrics, traces,     │
│  acme)      │   hash ring)  │   status, config)      │
├─────────────┴───────────────┴────────────────────────┤
│  PHP Embedding (Rust FFI + libphp + custom SAPI)     │
├──────────────────────────────────────────────────────┤
│  Observability Pipeline + Debug / Profiling          │
└──────────────────────────────────────────────────────┘
```

### Admin UI (`ephpm admin` — separate mode)

```
ephpm admin --nodes 10.0.1.1:9090,10.0.1.2:9090
  ├── Web UI :8080 (cluster overview, traces, queries, KV, profiling)
  └── Aggregates data from all Node APIs

# Or embedded for dev: ephpm serve --admin
```

## Core Subsystems

### 1. PHP Embedding Layer (Rust FFI + libphp)
- Embeds PHP via `libphp` compiled with `--enable-zts --enable-embed`
- Implements a custom SAPI — handles `echo`, `header()`, POST body reading, superglobal population
- Zero-cost C FFI — no CGO overhead (ePHPm's key competitive advantage over FrankenPHP)
- Superglobals (`$_GET`, `$_POST`, `$_SERVER`) work — this is critical for adoption
- Reference implementations: FrankenPHP's `frankenphp.c`, Pasir's ext-php-rs integration, `ripht-php-sapi` crate

### 2. HTTP Layer
- Built on `hyper` + `tokio` async runtime
- HTTP/2 via `hyper` (built-in)
- HTTP/3 (QUIC) via `quinn`
- Worker mode: boot PHP app once, handle requests in a loop (same model as FrankenPHP worker mode)

### 3. Automatic TLS
- Uses `rustls` + `rustls-acme` for automatic ACME certificate provisioning
- Let's Encrypt, ZeroSSL, any ACME CA
- Cert storage backed by ePHPm's own clustered KV for cert sharing across nodes

### 4. Database Connection Pooling / Proxy
- Acts as a MySQL/PostgreSQL wire-protocol proxy between PHP and the real database
- Maintains persistent connection pools to the database
- PHP connects to `localhost:3306` (the proxy) instead of the real DB
- Enables query digest, slow query detection, and auto-EXPLAIN

### 5. Clustered KV Store
- In-memory key-value store with multi-node replication
- Gossip protocol via `chitchat` (Quickwit's gossip lib) or custom SWIM implementation for peer discovery and membership
- Consistent hashing via `hashring` crate for key distribution
- Replaces external Redis for sessions, cache, and cert sharing

### 6. Node API (always present on every serving node)
- Lightweight HTTP/gRPC API on `:9090` (configurable)
- Exposes: `/health`, `/metrics` (Prometheus), `/api/workers`, `/api/db/digests`, `/api/db/slow`, `/api/db/pool`, `/api/kv/stats`, `/api/kv/cluster`, `/api/traces`, `/api/profiling`, `/api/config`
- Auth: shared secret (bearer token) or mTLS
- Consumes negligible resources — this is NOT the admin UI, just a data API
- Prometheus scrapes this. Admin UI consumes this. OTLP exports through this.

### 7. Admin UI (separate mode, not embedded)
- Runs as `ephpm admin --nodes 10.0.1.1:9090,...` (standalone) or `ephpm serve --admin` (embedded for dev)
- Same binary, different subcommand — no separate build artifact
- Connects to Node API on each serving node, aggregates data across the cluster
- Displays: cluster overview, worker pools, query digests, slow query log, trace viewer, KV cluster health, profiling results, debug captures, live config
- In production: runs on a small dedicated box/container, not on serving nodes
- Auth: username/password (separate from Node API auth), SSO in enterprise tier

### 8. Debug / Profiling
- Token-gated: send a secret header with your HTTP request to enable profiling for that request only
- Captures per-request data: SQL queries, cache hits, session data, timing
- Xdebug/cachegrind integration
- Results surfaced in the admin UI

## Key Rust Crates

| Component | Crate |
|---|---|
| Async runtime | `tokio` |
| HTTP/1.1 + HTTP/2 | `hyper` |
| HTTP/3 (QUIC) | `quinn` |
| TLS | `rustls` |
| Automatic ACME TLS | `rustls-acme` |
| PHP embedding | Rust FFI + libphp (reference: `ext-php-rs`, `ripht-php-sapi`) |
| Cluster membership | `chitchat` (Quickwit's gossip lib) or custom SWIM |
| Consistent hashing | `hashring` |
| MySQL protocol | `sqlparser-rs` (query parsing), custom wire protocol |
| Prometheus metrics | `prometheus` crate |
| Static PHP builds | `crazywhalecc/static-php-cli` (for CI) |
| Embedded static assets | `rust-embed` |
| CLI | `clap` |
| Configuration | `toml` / `serde` |

## PHP Worker Model (How It Works)

The worker lifecycle is:
1. PHP app boots once (framework, config, routes — the expensive part)
2. Worker blocks on a tokio channel / condvar, waiting for a request
3. HTTP request arrives → Rust dispatches to an idle worker
4. Worker wakes up, SAPI repopulates superglobals from the new request
5. PHP callback runs, response is written back through the SAPI
6. Completion signaled back to the HTTP task via oneshot channel
7. Worker loops back to step 2

Key design:
- Each PHP worker runs on a dedicated OS thread (PHP's ZTS requires this)
- Async HTTP layer (tokio) communicates with worker threads via channels
- No `runtime.LockOSThread()` hacks — Rust gives direct thread control
- PHP's ZTS thread model maps naturally to Rust's ownership/Send+Sync model

## Competitive Positioning

ePHPm's unique value is the combination of features that no competitor has:
- **vs FrankenPHP**: Zero-overhead PHP embedding (no CGO tax), no GC pauses, plus DB pooling, clustered KV, admin dashboard, query analysis, debug/profiling UI. FrankenPHP has none of these infrastructure features and pays ~2.2μs+ CGO overhead per request.
- **vs RoadRunner**: Zero-overhead PHP embedding, DB pooling, multi-node KV clustering, admin dashboard. Plus superglobals work (RoadRunner breaks them, requires PSR-7).
- **vs Swoole**: Cross-platform (not Linux-only), doesn't require a PECL extension, superglobals work. Swoole has DB pooling but ePHPm matches that and adds clustering + observability.
- **vs all three**: No GC pauses, lower memory footprint, predictable p99 latencies. Benchmarkable advantages.

The critical adoption advantage: **existing PHP apps work without code changes** (superglobals, sessions, `echo` — all work through the custom SAPI).

## Project Status

The project has a scaffolded Cargo workspace with four crates. The code compiles (stub mode — no libphp linked yet). The next step is to build `libphp.a` via `static-php-cli` and wire up the FFI bindings in `ephpm-php`.

## Repository Structure

```
crates/
├── ephpm/           # Binary crate — CLI (clap), config loading, server boot
├── ephpm-config/    # Config structs + figment TOML loading
├── ephpm-php/       # PHP embedding — SAPI callbacks, request/response mapping
└── ephpm-server/    # HTTP server — hyper + router + static file serving
```

Key files:
- `Cargo.toml` — Virtual workspace manifest
- `ephpm.toml` — Example configuration file
- `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `deny.toml` — Tooling config
- `.github/workflows/ci.yml` — Lint, test, deny checks
- `.github/workflows/release.yml` — Build matrix (PHP 8.3/8.4 × linux/mac)

## Documentation

### `docs/architecture/` — ePHPm design decisions

| File | What It Covers |
|---|---|
| `ephpm-architecture.md` | Language decision, PHP embedding strategy, SAPI callbacks, MVP specification, repository structure, full architecture |
| `implementation.md` | Implementation details and subsystem design |

### `docs/analysis/` — Competitive research

| File | What It Covers |
|---|---|
| `overview.md` | Index/hub — feature gap matrix, PHP-side invasiveness comparison, key market gaps |
| `frankenphp.md` | Deep dive — Caddy integration, CGO/SAPI embedding, worker suspend/resume mechanism, superglobal repopulation |
| `roadrunner.md` | Goridge IPC, plugin architecture, PSR-7 requirements, feature list |
| `swoole.md` | Coroutine runtime, connection pooling, invasiveness, Linux-only limitation |
| `caddy.md` | Module system, xcaddy build tool, automatic TLS capabilities |
| `certmagic.md` | Standalone ACME/TLS library, storage interface, no Caddy dependency |
| `laravel-octane.md` | Adapter layer (not a server), backend performance benchmarks |

## For the Next Agent

1. **Start with `docs/analysis/overview.md`** for the big picture, then read `docs/architecture/ephpm-architecture.md` for the chosen approach. Read `docs/analysis/frankenphp.md` if you need deep technical context on SAPI embedding and the worker suspend/resume mechanism.
2. **The project is written in Rust.** Standalone binary with `rustls-acme` for automatic TLS. Do not build on Caddy or any Go framework.
3. **Superglobal compatibility is non-negotiable.** The SAPI must populate `$_GET`, `$_POST`, `$_SERVER`, etc. This is the #1 adoption advantage over RoadRunner and Swoole.
4. **Zero-cost FFI to libphp is the key differentiator.** This is the core competitive advantage over FrankenPHP's CGO approach. Every design decision should preserve this.
5. **Code exists in stub mode.** The Cargo workspace compiles without libphp. The SAPI callbacks in `crates/ephpm-php/src/sapi.rs` need to be converted to `extern "C"` functions and registered with PHP when libphp is linked.
6. **Next milestone:** Build `libphp.a` using `static-php-cli`, configure `build.rs` to run `bindgen` against PHP headers, and replace the stub `PhpRuntime::execute()` with real FFI calls.
